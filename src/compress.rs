//! Compressing each chunk's instructions into one zstd frame.

use crate::patch::MAX_WINDOW_LOG;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;
use zstd::zstd_safe::{
    self, CCtx, CParameter, InBuffer, OutBuffer, ResetDirective,
    zstd_sys::{self, ZSTD_EndDirective},
};

/// The most of a chunk's compressed instructions held in memory under a
/// memory limit; the rest spill to a file. Also the most of its
/// instructions gathered before compressing.
pub(crate) const MAX_HELD: usize = 8 << 20;

fn zstd_error(code: zstd_safe::ErrorCode) -> io::Error {
    io::Error::other(zstd_safe::get_error_name(code))
}

fn out_of_memory() -> io::Error {
    io::Error::from(io::ErrorKind::OutOfMemory)
}

fn cparams(level: i32, size: usize) -> zstd_sys::ZSTD_compressionParameters {
    // zstd sizes its window and tables to fit the size: one no larger than
    // the window cap keeps the window within it, and 0 would mean unknown,
    // which picks the largest.
    let size = (size as u64).clamp(1, 1 << MAX_WINDOW_LOG);
    // SAFETY: a pure function of its arguments.
    unsafe { zstd_sys::ZSTD_getCParams(level, size, 0) }
}

fn context_size(cp: zstd_sys::ZSTD_compressionParameters) -> io::Result<usize> {
    // SAFETY: a pure function of its argument.
    let size = unsafe { zstd_sys::ZSTD_estimateCStreamSize_usingCParams(cp) };
    // SAFETY: as above.
    if unsafe { zstd_sys::ZSTD_isError(size) } != 0 {
        return Err(zstd_error(size));
    }
    Ok(size)
}

/// The most of a chunk's instructions gathered before compressing them,
/// so that zstd sees them whole, as it compresses best: twice the chunk
/// size, which only chunks of many short matches exceed.
fn gather_limit(chunk_len: usize) -> usize {
    chunk_len.saturating_mul(2).clamp(1, MAX_HELD)
}

/// Bytes a [`ChunkCompressor`] holds: its buffers, and zstd's own estimate
/// of its context for whichever way it compresses takes more.
pub(crate) fn memory(level: i32, chunk_len: usize) -> io::Result<usize> {
    let gathered = gather_limit(chunk_len);
    // zstd picks its parameters from four tables by size; within each, they
    // grow with the size, so the largest size each covers bounds it.
    let mut context = context_size(cparams(level, chunk_len))?;
    for size in [1 << 14, 1 << 17, 1 << 18, gathered] {
        context = context.max(context_size(cparams(level, size.min(gathered)))?);
    }
    Ok(context
        .saturating_add(gathered)
        .saturating_add(output_len(gathered)))
}

/// The output buffer, which holds a gathered chunk's whole frame.
fn output_len(gathered: usize) -> usize {
    zstd_safe::compress_bound(gathered).max(CCtx::out_size())
}

/// Makes zstd frames with one context for the whole diff.
///
/// A chunk's instructions are gathered and compressed whole, with the
/// level's parameters for their size, as zstd compresses best: in one call,
/// as `ZSTD_compress2`, since fed through its streaming buffer zstd sees
/// less at once and compresses slightly worse. Those of a
/// chunk that outgrows the gathering buffer are streamed instead, with
/// parameters for the chunk size, as zstd does not know where the stream
/// will end.
pub(crate) struct ChunkCompressor {
    cctx: CCtx<'static>,
    level: i32,
    streaming: zstd_sys::ZSTD_compressionParameters,
    gathered: Vec<u8>,
    output: Vec<u8>,
}

