use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use condvar::musl;

macro_rules! backend_suite {
    ($suite:ident, $module:ident) => {
        mod $suite {
            use super::*;

            type TestCondvar = condvar::$module::Condvar;
            type TestMutex<T> = condvar::$module::Mutex<T>;

            const STRESS_THREAD_LEVELS: [usize; 5] = [2, 8, 32, 64, 128];
            const STRESS_ITERATIONS: usize = 20;
            const STRESS_ITEMS_PER_PRODUCER: usize = 200;

            #[derive(Default)]
            struct PermitState {
                permits: usize,
                completed: usize,
            }

            struct Queue {
                state: TestMutex<QueueState>,
                not_empty: TestCondvar,
                not_full: TestCondvar,
                capacity: usize,
            }

            struct QueueState {
                items: VecDeque<u64>,
                closed: bool,
            }

            impl Queue {
                fn new(capacity: usize) -> Self {
                    Self {
                        state: TestMutex::new(QueueState {
                            items: VecDeque::new(),
                            closed: false,
                        }),
                        not_empty: TestCondvar::new(),
                        not_full: TestCondvar::new(),
                        capacity,
                    }
                }

                fn push(&self, value: u64) {
                    let mut guard = self.state.lock();
                    while guard.items.len() == self.capacity {
                        guard = self.not_full.wait(guard);
                    }
                    guard.items.push_back(value);
                    self.not_empty.notify_one();
                }

                fn pop(&self) -> Option<u64> {
                    let mut guard = self.state.lock();
                    loop {
                        if let Some(value) = guard.items.pop_front() {
                            self.not_full.notify_one();
                            return Some(value);
                        }
                        if guard.closed {
                            return None;
                        }
                        guard = self.not_empty.wait(guard);
                    }
                }

                fn close(&self) {
                    let mut guard = self.state.lock();
                    guard.closed = true;
                    self.not_empty.notify_all();
                }
            }

            #[test]
            fn notify_one_wakes_one_waiter() {
                let state = Arc::new((TestMutex::new(PermitState::default()), TestCondvar::new()));
                let waiting = Arc::new(AtomicUsize::new(0));
                let (tx, rx) = mpsc::channel();
                let mut handles = Vec::new();

                for _ in 0..2 {
                    let state = Arc::clone(&state);
                    let waiting = Arc::clone(&waiting);
                    let tx = tx.clone();
                    handles.push(thread::spawn(move || {
                        let (mutex, condvar) = &*state;
                        let mut guard = mutex.lock();
                        waiting.fetch_add(1, Ordering::SeqCst);
                        while guard.permits == 0 {
                            guard = condvar.wait(guard);
                        }
                        guard.permits -= 1;
                        guard.completed += 1;
                        tx.send(()).unwrap();
                    }));
                }
                drop(tx);

                while waiting.load(Ordering::SeqCst) < 2 {
                    thread::yield_now();
                }

                {
                    let (mutex, condvar) = &*state;
                    let mut guard = mutex.lock();
                    guard.permits += 1;
                    condvar.notify_one();
                }

                rx.recv_timeout(Duration::from_secs(1)).unwrap();
                assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

                {
                    let (mutex, condvar) = &*state;
                    let mut guard = mutex.lock();
                    guard.permits += 1;
                    condvar.notify_one();
                }

                rx.recv_timeout(Duration::from_secs(1)).unwrap();

                for handle in handles {
                    handle.join().unwrap();
                }

                let completed = {
                    let (mutex, _) = &*state;
                    mutex.lock().completed
                };
                assert_eq!(completed, 2);
            }

            #[test]
            fn notify_all_wakes_all_waiters() {
                let state = Arc::new((TestMutex::new(PermitState::default()), TestCondvar::new()));
                let waiting = Arc::new(AtomicUsize::new(0));
                let (tx, rx) = mpsc::channel();
                let waiter_count = 4;
                let mut handles = Vec::new();

                for _ in 0..waiter_count {
                    let state = Arc::clone(&state);
                    let waiting = Arc::clone(&waiting);
                    let tx = tx.clone();
                    handles.push(thread::spawn(move || {
                        let (mutex, condvar) = &*state;
                        let mut guard = mutex.lock();
                        waiting.fetch_add(1, Ordering::SeqCst);
                        while guard.permits == 0 {
                            guard = condvar.wait(guard);
                        }
                        guard.permits -= 1;
                        guard.completed += 1;
                        tx.send(()).unwrap();
                    }));
                }
                drop(tx);

                while waiting.load(Ordering::SeqCst) < waiter_count {
                    thread::yield_now();
                }

                {
                    let (mutex, condvar) = &*state;
                    let mut guard = mutex.lock();
                    guard.permits = waiter_count;
                    condvar.notify_all();
                }

                for _ in 0..waiter_count {
                    rx.recv_timeout(Duration::from_secs(1)).unwrap();
                }

                for handle in handles {
                    handle.join().unwrap();
                }

                let completed = {
                    let (mutex, _) = &*state;
                    mutex.lock().completed
                };
                assert_eq!(completed, waiter_count);
            }

            #[test]
            fn bounded_queue_round_trip() {
                let queue = Arc::new(Queue::new(64));
                let producers = 4usize;
                let consumers = 4usize;
                let items_per_producer = 2_000usize;
                let mut producer_handles = Vec::new();
                let mut consumer_handles = Vec::new();

                for producer_id in 0..producers {
                    let queue = Arc::clone(&queue);
                    producer_handles.push(thread::spawn(move || {
                        let base = producer_id * items_per_producer;
                        for offset in 0..items_per_producer {
                            queue.push((base + offset) as u64);
                        }
                    }));
                }

                for _ in 0..consumers {
                    let queue = Arc::clone(&queue);
                    consumer_handles.push(thread::spawn(move || {
                        let mut sum = 0u64;
                        let mut count = 0usize;
                        while let Some(value) = queue.pop() {
                            sum = sum.wrapping_add(value);
                            count += 1;
                        }
                        (sum, count)
                    }));
                }

                for handle in producer_handles {
                    handle.join().unwrap();
                }

                queue.close();

                let mut observed_sum = 0u64;
                let mut observed_count = 0usize;
                for handle in consumer_handles {
                    let (sum, count) = handle.join().unwrap();
                    observed_sum = observed_sum.wrapping_add(sum);
                    observed_count += count;
                }

                let total_items = producers * items_per_producer;
                let expected_sum = (total_items as u64 - 1) * total_items as u64 / 2;

                assert_eq!(observed_count, total_items);
                assert_eq!(observed_sum, expected_sum);
            }

            fn run_signal_mode_stress(total_threads: usize, use_broadcast: bool) {
                struct StressState {
                    produced: usize,
                    consumed: usize,
                    exited_consumers: AtomicUsize,
                }

                let producers = total_threads / 2;
                let consumers = total_threads - producers;
                let total_items = producers * STRESS_ITEMS_PER_PRODUCER;

                for _ in 0..STRESS_ITERATIONS {
                    let state = Arc::new((
                        TestMutex::new(StressState {
                            produced: 0,
                            consumed: 0,
                            exited_consumers: AtomicUsize::new(0),
                        }),
                        TestCondvar::new(),
                    ));

                    let mut producer_handles = Vec::new();
                    let mut consumer_handles = Vec::new();

                    for _ in 0..consumers {
                        let state = Arc::clone(&state);
                        consumer_handles.push(thread::spawn(move || {
                            let (mutex, condvar) = &*state;
                            let mut guard = mutex.lock();
                            while guard.consumed != total_items {
                                guard = condvar.wait(guard);
                                if guard.produced == 0 {
                                    continue;
                                }
                                guard.produced -= 1;
                                guard.consumed += 1;
                            }
                            guard.exited_consumers.fetch_add(1, Ordering::SeqCst);
                        }));
                    }

                    for _ in 0..producers {
                        let state = Arc::clone(&state);
                        producer_handles.push(thread::spawn(move || {
                            let (mutex, condvar) = &*state;
                            for _ in 0..STRESS_ITEMS_PER_PRODUCER {
                                let mut guard = mutex.lock();
                                guard.produced += 1;
                                if use_broadcast {
                                    condvar.notify_all();
                                } else {
                                    condvar.notify_one();
                                }
                            }
                        }));
                    }

                    for handle in producer_handles {
                        handle.join().unwrap();
                    }

                    let (_, condvar) = &*state;
                    while state.0.lock().exited_consumers.load(Ordering::SeqCst) != consumers {
                        if use_broadcast {
                            condvar.notify_all();
                        } else {
                            condvar.notify_one();
                        }
                        thread::yield_now();
                    }

                    for handle in consumer_handles {
                        handle.join().unwrap();
                    }

                    let consumed = state.0.lock().consumed;
                    assert_eq!(consumed, total_items);
                }
            }

            #[test]
            fn signal_stress() {
                for threads in STRESS_THREAD_LEVELS {
                    run_signal_mode_stress(threads, false);
                }
            }

            #[test]
            fn broadcast_stress() {
                for threads in STRESS_THREAD_LEVELS {
                    run_signal_mode_stress(threads, true);
                }
            }
        }
    };
}

