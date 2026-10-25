//! Reading and applying patches.
//!
//! # Format
//!
//! Integers are little-endian. A patch is a header followed by `num_chunks`
//! chunks:
//!
//! | Offset | Size | Field                                   |
//! |--------|------|-----------------------------------------|
//! | 0      | 4    | magic, [`MAGIC`]                        |
//! | 4      | 4    | format version, [`VERSION`]             |
//! | 8      | 8    | `old_size`: length of the base          |
//! | 16     | 32   | `old_hash`: BLAKE3 of the base          |
//! | 48     | 8    | `new_size`: length of the result        |
//! | 56     | 32   | `new_hash`: BLAKE3 of the result        |
//! | 88     | 4    | `num_chunks`                            |
//!
//! The hashes are plain BLAKE3, as printed by `b3sum`.
//!
//! Each chunk is 40 bytes of metadata followed by its data:
//!
//! | Size       | Field                                                  |
//! |------------|--------------------------------------------------------|
//! | 8          | `old_start`: base offset the first instruction reads   |
//! | 8          | `new_start`: result offset of the chunk's first byte   |
//! | 8          | `new_len`: bytes the chunk produces                    |
//! | 8          | `raw_len`: decompressed length of the instructions     |
//! | 8          | `data_len`                                             |
//! | `data_len` | one zstd frame holding the instruction stream          |
//!
//! Chunks are stored in output order and cover `0..new_size` exactly once,
//! so they can be applied in parallel to disjoint parts of the output.
//!
//! An instruction stream is a sequence of controls. Each control is:
//!
//! - `add_tag`, a varint. `add_tag >> 1` bytes are taken from the base at the
//!   base cursor, which then advances by that much. If the low bit is 0, as
//!   many delta bytes follow and each is added (wrapping) to its base byte; if
//!   1, the base bytes are output unchanged.
//! - `copy_tag`, a varint. If the low bit is 0, `copy_tag >> 1` literal bytes
//!   follow. If 1, a varint base offset follows and `copy_tag >> 1` bytes are
//!   copied from there; the base cursor does not move.
//! - `seek`, a zigzag varint added to the base cursor.
//!
//! Varints are unsigned LEB128 of at most 10 bytes.

use super::{MAGIC, VERSION};
use std::io::{self, ErrorKind, Write};

mod decode;
mod error;

pub use blake3::Hash;
pub use error::{Error, Malformed};

const CHUNK_META_LEN: usize = 40;

/// A parsed patch whose chunks are known to tile the output.
///
/// Chunk data is borrowed from the patch bytes.
pub struct PatchRef<'a> {
    old_size: u64,
    old_hash: Hash,
    new_size: u64,
    new_hash: Hash,
    chunks: Vec<ChunkRef<'a>>,
}

/// One independently compressed piece of a patch.
pub struct ChunkRef<'a> {
    index: usize,
    old_start: u64,
    new_start: u64,
    new_len: u64,
    raw_len: u64,
    data: &'a [u8],
}

impl ChunkRef<'_> {
    /// Position of this chunk in the patch.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Base offset the chunk's first instruction reads from.
    pub fn old_start(&self) -> u64 {
        self.old_start
    }

    /// Offset in the result where this chunk's output starts.
    pub fn new_start(&self) -> u64 {
        self.new_start
    }

    /// Bytes this chunk produces.
    pub fn new_len(&self) -> u64 {
        self.new_len
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.data.len() - self.off
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if n > self.remaining() {
            return Err(Malformed::Truncated {
                offset: self.data.len(),
            });
        }
        let s = &self.data[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let mut a = [0; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        self.array().map(u64::from_le_bytes)
    }
}

/// Parse and validate a patch's header and chunk table.
///
/// Nothing is allocated in proportion to sizes the patch declares, only to
/// the length of `data`.
pub fn read_patch(data: &[u8]) -> Result<PatchRef<'_>, Error> {
    let mut c = Cursor { data, off: 0 };
    let magic = c.u32()?;
    if magic != MAGIC {
        return Err(Error::WrongMagic(magic));
    }
    let version = c.u32()?;
    if version != VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let old_size = c.u64()?;
    let old_hash = Hash::from_bytes(c.array()?);
    let new_size = c.u64()?;
    let new_hash = Hash::from_bytes(c.array()?);
    let num_chunks = c.u32()?;
    if num_chunks as usize > c.remaining() / CHUNK_META_LEN {
        return Err(Malformed::ChunkCount {
            declared: num_chunks,
        }
        .into());
    }

    let mut chunks = Vec::with_capacity(num_chunks as usize);
    let mut covered = 0u64;
    for index in 0..num_chunks as usize {
        let old_start = c.u64()?;
        let new_start = c.u64()?;
        let new_len = c.u64()?;
        let raw_len = c.u64()?;
        let data_len = c.u64()?;
        if new_start != covered {
            return Err(Malformed::ChunkLayout { chunk: index }.into());
        }
        covered = new_start
            .checked_add(new_len)
            .filter(|&end| end <= new_size)
            .ok_or(Malformed::ChunkLayout { chunk: index })?;
        let data_len =
            usize::try_from(data_len).map_err(|_| Malformed::Truncated { offset: data.len() })?;
        chunks.push(ChunkRef {
            index,
            old_start,
            new_start,
            new_len,
            raw_len,
            data: c.take(data_len)?,
        });
    }
    if covered != new_size {
        return Err(Malformed::ChunkCoverage { covered, new_size }.into());
    }
    if c.remaining() != 0 {
        return Err(Malformed::TrailingData { offset: c.off }.into());
    }

    Ok(PatchRef {
        old_size,
        old_hash,
        new_size,
        new_hash,
        chunks,
    })
}