impl ChunkCompressor {
    pub(crate) fn new(level: i32, chunk_len: usize) -> io::Result<Self> {
        let cctx = CCtx::try_create().ok_or_else(out_of_memory)?;
        let mut gathered = Vec::new();
        gathered
            .try_reserve_exact(gather_limit(chunk_len))
            .map_err(|_| out_of_memory())?;
        let output_len = output_len(gather_limit(chunk_len));
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| out_of_memory())?;
        output.resize(output_len, 0);
        Ok(Self {
            cctx,
            level,
            streaming: cparams(level, chunk_len),
            gathered,
            output,
        })
    }

    /// Compress what `write` writes into `frame` as one zstd frame, and
    /// return how many bytes it wrote.
    pub(crate) fn frame(
        &mut self,
        frame: &mut Spill,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<u64> {
        self.cctx
            .reset(ResetDirective::SessionAndParameters)
            .map_err(zstd_error)?;
        self.cctx
            .set_parameter(CParameter::CompressionLevel(self.level))
            .map_err(zstd_error)?;
        frame.clear()?;
        self.gathered.clear();
        let mut stream = Stream {
            compressor: self,
            frame,
            raw: 0,
            streaming: false,
        };
        write(&mut stream)?;
        if stream.streaming {
            stream.flush_gathered(ZSTD_EndDirective::ZSTD_e_end)?;
        } else {
            // The output holds the compress bound of all that fits in the
            // gathering buffer, so this cannot run out of room.
            let c = &mut *stream.compressor;
            let written = c
                .cctx
                .compress2(c.output.as_mut_slice(), &c.gathered)
                .map_err(zstd_error)?;
            stream
                .frame
                .write_all(c.output.get(..written).unwrap_or_default())?;
        }
        Ok(stream.raw)
    }

    /// Compress `input`, writing what zstd produces to `frame`, until it has
    /// taken all of the input and, when ending, flushed the frame's end.
    fn compress(
        cctx: &mut CCtx<'static>,
        output: &mut [u8],
        frame: &mut Spill,
        input: &[u8],
        op: ZSTD_EndDirective,
    ) -> io::Result<()> {
        let mut input = InBuffer::around(input);
        loop {
            let (remaining, written) = {
                let mut out = OutBuffer::around(&mut *output);
                let remaining = cctx
                    .compress_stream2(&mut out, &mut input, op)
                    .map_err(zstd_error)?;
                (remaining, out.pos())
            };
            frame.write_all(output.get(..written).unwrap_or_default())?;
            let done = match op {
                ZSTD_EndDirective::ZSTD_e_end => remaining == 0,
                _ => input.pos() == input.src.len(),
            };
            if done {
                return Ok(());
            }
        }
    }
}

/// One frame being written.
struct Stream<'c> {
    compressor: &'c mut ChunkCompressor,
    frame: &'c mut Spill,
    raw: u64,
    streaming: bool,
}

impl Stream<'_> {
    fn flush_gathered(&mut self, op: ZSTD_EndDirective) -> io::Result<()> {
        let c = &mut *self.compressor;
        ChunkCompressor::compress(&mut c.cctx, &mut c.output, self.frame, &c.gathered, op)?;
        c.gathered.clear();
        Ok(())
    }

    /// Give zstd the parameters for the chunk size, which it takes only
    /// before a frame's first input, and pass it what is gathered.
    fn start_streaming(&mut self) -> io::Result<()> {
        let cp = self.compressor.streaming;
        for p in [
            CParameter::WindowLog(cp.windowLog),
            CParameter::HashLog(cp.hashLog),
            CParameter::ChainLog(cp.chainLog),
            CParameter::SearchLog(cp.searchLog),
            CParameter::MinMatch(cp.minMatch),
            CParameter::TargetLength(cp.targetLength),
            CParameter::Strategy(cp.strategy),
        ] {
            self.compressor.cctx.set_parameter(p).map_err(zstd_error)?;
        }
        self.streaming = true;
        self.flush_gathered(ZSTD_EndDirective::ZSTD_e_continue)
    }
}

impl Write for Stream<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let gathered = &self.compressor.gathered;
        if buf.len() > gathered.capacity() - gathered.len() {
            match self.streaming {
                false => self.start_streaming()?,
                true => self.flush_gathered(ZSTD_EndDirective::ZSTD_e_continue)?,
            }
        }
        // While streaming, the buffer still gathers small writes, so zstd
        // is called once per buffer rather than once per instruction.
        if buf.len() > self.compressor.gathered.capacity() {
            let c = &mut *self.compressor;
            ChunkCompressor::compress(
                &mut c.cctx,
                &mut c.output,
                self.frame,
                buf,
                ZSTD_EndDirective::ZSTD_e_continue,
            )?;
        } else {
            self.compressor.gathered.extend_from_slice(buf);
        }
        self.raw += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A chunk's compressed frame: up to `limit` bytes in memory, and the rest
/// in an unnamed file in `dir`, made when first needed and kept for later
/// chunks.
pub(crate) struct Spill {
    held: Vec<u8>,
    limit: usize,
    dir: PathBuf,
    file: Option<File>,
    spilled: u64,
}

