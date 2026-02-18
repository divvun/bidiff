use super::{MAGIC, VERSION};
use integer_encoding::VarInt;
use std::{
    error::Error as StdError,
    fmt,
    io::{self, ErrorKind},
};

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

    while let Some(add_tag) = read_varint_slice::<usize>(&ctrl, &mut pos) {
        let add_len = add_tag >> 1;
        if add_len > 0 {
            if add_tag & 1 == 0 {
                // Normal ADD: fused delta + wrapping_add
                let delta = &ctrl[pos..pos + add_len];
                let old_slice = &old[old_pos..old_pos + add_len];
                let out_slice = &mut output[out_pos..out_pos + add_len];
                for i in 0..add_len {
                    out_slice[i] = delta[i].wrapping_add(old_slice[i]);
                }
                pos += add_len;
            } else {
                // ZERO-COPY: memcpy from old, no delta bytes in stream
                output[out_pos..out_pos + add_len]
                    .copy_from_slice(&old[old_pos..old_pos + add_len]);
            }
            old_pos += add_len;
            out_pos += add_len;
        }

        // Read copy_tag (LSB = 0: literal, LSB = 1: copy-from-old)
        let copy_tag: usize = read_varint_slice(&ctrl, &mut pos)
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "truncated copy_tag"))?;
        let copy_len = copy_tag >> 1;

        if copy_len > 0 {
            if copy_tag & 1 == 0 {
                // Literal COPY: bytes from control stream
                output[out_pos..out_pos + copy_len].copy_from_slice(&ctrl[pos..pos + copy_len]);
                pos += copy_len;
            } else {
                // COPY_OLD: bytes from old file at specified position
                let old_copy_pos: usize = read_varint_slice(&ctrl, &mut pos).ok_or_else(|| {
                    io::Error::new(ErrorKind::UnexpectedEof, "truncated copy_old pos")
                })?;
                output[out_pos..out_pos + copy_len]
                    .copy_from_slice(&old[old_copy_pos..old_copy_pos + copy_len]);
            }
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
