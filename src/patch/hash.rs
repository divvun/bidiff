//! BLAKE3 of a result as it is written, computed on helper threads.
//!
//! The output is cut into [`BLOCK_LEN`] blocks at aligned offsets. Each is a
//! complete BLAKE3 subtree, so helpers hash blocks independently and their
//! chaining values are merged into the root at the end. The helpers are
//! plain threads rather than rayon jobs: a caller already inside a busy
//! rayon pool could otherwise deadlock waiting for them.

use super::Error;
use blake3::{
    Hash, Hasher,
    hazmat::{
        ChainingValue, HasherExt, Mode, left_subtree_len, merge_subtrees_non_root,
        merge_subtrees_root,
    },
};
use std::{
    io,
    sync::{
        Mutex, PoisonError,
        mpsc::{self, Receiver, SyncSender},
    },
};

/// A power of two, so every block starts a subtree at least this large.
pub(crate) const BLOCK_LEN: usize = 1 << 20;

/// Spawning helpers costs more than hashing a result this small inline.
const INLINE_MAX: u64 = 4 * BLOCK_LEN as u64;

/// One helper at about 2 GB/s cannot keep up with applying; four can.
const MAX_HELPERS: usize = 4;

/// Takes the output's blocks in order and produces its hash.
pub(crate) enum BlockHasher {
    Inline(Box<Hasher>),
    Helpers(Helpers),
}

pub(crate) struct Helpers {
    jobs: Option<SyncSender<(u64, Vec<u8>)>>,
    cvs: Receiver<(u64, ChainingValue)>,
    free: Receiver<Vec<u8>>,
    buffers: usize,
    max_buffers: usize,
    /// The latest full block, held back until a later byte shows it is not
    /// the last one: the last block of a one-block result is the root.
    pending: Option<Vec<u8>>,
    dispatched: u64,
}

fn helpers_failed() -> Error {
    Error::Io(io::Error::other("BLAKE3 helper thread stopped"))
}

pub(crate) fn with_block_hasher<R>(new_size: u64, f: impl FnOnce(&mut BlockHasher) -> R) -> R {
    if new_size <= INLINE_MAX {
        return f(&mut BlockHasher::Inline(Box::new(Hasher::new())));
    }
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get() - 1)
        .clamp(1, MAX_HELPERS);
    let (jobs, job_queue) = mpsc::sync_channel::<(u64, Vec<u8>)>(workers);
    let job_queue = Mutex::new(job_queue);
    let (cv_tx, cvs) = mpsc::channel();
    let (free_tx, free) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..workers {
            let (job_queue, cv_tx, free_tx) = (&job_queue, cv_tx.clone(), free_tx.clone());
            s.spawn(move || {
                loop {
                    let job = job_queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .recv();
                    let Ok((index, mut block)) = job else {
                        return;
                    };
                    let cv = Hasher::new()
                        .set_input_offset(index * BLOCK_LEN as u64)
                        .update(&block)
                        .finalize_non_root();
                    block.clear();
                    if cv_tx.send((index, cv)).is_err() || free_tx.send(block).is_err() {
                        return;
                    }
                }
            });
        }
        drop((cv_tx, free_tx));
        f(&mut BlockHasher::Helpers(Helpers {
            jobs: Some(jobs),
            cvs,
            free,
            buffers: 0,
            // One filling, one pending, one queued per helper and one being
            // hashed per helper.
            max_buffers: 2 + 2 * workers,
            pending: None,
            dispatched: 0,
        }))
    })
}

impl BlockHasher {
    pub(crate) fn buffer(&mut self) -> Result<Vec<u8>, Error> {
        match self {
            BlockHasher::Inline(_) => Ok(Vec::with_capacity(BLOCK_LEN)),
            BlockHasher::Helpers(h) => h.buffer(),
        }
    }

    /// Hash `block`, which is exactly [`BLOCK_LEN`] bytes, and return a
    /// buffer to fill next.
    pub(crate) fn push_full(&mut self, mut block: Vec<u8>) -> Result<Vec<u8>, Error> {
        debug_assert_eq!(block.len(), BLOCK_LEN);
        match self {
            BlockHasher::Inline(hasher) => {
                hasher.update(&block);
                block.clear();
                Ok(block)
            }
            BlockHasher::Helpers(h) => {
                if let Some(previous) = h.pending.replace(block) {
                    h.dispatch(previous)?;
                }
                h.buffer()
            }
        }
    }

