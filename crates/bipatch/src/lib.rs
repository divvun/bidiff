use integer_encoding::VarInt;
use integer_encoding::VarIntReader;
use std::{
    cmp::min,
    error::Error as StdError,
    fmt,
    io::{self, ErrorKind, Read, Seek, SeekFrom},
};

pub const MAGIC: u32 = 0xB1DF;
pub const VERSION: u32 = 0x2000;

#[derive(Debug)]
pub enum DecodeError {
    IO(io::Error),
    WrongMagic(u32),
    WrongVersion(u32),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DecodeError::IO(_) => write!(f, "I/O error"),
            DecodeError::WrongMagic(e) => {
                write!(f, "wrong magic: expected `{:X}`, got `{:X}`", MAGIC, e)
            }
            DecodeError::WrongVersion(e) => {
                write!(f, "wrong version: expected `{:X}`, got `{:X}`", VERSION, e)
            }
        }
    }
}

impl StdError for DecodeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DecodeError::IO(e) => Some(e),
            DecodeError::WrongMagic { .. } => None,
            DecodeError::WrongVersion { .. } => None,
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(source: io::Error) -> Self {
        DecodeError::IO(source)
    }
}

fn read_u32_le(r: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64_le(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

// --- Chunked patch format (zero-copy) ---

pub struct PatchRef<'a> {
    pub new_size: u64,
    pub chunks: Vec<ChunkRef<'a>>,
}

pub struct ChunkRef<'a> {
    pub old_start: u64,
    pub new_start: u64,
    pub new_len: u64,
    pub raw_len: u64,
    pub data: &'a [u8],
}

fn get_u32_le(data: &[u8], off: &mut usize) -> Result<u32, DecodeError> {
    let end = *off + 4;
    if end > data.len() {
        return Err(DecodeError::IO(io::Error::new(
            ErrorKind::UnexpectedEof,
            "truncated patch",
        )));
    }
    let v = u32::from_le_bytes(data[*off..end].try_into().unwrap());
    *off = end;
    Ok(v)
}

fn get_u64_le(data: &[u8], off: &mut usize) -> Result<u64, DecodeError> {
    let end = *off + 8;
    if end > data.len() {
        return Err(DecodeError::IO(io::Error::new(
            ErrorKind::UnexpectedEof,
            "truncated patch",
        )));
    }
    let v = u64::from_le_bytes(data[*off..end].try_into().unwrap());
    *off = end;
    Ok(v)
}

/// Read a chunked patch from a byte slice, zero-copy: chunk data is borrowed from the input.
pub fn read_patch(data: &[u8]) -> Result<PatchRef<'_>, DecodeError> {
    let mut off = 0;
    let magic = get_u32_le(data, &mut off)?;
    if magic != MAGIC {
        return Err(DecodeError::WrongMagic(magic));
    }
    let version = get_u32_le(data, &mut off)?;
    if version != VERSION {
        return Err(DecodeError::WrongVersion(version));
    }
    let new_size = get_u64_le(data, &mut off)?;
    let num_chunks = get_u32_le(data, &mut off)? as usize;

    let mut chunks = Vec::with_capacity(num_chunks);
    for _ in 0..num_chunks {
        let old_start = get_u64_le(data, &mut off)?;
        let new_start = get_u64_le(data, &mut off)?;
        let new_len = get_u64_le(data, &mut off)?;
        let raw_len = get_u64_le(data, &mut off)?;
        let data_len = get_u64_le(data, &mut off)? as usize;
        let end = off + data_len;
        if end > data.len() {
            return Err(DecodeError::IO(io::Error::new(
                ErrorKind::UnexpectedEof,
                "truncated chunk data",
            )));
        }
        chunks.push(ChunkRef {
            old_start,
            new_start,
            new_len,
            raw_len,
            data: &data[off..end],
        });
        off = end;
    }

    Ok(PatchRef { new_size, chunks })
}

/// Decode a varint from a byte slice at the given offset.
/// Returns `None` at EOF (pos == data.len()), or advances pos and returns the value.
#[inline]
fn read_varint_slice<V: VarInt>(data: &[u8], pos: &mut usize) -> Option<V> {
    if *pos >= data.len() {
        return None;
    }
    let (val, n) = V::decode_var(&data[*pos..])?;
    *pos += n;
    Some(val)
}

