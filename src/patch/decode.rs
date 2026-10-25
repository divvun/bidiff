use super::{ChunkRef, Error, MAX_WINDOW_LOG, Malformed, frame};
use std::io::{self, ErrorKind, Write};
use zstd::zstd_safe::{self, DCtx, DParameter, InBuffer, OutBuffer, ResetDirective};

pub(crate) const INSTRUCTION_BUF_LEN: usize = 128 * 1024;

/// What one decoder and its instruction buffer hold beyond the window: zstd's
/// context and block buffers, which tests::decoder_memory measures at about
/// half this.
#[cfg(any(feature = "rayon", test))]
pub(crate) const DECODER_OVERHEAD: usize = 1 << 20;

const OUTPUT_BUF_LEN: usize = 1024 * 1024;

const MAX_VARINT_LEN: usize = 10;

fn zstd_error(code: zstd_safe::ErrorCode) -> io::Error {
    io::Error::other(zstd_safe::get_error_name(code))
}

/// A zstd decoder that refuses frames needing more than the format's window.
pub(crate) fn decoder() -> Result<DCtx<'static>, Error> {
    let mut d = DCtx::try_create().ok_or_else(|| {
        Error::Io(io::Error::new(
            ErrorKind::OutOfMemory,
            "cannot allocate a zstd decoder",
        ))
    })?;
    d.set_parameter(DParameter::WindowLogMax(MAX_WINDOW_LOG))
        .map_err(|c| Error::Io(zstd_error(c)))?;
    Ok(d)
}

pub(crate) trait Sink {
    fn copy(&mut self, src: &[u8]) -> Result<(), Error>;
    /// `delta` and `old` have the same length.
    fn add(&mut self, delta: &[u8], old: &[u8]) -> Result<(), Error>;
}

pub(crate) struct SliceSink<'o> {
    out: &'o mut [u8],
    pos: usize,
}

impl<'o> SliceSink<'o> {
    pub(crate) fn new(out: &'o mut [u8]) -> Self {
        Self { out, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&mut [u8], Error> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.out.len());
        let Some(end) = end else {
            return Err(Error::Io(io::Error::new(
                ErrorKind::InvalidInput,
                "output slice is shorter than the chunk",
            )));
        };
        let dst = &mut self.out[self.pos..end];
        self.pos = end;
        Ok(dst)
    }
}

impl Sink for SliceSink<'_> {
    #[inline]
    fn copy(&mut self, src: &[u8]) -> Result<(), Error> {
        self.take(src.len())?.copy_from_slice(src);
        Ok(())
    }

    #[inline]
    fn add(&mut self, delta: &[u8], old: &[u8]) -> Result<(), Error> {
        let dst = self.take(delta.len())?;
        for ((o, d), b) in dst.iter_mut().zip(delta).zip(old) {
            *o = d.wrapping_add(*b);
        }
        Ok(())
    }
}

pub(crate) struct WriteSink<'w, W: Write + ?Sized> {
    out: &'w mut W,
    buf: Vec<u8>,
    hasher: blake3::Hasher,
}

impl<'w, W: Write + ?Sized> WriteSink<'w, W> {
    pub(crate) fn new(out: &'w mut W, new_size: u64) -> Self {
        // `new_size` comes from the patch, so it only ever shrinks the buffer.
        let cap = new_size.min(OUTPUT_BUF_LEN as u64) as usize;
        Self {
            out,
            buf: Vec::with_capacity(cap),
            hasher: blake3::Hasher::new(),
        }
    }

    fn emit(out: &mut W, hasher: &mut blake3::Hasher, data: &[u8]) -> Result<(), Error> {
        // Not update_rayon: called from outside the pool, each call waits on
        // a worker handoff that costs more than it saves at this block size.
        hasher.update(data);
        out.write_all(data).map_err(Error::Io)
    }

