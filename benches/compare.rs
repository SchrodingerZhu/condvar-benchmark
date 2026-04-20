use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar as StdCondvar, Mutex as StdMutex};
use std::thread;
use std::time::Instant;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const THREAD_LEVELS: [usize; 5] = [2, 8, 32, 64, 128];
const LARGE_INDEX_SIZE: usize = 1 << 15;
const STRESS_ITEMS_PER_PRODUCER: usize = 200;

fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^ (value >> 33)
}

trait IndexOps: Send + 'static {
    fn seeded(size: usize) -> Self;
    fn touch(&mut self, seed: u64) -> u64;
}

impl IndexOps for HashMap<u64, u64> {
    fn seeded(size: usize) -> Self {
        let mut map = HashMap::with_capacity(size * 2);
        for key in 0..size as u64 {
            map.insert(key, mix(key));
        }
        map
    }

    fn touch(&mut self, seed: u64) -> u64 {
        let span = self.len() as u64;
        let mut checksum = 0u64;

        for salt in 0..8u64 {
            let key = mix(seed ^ (salt.wrapping_mul(0x9e3779b97f4a7c15))) % span;
            let value = self
                .get_mut(&key)
                .expect("hash map key must remain inside the seeded range");
            checksum ^= *value;
            *value = mix(*value ^ seed.rotate_left((salt as u32) & 31));
        }

        checksum
    }
}

impl IndexOps for BTreeMap<u64, u64> {
    fn seeded(size: usize) -> Self {
        let mut map = BTreeMap::new();
        for key in 0..size as u64 {
            map.insert(key, mix(key));
        }
        map
    }

    fn touch(&mut self, seed: u64) -> u64 {
        let span = self.len() as u64;
        let mut checksum = 0u64;

        for salt in 0..8u64 {
            let key = mix(seed ^ (salt.wrapping_mul(0x9e3779b97f4a7c15))) % span;
            let value = self
                .get_mut(&key)
                .expect("btree key must remain inside the seeded range");
            checksum ^= *value;
            *value = mix(*value ^ seed.rotate_left((salt as u32) & 31));
        }

        checksum
    }
}

