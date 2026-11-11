#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub const MAGIC: u32 = 0xB1DF;
/// Patch format version. 0x2001 is the first to carry base and result
/// hashes; patches of any other version are refused, not misread.
pub const VERSION: u32 = 0x2001;

use std::error::Error;
use std::io;
#[cfg(all(feature = "diff", feature = "patch"))]
use std::io::Write;
use std::path::PathBuf;
#[cfg(feature = "diff")]
use tracing::info;

#[cfg(feature = "diff")]
use hashindex::HashIndex;
#[cfg(feature = "diff")]
use plan::Plan;

#[cfg(feature = "profiling")]
use std::time::Instant;

#[cfg(all(feature = "diff", feature = "patch"))]
mod compress;

#[cfg(feature = "patch")]
pub mod enc;

#[cfg(feature = "diff")]
pub mod hashindex;

#[cfg(feature = "patch")]
pub mod patch;

#[cfg(feature = "diff")]
mod plan;

#[cfg(feature = "diff")]
mod scan;

#[cfg(feature = "diff")]
mod threads;

#[cfg(any(test, feature = "instructions"))]
pub mod instructions;

#[derive(Debug)]
pub struct Match {
    pub add_old_start: usize,
    pub add_new_start: usize,
    pub add_length: usize,
    pub copy_end: usize,
}

fn match_out_of_range() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "a match lies outside the files being diffed",
    )
}

impl Match {
    #[inline(always)]
    pub fn copy_start(&self) -> usize {
        self.add_new_start.saturating_add(self.add_length)
    }

    /// The new bytes this match adds to old bytes, and those old bytes.
    fn add_spans<'a>(&self, obuf: &'a [u8], nbuf: &'a [u8]) -> io::Result<(&'a [u8], &'a [u8])> {
        let span = |buf: &'a [u8], start: usize| {
            buf.get(start..)
                .and_then(|b| b.get(..self.add_length))
                .ok_or_else(match_out_of_range)
        };
        Ok((
            span(nbuf, self.add_new_start)?,
            span(obuf, self.add_old_start)?,
        ))
    }

    /// The new bytes this match copies as they are.
    fn copy_span<'a>(&self, nbuf: &'a [u8]) -> io::Result<&'a [u8]> {
        nbuf.get(self.copy_start()..self.copy_end)
            .ok_or_else(match_out_of_range)
    }

    /// How far the old cursor moves from the end of this match's add bytes
    /// to the start of `next`'s.
    fn seek_to(&self, next: &Match) -> i64 {
        next.add_old_start as i64 - self.add_old_start.saturating_add(self.add_length) as i64
    }
}

#[derive(Debug, Clone)]
pub struct Control<'a> {
    pub add: &'a [u8],
    pub copy: &'a [u8],
    pub seek: i64,
}

/// Turns matches into controls, each sent once the next match gives its
/// seek. A match outside the files is an
/// [`InvalidInput`](io::ErrorKind::InvalidInput) error.
pub struct Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: From<io::Error>,
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
    E: From<io::Error>,
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
            let copy = pm.copy_span(self.nbuf)?;
            let seek = m.map_or(0, |m| pm.seek_to(m));
            (self.on_control)(&Control {
                add: &self.buf,
                copy,
                seek,
            })?;
        }
        Ok(())
    }

    pub fn translate(&mut self, m: Match) -> Result<(), E> {
        self.send_control(Some(&m))?;

        let (new, old) = m.add_spans(self.obuf, self.nbuf)?;
        self.buf.clear();
        self.buf
            .try_reserve(new.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        // Zipping slices lets the compiler see matching lengths and elide
        // bounds checks, enabling auto-vectorization of the wrapping_sub loop.
        self.buf
            .extend(new.iter().zip(old).map(|(a, b)| a.wrapping_sub(*b)));

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
    E: From<io::Error>,
{
    fn drop(&mut self) {
        // dropping a Translator ignores errors on purpose,
        // just like File does
        let _ = self.do_close();
    }
}

/// Parameters used when creating diffs
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "diff"), allow(dead_code))]
pub struct DiffParams {
    block_size: usize,
    scan_chunk_size: Option<usize>,
    num_threads: Option<usize>,
    memory_limit: Option<usize>,
    temp_dir: Option<PathBuf>,
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

