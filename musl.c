#define _GNU_SOURCE

#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#define CV_MUTEX_LOCKED 1
#define CV_MUTEX_WAITERS INT_MIN

struct cv_mutex {
    volatile int lock;
    volatile int waiters;
};

struct waiter {
    struct waiter *prev;
    struct waiter *next;
    volatile int state;
    volatile int barrier;
    volatile int *notify;
};

struct cv_condvar {
    volatile int lock;
    struct waiter *head;
    struct waiter *tail;
};

enum waiter_state {
    WAITING = 0,
    SIGNALED = 1,
    LEAVING = 2,
};

static inline int atomic_load_int(const volatile int *ptr)
{
    return __atomic_load_n(ptr, __ATOMIC_SEQ_CST);
}

static inline void atomic_store_int(volatile int *ptr, int value)
{
    __atomic_store_n(ptr, value, __ATOMIC_SEQ_CST);
}

static inline int atomic_swap_int(volatile int *ptr, int value)
{
    return __atomic_exchange_n(ptr, value, __ATOMIC_SEQ_CST);
}

static inline int atomic_cas_int(volatile int *ptr, int expected, int desired)
{
    __atomic_compare_exchange_n(
        ptr,
        &expected,
        desired,
        0,
        __ATOMIC_SEQ_CST,
        __ATOMIC_SEQ_CST);
    return expected;
}

static inline int atomic_fetch_add_int(volatile int *ptr, int value)
{
    return __atomic_fetch_add(ptr, value, __ATOMIC_SEQ_CST);
}

static inline void atomic_inc_int(volatile int *ptr)
{
    (void)__atomic_add_fetch(ptr, 1, __ATOMIC_SEQ_CST);
}

static inline void atomic_dec_int(volatile int *ptr)
{
    (void)__atomic_sub_fetch(ptr, 1, __ATOMIC_SEQ_CST);
}

static int futex_wait_private(volatile int *addr, int expected)
{
    int rc;
    do {
        rc = (int)syscall(
            SYS_futex,
            addr,
            FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
            expected,
            0,
            0,
            0);
    } while (rc == -1 && errno == EINTR);
    return rc;
}

static void futex_wake_private(volatile int *addr, int count)
{
    (void)syscall(
        SYS_futex,
        addr,
        FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
        count,
        0,
        0,
        0);
}

static void futex_requeue_private(volatile int *addr, volatile int *target)
{
    (void)syscall(
        SYS_futex,
        addr,
        FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG,
        0,
        1,
        target,
        0);
}

static inline void tiny_lock(volatile int *lock)
{
    if (atomic_cas_int(lock, 0, 1)) {
        atomic_cas_int(lock, 1, 2);
        do {
            futex_wait_private(lock, 2);
        } while (atomic_cas_int(lock, 0, 2));
    }
}

static inline void tiny_unlock(volatile int *lock)
{
    if (atomic_swap_int(lock, 0) == 2) {
        futex_wake_private(lock, 1);
    }
}

static inline void tiny_unlock_requeue(volatile int *lock, volatile int *target)
{
    atomic_store_int(lock, 0);
    futex_requeue_private(lock, target);
}

static inline void tiny_unlock_wake(volatile int *lock)
{
    atomic_store_int(lock, 0);
    futex_wake_private(lock, 1);
}

static int cv_mutex_init_impl(struct cv_mutex *mutex)
{
    mutex->lock = 0;
    mutex->waiters = 0;
    return 0;
}

static int cv_mutex_destroy_impl(struct cv_mutex *mutex)
{
    (void)mutex;
    return 0;
}

static int cv_mutex_lock_impl(struct cv_mutex *mutex)
{
    if (atomic_cas_int(&mutex->lock, 0, CV_MUTEX_LOCKED) == 0) {
        return 0;
    }

    for (;;) {
        int current = atomic_load_int(&mutex->lock);

        if ((current & CV_MUTEX_LOCKED) == 0) {
            int desired = (current & CV_MUTEX_WAITERS) | CV_MUTEX_LOCKED;
            if (atomic_cas_int(&mutex->lock, current, desired) == current) {
                return 0;
            }
            continue;
        }

        if ((current & CV_MUTEX_WAITERS) == 0) {
            if (atomic_cas_int(&mutex->lock, current, current | CV_MUTEX_WAITERS) != current) {
                continue;
            }
            current |= CV_MUTEX_WAITERS;
        }

        atomic_inc_int(&mutex->waiters);
        (void)futex_wait_private(&mutex->lock, current);
        atomic_dec_int(&mutex->waiters);
    }
}

static int cv_mutex_unlock_impl(struct cv_mutex *mutex)
{
    int waiters = atomic_load_int(&mutex->waiters);
    int previous = atomic_swap_int(&mutex->lock, 0);

    if (waiters || (previous & CV_MUTEX_WAITERS)) {
        futex_wake_private(&mutex->lock, 1);
    }
    return 0;
}

static int cv_cond_init_impl(struct cv_condvar *condvar)
{
    condvar->lock = 0;
    condvar->head = 0;
    condvar->tail = 0;
    return 0;
}

static int cv_cond_destroy_impl(struct cv_condvar *condvar)
{
    (void)condvar;
    return 0;
}

