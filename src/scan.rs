//! Finding matches: the bsdiff scan of one chunk, and the threads that scan
//! chunks ahead of the consumer.

use crate::hashindex::HashIndex;
use crate::plan::{BATCH, BATCHES_QUEUED};
use crate::{Match, threads};
use std::cmp::min;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// Count matching bytes between two slices (up to the shorter length).
/// Written as an iterator pattern that LLVM reliably auto-vectorizes
/// into pcmpeqb + horizontal sum.
#[inline(always)]
fn count_matching_bytes(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).filter(|(x, y)| x == y).count()
}

pub(crate) struct BsdiffIterator<'a> {
    scan: usize,
    pos: usize,
    length: usize,
    lastscan: usize,
    lastpos: usize,
    lastoffset: isize,
    /// Cached hash from prefetch_block, to avoid recomputing on lookup.
    cached_hash: Option<u64>,

    obuf: &'a [u8],
    nbuf: &'a [u8],
    sa: &'a HashIndex<'a>,
}

impl<'a> BsdiffIterator<'a> {
    pub(crate) fn new(obuf: &'a [u8], nbuf: &'a [u8], sa: &'a HashIndex<'a>) -> Self {
        Self {
            scan: 0,
            pos: 0,
            length: 0,
            lastscan: 0,
            lastpos: 0,
            lastoffset: 0,
            cached_hash: None,
            obuf,
            nbuf,
            sa,
        }
    }

    /// `nbuf` from `start`, or nothing past its end.
    #[inline(always)]
    fn new_from(&self, start: usize) -> &'a [u8] {
        self.nbuf.get(start..).unwrap_or_default()
    }

    /// How many bytes of `nbuf[from..to]` equal the old bytes `lastoffset`
    /// away, counting only those with an old byte there.
    #[inline(always)]
    fn old_score(&self, from: usize, to: usize) -> isize {
        let old_len = self.obuf.len() as isize;
        let o_from = (from as isize + self.lastoffset).clamp(0, old_len);
        let o_to = (to as isize + self.lastoffset).clamp(0, old_len);
        if o_from >= o_to {
            return 0;
        }
        let n_from = (o_from - self.lastoffset) as usize;
        let n_to = (o_to - self.lastoffset) as usize;
        count_matching_bytes(
            self.obuf
                .get(o_from as usize..o_to as usize)
                .unwrap_or_default(),
            self.nbuf.get(n_from..n_to).unwrap_or_default(),
        ) as isize
    }
}