/// Apply a single chunk's compressed Control stream to produce output bytes.
///
/// `old` is the full old file (mmap'd). The chunk's Controls read from `old` starting
/// at `chunk.old_start`, and write sequentially into `output` (which should be
/// `chunk.new_len` bytes long).
///
/// The chunk's `data` is zstd-compressed; this function decompresses it first using
/// `chunk.raw_len` as the expected uncompressed size.
pub fn apply_chunk(chunk: &ChunkRef<'_>, old: &[u8], output: &mut [u8]) -> io::Result<()> {
    // Decompress the chunk's control stream
    let ctrl = zstd::bulk::decompress(chunk.data, chunk.raw_len as usize)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;

    let mut pos: usize = 0;
    let mut old_pos = chunk.old_start as usize;
    let mut out_pos: usize = 0;

    while let Some(add_len) = read_varint_slice::<usize>(&ctrl, &mut pos) {
        // ADD: fused read + wrapping_add in one pass (single cache pass over output)
        if add_len > 0 {
            let delta = &ctrl[pos..pos + add_len];
            let old_slice = &old[old_pos..old_pos + add_len];
            let out_slice = &mut output[out_pos..out_pos + add_len];
            for i in 0..add_len {
                out_slice[i] = delta[i].wrapping_add(old_slice[i]);
            }
            pos += add_len;
            old_pos += add_len;
            out_pos += add_len;
        }

        // Read copy_len
        let copy_len: usize = read_varint_slice(&ctrl, &mut pos)
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "truncated copy_len"))?;

        // COPY: memcpy from control stream into output
        if copy_len > 0 {
            output[out_pos..out_pos + copy_len].copy_from_slice(&ctrl[pos..pos + copy_len]);
            pos += copy_len;
            out_pos += copy_len;
        }

        // SEEK: adjust old position
        let seek: i64 = read_varint_slice(&ctrl, &mut pos)
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "truncated seek"))?;
        old_pos = (old_pos as i64 + seek) as usize;
    }

    debug_assert_eq!(out_pos, chunk.new_len as usize);
    Ok(())
}

// --- Streaming reader (legacy) ---

pub struct Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    patch: R,
    old: RS,
    new_size: u64,
    state: ReaderState,
    buf: Vec<u8>,
}

#[derive(Debug)]
enum ReaderState {
    Initial,
    Add(usize),
    Copy(usize),
    Final,
}

impl<R, RS> Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    pub fn new(mut patch: R, old: RS) -> Result<Self, DecodeError> {
        let magic = read_u32_le(&mut patch)?;
        if magic != MAGIC {
            return Err(DecodeError::WrongMagic(magic));
        }

        let version = read_u32_le(&mut patch)?;
        if version != VERSION {
            return Err(DecodeError::WrongVersion(version));
        }

        let new_size = read_u64_le(&mut patch)?;

        Ok(Self {
            patch,
            old,
            new_size,
            state: ReaderState::Initial,
            buf: vec![0u8; 4096],
        })
    }

    /// Returns the size of the new (patched) file, as stored in the patch header.
    pub fn new_size(&self) -> u64 {
        self.new_size
    }
}

impl<R, RS> Read for Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut read: usize = 0;

        while !buf.is_empty() {
            let processed = match self.state {
                ReaderState::Initial => match self.patch.read_varint() {
                    Ok(add_len) => {
                        self.state = ReaderState::Add(add_len);
                        0
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::UnexpectedEof => {
                            self.state = ReaderState::Final;
                            0
                        }
                        _ => {
                            return Err(e);
                        }
                    },
                },
                ReaderState::Add(add_len) => {
                    let n = min(min(add_len, buf.len()), self.buf.len());

                    let out = &mut buf[..n];
                    self.old.read_exact(out)?;

                    let dif = &mut self.buf[..n];
                    self.patch.read_exact(dif)?;

                    for i in 0..n {
                        out[i] = out[i].wrapping_add(dif[i]);
                    }

                    if add_len == n {
                        let copy_len: usize = self.patch.read_varint()?;
                        self.state = ReaderState::Copy(copy_len)
                    } else {
                        self.state = ReaderState::Add(add_len - n);
                    }

                    n
                }
                ReaderState::Copy(copy_len) => {
                    let n = min(copy_len, buf.len());

                    let out = &mut buf[..n];
                    self.patch.read_exact(out)?;

                    if copy_len == n {
                        let seek: i64 = self.patch.read_varint()?;
                        self.old.seek(SeekFrom::Current(seek))?;
                        self.state = ReaderState::Initial;
                    } else {
                        self.state = ReaderState::Copy(copy_len - n);
                    }

                    n
                }
                ReaderState::Final => {
                    break;
                }
            };
            read += processed;
            buf = &mut buf[processed..];
        }

        Ok(read)
    }
}
