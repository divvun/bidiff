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
//! A frame may not ask for a window larger than 2^[`MAX_WINDOW_LOG`] bytes;
//! the encoder caps its window to fit and the decoder refuses larger ones.
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
mod frame;
mod hash;

pub use blake3::Hash;
pub use error::{Error, Malformed};

const CHUNK_META_LEN: usize = 40;

/// Largest zstd window, as a power of two, a chunk's frame may ask for.
/// It bounds what one decoder allocates, whatever the patch says.
///
/// 8 MiB is the largest window zstd's levels 1 to 19 use, so only levels 20
/// to 22 on chunks over 8 MiB are capped.
pub const MAX_WINDOW_LOG: u32 = 23;

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

    /// Refuse the patch unless it declares a result of `new_size` bytes with
    /// BLAKE3 `new_hash`, such as a size and hash attested elsewhere.
    ///
    /// This compares header fields only, so a patch for some other result is
    /// turned away before its base is hashed or any chunk is decompressed.
    pub fn expect(self, new_size: u64, new_hash: Hash) -> Result<Self, Error> {
        if self.new_size == new_size && self.new_hash == new_hash {
            Ok(self)
        } else {
            Err(Error::UnexpectedResult {
                expected_size: new_size,
                declared_size: self.new_size,
                expected_hash: new_hash,
                declared_hash: self.new_hash,
            })
        }
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
    /// Results over 4 MiB are hashed on up to four helper threads while they
    /// are applied. Memory use does not depend on `new_size`: one decoder of
    /// at most 2^[`MAX_WINDOW_LOG`] bytes plus 1 MiB, up to ten 1 MiB output
    /// blocks, and 32 bytes per MiB written for the hash tree. On any error
    /// `out` may already hold part or all of the result, so it must be
    /// discarded.
    pub fn apply<W: Write + ?Sized>(&self, out: &mut W) -> Result<(), Error> {
        let mut dec = decode::decoder()?;
        let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
        let actual = hash::with_block_hasher(self.patch.new_size, |hasher| {
            let mut sink = decode::WriteSink::new(out, hasher)?;
            for chunk in &self.patch.chunks {
                decode::run_chunk(chunk, self.old, &mut dec, &mut buf, &mut sink)?;
            }
            sink.finish()
        })?;
        self.patch.check_result(actual)
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
        decode_in_parallel(regions, old)?;
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

/// Decoder memory the parallel path may hold at once. Every worker's decoder
/// can grow to the largest window the patch declares plus
/// [`decode::DECODER_OVERHEAD`], so this, not the thread count, bounds the
/// worst case.
#[cfg(feature = "rayon")]
const PARALLEL_DECODE_BUDGET: usize = 128 << 20;

/// Apply each chunk into its region with a fixed set of workers pulling from
/// one queue, so decoders are allocated once per worker and their number is
/// bounded by [`PARALLEL_DECODE_BUDGET`].
#[cfg(feature = "rayon")]
fn decode_in_parallel(regions: Vec<(&ChunkRef<'_>, &mut [u8])>, old: &[u8]) -> Result<(), Error> {
    use rayon::prelude::*;
    use std::sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    };

    // Frames asking for more than MAX_WINDOW_LOG are refused before decoding.
    let window = regions
        .iter()
        .filter_map(|(chunk, _)| frame::window_size(chunk.data))
        .max()
        .unwrap_or(0)
        .min(1 << MAX_WINDOW_LOG) as usize;
    let workers = rayon::current_num_threads()
        .min(PARALLEL_DECODE_BUDGET / (window + decode::DECODER_OVERHEAD))
        .min(regions.len())
        .max(1);
    let queue = Mutex::new(regions.into_iter());
    let failed = AtomicBool::new(false);
    (0..workers).into_par_iter().try_for_each(|_| {
        let mut dec = decode::decoder()?;
        let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
        while !failed.load(Ordering::Relaxed) {
            let next = queue.lock().unwrap_or_else(PoisonError::into_inner).next();
            let Some((chunk, region)) = next else {
                break;
            };
            let mut sink = decode::SliceSink::new(region);
            if let Err(e) = decode::run_chunk(chunk, old, &mut dec, &mut buf, &mut sink) {
                failed.store(true, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(())
    })
}

#[cfg(feature = "mmap")]
impl VerifiedBase<'_, '_> {
    /// Apply the patch into `file`, which this sets to exactly
    /// [`new_size`](PatchRef::new_size) bytes and memory-maps, then check the
    /// result's hash and return it.
    ///
    /// `file` must be open for both reading and writing, as a writable shared
    /// mapping needs; [`File::create`](std::fs::File::create) opens for
    /// writing only. Chunks are applied in parallel with the `rayon` feature,
    /// as in [`apply_to_slice`](Self::apply_to_slice).
    ///
    /// The caller owns the file and its cleanup. On any error it may hold part
    /// of the result or wrong bytes and should be deleted; a
    /// `tempfile::NamedTempFile` does that when dropped. The bytes are in the
    /// page cache when this returns; call [`File::sync_all`](std::fs::File::sync_all)
    /// for durability. Not starting writeback here saves 40 to 70 ms per
    /// 200 MB on macOS, where `msync` is costly even when asynchronous.
    ///
    /// Nothing else may write to or truncate the file during the call: that
    /// would change memory this function is reading and writing.
    pub fn apply_to_file(&self, file: &std::fs::File) -> Result<Hash, Error> {
        let new_size = self.patch.new_size;
        if usize::try_from(new_size).is_err() {
            return Err(Error::Io(io::Error::new(
                ErrorKind::InvalidInput,
                format!("a {new_size}-byte result cannot be mapped on this platform"),
            )));
        }
        file.set_len(new_size).map_err(Error::Io)?;
        if new_size == 0 {
            // memmap2 fakes empty maps with a one-byte map, which an empty
            // file cannot back.
            self.apply_to_slice(&mut [])?;
            return Ok(self.patch.new_hash);
        }
        // SAFETY: the mapping lives only for this call, and the documented
        // contract is that nothing else modifies or truncates the file
        // meanwhile; its length was just set to new_size.
        let mut map = unsafe { memmap2::MmapMut::map_mut(file) }.map_err(Error::Io)?;
        self.apply_to_slice(&mut map)?;
        Ok(self.patch.new_hash)
    }
}

impl PatchRef<'_> {
    /// [`verify_base`](Self::verify_base), then
    /// [`VerifiedBase::apply_to_file`]. A wrong base leaves `file` untouched.
    #[cfg(feature = "mmap")]
    pub fn apply_to_file(&self, old: &[u8], file: &std::fs::File) -> Result<Hash, Error> {
        self.verify_base(old)?.apply_to_file(file)
    }

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

/// Compress a chunk's instruction stream into one frame whose window fits
/// [`MAX_WINDOW_LOG`].
///
/// zstd already shrinks the window to fit small inputs, so only levels whose
/// default window exceeds the cap are recompressed; capping never raises it.
#[cfg(feature = "diff")]
pub(crate) fn compress_chunk(raw: &[u8], level: i32) -> io::Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::new(level)?;
    let compressed = compressor.compress(raw)?;
    if frame::window_size(&compressed).is_some_and(|w| w > 1 << MAX_WINDOW_LOG) {
        compressor.set_parameter(zstd::zstd_safe::CParameter::WindowLog(MAX_WINDOW_LOG))?;
        return compressor.compress(raw);
    }
    Ok(compressed)
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

#[cfg(all(test, feature = "diff"))]
mod tests {
    use super::*;

    #[test]
    fn compress_chunk_caps_the_window() {
        // Sparse bytes over zeros, longer than the cap, so level 22 would
        // otherwise pick a larger window.
        let mut raw = vec![0u8; (1 << MAX_WINDOW_LOG) + (1 << 20)];
        let mut x = 1u32;
        for b in raw.iter_mut().step_by(61) {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (x >> 24) as u8;
        }
        let uncapped = zstd::bulk::compress(&raw, 22).unwrap();
        assert!(frame::window_size(&uncapped).unwrap() > 1 << MAX_WINDOW_LOG);

        let capped = compress_chunk(&raw, 22).unwrap();
        assert_eq!(frame::window_size(&capped), Some(1 << MAX_WINDOW_LOG));
        let chunk = ChunkRef {
            index: 0,
            old_start: 0,
            new_start: 0,
            new_len: raw.len() as u64,
            raw_len: raw.len() as u64,
            data: &capped,
        };
        let mut dec = decode::decoder().unwrap();
        let mut buf = vec![0; decode::INSTRUCTION_BUF_LEN];
        let mut r = decode::Instructions::new(&chunk, &mut dec, &mut buf);
        let mut out = Vec::new();
        r.take(raw.len() as u64, |piece| {
            out.extend_from_slice(piece);
            Ok(())
        })
        .unwrap();
        assert!(out == raw);

        // Below the cap, a level's own choice stands.
        let small = &raw[..1 << 20];
        assert_eq!(
            compress_chunk(small, 3).unwrap(),
            zstd::bulk::compress(small, 3).unwrap()
        );
    }
}
