pub const MAGIC: u32 = 0xB1DF;
pub const VERSION: u32 = 0x2000;

#[cfg(feature = "diff")]
use rayon::prelude::*;
#[cfg(feature = "patch")]
use std::io::{self, Write};
use std::{cmp::min, error::Error};
#[cfg(feature = "diff")]
use tracing::info;

#[cfg(feature = "diff")]
use hashindex::HashIndex;

#[cfg(feature = "diff")]
mod ring_channel;
#[cfg(feature = "diff")]
use ring_channel::{RingProducer, ring_channel};

#[cfg(feature = "profiling")]
use std::time::Instant;

/// Count matching bytes between two slices (up to the shorter length).
/// Written as an iterator pattern that LLVM reliably auto-vectorizes
/// into pcmpeqb + horizontal sum.
#[inline(always)]
fn count_matching_bytes(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).filter(|(x, y)| x == y).count()
}

#[cfg(feature = "patch")]
pub mod enc;

#[cfg(feature = "diff")]
pub mod hashindex;

#[cfg(feature = "patch")]
pub mod patch;

#[cfg(any(test, feature = "instructions"))]
pub mod instructions;

#[derive(Debug)]
pub struct Match {
    pub add_old_start: usize,
    pub add_new_start: usize,
    pub add_length: usize,
    pub copy_end: usize,
}

impl Match {
    #[inline(always)]
    pub fn copy_start(&self) -> usize {
        self.add_new_start + self.add_length
    }
}

#[derive(Debug, Clone)]
pub struct Control<'a> {
    pub add: &'a [u8],
    pub copy: &'a [u8],
    pub seek: i64,
}

pub struct Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: Error,
{
    obuf: &'a [u8],
    nbuf: &'a [u8],
    prev_match: Option<Match>,
    buf: Vec<u8>,
    on_control: F,
    closed: bool,
}

impl<'a, F, E> Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: Error,
{
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], on_control: F) -> Self {
        Self {
            obuf,
            nbuf,
            buf: Vec::with_capacity(16 * 1024),
            prev_match: None,
            on_control,
            closed: false,
        }
    }

    fn send_control(&mut self, m: Option<&Match>) -> Result<(), E> {
        if let Some(pm) = self.prev_match.take() {
            (self.on_control)(&Control {
                add: &self.buf[..pm.add_length],
                copy: &self.nbuf[pm.copy_start()..pm.copy_end],
                seek: if let Some(m) = m {
                    m.add_old_start as i64 - (pm.add_old_start + pm.add_length) as i64
                } else {
                    0
                },
            })?;
        }
        Ok(())
    }

    pub fn translate(&mut self, m: Match) -> Result<(), E> {
        self.send_control(Some(&m))?;

        self.buf.clear();

        // Slice + zip lets the compiler see matching lengths and elide bounds
        // checks, enabling auto-vectorization of the wrapping_sub loop.
        let n_slice = &self.nbuf[m.add_new_start..m.add_new_start + m.add_length];
        let o_slice = &self.obuf[m.add_old_start..m.add_old_start + m.add_length];
        self.buf
            .extend(n_slice.iter().zip(o_slice).map(|(a, b)| a.wrapping_sub(*b)));

        self.prev_match = Some(m);
        Ok(())
    }

    pub fn close(mut self) -> Result<(), E> {
        self.do_close()
    }

    fn do_close(&mut self) -> Result<(), E> {
        if !self.closed {
            self.send_control(None)?;
            self.closed = true;
        }
        Ok(())
    }
}

impl<'a, F, E> Drop for Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: Error,
{
    fn drop(&mut self) {
        // dropping a Translator ignores errors on purpose,
        // just like File does
        self.do_close().unwrap_or(());
    }
}

#[cfg(feature = "diff")]
struct BsdiffIterator<'a> {
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

#[cfg(feature = "diff")]
impl<'a> BsdiffIterator<'a> {
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], sa: &'a HashIndex<'a>) -> Self {
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
}

