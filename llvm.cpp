#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>

extern "C" {

struct llvm_mutex {
  std::atomic<uint32_t> futex_word;
};

int llvm_mutex_init(llvm_mutex *mutex);
int llvm_mutex_destroy(llvm_mutex *mutex);
int llvm_mutex_lock(llvm_mutex *mutex);
int llvm_mutex_unlock(llvm_mutex *mutex);
void llvm_mutex_reset(llvm_mutex *mutex);

struct llvm_old_waiter {
  llvm_old_waiter *next;
  std::atomic<uint32_t> futex_word;
};

struct llvm_old_condvar {
  llvm_mutex qmtx;
  llvm_old_waiter *head;
  llvm_old_waiter *tail;
};

enum llvm_old_waiter_state : uint32_t {
  LLVM_OLD_WAITING = 0,
  LLVM_OLD_SIGNALLED = 1,
};

static int llvm_old_futex_wait_private(volatile uint32_t *addr,
                                       uint32_t expected) {
  for (;;) {
    int rc = static_cast<int>(syscall(SYS_futex, const_cast<uint32_t *>(addr),
                                      FUTEX_WAIT | FUTEX_PRIVATE_FLAG, expected,
                                      nullptr, nullptr, 0));
    if (rc == 0) {
      if (__atomic_load_n(const_cast<const uint32_t *>(addr), __ATOMIC_RELAXED) !=
          expected) {
        return 0;
      }
      continue;
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno == EAGAIN &&
        __atomic_load_n(const_cast<const uint32_t *>(addr), __ATOMIC_RELAXED) !=
            expected) {
      return 0;
    }
    return rc;
  }
}

static void llvm_old_futex_wake_op_private(volatile uint32_t *uaddr,
                                           volatile uint32_t *uaddr2,
                                           uint32_t op) {
  (void)syscall(SYS_futex, const_cast<uint32_t *>(uaddr),
                FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG, 1, 1,
                const_cast<uint32_t *>(uaddr2), op);
}

static void llvm_old_wait_on_waiter(llvm_old_waiter *waiter) {
  while (waiter->futex_word.load(std::memory_order_relaxed) == LLVM_OLD_WAITING) {
    (void)llvm_old_futex_wait_private(
        reinterpret_cast<volatile uint32_t *>(&waiter->futex_word),
        LLVM_OLD_WAITING);
  }
}

int llvm_old_cond_init(llvm_old_condvar *cond) {
  llvm_mutex_init(&cond->qmtx);
  cond->head = nullptr;
  cond->tail = nullptr;
  return 0;
}

int llvm_old_cond_destroy(llvm_old_condvar *cond) {
  return llvm_mutex_destroy(&cond->qmtx);
}

int llvm_old_cond_wait(llvm_old_condvar *cond, llvm_mutex *mutex) {
  llvm_old_waiter waiter = {};
  waiter.futex_word.store(LLVM_OLD_WAITING);

  llvm_mutex_lock(&cond->qmtx);
  llvm_old_waiter *old_tail = cond->tail;
  if (cond->head == nullptr) {
    cond->head = cond->tail = &waiter;
  } else {
    cond->tail->next = &waiter;
    cond->tail = &waiter;
  }

  if (llvm_mutex_unlock(mutex) != 0) {
    cond->tail = old_tail;
    if (cond->tail == nullptr) {
      cond->head = nullptr;
    } else {
      cond->tail->next = nullptr;
    }
    llvm_mutex_unlock(&cond->qmtx);
    return EINVAL;
  }

  llvm_mutex_unlock(&cond->qmtx);
  llvm_old_wait_on_waiter(&waiter);
  return llvm_mutex_lock(mutex);
}

int llvm_old_cond_signal(llvm_old_condvar *cond) {
  llvm_mutex_lock(&cond->qmtx);
  if (cond->head == nullptr) {
    llvm_mutex_unlock(&cond->qmtx);
    return 0;
  }

  llvm_old_waiter *first = cond->head;
  cond->head = cond->head->next;
  if (cond->head == nullptr) {
    cond->tail = nullptr;
  }

  llvm_mutex_reset(&cond->qmtx);
  llvm_old_futex_wake_op_private(
      reinterpret_cast<volatile uint32_t *>(&cond->qmtx.futex_word),
      reinterpret_cast<volatile uint32_t *>(&first->futex_word),
      FUTEX_OP(FUTEX_OP_SET, LLVM_OLD_SIGNALLED, FUTEX_OP_CMP_EQ,
               LLVM_OLD_WAITING));
  return 0;
}

int llvm_old_cond_broadcast(llvm_old_condvar *cond) {
  llvm_mutex_lock(&cond->qmtx);
  uint32_t dummy_futex_word = 0;
  llvm_old_waiter *waiter = cond->head;
  cond->head = nullptr;
  cond->tail = nullptr;
  llvm_mutex_unlock(&cond->qmtx);

  while (waiter != nullptr) {
    llvm_old_waiter *next = waiter->next;
    llvm_old_futex_wake_op_private(
        &dummy_futex_word,
        reinterpret_cast<volatile uint32_t *>(&waiter->futex_word),
        FUTEX_OP(FUTEX_OP_SET, LLVM_OLD_SIGNALLED, FUTEX_OP_CMP_EQ,
                 LLVM_OLD_WAITING));
    waiter = next;
  }

  return 0;
}

} // extern "C"