    /// Like `new`, but also sets the maximum number of threads that build
    /// the hash table and scan. `None` uses one per core.
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
            memory_limit: None,
            temp_dir: None,
        })
    }

    /// Hold the memory bidiff itself uses for a diff to `bytes`. That is:
    ///
    /// - the hash table, half a byte to a byte per byte of the old file
    ///   ([`hashindex::table_bytes`]);
    /// - the matches scanned ahead of the one chunk being written, about
    ///   400 KiB per thread with each thread's stack;
    /// - when writing a patch, the zstd compressor, which zstd sizes for the
    ///   level and scan chunk size, and the chunk being compressed, up to its
    ///   own size or 8 MiB, whichever is less.
    ///
    /// All but the table have a fixed size, which comes out of the limit
    /// first, with fewer threads than asked for if they do not all fit; if
    /// one thread does not, the diff fails with
    /// [`OutOfMemory`](io::ErrorKind::OutOfMemory) before it starts. The
    /// table then stays in RAM if the rest of the limit holds it, and
    /// otherwise goes in a file in the [temporary
    /// directory](Self::with_temp_dir), whose pages the kernel writes out
    /// under memory pressure rather than holding them; so does the part of a
    /// compressed chunk past what is held.
    ///
    /// The two files being diffed and whatever the patch is written to are
    /// the caller's, and not counted. Without a limit, everything is held in
    /// RAM.
    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// The directory for the files a [memory limit](Self::with_memory_limit)
    /// sends work to. The default, [`std::env::temp_dir`], is kept in RAM
    /// (tmpfs) on some systems, where those files hold memory after all.
    pub fn with_temp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(dir.into());
        self
    }

    #[cfg(feature = "diff")]
    pub(crate) fn temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

impl Default for DiffParams {
    fn default() -> Self {
        Self {
            block_size: 32,
            scan_chunk_size: None,
            num_threads: None,
            memory_limit: None,
            temp_dir: None,
        }
    }
}

/// Diff two files
#[cfg(feature = "diff")]
pub fn diff<F, E>(obuf: &[u8], nbuf: &[u8], params: &DiffParams, mut on_match: F) -> Result<(), E>
where
    F: FnMut(Match) -> Result<(), E>,
    E: From<io::Error>,
{
    let chunk_size = params.scan_chunk_size;
    diff_chunked(obuf, nbuf, params, |i, _, matches, _| {
        let offset = chunk_size.map_or(0, |size| i * size);
        for mut m in matches {
            m.add_new_start += offset;
            m.copy_end += offset;
            on_match(m)?;
        }
        Ok(())
    })
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
///
/// Chunks come in order, on the calling thread, while later ones are
/// scanned on others. If `on_chunk` fails, scanning stops and its error is
/// returned; it need not take all of a chunk's matches.
#[cfg(feature = "diff")]
pub fn diff_chunked<F, E>(
    obuf: &[u8],
    nbuf: &[u8],
    params: &DiffParams,
    on_chunk: F,
) -> Result<(), E>
where
    F: FnMut(usize, &[u8], &mut dyn Iterator<Item = Match>, &HashIndex) -> Result<(), E>,
    E: From<io::Error>,
{
    let plan = Plan::new(params, obuf.len(), 0)?;
    run(obuf, nbuf, params, &plan, on_chunk)
}

#[cfg(feature = "diff")]
fn run<F, E>(
    obuf: &[u8],
    nbuf: &[u8],
    params: &DiffParams,
    plan: &Plan,
    mut on_chunk: F,
) -> Result<(), E>
where
    F: FnMut(usize, &[u8], &mut dyn Iterator<Item = Match>, &HashIndex) -> Result<(), E>,
    E: From<io::Error>,
{
    #[cfg(feature = "profiling")]
    let before_index = Instant::now();

    let index = HashIndex::new(
        obuf,
        params.block_size,
        plan.table_dir.as_deref(),
        plan.threads,
    )?;

    #[cfg(feature = "profiling")]
    info!(
        "index build took {}",
        DurationSpeed(obuf.len() as u64, before_index.elapsed())
    );

    #[cfg(feature = "profiling")]
    let before_scan = Instant::now();

    match params.scan_chunk_size {
        Some(chunk_size) => {
            info!(
                "scanning with {}B chunks... ({} chunks total)",
                chunk_size,
                nbuf.len().div_ceil(chunk_size)
            );
            scan::chunks(
                obuf,
                nbuf,
                chunk_size,
                &index,
                plan.threads,
                |i, chunk, matches| on_chunk(i, chunk, matches, &index),
            )?;
        }
        None => {
            let mut matches = scan::BsdiffIterator::new(obuf, nbuf, &index);
            on_chunk(0, nbuf, &mut matches, &index)?;
        }
    }

    #[cfg(feature = "profiling")]
    info!(
        "scanning took {}",
        DurationSpeed(obuf.len() as u64, before_scan.elapsed())
    );

    Ok(())
}

/// Write a chunk's instruction stream to `w`: a control per match, each
/// written once the next match gives its seek. Returns where in `older` the
/// first match starts.
#[cfg(all(feature = "diff", feature = "patch"))]
fn write_instructions(
    older: &[u8],
    chunk: &[u8],
    matches: &mut dyn Iterator<Item = Match>,
    w: &mut dyn Write,
) -> io::Result<usize> {
    let mut w = enc::Writer::new(w);
    let write = |w: &mut enc::Writer<_>, m: &Match, seek| {
        let (new, old) = m.add_spans(older, chunk)?;
        w.write_diff(new, old, m.copy_span(chunk)?, seek)
    };
    let Some(mut prev) = matches.next() else {
        return Ok(0);
    };
    let first_old_start = prev.add_old_start;
    for m in matches {
        write(&mut w, &prev, prev.seek_to(&m))?;
        prev = m;
    }
    write(&mut w, &prev, 0)?;
    Ok(first_old_start)
}

/// Produce a chunked patch: header + independent zstd-compressed sub-patches per scan chunk.
///
/// Nothing is written if the diff cannot start, as when the memory limit is
/// too small. Otherwise an error leaves `out` holding part of a patch.
#[cfg(all(feature = "diff", feature = "patch"))]
pub fn simple_diff_chunked_with_params(
    older: &[u8],
    newer: &[u8],
    out: &mut dyn Write,
    diff_params: &DiffParams,
    zstd_level: i32,
) -> Result<(), io::Error> {
    let chunk_size = diff_params.scan_chunk_size.unwrap_or(newer.len().max(1));
    // Must match how many times diff_chunked calls back: once per scan chunk,
    // or exactly once when unchunked, even for an empty `newer`.
    let num_chunks = match diff_params.scan_chunk_size {
        Some(size) => newer.len().div_ceil(size),
        None => 1,
    };
    let num_chunks = u32::try_from(num_chunks).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "more than u32::MAX chunks; use a larger scan chunk size",
        )
    })?;

    let held = match diff_params.memory_limit {
        Some(_) => chunk_size.min(compress::MAX_HELD),
        None => usize::MAX,
    };
    let compressor = compress::memory(zstd_level, chunk_size)?;
    let plan = Plan::new(diff_params, older.len(), compressor.saturating_add(held))?;
    let mut compressor = compress::ChunkCompressor::new(zstd_level, chunk_size)?;
    let mut frame = compress::Spill::new(held, diff_params.temp_dir());

    patch::write_header(out, older, newer, num_chunks)?;

    run(
        older,
        newer,
        diff_params,
        &plan,
        |i, chunk_nbuf, matches, _index| {
            let mut first_old_start = 0;
            let raw_len = compressor.frame(&mut frame, |w| {
                first_old_start = write_instructions(older, chunk_nbuf, matches, w)?;
                Ok(())
            })?;
            patch::write_chunk(
                out,
                first_old_start as u64,
                (i * chunk_size) as u64,
                chunk_nbuf.len() as u64,
                raw_len,
                &mut frame,
            )
        },
    )
}