impl<'a> Iterator for BsdiffIterator<'a> {
    type Item = Match;
    fn next(&mut self) -> Option<Self::Item> {
        let nbuflen = self.nbuf.len();

        while self.scan < nbuflen {
            let mut oldscore = 0_isize;
            self.scan += self.length;

            let mut scsc = self.scan;
            'inner: while self.scan < nbuflen {
                let needle = self.new_from(self.scan);
                let res = if let Some(h) = self.cached_hash.take() {
                    self.sa.longest_substring_match_with_hash(needle, h)
                } else {
                    self.sa.longest_substring_match(needle)
                };
                // Prefetch the table bucket for the next scan position and cache the hash.
                // The oldscore + scoring work below provides the latency window.
                self.cached_hash = self.sa.prefetch_block(self.new_from(self.scan + 1));
                // Lookahead prefetches: warm table buckets for upcoming scan positions.
                // We don't cache these hashes — the CPU pipelines the independent
                // hash computations via out-of-order execution, so recomputation is free.
                self.sa.prefetch_block(self.new_from(self.scan + 2));
                self.sa.prefetch_block(self.new_from(self.scan + 3));
                self.pos = res.start;
                self.length = res.len;

                let end = self.scan + self.length;
                if scsc < end {
                    oldscore += self.old_score(scsc, end);
                    scsc = end;
                }

                let significantly_better = self.length as isize > oldscore + 8;
                let same_length = self.length as isize == oldscore && self.length != 0;

                if same_length || significantly_better {
                    // Prefetch for the actual next scan position (scan + length),
                    // not scan + 1 which is now stale. The lenf/lenb computation
                    // below provides ample latency window for the prefetch.
                    self.cached_hash = self.sa.prefetch_block(self.new_from(end));
                    break 'inner;
                }

                let oi = self.scan as isize + self.lastoffset;
                if oi >= 0
                    && self
                        .obuf
                        .get(oi as usize)
                        .is_some_and(|o| self.nbuf.get(self.scan) == Some(o))
                {
                    oldscore -= 1;
                }
                self.scan += 1;
            } // 'inner

            let done_scanning = self.scan == nbuflen;
            if self.length as isize != oldscore || done_scanning {
                // The bytes from lastscan to scan, which the forward and
                // backward extensions share.
                let between = self.nbuf.get(self.lastscan..self.scan).unwrap_or_default();

                // length forward from lastscan
                let mut lenf = {
                    let (mut s, mut sf, mut lenf) = (0_isize, 0_isize, 0_isize);
                    let old = self.obuf.get(self.lastpos..).unwrap_or_default();
                    for (i, (o, n)) in old.iter().zip(between).enumerate() {
                        if o == n {
                            s += 1;
                        }
                        let i = i as isize + 1;
                        if s * 2 - i > sf * 2 - lenf {
                            sf = s;
                            lenf = i;
                        }
                    }
                    lenf as usize
                };

                // length backwards from scan
                let mut lenb = if self.scan >= nbuflen {
                    0
                } else {
                    let (mut s, mut sb, mut lenb) = (0_isize, 0_isize, 0_isize);
                    let old = self.obuf.get(..self.pos).unwrap_or_default();
                    for (i, (o, n)) in old.iter().rev().zip(between.iter().rev()).enumerate() {
                        if o == n {
                            s += 1;
                        }
                        let i = i as isize + 1;
                        if s * 2 - i > sb * 2 - lenb {
                            sb = s;
                            lenb = i;
                        }
                    }
                    lenb as usize
                };

                // The two extensions stay within lastscan..scan, so the
                // subtractions below do not wrap; saturating keeps a broken
                // invariant from wrapping into a huge range.
                let lastscan_was_better = self.lastscan + lenf > self.scan.saturating_sub(lenb);
                if lastscan_was_better {
                    // if our last scan went forward more than
                    // our current scan went back, figure out how much
                    // of our current scan to crop based on scoring
                    let overlap =
                        (self.lastscan + lenf).saturating_sub(self.scan.saturating_sub(lenb));

                    let lens = {
                        let (mut s, mut ss, mut lens) = (0, 0, 0);
                        let span = |buf: &'a [u8], start: usize| {
                            buf.get(start..)
                                .and_then(|b| b.get(..overlap))
                                .unwrap_or_default()
                        };
                        let last_n =
                            span(self.nbuf, (self.lastscan + lenf).saturating_sub(overlap));
                        let last_o = span(self.obuf, (self.lastpos + lenf).saturating_sub(overlap));
                        let cur_n = span(self.nbuf, self.scan.saturating_sub(lenb));
                        let cur_o = span(self.obuf, self.pos.saturating_sub(lenb));
                        let last = last_n.iter().zip(last_o);
                        let cur = cur_n.iter().zip(cur_o);
                        for (i, ((ln, lo), (cn, co))) in last.zip(cur).enumerate() {
                            if ln == lo {
                                // point goes to last scan
                                s += 1;
                            }
                            if cn == co {
                                // point goes to current scan
                                s -= 1;
                            }

                            // new high score for last scan?
                            if s > ss {
                                ss = s;
                                lens = i + 1;
                            }
                        }
                        lens
                    };
                    lenf = (lenf + lens).saturating_sub(overlap);
                    lenb = lenb.saturating_sub(lens);
                } // lastscan was better

                let m = Match {
                    add_old_start: self.lastpos,
                    add_new_start: self.lastscan,
                    add_length: lenf,
                    copy_end: self.scan.saturating_sub(lenb),
                };

                self.lastscan = self.scan.saturating_sub(lenb);
                self.lastpos = self.pos.saturating_sub(lenb);
                self.lastoffset = self.pos as isize - self.scan as isize;

                return Some(m);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good

        None
    }
}

/// The chunks being scanned ahead of the consumer, at most `size` at once.
struct Window {
    state: Mutex<State>,
    changed: Condvar,
    size: usize,
}

struct State {
    /// Chunks the consumer is done with.
    consumed: usize,
    /// Each chunk being scanned or waiting for the consumer, at its index
    /// modulo the window's size: the receiving end of its matches.
    slots: Vec<Option<Receiver<Vec<Match>>>>,
    /// Scanning threads still running.
    live: usize,
    cancelled: bool,
}

