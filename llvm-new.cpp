#include <errno.h>
#include <linux/futex.h>
#include <limits.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <cstddef>

extern "C" {

struct llvm_mutex {
  std::atomic<uint32_t> futex_word;
};

int llvm_mutex_init(llvm_mutex *mutex);
int llvm_mutex_destroy(llvm_mutex *mutex);
int llvm_mutex_lock(llvm_mutex *mutex);
int llvm_mutex_unlock(llvm_mutex *mutex);
int llvm_mutex_try_lock(llvm_mutex *mutex);
void llvm_mutex_reset(llvm_mutex *mutex);

enum llvm_mutex_state : uint32_t {
  LLVM_MUTEX_UNLOCKED = 0,
  LLVM_MUTEX_LOCKED = 1,
  LLVM_MUTEX_IN_CONTENTION = 2,
};

struct llvm_new_waiter_header {
  llvm_new_waiter_header *prev;
  llvm_new_waiter_header *next;
};

struct llvm_new_cancellation_barrier {
  std::atomic<uint32_t> futex_word;
};

struct llvm_new_waiter : llvm_new_waiter_header {
  std::atomic<llvm_new_cancellation_barrier *> cancellation_barrier;
  llvm_mutex barrier;
  std::atomic<uint8_t> state;
};

struct llvm_new_condvar {
  llvm_new_waiter_header waiter_queue;
  llvm_mutex queue_lock;
};

enum llvm_new_waiter_state : uint8_t {
  LLVM_NEW_WAITING = 0,
  LLVM_NEW_SIGNALLED = 1,
  LLVM_NEW_CANCELLED = 2,
  LLVM_NEW_REQUEUED = 3,
};

static int llvm_new_futex_wait_private(volatile uint32_t *addr,
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

static void llvm_new_futex_wake_private(volatile uint32_t *addr, int count) {
  (void)syscall(SYS_futex, const_cast<uint32_t *>(addr),
                FUTEX_WAKE | FUTEX_PRIVATE_FLAG, count, nullptr, nullptr, 0);
}

static int llvm_new_futex_requeue_private(volatile uint32_t *addr,
                                          volatile uint32_t *target,
                                          int wake_limit, int requeue_limit) {
  return static_cast<int>(syscall(SYS_futex, const_cast<uint32_t *>(addr),
                                  FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG,
                                  wake_limit, requeue_limit,
                                  const_cast<uint32_t *>(target), 0));
}

static inline void llvm_new_sleep_briefly() {
#if defined(__x86_64__) || defined(__i386__) || defined(_M_X64) || defined(_M_IX86)
  __builtin_ia32_pause();
#else
  std::atomic_signal_fence(std::memory_order_seq_cst);
#endif
}

static inline void llvm_new_waiter_header_init(llvm_new_waiter_header *header) {
  header->prev = header;
  header->next = header;
}

static inline void
llvm_new_waiter_queue_ensure_init(llvm_new_waiter_header *dummy) {
  if (dummy->prev == nullptr) {
    llvm_new_waiter_header_init(dummy);
  }
}

static inline void llvm_new_waiter_push_back(llvm_new_waiter_header *dummy,
                                             llvm_new_waiter_header *waiter) {
  llvm_new_waiter_queue_ensure_init(dummy);
  waiter->next = dummy;
  waiter->prev = dummy->prev;
  waiter->next->prev = waiter;
  waiter->prev->next = waiter;
}

static inline void llvm_new_waiter_remove(llvm_new_waiter_header *waiter) {
  waiter->next->prev = waiter->prev;
  waiter->prev->next = waiter->next;
  llvm_new_waiter_header_init(waiter);
}

static inline void llvm_new_cancellation_barrier_init(
    llvm_new_cancellation_barrier *barrier) {
  barrier->futex_word.store(0, std::memory_order_relaxed);
}

static inline void llvm_new_cancellation_add_one(
    llvm_new_cancellation_barrier *barrier) {
  barrier->futex_word.fetch_add(2, std::memory_order_relaxed);
}

static inline void llvm_new_cancellation_notify(
    llvm_new_cancellation_barrier *barrier) {
  uint32_t res = barrier->futex_word.fetch_sub(2, std::memory_order_acq_rel);
  if (res <= 3 && (res & 1) != 0) {
    llvm_new_futex_wake_private(
        reinterpret_cast<volatile uint32_t *>(&barrier->futex_word), 1);
  }
}

static inline void llvm_new_cancellation_wait(
    llvm_new_cancellation_barrier *barrier) {
  constexpr uint32_t spin_limit = 100;
  uint32_t spin = 0;

  while (true) {
    uint32_t remaining = barrier->futex_word.load(std::memory_order_relaxed);
    if (remaining == 0) {
      return;
    }

    uint32_t sleeping = remaining | 1;
    if (spin > spin_limit &&
        barrier->futex_word.compare_exchange_strong(
            remaining, sleeping, std::memory_order_acq_rel,
            std::memory_order_relaxed)) {
      (void)llvm_new_futex_wait_private(
          reinterpret_cast<volatile uint32_t *>(&barrier->futex_word),
          sleeping);
      barrier->futex_word.fetch_sub(1, std::memory_order_acq_rel);
      spin = 0;
      continue;
    }

    llvm_new_sleep_briefly();
    ++spin;
  }
}

static inline void llvm_new_waiter_init(llvm_new_waiter *waiter) {
  llvm_new_waiter_header_init(waiter);
  waiter->cancellation_barrier.store(nullptr, std::memory_order_relaxed);
  llvm_mutex_init(&waiter->barrier);
  (void)llvm_mutex_try_lock(&waiter->barrier);
  waiter->state.store(LLVM_NEW_WAITING, std::memory_order_relaxed);
}

static inline bool llvm_new_waiter_gate_lock(llvm_mutex *mutex) {
  return llvm_mutex_lock(mutex) == 0;
}

static inline void llvm_new_confirm_cancellation(llvm_new_waiter *waiter) {
  llvm_new_cancellation_barrier *sender =
      waiter->cancellation_barrier.load(std::memory_order_acquire);
  if (sender != nullptr) {
    llvm_new_cancellation_notify(sender);
  }
}

static inline void llvm_new_notify(llvm_new_condvar *cond, size_t limit) {
  llvm_new_cancellation_barrier cancellation_barrier;
  llvm_new_waiter *head = nullptr;
  llvm_new_waiter *cursor = nullptr;

  llvm_new_cancellation_barrier_init(&cancellation_barrier);

  llvm_mutex_lock(&cond->queue_lock);
  llvm_new_waiter_queue_ensure_init(&cond->waiter_queue);
  if (cond->waiter_queue.next == &cond->waiter_queue) {
    llvm_mutex_unlock(&cond->queue_lock);
    return;
  }

  for (cursor = static_cast<llvm_new_waiter *>(cond->waiter_queue.next);
       cursor != &cond->waiter_queue;
       cursor = static_cast<llvm_new_waiter *>(cursor->next)) {
    if (limit == 0) {
      break;
    }

    uint8_t expected = LLVM_NEW_WAITING;
    if (!cursor->state.compare_exchange_strong(expected, LLVM_NEW_SIGNALLED)) {
      llvm_new_cancellation_add_one(&cancellation_barrier);
      cursor->cancellation_barrier.store(&cancellation_barrier);
      continue;
    }

    if (head == nullptr) {
      head = cursor;
    }
    --limit;
  }

  llvm_new_waiter_header *removed_head = cond->waiter_queue.next;
  llvm_new_waiter_header *removed_tail = cursor->prev;
  cond->waiter_queue.next = cursor;
  cursor->prev = &cond->waiter_queue;
  removed_tail->next = removed_head;
  removed_head->prev = removed_tail;

  llvm_mutex_unlock(&cond->queue_lock);

  llvm_new_cancellation_wait(&cancellation_barrier);

  if (head != nullptr) {
    (void)llvm_mutex_unlock(&head->barrier);
  }
}

int llvm_new_cond_init(llvm_new_condvar *cond) {
  llvm_new_waiter_header_init(&cond->waiter_queue);
  llvm_mutex_init(&cond->queue_lock);
  return 0;
}

int llvm_new_cond_destroy(llvm_new_condvar *cond) {
  return llvm_mutex_destroy(&cond->queue_lock);
}

int llvm_new_cond_wait(llvm_new_condvar *cond, llvm_mutex *mutex) {
  llvm_new_waiter waiter;
  llvm_new_waiter_init(&waiter);

  llvm_mutex_lock(&cond->queue_lock);
  llvm_new_waiter_push_back(&cond->waiter_queue, &waiter);
  llvm_mutex_unlock(&cond->queue_lock);

  if (llvm_mutex_unlock(mutex) != 0) {
    return EINVAL;
  }

  bool locked = llvm_new_waiter_gate_lock(&waiter.barrier);

  uint8_t old_state = LLVM_NEW_WAITING;
  if (waiter.state.compare_exchange_strong(old_state, LLVM_NEW_CANCELLED,
                                           std::memory_order_acq_rel,
                                           std::memory_order_relaxed)) {
    llvm_mutex_lock(&cond->queue_lock);
    llvm_new_waiter_remove(&waiter);
    llvm_mutex_unlock(&cond->queue_lock);
    llvm_new_confirm_cancellation(&waiter);
  } else if (!locked) {
    if (llvm_mutex_lock(&waiter.barrier) != 0) {
      return EINVAL;
    }
  }

  int mutex_result = llvm_mutex_lock(mutex);
  if (waiter.state.load(std::memory_order_relaxed) == LLVM_NEW_REQUEUED) {
    mutex->futex_word.store(LLVM_MUTEX_IN_CONTENTION, std::memory_order_relaxed);
  }

  if (waiter.next != &waiter) {
    llvm_new_waiter *next_waiter = static_cast<llvm_new_waiter *>(waiter.next);
    llvm_new_waiter_remove(&waiter);

    uint32_t prev = next_waiter->barrier.futex_word.exchange(
        LLVM_MUTEX_UNLOCKED, std::memory_order_release);
    if (prev == LLVM_MUTEX_IN_CONTENTION) {
      int res = llvm_new_futex_requeue_private(
          reinterpret_cast<volatile uint32_t *>(&next_waiter->barrier.futex_word),
          reinterpret_cast<volatile uint32_t *>(&mutex->futex_word), 0, 1);
      if (res < 0) {
        llvm_new_futex_wake_private(
            reinterpret_cast<volatile uint32_t *>(
                &next_waiter->barrier.futex_word),
            1);
      } else if (res > 0) {
        next_waiter->state.store(LLVM_NEW_REQUEUED, std::memory_order_relaxed);
        mutex->futex_word.store(LLVM_MUTEX_IN_CONTENTION,
                                std::memory_order_relaxed);
      }
    }
  }

  return mutex_result;
}

int llvm_new_cond_signal(llvm_new_condvar *cond) {
  llvm_new_notify(cond, 1);
  return 0;
}

int llvm_new_cond_broadcast(llvm_new_condvar *cond) {
  llvm_new_notify(cond, SIZE_MAX);
  return 0;
}

} // extern "C"