    fn flush_buf(&mut self) -> Result<(), Error> {
        if !self.buf.is_empty() {
            Self::emit(self.out, &mut self.hasher, &self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Flush everything and return the hash of all bytes written.
    pub(crate) fn finish(mut self) -> Result<blake3::Hash, Error> {
        self.flush_buf()?;
        self.out.flush().map_err(Error::Io)?;
        Ok(self.hasher.finalize())
    }
}

impl<W: Write + ?Sized> Sink for WriteSink<'_, W> {
    #[inline]
    fn copy(&mut self, src: &[u8]) -> Result<(), Error> {
        if self.buf.len() + src.len() > OUTPUT_BUF_LEN {
            self.flush_buf()?;
        }
        if src.len() >= OUTPUT_BUF_LEN {
            return Self::emit(self.out, &mut self.hasher, src);
        }
        self.buf.extend_from_slice(src);
        Ok(())
    }

    #[inline]
    fn add(&mut self, mut delta: &[u8], mut old: &[u8]) -> Result<(), Error> {
        while !delta.is_empty() {
            if self.buf.len() == OUTPUT_BUF_LEN {
                self.flush_buf()?;
            }
            let n = (OUTPUT_BUF_LEN - self.buf.len()).min(delta.len());
            self.buf.extend(
                delta[..n]
                    .iter()
                    .zip(&old[..n])
                    .map(|(d, o)| d.wrapping_add(*o)),
            );
            delta = &delta[n..];
            old = &old[n..];
        }
        Ok(())
    }
}

/// A window over a chunk's instruction stream, decompressed on demand so
/// memory stays bounded whatever lengths the patch declares.
pub(crate) struct Instructions<'c, 'd> {
    chunk: usize,
    dec: &'d mut DCtx<'static>,
    input: &'c [u8],
    buf: &'d mut [u8],
    start: usize,
    end: usize,
    /// Stream offset of `buf[start]`.
    pos: u64,
    produced: u64,
    raw_len: u64,
    frame_done: bool,
}

impl<'c, 'd> Instructions<'c, 'd> {
    pub(crate) fn new(chunk: &ChunkRef<'c>, dec: &'d mut DCtx<'static>, buf: &'d mut [u8]) -> Self {
        Self {
            chunk: chunk.index,
            dec,
            input: chunk.data,
            buf,
            start: 0,
            end: 0,
            pos: 0,
            produced: 0,
            raw_len: chunk.raw_len,
            frame_done: false,
        }
    }

    #[inline]
    fn available(&self) -> usize {
        self.end - self.start
    }

    fn compression(&self, source: io::Error) -> Error {
        Malformed::Compression {
            chunk: self.chunk,
            source,
        }
        .into()
    }

    fn truncated(&self) -> Error {
        Malformed::Instruction {
            chunk: self.chunk,
            offset: self.pos,
        }
        .into()
    }

    /// Decompress until `want` bytes are buffered or the frame ends.
    #[inline]
    fn fill(&mut self, want: usize) -> Result<(), Error> {
        if self.available() >= want || self.frame_done {
            return Ok(());
        }
        self.refill(want)
    }

    #[cold]
    fn refill(&mut self, want: usize) -> Result<(), Error> {
        self.buf.copy_within(self.start..self.end, 0);
        self.end -= self.start;
        self.start = 0;
        while self.end < want && !self.frame_done {
            let mut input = InBuffer::around(self.input);
            let mut output = OutBuffer::around(&mut self.buf[self.end..]);
            let status = self.dec.decompress_stream(&mut output, &mut input);
            let (read, written) = (input.pos(), output.pos());
            let remaining = status.map_err(|c| self.compression(zstd_error(c)))?;
            self.input = &self.input[read..];
            self.end += written;
            self.produced += written as u64;
            if self.produced > self.raw_len {
                return Err(Malformed::InstructionLength {
                    chunk: self.chunk,
                    expected: self.raw_len,
                    actual: self.produced,
                }
                .into());
            }
            if remaining == 0 {
                self.frame_done = true;
            } else if read == 0 && written == 0 {
                return Err(self.compression(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "zstd frame is truncated",
                )));
            }
        }
        Ok(())
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.start += n;
        self.pos += n as u64;
    }

    fn at_end(&mut self) -> Result<bool, Error> {
        self.fill(1)?;
        Ok(self.available() == 0)
    }

    #[inline]
    fn varint(&mut self) -> Result<u64, Error> {
        self.fill(MAX_VARINT_LEN)?;
        let bytes = &self.buf[self.start..self.end];
        let mut value = 0u64;
        for (i, &b) in bytes.iter().take(MAX_VARINT_LEN).enumerate() {
            // The tenth byte holds bit 63 alone; anything more overflows.
            if i == MAX_VARINT_LEN - 1 && b > 1 {
                break;
            }
            value |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                self.advance(i + 1);
                return Ok(value);
            }
        }
        Err(self.truncated())
    }