backend_suite!(musl_suite, musl);
backend_suite!(llvm_old_suite, llvm_old);
backend_suite!(llvm_new_suite, llvm_new);
backend_suite!(system_suite, system);

#[test]
fn musl_broadcast_preserves_fifo_waiter_order() {
    struct State {
        released: bool,
        order: Vec<usize>,
    }

    let state = Arc::new((
        musl::Mutex::new(State {
            released: false,
            order: Vec::new(),
        }),
        musl::Condvar::new(),
    ));
    let waiting = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut handles = Vec::new();
    let waiter_count = 4usize;

    for id in 0..waiter_count {
        let state = Arc::clone(&state);
        let waiting = Arc::clone(&waiting);
        let ready_tx = ready_tx.clone();
        handles.push(thread::spawn(move || {
            let (mutex, condvar) = &*state;
            let mut guard = mutex.lock();
            waiting.fetch_add(1, Ordering::SeqCst);
            ready_tx.send(()).unwrap();
            while !guard.released || guard.order.len() != id {
                guard = condvar.wait(guard);
            }
            guard.order.push(id);
            condvar.notify_all();
        }));
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }
    drop(ready_tx);

    while waiting.load(Ordering::SeqCst) < waiter_count {
        thread::yield_now();
    }

    {
        let (mutex, condvar) = &*state;
        let mut guard = mutex.lock();
        assert!(guard.order.is_empty());
        guard.released = true;
        condvar.notify_all();
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let order = {
        let (mutex, _) = &*state;
        mutex.lock().order.clone()
    };
    assert_eq!(order, vec![0, 1, 2, 3]);
}