#[cfg(all(test, feature = "diff"))]
mod tests {
    use super::instructions::apply_instructions;
    use super::*;
    use proptest::prelude::*;

    fn assert_cycle(older: &[u8], newer: &[u8]) {
        assert_cycle_with_params(older, newer, &Default::default());
    }

    fn assert_cycle_with_params(older: &[u8], newer: &[u8], params: &DiffParams) {
        let mut older_pos = 0_usize;
        let mut newer_pos = 0_usize;

        let mut translator = Translator::new(older, newer, |control| -> Result<(), io::Error> {
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

        assert_cycle(&older[..], &newer[..]);
    }

    #[test]
    fn translator_refuses_matches_outside_the_files() {
        let (older, newer) = (b"abcdefgh", b"abcdefgh");
        let mut translator = Translator::new(older, newer, |_| -> Result<(), io::Error> { Ok(()) });
        let m = Match {
            add_old_start: 4,
            add_new_start: 0,
            add_length: 5,
            copy_end: 5,
        };
        let err = translator.translate(m).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    proptest! {
        #[test]
        fn cycle(older: [u8; 32], instructions: [u8; 32]) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            println!("{} => {}", older.len(), newer.len());
            assert_cycle(&older[..], &newer[..]);
        }

        #[test]
        fn cycle_hashindex(
            older in proptest::collection::vec(any::<u8>(), 64..256),
            instructions in proptest::collection::vec(any::<u8>(), 32..128),
        ) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            let params = DiffParams::new(4, None).unwrap();
            assert_cycle_with_params(&older[..], &newer[..], &params);
        }

        #[test]
        fn cycle_chunked(
            older in proptest::collection::vec(any::<u8>(), 64..512),
            instructions in proptest::collection::vec(any::<u8>(), 32..128),
            chunk in 1usize..100,
        ) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            let params = DiffParams::with_threads(4, Some(chunk), Some(3)).unwrap();
            assert_cycle_with_params(&older[..], &newer[..], &params);
        }
    }
}
