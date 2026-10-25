#![allow(dead_code)]

use bidiff::{
    DiffParams,
    patch::{Error, read_patch},
};
use std::io::{self, Write};

/// Byte offsets of header fields, per the format table in `bidiff::patch`.
pub mod layout {
    pub const VERSION: usize = 4;
    pub const NEW_SIZE: usize = 8;
    pub const NUM_CHUNKS: usize = 16;
    pub const HEADER_LEN: usize = 20;
    pub const CHUNK_META_LEN: usize = 40;

    // Offsets within a chunk's metadata.
    pub const OLD_START: usize = 0;
    pub const NEW_START: usize = 8;
    pub const NEW_LEN: usize = 16;
    pub const RAW_LEN: usize = 24;
    pub const DATA_LEN: usize = 32;
}

pub fn make_patch(old: &[u8], new: &[u8], scan_chunk: Option<usize>, block: usize) -> Vec<u8> {
    let mut params = DiffParams::new(block, scan_chunk).unwrap();
    params.use_ram = true;
    let mut out = Vec::new();
    bidiff::simple_diff_chunked_with_params(old, new, &mut out, &params, 3).unwrap();
    out
}

/// A writer that fails once more than `limit` bytes arrive, so a hostile
/// patch cannot make a test buffer grow without bound.
pub struct Capped {
    pub data: Vec<u8>,
    pub limit: usize,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.data.len() + buf.len() > self.limit {
            return Err(io::Error::other("output cap exceeded"));
        }
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn apply_stream(old: &[u8], patch: &[u8]) -> Result<Vec<u8>, Error> {
    apply_stream_capped(old, patch, usize::MAX)
}

pub fn apply_stream_capped(old: &[u8], patch: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    let p = read_patch(patch)?;
    let mut out = Capped {
        data: Vec::new(),
        limit,
    };
    p.apply(old, &mut out)?;
    Ok(out.data)
}

/// `None` when the declared size is too large for the test to allocate.
pub fn apply_slice(old: &[u8], patch: &[u8], limit: u64) -> Option<Result<Vec<u8>, Error>> {
    let p = match read_patch(patch) {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    if p.new_size() > limit {
        return None;
    }
    let mut out = vec![0; p.new_size() as usize];
    Some(p.apply_to_slice(old, &mut out).map(|()| out))
}

pub fn get_u32(p: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(p[at..at + 4].try_into().unwrap())
}

pub fn get_u64(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}

pub fn put_u32(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u64(p: &mut [u8], at: usize, v: u64) {
    p[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Offsets of each chunk's metadata in a well-formed patch.
pub fn chunk_offsets(p: &[u8]) -> Vec<usize> {
    let n = get_u32(p, layout::NUM_CHUNKS) as usize;
    let mut offsets = Vec::with_capacity(n);
    let mut at = layout::HEADER_LEN;
    for _ in 0..n {
        offsets.push(at);
        at += layout::CHUNK_META_LEN + get_u64(p, at + layout::DATA_LEN) as usize;
    }
    offsets
}

pub fn instructions_of(p: &[u8], chunk_at: usize) -> Vec<u8> {
    let data_len = get_u64(p, chunk_at + layout::DATA_LEN) as usize;
    let start = chunk_at + layout::CHUNK_META_LEN;
    zstd::stream::decode_all(&p[start..start + data_len]).unwrap()
}

/// Rebuild `p` with chunk `chunk_at`'s instruction stream replaced, keeping
/// every other field.
pub fn replace_instructions(p: &[u8], chunk_at: usize, instructions: &[u8]) -> Vec<u8> {
    let data_len = get_u64(p, chunk_at + layout::DATA_LEN) as usize;
    let data_start = chunk_at + layout::CHUNK_META_LEN;
    let compressed = zstd::bulk::compress(instructions, 3).unwrap();
    let mut out = p[..data_start].to_vec();
    put_u64(
        &mut out,
        chunk_at + layout::RAW_LEN,
        instructions.len() as u64,
    );
    put_u64(
        &mut out,
        chunk_at + layout::DATA_LEN,
        compressed.len() as u64,
    );
    out.extend_from_slice(&compressed);
    out.extend_from_slice(&p[data_start + data_len..]);
    out
}

/// Build a one-chunk patch from a raw instruction stream.
pub fn single_chunk_patch(old: &[u8], new: &[u8], old_start: u64, instructions: &[u8]) -> Vec<u8> {
    let _ = old;
    let compressed = zstd::bulk::compress(instructions, 3).unwrap();
    let mut p = Vec::new();
    p.extend_from_slice(&bidiff::MAGIC.to_le_bytes());
    p.extend_from_slice(&bidiff::VERSION.to_le_bytes());
    p.extend_from_slice(&(new.len() as u64).to_le_bytes());
    p.extend_from_slice(&1u32.to_le_bytes());
    for v in [
        old_start,
        0,
        new.len() as u64,
        instructions.len() as u64,
        compressed.len() as u64,
    ] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(&compressed);
    p
}

pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// xorshift64*: deterministic, so a failing case can be replayed from its seed.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// A base and a related newer version: edits on top of random data, so the
/// diff finds both matches and literals.
pub fn related_pair(rng: &mut Rng, len: usize) -> (Vec<u8>, Vec<u8>) {
    let old = rng.bytes(len);
    let mut new = old.clone();
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(new.len() + 1);
        match rng.below(3) {
            0 => {
                let n = rng.below(64);
                let ins = rng.bytes(n);
                new.splice(at..at, ins);
            }
            1 => {
                let end = (at + rng.below(64)).min(new.len());
                new.drain(at..end);
            }
            _ => {
                let end = (at + rng.below(64)).min(new.len());
                for b in &mut new[at..end] {
                    *b = b.wrapping_add(1);
                }
            }
        }
    }
    (old, new)
}
