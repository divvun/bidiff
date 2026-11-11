//! How a diff spends its memory limit.
//!
//! Everything but the hash table has a fixed cost known before the diff
//! starts: the consumer's (what the caller does with each chunk), and each
//! scanning thread's stack and match batches. Those come first, with fewer
//! threads if all of them do not fit. The table then stays in RAM if what is
//! left holds it, and otherwise goes in a file in the temporary directory,
//! whose pages the kernel can write out rather than count against the limit.

use crate::{DiffParams, Match, hashindex, threads};
use std::io;
use std::path::PathBuf;

/// Matches a scanning thread hands over at once.
pub(crate) const BATCH: usize = 1024;

/// Batches of a chunk that may wait to be taken before its thread waits too.
pub(crate) const BATCHES_QUEUED: usize = 4;

const BATCH_BYTES: usize = BATCH * size_of::<Match>();

/// A chunk scanned ahead: its queued batches, and the one its thread fills
/// or the consumer empties.
const CHUNK_BYTES: usize = (BATCHES_QUEUED + 1) * BATCH_BYTES;

pub(crate) struct Plan {
    /// Threads that build the table and scan.
    pub threads: usize,
    /// The directory for the table's file, or `None` to keep it in RAM.
    pub table_dir: Option<PathBuf>,
}

impl Plan {
    /// Plan a diff of an old file of `old_len` bytes whose consumer holds
    /// `consumer` bytes.
    pub(crate) fn new(params: &DiffParams, old_len: usize, consumer: usize) -> io::Result<Self> {
        let wanted = params.num_threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
        });
        let table = hashindex::table_bytes(old_len, params.block_size)?;
        let Some(limit) = params.memory_limit else {
            return Ok(Self {
                threads: wanted,
                table_dir: None,
            });
        };

        // While scanning, each thread has a chunk in hand and the consumer
        // one more, which a thread may already be scanning.
        let chunked = params.scan_chunk_size.is_some();
        let per_thread = threads::STACK + if chunked { CHUNK_BYTES } else { 0 };
        let fixed = consumer.saturating_add(if chunked { CHUNK_BYTES } else { 0 });
        let need = fixed.saturating_add(per_thread);
        if limit < need {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!(
                    "a memory limit of {limit} bytes is below the {need} bytes this diff needs"
                ),
            ));
        }
        let threads = wanted.min((limit - fixed) / per_thread).max(1);
        let left = limit - fixed - threads * per_thread;
        Ok(Self {
            threads,
            table_dir: (table > left).then(|| params.temp_dir()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    fn params(threads: usize, limit: usize) -> DiffParams {
        DiffParams::with_threads(32, Some(MIB), Some(threads))
            .unwrap()
            .with_memory_limit(limit)
    }

    #[test]
    fn without_a_limit_everything_is_in_ram() {
        let p = DiffParams::with_threads(32, Some(MIB), Some(3)).unwrap();
        let plan = Plan::new(&p, 1 << 30, usize::MAX).unwrap();
        assert_eq!(plan.threads, 3);
        assert_eq!(plan.table_dir, None);
    }

    #[test]
    fn the_table_stays_in_ram_when_it_fits() {
        let table = hashindex::table_bytes(100 * MIB, 32).unwrap();
        let limit = MIB + CHUNK_BYTES + 4 * (threads::STACK + CHUNK_BYTES) + table;
        let plan = Plan::new(&params(4, limit), 100 * MIB, MIB).unwrap();
        assert_eq!((plan.threads, plan.table_dir), (4, None));

        let plan = Plan::new(&params(4, limit - 1), 100 * MIB, MIB).unwrap();
        assert_eq!(plan.threads, 4);
        assert!(plan.table_dir.is_some());
    }

    #[test]
    fn fewer_threads_fit_a_small_limit() {
        let one = MIB + CHUNK_BYTES + threads::STACK + CHUNK_BYTES;
        let plan = Plan::new(&params(8, one + threads::STACK + CHUNK_BYTES), 0, MIB).unwrap();
        assert_eq!(plan.threads, 2);

        assert_eq!(Plan::new(&params(8, one), 0, MIB).unwrap().threads, 1);
        let err = Plan::new(&params(8, one - 1), 0, MIB).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
    }
}