#[cfg(feature = "diff")]
impl<'a> Iterator for BsdiffIterator<'a> {
    type Item = Match;
    fn next(&mut self) -> Option<Self::Item> {
        let obuflen = self.obuf.len();
        let nbuflen = self.nbuf.len();

        while self.scan < nbuflen {
            let mut oldscore = 0_isize;
            self.scan += self.length;

            let mut scsc = self.scan;
            'inner: while self.scan < nbuflen {
                let res = if let Some(h) = self.cached_hash.take() {
                    self.sa
                        .longest_substring_match_with_hash(&self.nbuf[self.scan..], h)
                } else {
                    self.sa.longest_substring_match(&self.nbuf[self.scan..])
                };
                // Prefetch the table bucket for the next scan position and cache the hash.
                // The oldscore + scoring work below provides the latency window.
                self.cached_hash = if self.scan + 1 < nbuflen {
                    self.sa.prefetch_block(&self.nbuf[self.scan + 1..])
                } else {
                    None
                };
                // Lookahead prefetches: warm table buckets for upcoming scan positions.
                // We don't cache these hashes — the CPU pipelines the independent
                // hash computations via out-of-order execution, so recomputation is free.
                if self.scan + 2 < nbuflen {
                    self.sa.prefetch_block(&self.nbuf[self.scan + 2..]);
                }
                if self.scan + 3 < nbuflen {
                    self.sa.prefetch_block(&self.nbuf[self.scan + 3..]);
                }
                self.pos = res.start;
                self.length = res.len;

                {
                    let end = self.scan + self.length;
                    if scsc < end {
                        // Pre-compute the obuf range corresponding to nbuf[scsc..end]
                        let o_start = scsc as isize + self.lastoffset;
                        let o_end = end as isize + self.lastoffset;
                        if o_start >= 0 && o_end as usize <= obuflen {
                            // Fast path: entire range is in bounds — auto-vectorizable
                            let o_start = o_start as usize;
                            oldscore += count_matching_bytes(
                                &self.obuf[o_start..o_start + (end - scsc)],
                                &self.nbuf[scsc..end],
                            ) as isize;
                        } else {
                            // Slow path: partial bounds (rare, near buffer edges)
                            for i in scsc..end {
                                let oi = (i as isize + self.lastoffset) as usize;
                                if oi < obuflen && self.obuf[oi] == self.nbuf[i] {
                                    oldscore += 1;
                                }
                            }
                        }
                        scsc = end;
                    }
                }

                let significantly_better = self.length as isize > oldscore + 8;
                let same_length = self.length as isize == oldscore && self.length != 0;

                if same_length || significantly_better {
                    // Prefetch for the actual next scan position (scan + length),
                    // not scan + 1 which is now stale. The lenf/lenb computation
                    // below provides ample latency window for the prefetch.
                    self.cached_hash = if self.scan + self.length < nbuflen {
                        self.sa
                            .prefetch_block(&self.nbuf[self.scan + self.length..])
                    } else {
                        None
                    };
                    break 'inner;
                }

                {
                    let oi = (self.scan as isize + self.lastoffset) as usize;
                    if oi < obuflen && self.obuf[oi] == self.nbuf[self.scan] {
                        oldscore -= 1;
                    }
                }
                self.scan += 1;
            } // 'inner

            let done_scanning = self.scan == nbuflen;
            if self.length as isize != oldscore || done_scanning {
                // length forward from lastscan
                let mut lenf = {
                    let (mut s, mut sf, mut lenf) = (0_isize, 0_isize, 0_isize);

                    let n = min(self.scan - self.lastscan, obuflen - self.lastpos);
                    let o_slice = &self.obuf[self.lastpos..self.lastpos + n];
                    let n_slice = &self.nbuf[self.lastscan..self.lastscan + n];

                    for i in 0..n {
                        if o_slice[i] == n_slice[i] {
                            s += 1;
                        }

                        {
                            // the original code has an `i++` in the
                            // middle of what's essentially a while loop.
                            let i = i + 1;
                            if s * 2 - i as isize > sf * 2 - lenf {
                                sf = s;
                                lenf = i as isize;
                            }
                        }
                    }
                    lenf as usize
                };

                // length backwards from scan
                let mut lenb = if self.scan >= nbuflen {
                    0
                } else {
                    let (mut s, mut sb, mut lenb) = (0_isize, 0_isize, 0_isize);

                    let n = min(self.scan - self.lastscan, self.pos);
                    // Pre-slice: iterate backwards from pos/scan
                    let o_slice = &self.obuf[self.pos - n..self.pos];
                    let n_slice = &self.nbuf[self.scan - n..self.scan];

                    for i in 1..=n {
                        // index from end of pre-sliced regions
                        if o_slice[n - i] == n_slice[n - i] {
                            s += 1;
                        }

                        if (s * 2 - i as isize) > (sb * 2 - lenb) {
                            sb = s;
                            lenb = i as isize;
                        }
                    }
                    lenb as usize
                };

                let lastscan_was_better = self.lastscan + lenf > self.scan - lenb;
                if lastscan_was_better {
                    // if our last scan went forward more than
                    // our current scan went back, figure out how much
                    // of our current scan to crop based on scoring
                    let overlap = (self.lastscan + lenf) - (self.scan - lenb);

                    let lens = {
                        let (mut s, mut ss, mut lens) = (0, 0, 0);
                        // Pre-slice all four regions to eliminate bounds checks
                        let last_n =
                            &self.nbuf[self.lastscan + lenf - overlap..self.lastscan + lenf];
                        let last_o = &self.obuf[self.lastpos + lenf - overlap..self.lastpos + lenf];
                        let cur_n = &self.nbuf[self.scan - lenb..self.scan - lenb + overlap];
                        let cur_o = &self.obuf[self.pos - lenb..self.pos - lenb + overlap];
                        for i in 0..overlap {
                            if last_n[i] == last_o[i] {
                                // point goes to last scan
                                s += 1;
                            }
                            if cur_n[i] == cur_o[i] {
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
                    // order matters to avoid overflow
                    lenf += lens;
                    lenf -= overlap;

                    lenb -= lens;
                } // lastscan was better

                let m = Match {
                    add_old_start: self.lastpos,
                    add_new_start: self.lastscan,
                    add_length: lenf,
                    copy_end: self.scan - lenb,
                };

                self.lastscan = self.scan - lenb;
                self.lastpos = self.pos - lenb;
                self.lastoffset = self.pos as isize - self.scan as isize;

                return Some(m);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good

        None
    }
}

/// Parameters used when creating diffs
pub struct DiffParams {
    /// Block size for hash index (default 32). Must be >= 4.
    pub block_size: usize,
    pub(crate) scan_chunk_size: Option<usize>,
    /// Max threads for parallel scanning. `None` = use all available cores.
    pub(crate) num_threads: Option<usize>,
    /// Use anonymous mmap (RAM) instead of file-backed mmap for the hash table.
    /// Faster but pins memory that can't be paged to disk.
    pub use_ram: bool,
}

impl DiffParams {
    /// Construct new diff params and check validity
    ///
    /// # Parameters
    ///
    /// - `block_size`: Hash index block size. Controls granularity of matching.
    ///   Smaller blocks find more matches but use more memory. Must be >= 4.
    /// - `scan_chunk_size`: Size of chunks to use for scanning. When `None`, treat
    ///   the input as a single chunk. Smaller chunks increase parallelism but
    ///   produce slightly worse patches. When `Some`, it needs to be at least 1.
    pub fn new(
        block_size: usize,
        scan_chunk_size: Option<usize>,
    ) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
        Self::with_threads(block_size, scan_chunk_size, None)
    }

    /// Like `new`, but also sets the maximum number of threads for parallel scanning.
    pub fn with_threads(
        block_size: usize,
        scan_chunk_size: Option<usize>,
        num_threads: Option<usize>,
    ) -> Result<Self, Box<dyn Error + Send + Sync + 'static>> {
        if block_size < 4 {
            return Err("block size cannot be less than 4".into());
        }
        if scan_chunk_size.filter(|s| *s < 1).is_some() {
            return Err("scan chunk size cannot be less than 1".into());
        }
        if num_threads.filter(|n| *n < 1).is_some() {
            return Err("num_threads cannot be less than 1".into());
        }

        Ok(Self {
            block_size,
            scan_chunk_size,
            num_threads,
            use_ram: false,
        })
    }
}

impl Default for DiffParams {
    fn default() -> Self {
        Self {
            block_size: 32,
            scan_chunk_size: None,
            num_threads: None,
            use_ram: false,
        }
    }
}

/// Diff two files
#[cfg(feature = "diff")]
pub fn diff<F, E>(obuf: &[u8], nbuf: &[u8], params: &DiffParams, mut on_match: F) -> Result<(), E>
where
    F: FnMut(Match) -> Result<(), E>,
{
    #[cfg(feature = "profiling")]
    let before_index = Instant::now();

    let index = HashIndex::new(obuf, params.block_size, params.use_ram);

    #[cfg(feature = "profiling")]
    info!(
        "index build took {}",
        DurationSpeed(obuf.len() as u64, before_index.elapsed())
    );

    #[cfg(feature = "profiling")]
    let before_scan = Instant::now();

    if let Some(chunk_size) = params.scan_chunk_size {
        let num_chunks = nbuf.len().div_ceil(chunk_size);

        info!(
            "scanning with {}B chunks... ({} chunks total)",
            chunk_size, num_chunks
        );

        let mut producers = Vec::with_capacity(num_chunks);
        let mut consumers = Vec::with_capacity(num_chunks);
        for _ in 0..num_chunks {
            let (cons, prod) = ring_channel::<Match>(8192);
            producers.push(prod);
            consumers.push(cons);
        }

        let do_scan = |producers: Vec<RingProducer<Match>>| {
            nbuf.par_chunks(chunk_size)
                .zip(producers)
                .for_each(|(nbuf, mut prod)| {
                    for m in BsdiffIterator::new(obuf, nbuf, &index) {
                        prod.push(m);
                    }
                });
        };

        let num_threads = params.num_threads;
        std::thread::scope(|s| {
            s.spawn(|| {
                if let Some(n) = num_threads {
                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(n)
                        .build()
                        .expect("failed to build thread pool");
                    pool.install(|| do_scan(producers));
                } else {
                    do_scan(producers);
                }
            });

            for (i, mut cons) in consumers.into_iter().enumerate() {
                let offset = i * chunk_size;
                while let Some(mut m) = cons.pop() {
                    m.add_new_start += offset;
                    m.copy_end += offset;
                    on_match(m)?;
                }
            }

            Ok::<(), E>(())
        })?;
    } else {
        for m in BsdiffIterator::new(obuf, nbuf, &index) {
            on_match(m)?
        }
    }

    #[cfg(feature = "profiling")]
    info!(
        "scanning took {}",
        DurationSpeed(obuf.len() as u64, before_scan.elapsed())
    );

    Ok(())
}

#[cfg(feature = "profiling")]
mod profiling {
    use std::fmt;

    pub struct DurationSpeed(pub u64, pub std::time::Duration);

    impl fmt::Display for DurationSpeed {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let (size, duration) = (self.0, self.1);
            write!(f, "{:?} ({})", duration, Speed(size, duration))
        }
    }

    pub struct Speed(u64, std::time::Duration);

    impl fmt::Display for Speed {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let (size, duration) = (self.0, self.1);
            let per_sec = size as f64 / duration.as_secs_f64();
            write!(f, "{} / s", Size(per_sec as u64))
        }
    }

