//! The threads bidiff starts, whose failures come back as errors.

use std::io;
use std::thread::{self, Scope, ScopedJoinHandle};

/// Stack of each thread bidiff starts. Building the table and scanning use
/// a few small frames.
pub(crate) const STACK: usize = 256 << 10;

pub(crate) fn spawn<'scope, 'env, T: Send + 'scope>(
    scope: &'scope Scope<'scope, 'env>,
    f: impl FnOnce() -> T + Send + 'scope,
) -> io::Result<ScopedJoinHandle<'scope, T>> {
    thread::Builder::new()
        .stack_size(STACK)
        .spawn_scoped(scope, f)
}

pub(crate) fn panicked() -> io::Error {
    io::Error::other("a bidiff thread panicked")
}

/// Run `work(0)` to `work(n - 1)`, each on its own thread, and return the
/// first error. Every thread that starts is joined.
pub(crate) fn run(n: usize, work: impl Fn(usize) -> io::Result<()> + Sync) -> io::Result<()> {
    let work = &work;
    thread::scope(|s| {
        let mut handles = Vec::new();
        let mut result = Ok(());
        for i in 0..n {
            match spawn(s, move || work(i)) {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        for handle in handles {
            let joined = handle.join().unwrap_or_else(|_| Err(panicked()));
            result = result.and(joined);
        }
        result
    })
}
