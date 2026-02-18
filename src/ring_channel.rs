use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Shared {
    done: AtomicBool,
    consumer_waiting: AtomicBool,
    producer_waiting: AtomicBool,
    condvar: Condvar,
    mu: Mutex<()>,
}

pub struct RingProducer<T> {
    prod: HeapProd<T>,
    shared: Arc<Shared>,
}

pub struct RingConsumer<T> {
    cons: HeapCons<T>,
    shared: Arc<Shared>,
}

impl<T> RingProducer<T> {
    /// Blocking push. Blocks until space is available.
    pub fn push(&mut self, item: T) {
        let mut item = item;
        loop {
            match self.prod.try_push(item) {
                Ok(()) => {
                    if self.shared.consumer_waiting.load(Ordering::Acquire) {
                        let _guard = self.shared.mu.lock().unwrap();
                        self.shared.condvar.notify_one();
                    }
                    return;
                }
                Err(rejected) => {
                    item = rejected;
                    for _ in 0..32 {
                        std::hint::spin_loop();
                        if !self.prod.is_full() {
                            break;
                        }
                    }
                    if !self.prod.is_full() {
                        continue;
                    }
                    let guard = self.shared.mu.lock().unwrap();
                    if self.prod.is_full() {
                        self.shared.producer_waiting.store(true, Ordering::Release);
                        drop(self.shared.condvar.wait(guard));
                        self.shared.producer_waiting.store(false, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

impl<T> Drop for RingProducer<T> {
    fn drop(&mut self) {
        self.shared.done.store(true, Ordering::Release);
        let _guard = self.shared.mu.lock().unwrap();
        self.shared.condvar.notify_one();
    }
}

impl<T> RingConsumer<T> {
    /// Blocking pop. Returns `None` when the producer is dropped and the buffer is empty (EOF).
    pub fn pop(&mut self) -> Option<T> {
        loop {
            if let Some(item) = self.cons.try_pop() {
                if self.shared.producer_waiting.load(Ordering::Acquire) {
                    let _guard = self.shared.mu.lock().unwrap();
                    self.shared.condvar.notify_one();
                }
                return Some(item);
            }
            if self.shared.done.load(Ordering::Acquire) {
                return self.cons.try_pop();
            }
            for _ in 0..32 {
                std::hint::spin_loop();
                if !self.cons.is_empty() || self.shared.done.load(Ordering::Relaxed) {
                    break;
                }
            }
            if !self.cons.is_empty() {
                continue;
            }
            let guard = self.shared.mu.lock().unwrap();
            if self.cons.is_empty() && !self.shared.done.load(Ordering::Acquire) {
                self.shared.consumer_waiting.store(true, Ordering::Release);
                drop(self.shared.condvar.wait(guard));
                self.shared.consumer_waiting.store(false, Ordering::Relaxed);
            }
        }
    }
}

pub fn ring_channel<T>(capacity: usize) -> (RingConsumer<T>, RingProducer<T>) {
    let rb = HeapRb::<T>::new(capacity);
    let (prod, cons) = rb.split();
    let shared = Arc::new(Shared {
        done: AtomicBool::new(false),
        consumer_waiting: AtomicBool::new(false),
        producer_waiting: AtomicBool::new(false),
        condvar: Condvar::new(),
        mu: Mutex::new(()),
    });
    (
        RingConsumer {
            cons,
            shared: shared.clone(),
        },
        RingProducer { prod, shared },
    )
}
