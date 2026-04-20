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

enum llvm_mutex_state : uint32_t {
  LLVM_MUTEX_UNLOCKED = 0,
  LLVM_MUTEX_LOCKED = 1,
  LLVM_MUTEX_IN_CONTENTION = 2,
};

static int llvm_futex_wait_private(volatile uint32_t *addr, uint32_t expected) {
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

static void llvm_futex_wake_private(volatile uint32_t *addr, int count) {
  (void)syscall(SYS_futex, const_cast<uint32_t *>(addr),
                FUTEX_WAKE | FUTEX_PRIVATE_FLAG, count, nullptr, nullptr, 0);
}

int llvm_mutex_init(llvm_mutex *mutex) {
  mutex->futex_word.store(LLVM_MUTEX_UNLOCKED, std::memory_order_relaxed);
  return 0;
}

int llvm_mutex_destroy(llvm_mutex *mutex) {
  (void)mutex;
  return 0;
}

int llvm_mutex_try_lock(llvm_mutex *mutex) {
  uint32_t expected = LLVM_MUTEX_UNLOCKED;
  return mutex->futex_word.compare_exchange_strong(
      expected, LLVM_MUTEX_LOCKED, std::memory_order_acquire,
      std::memory_order_relaxed);
}

static uint32_t llvm_mutex_spin(llvm_mutex *mutex, unsigned spin_count) {
  for (;;) {
    uint32_t state = mutex->futex_word.load(std::memory_order_relaxed);
    if (state != LLVM_MUTEX_LOCKED || spin_count == 0) {
      return state;
    }
    --spin_count;
  }
}

int llvm_mutex_lock(llvm_mutex *mutex) {
  if (llvm_mutex_try_lock(mutex)) {
    return 0;
  }

  uint32_t state = llvm_mutex_spin(mutex, 100);
  if (state == LLVM_MUTEX_UNLOCKED) {
    uint32_t expected = LLVM_MUTEX_UNLOCKED;
    if (mutex->futex_word.compare_exchange_strong(
            expected, LLVM_MUTEX_LOCKED, std::memory_order_acquire,
            std::memory_order_relaxed)) {
      return 0;
    }
    state = expected;
  }

  for (;;) {
    if (state != LLVM_MUTEX_IN_CONTENTION &&
        mutex->futex_word.exchange(LLVM_MUTEX_IN_CONTENTION,
                                   std::memory_order_acquire) ==
            LLVM_MUTEX_UNLOCKED) {
      return 0;
    }

    (void)llvm_futex_wait_private(
        reinterpret_cast<volatile uint32_t *>(&mutex->futex_word),
        LLVM_MUTEX_IN_CONTENTION);
    state = llvm_mutex_spin(mutex, 100);
  }
}

int llvm_mutex_unlock(llvm_mutex *mutex) {
  uint32_t prev =
      mutex->futex_word.exchange(LLVM_MUTEX_UNLOCKED, std::memory_order_release);
  if (prev == LLVM_MUTEX_IN_CONTENTION) {
    llvm_futex_wake_private(
        reinterpret_cast<volatile uint32_t *>(&mutex->futex_word), 1);
  }
  return prev == LLVM_MUTEX_UNLOCKED ? EINVAL : 0;
}

void llvm_mutex_reset(llvm_mutex *mutex) {
  mutex->futex_word.store(LLVM_MUTEX_UNLOCKED, std::memory_order_relaxed);
}

} // extern "C"