static int cv_cond_signal_n_impl(struct cv_condvar *condvar, int count)
{
    struct waiter *first = 0;
    struct waiter *p = 0;
    int ref = 0;

    tiny_lock(&condvar->lock);

    for (p = condvar->tail; count && p; p = p->prev) {
        if (atomic_cas_int(&p->state, WAITING, SIGNALED) != WAITING) {
            atomic_inc_int(&ref);
            p->notify = &ref;
        } else {
            count--;
            if (!first) {
                first = p;
            }
        }
    }

    if (p) {
        if (p->next) {
            p->next->prev = 0;
        }
        p->next = 0;
    } else {
        condvar->head = 0;
    }
    condvar->tail = p;

    tiny_unlock(&condvar->lock);

    for (;;) {
        int current = atomic_load_int(&ref);
        if (current == 0) {
            break;
        }
        (void)futex_wait_private(&ref, current);
    }

    if (first) {
        tiny_unlock(&first->barrier);
    }

    return 0;
}

static int cv_cond_signal_impl(struct cv_condvar *condvar)
{
    return cv_cond_signal_n_impl(condvar, 1);
}

static int cv_cond_broadcast_impl(struct cv_condvar *condvar)
{
    return cv_cond_signal_n_impl(condvar, INT_MAX);
}

static int cv_cond_wait_impl(struct cv_condvar *condvar, struct cv_mutex *mutex, int use_requeue)
{
    struct waiter node = {0};
    int old_state;

    tiny_lock(&condvar->lock);

    node.barrier = 2;
    node.state = WAITING;
    node.next = condvar->head;
    condvar->head = &node;

    if (!condvar->tail) {
        condvar->tail = &node;
    } else {
        node.next->prev = &node;
    }

    tiny_unlock(&condvar->lock);

    cv_mutex_unlock_impl(mutex);

    while (atomic_load_int(&node.barrier) == 2) {
        (void)futex_wait_private(&node.barrier, 2);
    }

    old_state = atomic_cas_int(&node.state, WAITING, LEAVING);

    if (old_state == WAITING) {
        tiny_lock(&condvar->lock);

        if (condvar->head == &node) {
            condvar->head = node.next;
        } else if (node.prev) {
            node.prev->next = node.next;
        }

        if (condvar->tail == &node) {
            condvar->tail = node.prev;
        } else if (node.next) {
            node.next->prev = node.prev;
        }

        tiny_unlock(&condvar->lock);

        if (node.notify && atomic_fetch_add_int(node.notify, -1) == 1) {
            futex_wake_private(node.notify, 1);
        }
    } else {
        tiny_lock(&node.barrier);
    }

    cv_mutex_lock_impl(mutex);

    if (old_state == WAITING) {
        return 0;
    }

    if (!node.next) {
        atomic_inc_int(&mutex->waiters);
    }

    if (node.prev) {
        int value = atomic_load_int(&mutex->lock);
        if (value > 0) {
            (void)atomic_cas_int(&mutex->lock, value, value | CV_MUTEX_WAITERS);
        }
        if (use_requeue) {
            tiny_unlock_requeue(&node.prev->barrier, &mutex->lock);
        } else {
            tiny_unlock_wake(&node.prev->barrier);
        }
    } else {
        atomic_dec_int(&mutex->waiters);
    }

    return 0;
}

int cv_mutex_init(struct cv_mutex *mutex) { return cv_mutex_init_impl(mutex); }
int cv_mutex_destroy(struct cv_mutex *mutex) { return cv_mutex_destroy_impl(mutex); }
int cv_mutex_lock(struct cv_mutex *mutex) { return cv_mutex_lock_impl(mutex); }
int cv_mutex_unlock(struct cv_mutex *mutex) { return cv_mutex_unlock_impl(mutex); }
int cv_cond_init(struct cv_condvar *condvar) { return cv_cond_init_impl(condvar); }
int cv_cond_destroy(struct cv_condvar *condvar) { return cv_cond_destroy_impl(condvar); }
int cv_cond_wait(struct cv_condvar *condvar, struct cv_mutex *mutex)
{
    return cv_cond_wait_impl(condvar, mutex, 1);
}
int cv_cond_signal(struct cv_condvar *condvar) { return cv_cond_signal_impl(condvar); }
int cv_cond_broadcast(struct cv_condvar *condvar) { return cv_cond_broadcast_impl(condvar); }

int cvw_mutex_init(struct cv_mutex *mutex) { return cv_mutex_init_impl(mutex); }
int cvw_mutex_destroy(struct cv_mutex *mutex) { return cv_mutex_destroy_impl(mutex); }
int cvw_mutex_lock(struct cv_mutex *mutex) { return cv_mutex_lock_impl(mutex); }
int cvw_mutex_unlock(struct cv_mutex *mutex) { return cv_mutex_unlock_impl(mutex); }
int cvw_cond_init(struct cv_condvar *condvar) { return cv_cond_init_impl(condvar); }
int cvw_cond_destroy(struct cv_condvar *condvar) { return cv_cond_destroy_impl(condvar); }
int cvw_cond_wait(struct cv_condvar *condvar, struct cv_mutex *mutex)
{
    return cv_cond_wait_impl(condvar, mutex, 0);
}
int cvw_cond_signal(struct cv_condvar *condvar) { return cv_cond_signal_impl(condvar); }
int cvw_cond_broadcast(struct cv_condvar *condvar) { return cv_cond_broadcast_impl(condvar); }