impl Spill {
    pub(crate) fn new(limit: usize, dir: PathBuf) -> Self {
        Self {
            held: Vec::new(),
            limit,
            dir,
            file: None,
            spilled: 0,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.held.len() as u64 + self.spilled
    }

    fn clear(&mut self) -> io::Result<()> {
        self.held.clear();
        if let Some(file) = &mut self.file
            && self.spilled > 0
        {
            file.set_len(0)?;
            file.rewind()?;
        }
        self.spilled = 0;
        Ok(())
    }

    /// Write the whole frame to `out`.
    pub(crate) fn copy_to(&mut self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(&self.held)?;
        if let Some(file) = &mut self.file
            && self.spilled > 0
        {
            file.rewind()?;
            let copied = io::copy(&mut file.take(self.spilled), out)?;
            if copied != self.spilled {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the spilled part of a compressed chunk is shorter than was written",
                ));
            }
        }
        Ok(())
    }
}

impl Write for Spill {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = self.held.len();
        if self.spilled == 0 && buf.len() <= self.limit.saturating_sub(len) {
            if self.held.capacity() - len < buf.len() {
                // Grow as a Vec would, but never past the limit.
                let want = (self.held.capacity() * 2)
                    .max(len + buf.len())
                    .min(self.limit);
                self.held
                    .try_reserve_exact(want - len)
                    .map_err(|_| out_of_memory())?;
            }
            self.held.extend_from_slice(buf);
        } else {
            let file = match &mut self.file {
                Some(file) => file,
                None => self.file.insert(tempfile::tempfile_in(&self.dir)?),
            };
            file.write_all(buf)?;
            self.spilled += buf.len() as u64;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::frame;

    fn compress(compressor: &mut ChunkCompressor, frame: &mut Spill, raw: &[u8]) -> Vec<u8> {
        let written = compressor
            .frame(frame, |w| {
                for piece in raw.chunks(1000) {
                    w.write_all(piece)?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(written, raw.len() as u64);
        let mut out = Vec::new();
        frame.copy_to(&mut out).unwrap();
        assert_eq!(out.len() as u64, frame.len());
        assert!(zstd::bulk::decompress(&out, raw.len()).unwrap() == raw);
        out
    }

    fn sparse(len: usize) -> Vec<u8> {
        let mut raw = vec![0u8; len];
        let mut x = 1u32;
        for b in raw.iter_mut().step_by(61) {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 24) as u8;
        }
        raw
    }

    fn in_memory() -> Spill {
        Spill::new(usize::MAX, std::env::temp_dir())
    }

    #[test]
    fn gathered_chunks_compress_as_zstd_does_them_whole() {
        for level in [3, 19] {
            let mut c = ChunkCompressor::new(level, 1 << 20).unwrap();
            for len in [0, 100, 300_000, 1_047_713, 2 << 20] {
                let raw = sparse(len);
                let out = compress(&mut c, &mut in_memory(), &raw);
                let whole = zstd::bulk::compress(&raw, level).unwrap();
                assert!(out == whole, "level {level}, len {len}");
            }
        }
    }

    #[test]
    fn caps_the_window() {
        // Longer than the cap, so level 22 would otherwise pick a larger window.
        let raw = sparse((1 << MAX_WINDOW_LOG) + (1 << 20));
        let uncapped = zstd::bulk::compress(&raw, 22).unwrap();
        assert!(frame::window_size(&uncapped).unwrap() > 1 << MAX_WINDOW_LOG);

        for chunk_len in [1 << 20, raw.len()] {
            let mut c = ChunkCompressor::new(22, chunk_len).unwrap();
            let capped = compress(&mut c, &mut in_memory(), &raw);
            assert!(frame::window_size(&capped).unwrap() <= 1 << MAX_WINDOW_LOG);
        }
    }

    #[test]
    fn frames_spill_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ChunkCompressor::new(3, 1 << 20).unwrap();
        let mut spill = Spill::new(100, dir.path().to_owned());
        for len in [1 << 20, 10, 300_000, 5 << 20] {
            compress(&mut c, &mut spill, &sparse(len));
            assert!(spill.held.len() <= 100);
        }
        // The file is unnamed, so nothing is left in the directory.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn memory_covers_the_context() {
        for level in [1, 3, 9, 19, 22] {
            for chunk_len in [64 << 10, 1 << 20, 4 << 20] {
                let mut c = ChunkCompressor::new(level, chunk_len).unwrap();
                let planned = memory(level, chunk_len).unwrap();
                for len in [10_000, 200_000, chunk_len, 3 * chunk_len] {
                    compress(&mut c, &mut in_memory(), &sparse(len));
                    let used = c.cctx.sizeof() + c.gathered.capacity() + c.output.capacity();
                    assert!(
                        used <= planned,
                        "level {level}, chunk {chunk_len}, {len} bytes: {used} > {planned}"
                    );
                }
            }
        }
    }
}