    /// The hash of everything pushed, followed by `last`, which holds fewer
    /// than [`BLOCK_LEN`] bytes; `total` counts all of them.
    pub(crate) fn finish(&mut self, last: Vec<u8>, total: u64) -> Result<Hash, Error> {
        match self {
            BlockHasher::Inline(hasher) => Ok(hasher.update(&last).finalize()),
            BlockHasher::Helpers(h) => h.finish(last, total),
        }
    }
}

impl Helpers {
    fn buffer(&mut self) -> Result<Vec<u8>, Error> {
        if let Ok(buffer) = self.free.try_recv() {
            return Ok(buffer);
        }
        if self.buffers < self.max_buffers {
            self.buffers += 1;
            return Ok(Vec::with_capacity(BLOCK_LEN));
        }
        self.free.recv().map_err(|_| helpers_failed())
    }

    fn dispatch(&mut self, block: Vec<u8>) -> Result<(), Error> {
        let jobs = self.jobs.as_ref().ok_or_else(helpers_failed)?;
        jobs.send((self.dispatched, block))
            .map_err(|_| helpers_failed())?;
        self.dispatched += 1;
        Ok(())
    }

    fn finish(&mut self, last: Vec<u8>, total: u64) -> Result<Hash, Error> {
        if total <= BLOCK_LEN as u64 {
            let only = self.pending.take().unwrap_or(last);
            return Ok(blake3::hash(&only));
        }
        if let Some(pending) = self.pending.take() {
            self.dispatch(pending)?;
        }
        if !last.is_empty() {
            self.dispatch(last)?;
        }
        self.jobs = None;

        let blocks = usize::try_from(self.dispatched).map_err(|_| helpers_failed())?;
        let mut cvs = vec![[0; 32]; blocks];
        for _ in 0..blocks {
            let (index, cv) = self.cvs.recv().map_err(|_| helpers_failed())?;
            *cvs.get_mut(index as usize).ok_or_else(helpers_failed)? = cv;
        }
        Ok(root(&cvs, total))
    }
}

/// The chaining value of `len` bytes whose blocks have chaining values
/// `cvs`, split as BLAKE3 splits them.
fn subtree(cvs: &[ChainingValue], len: u64) -> ChainingValue {
    if let [only] = cvs {
        return *only;
    }
    let (left, left_len, right) = split(cvs, len);
    merge_subtrees_non_root(
        &subtree(left, left_len),
        &subtree(right, len - left_len),
        Mode::Hash,
    )
}

/// Split `len` bytes, more than one block, into BLAKE3's left and right
/// subtrees. The left one is a power of two of at least [`BLOCK_LEN`], so the
/// split falls between blocks.
fn split(cvs: &[ChainingValue], len: u64) -> (&[ChainingValue], u64, &[ChainingValue]) {
    let left_len = left_subtree_len(len);
    let (left, right) = cvs.split_at((left_len / BLOCK_LEN as u64) as usize);
    (left, left_len, right)
}

/// The root hash of `total` bytes, more than one block, from its blocks'
/// chaining values.
fn root(cvs: &[ChainingValue], total: u64) -> Hash {
    let (left, left_len, right) = split(cvs, total);
    merge_subtrees_root(
        &subtree(left, left_len),
        &subtree(right, total - left_len),
        Mode::Hash,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_in_blocks(data: &[u8]) -> Hash {
        // A declared size over INLINE_MAX selects the helpers whatever the
        // real length, which the result must not depend on.
        with_block_hasher(u64::MAX, |h| {
            let mut buf = h.buffer().unwrap();
            for &b in data {
                if buf.len() == BLOCK_LEN {
                    buf = h.push_full(buf).unwrap();
                }
                buf.push(b);
            }
            if buf.len() == BLOCK_LEN {
                buf = h.push_full(buf).unwrap();
            }
            h.finish(buf, data.len() as u64).unwrap()
        })
    }

    #[test]
    fn matches_blake3_around_block_boundaries() {
        let data: Vec<u8> = (0..(9 * BLOCK_LEN + 3))
            .map(|i| (i * 31 % 253) as u8)
            .collect();
        let b = BLOCK_LEN;
        for len in [
            0,
            1,
            1024,
            b - 1,
            b,
            b + 1,
            2 * b,
            2 * b + 1,
            3 * b - 1,
            3 * b,
            4 * b + 17,
            5 * b,
            7 * b + 1,
            8 * b,
            9 * b + 3,
        ] {
            assert_eq!(
                hash_in_blocks(&data[..len]),
                blake3::hash(&data[..len]),
                "len {len}"
            );
        }
    }
}