impl<'a> PatchRef<'a> {
    /// Length of the base the patch was made from.
    pub fn old_size(&self) -> u64 {
        self.old_size
    }

    /// BLAKE3 hash of the base the patch was made from.
    pub fn old_hash(&self) -> Hash {
        self.old_hash
    }

    /// Length of the result, as declared by the patch.
    pub fn new_size(&self) -> u64 {
        self.new_size
    }

    /// BLAKE3 hash of the result, as declared by the patch. Compare it with
    /// an independent expectation before applying: applying only proves the
    /// result matches this value.
    pub fn new_hash(&self) -> Hash {
        self.new_hash
    }

    pub fn chunks(&self) -> &[ChunkRef<'a>] {
        &self.chunks
    }

    /// Check that `old` is the base this patch was made from: its size first,
    /// then its BLAKE3 hash.
    pub fn verify_base<'s>(&'s self, old: &'s [u8]) -> Result<VerifiedBase<'s, 'a>, Error> {
        let mismatch = |actual_hash| Error::BaseMismatch {
            expected_size: self.old_size,
            actual_size: old.len() as u64,
            expected_hash: self.old_hash,
            actual_hash,
        };
        if old.len() as u64 != self.old_size {
            return Err(mismatch(None));
        }
        let actual = hash(old);
        if actual != self.old_hash {
            return Err(mismatch(Some(actual)));
        }
        Ok(VerifiedBase { patch: self, old })
    }

    /// [`verify_base`](Self::verify_base), then [`VerifiedBase::apply`].
    pub fn apply<W: Write + ?Sized>(&self, old: &[u8], out: &mut W) -> Result<(), Error> {
        self.verify_base(old)?.apply(out)
    }

    /// [`verify_base`](Self::verify_base), then [`VerifiedBase::apply_to_slice`].
    pub fn apply_to_slice(&self, old: &[u8], out: &mut [u8]) -> Result<(), Error> {
        self.verify_base(old)?.apply_to_slice(out)
    }
}

/// A patch and a base whose size and hash match it.
pub struct VerifiedBase<'s, 'a> {
    patch: &'s PatchRef<'a>,
    old: &'s [u8],
}

impl VerifiedBase<'_, '_> {
    /// Apply the patch, streaming the result to `out`, then check the
    /// result's hash.
    ///
    /// Memory use is bounded by fixed buffers and zstd's window limit, not by
    /// `new_size`. On any error `out` may already hold part or all of the
    /// result, so it must be discarded.
    pub fn apply<W: Write + ?Sized>(&self, out: &mut W) -> Result<(), Error> {
        let mut dec = decode::decoder()?;
        let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
        let mut sink = decode::WriteSink::new(out, self.patch.new_size);
        for chunk in &self.patch.chunks {
            decode::run_chunk(chunk, self.old, &mut dec, &mut buf, &mut sink)?;
        }
        self.patch.check_result(sink.finish()?)
    }