impl Window {
    fn new(size: usize, live: usize) -> Self {
        Self {
            state: Mutex::new(State {
                consumed: 0,
                slots: (0..size).map(|_| None).collect(),
                live,
                cancelled: false,
            }),
            changed: Condvar::new(),
            size,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'g>(&self, guard: MutexGuard<'g, State>) -> MutexGuard<'g, State> {
        self.changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// Wait until chunk `i` is in the window, then hand the consumer its
    /// receiver. False if the diff was cancelled.
    fn open(&self, i: usize, rx: Receiver<Vec<Match>>) -> bool {
        let mut state = self.lock();
        while !state.cancelled && i >= state.consumed + self.size {
            state = self.wait(state);
        }
        if state.cancelled {
            return false;
        }
        // The previous chunk in this slot, i - size, has been taken.
        if let Some(slot) = state.slots.get_mut(i % self.size) {
            *slot = Some(rx);
        }
        drop(state);
        self.changed.notify_all();
        true
    }

    /// Wait for chunk `i`'s receiver.
    fn take(&self, i: usize) -> io::Result<Receiver<Vec<Match>>> {
        let mut state = self.lock();
        loop {
            if let Some(rx) = state.slots.get_mut(i % self.size).and_then(Option::take) {
                return Ok(rx);
            }
            if state.live == 0 || state.cancelled {
                return Err(io::Error::other(format!(
                    "scanning stopped before chunk {i}"
                )));
            }
            state = self.wait(state);
        }
    }

    /// Stop the scanning threads: those waiting to open a chunk give up, and
    /// those sending matches find no one receiving.
    fn cancel(&self) {
        self.update(|state| {
            state.cancelled = true;
            state.slots.iter_mut().for_each(|slot| *slot = None);
        });
    }
}

/// Counts a scanning thread out of the window when it stops, however it stops.
struct Live<'w>(&'w Window);

impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0
            .update(|state| state.live = state.live.saturating_sub(1));
    }
}

/// Send `chunk`'s matches to `tx` in batches, until they run out or no one
/// receives them.
fn scan_chunk(obuf: &[u8], chunk: &[u8], index: &HashIndex, tx: SyncSender<Vec<Match>>) {
    let mut batch = Vec::with_capacity(BATCH);
    for m in BsdiffIterator::new(obuf, chunk, index) {
        batch.push(m);
        if batch.len() == BATCH {
            // Taken, not replaced, so a thread waiting to send holds one batch.
            if tx.send(std::mem::take(&mut batch)).is_err() {
                return;
            }
            batch = Vec::with_capacity(BATCH);
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(batch);
    }
}

/// Scan `nbuf` in chunks of `chunk_size` bytes on up to `threads` threads,
/// and call `on_chunk` with each chunk's matches in order on this thread.
pub(crate) fn chunks<E>(
    obuf: &[u8],
    nbuf: &[u8],
    chunk_size: usize,
    index: &HashIndex,
    threads: usize,
    mut on_chunk: impl FnMut(usize, &[u8], &mut dyn Iterator<Item = Match>) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<io::Error>,
{
    let num_chunks = nbuf.len().div_ceil(chunk_size);
    let chunk = |i: usize| nbuf.chunks(chunk_size).nth(i).unwrap_or_default();
    let threads = min(threads, num_chunks).max(1);
    // Room for every thread to scan a chunk ahead of the consumer's.
    let window = Window::new(threads + 1, threads);
    let next = AtomicUsize::new(0);

    let worker = || {
        let _live = Live(&window);
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= num_chunks {
                return;
            }
            let (tx, rx) = sync_channel(BATCHES_QUEUED);
            if !window.open(i, rx) {
                return;
            }
            scan_chunk(obuf, chunk(i), index, tx);
        }
    };

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);
        let mut result = Ok(());
        for _ in 0..threads {
            match threads::spawn(s, worker) {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    result = Err(E::from(e));
                    break;
                }
            }
        }
        if result.is_ok() {
            result = (0..num_chunks).try_for_each(|i| {
                let rx = window.take(i)?;
                let mut matches = rx.iter().flatten();
                let done = on_chunk(i, chunk(i), &mut matches);
                drop(matches);
                drop(rx);
                window.update(|state| state.consumed = i + 1);
                done
            });
        }
        if result.is_err() {
            window.cancel();
        }
        for handle in handles {
            if handle.join().is_err() && result.is_ok() {
                result = Err(threads::panicked().into());
            }
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn a_failing_consumer_stops_the_scan() {
        let old = text(1 << 16, 1);
        let mut new = old.clone();
        new.iter_mut().step_by(7).for_each(|b| *b ^= 1);
        let index = HashIndex::new(&old, 4, None, 2).unwrap();
        for fail_at in [0, 3] {
            let mut seen = Vec::new();
            let result = chunks(&old, &new, 1024, &index, 4, |i, _, _| {
                seen.push(i);
                match i == fail_at {
                    true => Err(io::Error::other("stop")),
                    false => Ok(()),
                }
            });
            assert_eq!(result.unwrap_err().to_string(), "stop");
            assert_eq!(seen, (0..=fail_at).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_consumer_may_leave_matches_unread() {
        let old = text(1 << 16, 2);
        let new = text(1 << 16, 3);
        let index = HashIndex::new(&old, 4, None, 2).unwrap();
        let mut seen = 0;
        chunks::<io::Error>(&old, &new, 512, &index, 3, |_, _, _| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 128);
    }
}