    pub struct Size(u64);

    impl fmt::Display for Size {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let x = self.0;

            if x > 1024 * 1024 {
                write!(f, "{:.2} MiB", x as f64 / (1024.0 * 1024.0))
            } else if x > 1024 {
                write!(f, "{:.1} KiB", x as f64 / (1024.0))
            } else {
                write!(f, "{} B", x)
            }
        }
    }
}

#[cfg(feature = "profiling")]
use profiling::DurationSpeed;

/// Like `diff()`, but calls `on_chunk(chunk_index, chunk_nbuf, matches, hash_index)` per chunk
/// with chunk-relative match positions (add_new_start/copy_end NOT offset-adjusted).
#[cfg(feature = "diff")]
pub fn diff_chunked<F, E>(
    obuf: &[u8],
    nbuf: &[u8],
    params: &DiffParams,
    mut on_chunk: F,
) -> Result<(), E>
where
    F: FnMut(usize, &[u8], &mut dyn Iterator<Item = Match>, &HashIndex) -> Result<(), E>,
{
    #[cfg(feature = "profiling")]
    let before_index = Instant::now();

    let index = HashIndex::new(obuf, params.block_size, params.use_ram);

    #[cfg(feature = "profiling")]
    info!(
        "index build took {}",
        DurationSpeed(obuf.len() as u64, before_index.elapsed())
    );

    #[cfg(feature = "profiling")]
    let before_scan = Instant::now();

    if let Some(chunk_size) = params.scan_chunk_size {
        let num_chunks = nbuf.len().div_ceil(chunk_size);

        info!(
            "scanning with {}B chunks... ({} chunks total)",
            chunk_size, num_chunks
        );

        let mut producers = Vec::with_capacity(num_chunks);
        let mut consumers = Vec::with_capacity(num_chunks);
        for _ in 0..num_chunks {
            let (cons, prod) = ring_channel::<Match>(8192);
            producers.push(prod);
            consumers.push(cons);
        }

        let do_scan = |producers: Vec<RingProducer<Match>>| {
            nbuf.par_chunks(chunk_size)
                .zip(producers)
                .for_each(|(nbuf, mut prod)| {
                    for m in BsdiffIterator::new(obuf, nbuf, &index) {
                        prod.push(m);
                    }
                });
        };

        let num_threads = params.num_threads;
        std::thread::scope(|s| {
            s.spawn(|| {
                if let Some(n) = num_threads {
                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(n)
                        .build()
                        .expect("failed to build thread pool");
                    pool.install(|| do_scan(producers));
                } else {
                    do_scan(producers);
                }
            });

            for (i, mut cons) in consumers.into_iter().enumerate() {
                let start = i * chunk_size;
                let end = (start + chunk_size).min(nbuf.len());
                let chunk_nbuf = &nbuf[start..end];
                let mut iter = std::iter::from_fn(|| cons.pop());
                on_chunk(i, chunk_nbuf, &mut iter, &index)?;
            }

            Ok::<(), E>(())
        })?;
    } else {
        // Non-chunked path: single-threaded scan.
        index.populate();
        let mut iter = BsdiffIterator::new(obuf, nbuf, &index);
        on_chunk(0, nbuf, &mut iter, &index)?;
    }

    #[cfg(feature = "profiling")]
    info!(
        "scanning took {}",
        DurationSpeed(obuf.len() as u64, before_scan.elapsed())
    );

    Ok(())
}