    /// Apply the patch into `out`, which must be exactly
    /// [`new_size`](PatchRef::new_size) bytes long, then check the result's
    /// hash.
    ///
    /// With the `rayon` feature, chunks are applied in parallel.
    pub fn apply_to_slice(&self, out: &mut [u8]) -> Result<(), Error> {
        let (patch, old) = (self.patch, self.old);
        if out.len() as u64 != patch.new_size {
            return Err(wrong_output_len(out.len(), patch.new_size));
        }
        let mut regions = Vec::with_capacity(patch.chunks.len());
        let mut rest = &mut *out;
        for chunk in &patch.chunks {
            let split = usize::try_from(chunk.new_len)
                .ok()
                .and_then(|len| rest.split_at_mut_checked(len));
            // read_patch checked that the chunks tile new_size, which is out.len().
            let Some((region, tail)) = split else {
                return Err(Malformed::ChunkLayout { chunk: chunk.index }.into());
            };
            regions.push((chunk, region));
            rest = tail;
        }

        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            regions.into_par_iter().try_for_each_init(
                || None,
                |state, (chunk, region)| {
                    let (dec, buf) = match state {
                        Some(s) => s,
                        None => {
                            state.insert((decode::decoder()?, vec![0; decode::INSTRUCTION_BUF_LEN]))
                        }
                    };
                    decode::run_chunk(chunk, old, dec, buf, &mut decode::SliceSink::new(region))
                },
            )?;
        }
        #[cfg(not(feature = "rayon"))]
        {
            let mut dec = decode::decoder()?;
            let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
            for (chunk, region) in regions {
                let mut sink = decode::SliceSink::new(region);
                decode::run_chunk(chunk, old, &mut dec, &mut buf, &mut sink)?;
            }
        }

        patch.check_result(hash(out))
    }
}

impl PatchRef<'_> {
    fn check_result(&self, actual: Hash) -> Result<(), Error> {
        if actual == self.new_hash {
            Ok(())
        } else {
            Err(Error::ResultMismatch {
                expected: self.new_hash,
                actual,
            })
        }
    }
}

/// `update_rayon` is slower than `update` below about 128 KiB.
#[cfg(feature = "rayon")]
const PARALLEL_HASH_MIN: usize = 128 * 1024;

/// BLAKE3 of `data`, on several cores with the `rayon` feature.
pub(crate) fn hash(data: &[u8]) -> Hash {
    #[cfg(feature = "rayon")]
    if data.len() >= PARALLEL_HASH_MIN {
        return blake3::Hasher::new().update_rayon(data).finalize();
    }
    blake3::hash(data)
}

fn wrong_output_len(actual: usize, expected: u64) -> Error {
    Error::Io(io::Error::new(
        ErrorKind::InvalidInput,
        format!("output is {actual} bytes but the patch produces {expected}"),
    ))
}

/// Apply one chunk to `old`, writing into `out`, which must be exactly
/// [`new_len`](ChunkRef::new_len) bytes long.
///
/// This checks neither the base's hash nor the result's. Callers that apply
/// chunks themselves must call [`PatchRef::verify_base`] first and compare
/// the assembled result with [`PatchRef::new_hash`]; [`VerifiedBase`] does both.
pub fn apply_chunk(chunk: &ChunkRef<'_>, old: &[u8], out: &mut [u8]) -> Result<(), Error> {
    if out.len() as u64 != chunk.new_len {
        return Err(wrong_output_len(out.len(), chunk.new_len));
    }
    let mut dec = decode::decoder()?;
    let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
    decode::run_chunk(
        chunk,
        old,
        &mut dec,
        &mut buf,
        &mut decode::SliceSink::new(out),
    )
}

#[cfg(feature = "diff")]
pub(crate) fn write_header(
    out: &mut dyn Write,
    old: &[u8],
    new: &[u8],
    num_chunks: u32,
) -> io::Result<()> {
    let (old_hash, new_hash) = rayon::join(|| hash(old), || hash(new));
    out.write_all(&MAGIC.to_le_bytes())?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(old.len() as u64).to_le_bytes())?;
    out.write_all(old_hash.as_bytes())?;
    out.write_all(&(new.len() as u64).to_le_bytes())?;
    out.write_all(new_hash.as_bytes())?;
    out.write_all(&num_chunks.to_le_bytes())
}

#[cfg(feature = "diff")]
pub(crate) fn write_chunk(
    out: &mut dyn Write,
    old_start: u64,
    new_start: u64,
    new_len: u64,
    raw_len: u64,
    data: &[u8],
) -> io::Result<()> {
    for v in [old_start, new_start, new_len, raw_len, data.len() as u64] {
        out.write_all(&v.to_le_bytes())?;
    }
    out.write_all(data)
}