    /// Feed the next `n` stream bytes to `f`, in pieces.
    #[inline]
    pub(crate) fn take(
        &mut self,
        mut n: u64,
        mut f: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        while n > 0 {
            self.fill(1)?;
            if self.available() == 0 {
                return Err(self.truncated());
            }
            let piece = (self.available() as u64).min(n) as usize;
            f(&self.buf[self.start..self.start + piece])?;
            self.advance(piece);
            n -= piece as u64;
        }
        Ok(())
    }
}

#[inline]
fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Run one chunk's instructions against `old`, sending output to `sink`.
///
/// Every read of `old` and every output byte is checked against the
/// lengths actually present, so a wrong base or hostile patch yields an error.
pub(crate) fn run_chunk<S: Sink>(
    chunk: &ChunkRef<'_>,
    old: &[u8],
    dec: &mut DCtx<'static>,
    buf: &mut [u8],
    sink: &mut S,
) -> Result<(), Error> {
    let index = chunk.index;
    // zstd would refuse this too, but checking first gives a precise error.
    if let Some(window) = frame::window_size(chunk.data)
        && window > 1 << MAX_WINDOW_LOG
    {
        return Err(Malformed::WindowTooLarge {
            chunk: index,
            window,
        }
        .into());
    }
    dec.reset(ResetDirective::SessionOnly)
        .map_err(|c| Malformed::Compression {
            chunk: index,
            source: zstd_error(c),
        })?;
    let mut r = Instructions::new(chunk, dec, buf);

    let old_len = old.len() as u64;
    let new_len = chunk.new_len;
    let base_range = |offset| Malformed::BaseRange {
        chunk: index,
        offset,
    };
    let old_range = |from: u64, len: u64, at: u64| {
        from.checked_add(len)
            .filter(|&end| end <= old_len)
            .map(|end| from as usize..end as usize)
            .ok_or(base_range(at))
    };
    let reserve = |out_pos: &mut u64, len: u64, at: u64| match out_pos
        .checked_add(len)
        .filter(|&end| end <= new_len)
    {
        Some(end) => {
            *out_pos = end;
            Ok(())
        }
        None => Err(Malformed::Overrun {
            chunk: index,
            offset: at,
        }),
    };

    let mut old_pos = chunk.old_start;
    if old_pos > old_len {
        return Err(base_range(0).into());
    }
    let mut out_pos = 0u64;

    while !r.at_end()? {
        let at = r.pos;

        let add_tag = r.varint()?;
        let add_len = add_tag >> 1;
        if add_len > 0 {
            let src = old_range(old_pos, add_len, at)?;
            reserve(&mut out_pos, add_len, at)?;
            old_pos += add_len;
            let base = &old[src];
            if add_tag & 1 == 1 {
                sink.copy(base)?;
            } else {
                let mut done = 0;
                r.take(add_len, |delta| {
                    sink.add(delta, &base[done..done + delta.len()])?;
                    done += delta.len();
                    Ok(())
                })?;
            }
        }

        let copy_tag = r.varint()?;
        let copy_len = copy_tag >> 1;
        if copy_tag & 1 == 1 {
            // The encoder writes the source offset even for empty copies.
            let from = r.varint()?;
            if copy_len > 0 {
                let src = old_range(from, copy_len, at)?;
                reserve(&mut out_pos, copy_len, at)?;
                sink.copy(&old[src])?;
            }
        } else if copy_len > 0 {
            reserve(&mut out_pos, copy_len, at)?;
            r.take(copy_len, |lit| sink.copy(lit))?;
        }

        let seek = zigzag(r.varint()?);
        old_pos = old_pos
            .checked_add_signed(seek)
            .filter(|&p| p <= old_len)
            .ok_or(base_range(at))?;
    }

    if out_pos != new_len {
        return Err(Malformed::Underrun {
            chunk: index,
            expected: new_len,
            actual: out_pos,
        }
        .into());
    }
    if r.produced != r.raw_len {
        return Err(Malformed::InstructionLength {
            chunk: index,
            expected: r.raw_len,
            actual: r.produced,
        }
        .into());
    }
    if !r.input.is_empty() {
        return Err(r.compression(io::Error::new(
            ErrorKind::InvalidData,
            "data follows the zstd frame",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use integer_encoding::VarInt;

    fn decode_one(bytes: &[u8]) -> Result<u64, Error> {
        let compressed = zstd::bulk::compress(bytes, 3).unwrap();
        let mut dec = decoder().unwrap();
        let mut buf = vec![0; INSTRUCTION_BUF_LEN];
        let mut r = Instructions {
            chunk: 0,
            dec: &mut dec,
            input: &compressed,
            buf: &mut buf,
            start: 0,
            end: 0,
            pos: 0,
            produced: 0,
            raw_len: bytes.len() as u64,
            frame_done: false,
        };
        r.varint()
    }

    /// Stream `frame` through `dec`, returning whether it decoded.
    fn stream(dec: &mut DCtx<'static>, frame: &[u8]) -> bool {
        let mut input = InBuffer::around(frame);
        let mut buf = vec![0; 1 << 16];
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            match dec.decompress_stream(&mut output, &mut input) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn decoder_memory() {
        let largest = frame::raw_frame(MAX_WINDOW_LOG, 0, b"payload");
        let mut dec = decoder().unwrap();
        assert!(stream(&mut dec, &largest));
        let used = dec.sizeof();
        assert!(used >= 1 << MAX_WINDOW_LOG, "window not allocated: {used}");
        assert!(
            used + INSTRUCTION_BUF_LEN <= (1 << MAX_WINDOW_LOG) + DECODER_OVERHEAD,
            "decoder holds {used} bytes"
        );

        let hostile = frame::raw_frame(27, 0, b"payload");
        let mut dec = decoder().unwrap();
        let before = dec.sizeof();
        assert!(!stream(&mut dec, &hostile));
        assert!(
            dec.sizeof() <= before + (64 << 10),
            "refusal allocated {}",
            dec.sizeof()
        );

        // zstd's default limit takes the same frame and allocates its window.
        let mut permissive = DCtx::create();
        permissive
            .set_parameter(DParameter::WindowLogMax(27))
            .unwrap();
        assert!(stream(&mut permissive, &hostile));
        assert!(permissive.sizeof() > 1 << 27);
    }

    #[test]
    fn varint_matches_integer_encoding() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            u32::MAX as u64,
            u64::MAX >> 1,
            u64::MAX,
        ] {
            assert_eq!(decode_one(&v.encode_var_vec()).unwrap(), v);
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            let enc = v.encode_var_vec();
            assert_eq!(zigzag(decode_one(&enc).unwrap()), v);
        }
    }

    #[test]
    fn varint_rejects_overflow_and_truncation() {
        // Tenth byte above 1 sets bits past 63.
        let mut overflow = vec![0xff; 9];
        overflow.push(0x02);
        assert!(decode_one(&overflow).is_err());
        assert!(decode_one(&[0xff; 11]).is_err());
        assert!(decode_one(&[0x80, 0x80]).is_err());
        assert!(decode_one(&[]).is_err());
    }
}