macro_rules! define_backend_workloads {
    (
        $turn_ring:ident,
        $queue_run:ident,
        $indexed_queue_run:ident,
        $hash_map_queue:ident,
        $btree_map_queue:ident,
        $signal_stress:ident,
        $broadcast_stress:ident,
        $module:ident
    ) => {
        fn $turn_ring(total_threads: usize, turns_per_thread: usize) {
            struct RingState {
                turn: usize,
                done: usize,
            }

            let pair = Arc::new((
                condvar::$module::Mutex::new(RingState { turn: 0, done: 0 }),
                condvar::$module::Condvar::new(),
            ));
            let mut handles = Vec::new();

            for thread_id in 1..total_threads {
                let pair = Arc::clone(&pair);
                handles.push(thread::spawn(move || {
                    let (mutex, condvar) = &*pair;
                    let mut guard = mutex.lock();
                    for _ in 0..turns_per_thread {
                        while guard.turn != thread_id {
                            guard = condvar.wait(guard);
                        }
                        guard.turn = (guard.turn + 1) % total_threads;
                        condvar.notify_all();
                    }
                    guard.done += 1;
                    condvar.notify_all();
                }));
            }

            let (mutex, condvar) = &*pair;
            let mut guard = mutex.lock();
            for _ in 0..turns_per_thread {
                while guard.turn != 0 {
                    guard = condvar.wait(guard);
                }
                guard.turn = 1 % total_threads;
                condvar.notify_all();
            }
            guard.done += 1;
            condvar.notify_all();
            while guard.done != total_threads {
                guard = condvar.wait(guard);
            }
            drop(guard);

            for handle in handles {
                handle.join().unwrap();
            }
        }

        fn $queue_run(total_threads: usize, items_per_producer: usize) {
            struct Queue {
                state: condvar::$module::Mutex<QueueState>,
                not_empty: condvar::$module::Condvar,
                not_full: condvar::$module::Condvar,
                capacity: usize,
            }

            struct QueueState {
                items: VecDeque<u64>,
                closed: bool,
            }

            impl Queue {
                fn new(capacity: usize) -> Self {
                    Self {
                        state: condvar::$module::Mutex::new(QueueState {
                            items: VecDeque::new(),
                            closed: false,
                        }),
                        not_empty: condvar::$module::Condvar::new(),
                        not_full: condvar::$module::Condvar::new(),
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

            let producers = (total_threads / 2).max(1);
            let consumers = (total_threads - producers).max(1);
            let queue = Arc::new(Queue::new(256));
            let mut producer_handles = Vec::new();
            let mut consumer_handles = Vec::new();

            for producer_id in 0..producers {
                let queue = Arc::clone(&queue);
                producer_handles.push(thread::spawn(move || {
                    let base = producer_id * items_per_producer;
                    for offset in 0..items_per_producer {
                        queue.push(mix((base + offset) as u64));
                    }
                }));
            }

            for _ in 0..consumers {
                let queue = Arc::clone(&queue);
                consumer_handles.push(thread::spawn(move || {
                    let mut checksum = 0u64;
                    while let Some(value) = queue.pop() {
                        checksum ^= mix(value);
                    }
                    checksum
                }));
            }

            for handle in producer_handles {
                handle.join().unwrap();
            }
            queue.close();

            let mut checksum = 0u64;
            for handle in consumer_handles {
                checksum ^= handle.join().unwrap();
            }
            black_box(checksum);
        }

        fn $indexed_queue_run<M: IndexOps>(total_threads: usize, items_per_producer: usize) {
            struct IndexedQueue<M: IndexOps> {
                state: condvar::$module::Mutex<IndexedState<M>>,
                not_empty: condvar::$module::Condvar,
                not_full: condvar::$module::Condvar,
                capacity: usize,
            }

            struct IndexedState<M: IndexOps> {
                items: VecDeque<u64>,
                index: M,
                closed: bool,
            }

            impl<M: IndexOps> IndexedQueue<M> {
                fn new(capacity: usize) -> Self {
                    Self {
                        state: condvar::$module::Mutex::new(IndexedState {
                            items: VecDeque::new(),
                            index: M::seeded(LARGE_INDEX_SIZE),
                            closed: false,
                        }),
                        not_empty: condvar::$module::Condvar::new(),
                        not_full: condvar::$module::Condvar::new(),
                        capacity,
                    }
                }

                fn push(&self, value: u64) {
                    let mut guard = self.state.lock();
                    while guard.items.len() == self.capacity {
                        guard = self.not_full.wait(guard);
                    }
                    let tag = guard.index.touch(value);
                    guard.items.push_back(value ^ tag);
                    self.not_empty.notify_one();
                }

                fn pop(&self) -> Option<u64> {
                    let mut guard = self.state.lock();
                    loop {
                        if let Some(value) = guard.items.pop_front() {
                            let tag = guard.index.touch(value.rotate_left(7));
                            self.not_full.notify_one();
                            return Some(value ^ tag);
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

            let producers = (total_threads / 2).max(1);
            let consumers = (total_threads - producers).max(1);
            let queue = Arc::new(IndexedQueue::<M>::new(256));
            let mut producer_handles = Vec::new();
            let mut consumer_handles = Vec::new();

            for producer_id in 0..producers {
                let queue = Arc::clone(&queue);
                producer_handles.push(thread::spawn(move || {
                    let base = producer_id * items_per_producer;
                    for offset in 0..items_per_producer {
                        queue.push(mix((base + offset) as u64));
                    }
                }));
            }

            for _ in 0..consumers {
                let queue = Arc::clone(&queue);
                consumer_handles.push(thread::spawn(move || {
                    let mut checksum = 0u64;
                    while let Some(value) = queue.pop() {
                        checksum ^= mix(value);
                    }
                    checksum
                }));
            }

            for handle in producer_handles {
                handle.join().unwrap();
            }
            queue.close();

            let mut checksum = 0u64;
            for handle in consumer_handles {
                checksum ^= handle.join().unwrap();
            }
            black_box(checksum);
        }

        fn $hash_map_queue(total_threads: usize, items_per_producer: usize) {
            $indexed_queue_run::<HashMap<u64, u64>>(total_threads, items_per_producer);
        }

        fn $btree_map_queue(total_threads: usize, items_per_producer: usize) {
            $indexed_queue_run::<BTreeMap<u64, u64>>(total_threads, items_per_producer);
        }

        fn $signal_stress(total_threads: usize, rounds: usize) {
            let producers = (total_threads / 2).max(1);
            let consumers = (total_threads - producers).max(1);
            let total_items = producers * STRESS_ITEMS_PER_PRODUCER;

            for _ in 0..rounds {
                let state = Arc::new((
                    condvar::$module::Mutex::new(StressState {
                        produced: 0,
                        consumed: 0,
                        exited_consumers: AtomicUsize::new(0),
                    }),
                    condvar::$module::Condvar::new(),
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
                            condvar.notify_one();
                        }
                    }));
                }

                for handle in producer_handles {
                    handle.join().unwrap();
                }

                let (mutex, condvar) = &*state;
                loop {
                    let exited = {
                        let guard = mutex.lock();
                        guard.exited_consumers.load(Ordering::SeqCst)
                    };
                    if exited == consumers {
                        break;
                    }
                    condvar.notify_one();
                    thread::yield_now();
                }

                for handle in consumer_handles {
                    handle.join().unwrap();
                }

                let consumed = {
                    let guard = mutex.lock();
                    guard.consumed
                };
                black_box(consumed);
            }
        }

        fn $broadcast_stress(total_threads: usize, rounds: usize) {
            let producers = (total_threads / 2).max(1);
            let consumers = (total_threads - producers).max(1);
            let total_items = producers * STRESS_ITEMS_PER_PRODUCER;

            for _ in 0..rounds {
                let state = Arc::new((
                    condvar::$module::Mutex::new(StressState {
                        produced: 0,
                        consumed: 0,
                        exited_consumers: AtomicUsize::new(0),
                    }),
                    condvar::$module::Condvar::new(),
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
                            condvar.notify_all();
                        }
                    }));
                }

                for handle in producer_handles {
                    handle.join().unwrap();
                }

                let (mutex, condvar) = &*state;
                loop {
                    let exited = {
                        let guard = mutex.lock();
                        guard.exited_consumers.load(Ordering::SeqCst)
                    };
                    if exited == consumers {
                        break;
                    }
                    condvar.notify_all();
                    thread::yield_now();
                }

                for handle in consumer_handles {
                    handle.join().unwrap();
                }

                let consumed = {
                    let guard = mutex.lock();
                    guard.consumed
                };
                black_box(consumed);
            }
        }
    };
}

struct StressState {
    produced: usize,
    consumed: usize,
    exited_consumers: AtomicUsize,
}

define_backend_workloads!(
    run_turn_ring_musl,
    run_queue_musl,
    run_indexed_queue_musl,
    run_hash_map_queue_musl,
    run_btree_map_queue_musl,
    run_signal_stress_musl,
    run_broadcast_stress_musl,
    musl
);
define_backend_workloads!(
    run_turn_ring_system,
    run_queue_system,
    run_indexed_queue_system,
    run_hash_map_queue_system,
    run_btree_map_queue_system,
    run_signal_stress_system,
    run_broadcast_stress_system,
    system
);
define_backend_workloads!(
    run_turn_ring_musl_wake,
    run_queue_musl_wake,
    run_indexed_queue_musl_wake,
    run_hash_map_queue_musl_wake,
    run_btree_map_queue_musl_wake,
    run_signal_stress_musl_wake,
    run_broadcast_stress_musl_wake,
    musl_wake
);
define_backend_workloads!(
    run_turn_ring_llvm_old,
    run_queue_llvm_old,
    run_indexed_queue_llvm_old,
    run_hash_map_queue_llvm_old,
    run_btree_map_queue_llvm_old,
    run_signal_stress_llvm_old,
    run_broadcast_stress_llvm_old,
    llvm_old
);
define_backend_workloads!(
    run_turn_ring_llvm_new,
    run_queue_llvm_new,
    run_indexed_queue_llvm_new,
    run_hash_map_queue_llvm_new,
    run_btree_map_queue_llvm_new,
    run_signal_stress_llvm_new,
    run_broadcast_stress_llvm_new,
    llvm_new
);

fn run_turn_ring_std(total_threads: usize, turns_per_thread: usize) {
    struct RingState {
        turn: usize,
        done: usize,
    }

    let pair = Arc::new((
        StdMutex::new(RingState { turn: 0, done: 0 }),
        StdCondvar::new(),
    ));
    let mut handles = Vec::new();

    for thread_id in 1..total_threads {
        let pair = Arc::clone(&pair);
        handles.push(thread::spawn(move || {
            let (mutex, condvar) = &*pair;
            let mut guard = mutex.lock().unwrap();
            for _ in 0..turns_per_thread {
                while guard.turn != thread_id {
                    guard = condvar.wait(guard).unwrap();
                }
                guard.turn = (guard.turn + 1) % total_threads;
                condvar.notify_all();
            }
            guard.done += 1;
            condvar.notify_all();
        }));
    }

    let (mutex, condvar) = &*pair;
    let mut guard = mutex.lock().unwrap();
    for _ in 0..turns_per_thread {
        while guard.turn != 0 {
            guard = condvar.wait(guard).unwrap();
        }
        guard.turn = 1 % total_threads;
        condvar.notify_all();
    }
    guard.done += 1;
    condvar.notify_all();
    while guard.done != total_threads {
        guard = condvar.wait(guard).unwrap();
    }
    drop(guard);

    for handle in handles {
        handle.join().unwrap();
    }
}

fn run_queue_std(total_threads: usize, items_per_producer: usize) {
    struct Queue {
        state: StdMutex<QueueState>,
        not_empty: StdCondvar,
        not_full: StdCondvar,
        capacity: usize,
    }

    struct QueueState {
        items: VecDeque<u64>,
        closed: bool,
    }

    impl Queue {
        fn new(capacity: usize) -> Self {
            Self {
                state: StdMutex::new(QueueState {
                    items: VecDeque::new(),
                    closed: false,
                }),
                not_empty: StdCondvar::new(),
                not_full: StdCondvar::new(),
                capacity,
            }
        }

        fn push(&self, value: u64) {
            let mut guard = self.state.lock().unwrap();
            while guard.items.len() == self.capacity {
                guard = self.not_full.wait(guard).unwrap();
            }
            guard.items.push_back(value);
            self.not_empty.notify_one();
        }

        fn pop(&self) -> Option<u64> {
            let mut guard = self.state.lock().unwrap();
            loop {
                if let Some(value) = guard.items.pop_front() {
                    self.not_full.notify_one();
                    return Some(value);
                }
                if guard.closed {
                    return None;
                }
                guard = self.not_empty.wait(guard).unwrap();
            }
        }

        fn close(&self) {
            let mut guard = self.state.lock().unwrap();
            guard.closed = true;
            self.not_empty.notify_all();
        }
    }

    let producers = (total_threads / 2).max(1);
    let consumers = (total_threads - producers).max(1);
    let queue = Arc::new(Queue::new(256));
    let mut producer_handles = Vec::new();
    let mut consumer_handles = Vec::new();

    for producer_id in 0..producers {
        let queue = Arc::clone(&queue);
        producer_handles.push(thread::spawn(move || {
            let base = producer_id * items_per_producer;
            for offset in 0..items_per_producer {
                queue.push(mix((base + offset) as u64));
            }
        }));
    }

    for _ in 0..consumers {
        let queue = Arc::clone(&queue);
        consumer_handles.push(thread::spawn(move || {
            let mut checksum = 0u64;
            while let Some(value) = queue.pop() {
                checksum ^= mix(value);
            }
            checksum
        }));
    }

    for handle in producer_handles {
        handle.join().unwrap();
    }
    queue.close();

    let mut checksum = 0u64;
    for handle in consumer_handles {
        checksum ^= handle.join().unwrap();
    }
    black_box(checksum);
}

fn run_indexed_queue_std<M: IndexOps>(total_threads: usize, items_per_producer: usize) {
    struct IndexedQueue<M: IndexOps> {
        state: StdMutex<IndexedState<M>>,
        not_empty: StdCondvar,
        not_full: StdCondvar,
        capacity: usize,
    }

    struct IndexedState<M: IndexOps> {
        items: VecDeque<u64>,
        index: M,
        closed: bool,
    }

    impl<M: IndexOps> IndexedQueue<M> {
        fn new(capacity: usize) -> Self {
            Self {
                state: StdMutex::new(IndexedState {
                    items: VecDeque::new(),
                    index: M::seeded(LARGE_INDEX_SIZE),
                    closed: false,
                }),
                not_empty: StdCondvar::new(),
                not_full: StdCondvar::new(),
                capacity,
            }
        }

        fn push(&self, value: u64) {
            let mut guard = self.state.lock().unwrap();
            while guard.items.len() == self.capacity {
                guard = self.not_full.wait(guard).unwrap();
            }
            let tag = guard.index.touch(value);
            guard.items.push_back(value ^ tag);
            self.not_empty.notify_one();
        }

        fn pop(&self) -> Option<u64> {
            let mut guard = self.state.lock().unwrap();
            loop {
                if let Some(value) = guard.items.pop_front() {
                    let tag = guard.index.touch(value.rotate_left(7));
                    self.not_full.notify_one();
                    return Some(value ^ tag);
                }
                if guard.closed {
                    return None;
                }
                guard = self.not_empty.wait(guard).unwrap();
            }
        }

        fn close(&self) {
            let mut guard = self.state.lock().unwrap();
            guard.closed = true;
            self.not_empty.notify_all();
        }
    }

    let producers = (total_threads / 2).max(1);
    let consumers = (total_threads - producers).max(1);
    let queue = Arc::new(IndexedQueue::<M>::new(256));
    let mut producer_handles = Vec::new();
    let mut consumer_handles = Vec::new();

    for producer_id in 0..producers {
        let queue = Arc::clone(&queue);
        producer_handles.push(thread::spawn(move || {
            let base = producer_id * items_per_producer;
            for offset in 0..items_per_producer {
                queue.push(mix((base + offset) as u64));
            }
        }));
    }

    for _ in 0..consumers {
        let queue = Arc::clone(&queue);
        consumer_handles.push(thread::spawn(move || {
            let mut checksum = 0u64;
            while let Some(value) = queue.pop() {
                checksum ^= mix(value);
            }
            checksum
        }));
    }

    for handle in producer_handles {
        handle.join().unwrap();
    }
    queue.close();

    let mut checksum = 0u64;
    for handle in consumer_handles {
        checksum ^= handle.join().unwrap();
    }
    black_box(checksum);
}

fn run_hash_map_queue_std(total_threads: usize, items_per_producer: usize) {
    run_indexed_queue_std::<HashMap<u64, u64>>(total_threads, items_per_producer);
}

fn run_btree_map_queue_std(total_threads: usize, items_per_producer: usize) {
    run_indexed_queue_std::<BTreeMap<u64, u64>>(total_threads, items_per_producer);
}

fn run_signal_stress_std(total_threads: usize, rounds: usize) {
    let producers = (total_threads / 2).max(1);
    let consumers = (total_threads - producers).max(1);
    let total_items = producers * STRESS_ITEMS_PER_PRODUCER;

    for _ in 0..rounds {
        let state = Arc::new((
            StdMutex::new(StressState {
                produced: 0,
                consumed: 0,
                exited_consumers: AtomicUsize::new(0),
            }),
            StdCondvar::new(),
        ));

        let mut producer_handles = Vec::new();
        let mut consumer_handles = Vec::new();

        for _ in 0..consumers {
            let state = Arc::clone(&state);
            consumer_handles.push(thread::spawn(move || {
                let (mutex, condvar) = &*state;
                let mut guard = mutex.lock().unwrap();
                while guard.consumed != total_items {
                    guard = condvar.wait(guard).unwrap();
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
                    let mut guard = mutex.lock().unwrap();
                    guard.produced += 1;
                    condvar.notify_one();
                }
            }));
        }

        for handle in producer_handles {
            handle.join().unwrap();
        }

        let (mutex, condvar) = &*state;
        loop {
            let exited = {
                let guard = mutex.lock().unwrap();
                guard.exited_consumers.load(Ordering::SeqCst)
            };
            if exited == consumers {
                break;
            }
            condvar.notify_one();
            thread::yield_now();
        }

        for handle in consumer_handles {
            handle.join().unwrap();
        }

        let consumed = {
            let guard = mutex.lock().unwrap();
            guard.consumed
        };
        black_box(consumed);
    }
}

fn run_broadcast_stress_std(total_threads: usize, rounds: usize) {
    let producers = (total_threads / 2).max(1);
    let consumers = (total_threads - producers).max(1);
    let total_items = producers * STRESS_ITEMS_PER_PRODUCER;

    for _ in 0..rounds {
        let state = Arc::new((
            StdMutex::new(StressState {
                produced: 0,
                consumed: 0,
                exited_consumers: AtomicUsize::new(0),
            }),
            StdCondvar::new(),
        ));

        let mut producer_handles = Vec::new();
        let mut consumer_handles = Vec::new();

        for _ in 0..consumers {
            let state = Arc::clone(&state);
            consumer_handles.push(thread::spawn(move || {
                let (mutex, condvar) = &*state;
                let mut guard = mutex.lock().unwrap();
                while guard.consumed != total_items {
                    guard = condvar.wait(guard).unwrap();
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
                    let mut guard = mutex.lock().unwrap();
                    guard.produced += 1;
                    condvar.notify_all();
                }
            }));
        }

        for handle in producer_handles {
            handle.join().unwrap();
        }

        let (mutex, condvar) = &*state;
        loop {
            let exited = {
                let guard = mutex.lock().unwrap();
                guard.exited_consumers.load(Ordering::SeqCst)
            };
            if exited == consumers {
                break;
            }
            condvar.notify_all();
            thread::yield_now();
        }

        for handle in consumer_handles {
            handle.join().unwrap();
        }

        let consumed = {
            let guard = mutex.lock().unwrap();
            guard.consumed
        };
        black_box(consumed);
    }
}

fn bench_turn_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("turn_ring");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let turns_per_thread = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_turn_ring_musl(threads, turns_per_thread);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let turns_per_thread = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_turn_ring_system(threads, turns_per_thread);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let turns_per_thread = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_turn_ring_musl_wake(threads, turns_per_thread);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let turns_per_thread = (iters as usize).max(1) * 64;
                let start = Instant::now();
                run_turn_ring_std(threads, turns_per_thread);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let turns_per_thread = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_turn_ring_llvm_old(threads, turns_per_thread);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let turns_per_thread = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_turn_ring_llvm_new(threads, turns_per_thread);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

fn bench_bounded_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("bounded_queue");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 512;
                    let start = Instant::now();
                    run_queue_musl(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 512;
                    let start = Instant::now();
                    run_queue_system(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 512;
                    let start = Instant::now();
                    run_queue_musl_wake(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let items_per_producer = (iters as usize).max(1) * 512;
                let start = Instant::now();
                run_queue_std(threads, items_per_producer);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 512;
                    let start = Instant::now();
                    run_queue_llvm_old(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 512;
                    let start = Instant::now();
                    run_queue_llvm_new(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

fn bench_hash_map_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_queue");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 128;
                    let start = Instant::now();
                    run_hash_map_queue_musl(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 128;
                    let start = Instant::now();
                    run_hash_map_queue_system(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 128;
                    let start = Instant::now();
                    run_hash_map_queue_musl_wake(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let items_per_producer = (iters as usize).max(1) * 128;
                let start = Instant::now();
                run_hash_map_queue_std(threads, items_per_producer);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 128;
                    let start = Instant::now();
                    run_hash_map_queue_llvm_old(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 128;
                    let start = Instant::now();
                    run_hash_map_queue_llvm_new(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

fn bench_btree_map_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_map_queue");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_btree_map_queue_musl(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_btree_map_queue_system(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_btree_map_queue_musl_wake(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let items_per_producer = (iters as usize).max(1) * 64;
                let start = Instant::now();
                run_btree_map_queue_std(threads, items_per_producer);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_btree_map_queue_llvm_old(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let items_per_producer = (iters as usize).max(1) * 64;
                    let start = Instant::now();
                    run_btree_map_queue_llvm_new(threads, items_per_producer);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

fn bench_signal_stress(c: &mut Criterion) {
    let mut group = c.benchmark_group("signal_stress");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_signal_stress_musl(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_signal_stress_system(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_signal_stress_musl_wake(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let rounds = (iters as usize).max(1);
                let start = Instant::now();
                run_signal_stress_std(threads, rounds);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_signal_stress_llvm_old(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_signal_stress_llvm_new(threads, rounds);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

fn bench_broadcast_stress(c: &mut Criterion) {
    let mut group = c.benchmark_group("broadcast_stress");

    for threads in THREAD_LEVELS {
        group.bench_with_input(
            BenchmarkId::new("musl", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_broadcast_stress_musl(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("glibc", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_broadcast_stress_system(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("musl_wake", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_broadcast_stress_musl_wake(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("std", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| {
                let rounds = (iters as usize).max(1);
                let start = Instant::now();
                run_broadcast_stress_std(threads, rounds);
                start.elapsed()
            });
        });

        group.bench_with_input(
            BenchmarkId::new("llvm_old", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_broadcast_stress_llvm_old(threads, rounds);
                    start.elapsed()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("llvm_new", threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let rounds = (iters as usize).max(1);
                    let start = Instant::now();
                    run_broadcast_stress_llvm_new(threads, rounds);
                    start.elapsed()
                });
            },
        );
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets =
        bench_turn_ring,
        bench_bounded_queue,
        bench_hash_map_queue,
        bench_btree_map_queue,
        bench_signal_stress,
        bench_broadcast_stress
}
criterion_main!(benches);