/// Produce a chunked patch: header + independent zstd-compressed sub-patches per scan chunk.
#[cfg(all(feature = "diff", feature = "patch"))]
pub fn simple_diff_chunked_with_params(
    older: &[u8],
    newer: &[u8],
    out: &mut dyn Write,
    diff_params: &DiffParams,
    zstd_level: i32,
) -> Result<(), io::Error> {
    let chunk_size = diff_params.scan_chunk_size.unwrap_or(newer.len().max(1));
    let num_chunks = if newer.is_empty() {
        1
    } else {
        newer.len().div_ceil(chunk_size)
    };

    // Write header
    out.write_all(&MAGIC.to_le_bytes())?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(newer.len() as u64).to_le_bytes())?;
    out.write_all(&(num_chunks as u32).to_le_bytes())?;

    diff_chunked(
        older,
        newer,
        diff_params,
        |i, chunk_nbuf, matches, _index| {
            let new_start = i * chunk_size;
            let new_len = chunk_nbuf.len();

            // Collect sub-patch Controls into a buffer
            let mut sub_patch = Vec::new();
            let mut w = enc::Writer::new(&mut sub_patch);
            let mut first_old_start: u64 = 0;
            let mut is_first = true;

            let mut translator =
                Translator::new(older, chunk_nbuf, |control| -> Result<(), io::Error> {
                    w.write_extended(control, None)
                });

            #[allow(clippy::while_let_on_iterator)]
            while let Some(m) = matches.next() {
                if is_first {
                    first_old_start = m.add_old_start as u64;
                    is_first = false;
                }
                translator.translate(m)?;
            }
            translator.close()?;

            // Compress sub-patch independently
            let raw_len = sub_patch.len() as u64;
            let compressed =
                zstd::bulk::compress(&sub_patch, zstd_level).map_err(io::Error::other)?;

            // Write chunk metadata + compressed data
            out.write_all(&first_old_start.to_le_bytes())?;
            out.write_all(&(new_start as u64).to_le_bytes())?;
            out.write_all(&(new_len as u64).to_le_bytes())?;
            out.write_all(&raw_len.to_le_bytes())?;
            out.write_all(&(compressed.len() as u64).to_le_bytes())?;
            out.write_all(&compressed)?;

            Ok::<(), io::Error>(())
        },
    )?;

    Ok(())
}

