use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use std::{
    io::{self, Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Shared {
    done: AtomicBool,
    condvar: Condvar,
    mu: Mutex<()>,
}

pub struct RingWriter {
    prod: HeapProd<u8>,
    shared: Arc<Shared>,
}

pub struct RingReader {
    cons: HeapCons<u8>,
    shared: Arc<Shared>,
}

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let n = self.prod.push_slice(buf);
            if n > 0 {
                self.shared.condvar.notify_one();
                return Ok(n);
            }
            let guard = self.shared.mu.lock().unwrap();
            if self.prod.is_full() {
                drop(self.shared.condvar.wait(guard));
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared.condvar.notify_one();
        Ok(())
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        self.shared.done.store(true, Ordering::Release);
        self.shared.condvar.notify_one();
    }
}

impl Read for RingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.cons.pop_slice(buf);
            if n > 0 {
                self.shared.condvar.notify_one();
                return Ok(n);
            }
            if self.shared.done.load(Ordering::Acquire) {
                return Ok(0);
            }
            let guard = self.shared.mu.lock().unwrap();
            if self.cons.is_empty() && !self.shared.done.load(Ordering::Acquire) {
                drop(self.shared.condvar.wait(guard));
            }
        }
    }
}

pub fn ring_pipe(capacity: usize) -> (RingReader, RingWriter) {
    let rb = HeapRb::<u8>::new(capacity);
    let (prod, cons) = rb.split();
    let shared = Arc::new(Shared {
        done: AtomicBool::new(false),
        condvar: Condvar::new(),
        mu: Mutex::new(()),
    });
    (
        RingReader {
            cons,
            shared: shared.clone(),
        },
        RingWriter { prod, shared },
    )
}