#[cfg(feature = "diff")]
pub fn assert_cycle(older: &[u8], newer: &[u8]) {
    assert_cycle_with_params(older, newer, &Default::default());
}

#[cfg(feature = "diff")]
pub fn assert_cycle_with_params(older: &[u8], newer: &[u8], params: &DiffParams) {
    let mut older_pos = 0_usize;
    let mut newer_pos = 0_usize;

    let mut translator = Translator::new(older, newer, |control| -> Result<(), std::io::Error> {
        for &ab in control.add {
            let fb = ab.wrapping_add(older[older_pos]);
            older_pos += 1;

            let nb = newer[newer_pos];
            newer_pos += 1;

            assert_eq!(fb, nb);
        }

        for &cb in control.copy {
            let nb = newer[newer_pos];
            newer_pos += 1;

            assert_eq!(cb, nb);
        }

        older_pos = (older_pos as i64 + control.seek) as usize;

        Ok(())
    });

    diff(older, newer, params, |m| translator.translate(m)).unwrap();

    translator.close().unwrap();

    assert_eq!(
        newer_pos,
        newer.len(),
        "fresh should have same length as newer"
    );
}

#[cfg(test)]
mod tests {
    use super::instructions::apply_instructions;
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn short_patch() {
        let older = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            1, 2, 0,
        ];
        let instructions = [
            12, 16, 5, 40, 132, 1, 47, 43, 20, 86, 150, 0, 150, 0, 150, 0, 115, 31, 0, 0, 0, 0, 0,
            0, 0, 1, 38, 188, 128, 0, 150, 0,
        ];
        let newer = apply_instructions(&older[..], &instructions[..]);

        super::assert_cycle(&older[..], &newer[..]);
    }

    proptest! {
        #[test]
        fn cycle(older: [u8; 32], instructions: [u8; 32]) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            println!("{} => {}", older.len(), newer.len());
            super::assert_cycle(&older[..], &newer[..]);
        }

        #[test]
        fn cycle_hashindex(
            older in proptest::collection::vec(any::<u8>(), 64..256),
            instructions in proptest::collection::vec(any::<u8>(), 32..128),
        ) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            let params = DiffParams::new(4, None).unwrap();
            super::assert_cycle_with_params(&older[..], &newer[..], &params);
        }
    }
}
