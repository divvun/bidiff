use crate::threads;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Inlined from sacabase: common_prefix_len, LongestCommonSubstring
// ---------------------------------------------------------------------------

pub struct LongestCommonSubstring {
    pub start: usize,
    pub len: usize,
}

impl fmt::Debug for LongestCommonSubstring {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "T[{}..{}]",
            self.start,
            self.start.saturating_add(self.len)
        )
    }
}

const NO_MATCH: LongestCommonSubstring = LongestCommonSubstring { start: 0, len: 0 };

pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let (a_words, _) = a.as_chunks::<8>();
    let (b_words, _) = b.as_chunks::<8>();
    let mut len = 0;

    // Compare 8 bytes at a time using u64 XOR
    for (x, y) in a_words.iter().zip(b_words) {
        let xor = u64::from_ne_bytes(*x) ^ u64::from_ne_bytes(*y);
        if xor != 0 {
            #[cfg(target_endian = "little")]
            return len + (xor.trailing_zeros() / 8) as usize;
            #[cfg(target_endian = "big")]
            return len + (xor.leading_zeros() / 8) as usize;
        }
        len += 8;
    }

    // Scalar tail for remaining bytes
    let a = a.get(len..).unwrap_or_default();
    let b = b.get(len..).unwrap_or_default();
    len + a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

// ---------------------------------------------------------------------------
// MmapTable: hash table storage in RAM or in a file, via memmap2
// ---------------------------------------------------------------------------

mod mmap_table {
    use memmap2::{MmapMut, MmapOptions};
    use std::sync::atomic::AtomicU64;
    use std::{io, path::Path};

    /// A u64 array in an anonymous mmap, or in a mmap of an unnamed file,
    /// whose pages the kernel can write out under memory pressure. Works
    /// cross-platform (Linux, macOS, Windows).
    pub struct MmapTable {
        mmap: MmapMut,
        len: usize,
    }

    impl MmapTable {
        /// Create a new table of `len` u64 slots, all initialized to zero (EMPTY).
        ///
        /// With no `dir`, uses an anonymous mmap (in RAM, faster).
        /// Otherwise, uses a mmap of an unnamed file in `dir`, which the kernel
        /// can page out under memory pressure, unless `dir` is itself in RAM
        /// (tmpfs).
        pub fn new(len: usize, dir: Option<&Path>) -> io::Result<Self> {
            let byte_len = len.checked_mul(size_of::<u64>()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::OutOfMemory, "hash table too large")
            })?;

            let mmap = match dir {
                None => MmapMut::map_anon(byte_len)?,
                Some(dir) => {
                    let file = tempfile::tempfile_in(dir)?;
                    file.set_len(byte_len as u64)?;
                    // SAFETY: Private tempfile — no other process can modify the mapping.
                    // The file handle is dropped after mmap creation; the mapping persists.
                    unsafe { MmapOptions::new().len(byte_len).map_mut(&file)? }
                }
            };

            // EMPTY = 0, so kernel-zeroed pages are already initialized.
            // No memset needed — saves ~100ms for large tables.
            #[cfg(target_os = "linux")]
            {
                use memmap2::Advice;
                // Request transparent huge pages to reduce TLB pressure.
                let _ = mmap.advise(Advice::HugePage);
                // Random access pattern — disable kernel readahead.
                let _ = mmap.advise(Advice::Random);
            }

            Ok(Self { mmap, len })
        }

        /// The slots, which are only ever read and written atomically, so
        /// threads may insert concurrently.
        #[inline(always)]
        pub fn slots(&self) -> &[AtomicU64] {
            // SAFETY: the mapping is `len` u64s long and page-aligned, so
            // aligned for AtomicU64, which has the size and alignment of u64
            // and accepts any bit pattern. It lives as long as `self`, and
            // all access goes through the atomics, never through `mmap`.
            unsafe { std::slice::from_raw_parts(self.mmap.as_ptr().cast::<AtomicU64>(), self.len) }
        }
    }
}

use mmap_table::MmapTable;

/// A hash-table based string index. Uses a hash over fixed-size blocks
/// of the text to build an O(n/B) sized index, where B is the block size.
///
/// This uses dramatically less memory than a suffix array (roughly n/4 bytes
/// for the index vs 4n bytes for a suffix array), at the cost of slightly
/// worse match quality (larger patches).
///
/// The hash table is backed by an anonymous mmap in RAM, or by a mmap of an
/// unnamed file in a directory the caller names (via tempfile + memmap2), so
/// the kernel can page out entries under memory pressure. On Linux,
/// transparent huge pages are requested to reduce TLB pressure.
///
/// The index stores every block-aligned position in the old text. When queried,
/// it checks if the first `block_size` bytes of the needle match any indexed
/// block, then extends the match forward. This works correctly with
/// BsdiffIterator which calls `longest_substring_match` at each scan position.
pub struct HashIndex<'a> {
    text: &'a [u8],
    block_size: usize,
    /// Cache-line-aligned bucket hash table. Each bucket is 8 packed u64 entries
    /// = 64 bytes = 1 cache line. Hash → bucket index, scan all 8 entries in one
    /// DRAM fetch. Overflow probes to next bucket (rare at ~42% load).
    table: MmapTable,
    mask: usize,
}

/// EMPTY = 0: kernel-zeroed mmap pages are born initialized, no memset needed.
/// Valid entries always have lower 32 bits >= 1 (we store the block index + 1).
const EMPTY: u64 = 0;

/// 8 entries per bucket = 8 × 8 bytes = 64 bytes = 1 cache line.
/// A single DRAM fetch loads the entire bucket's probe sequence.
const BUCKET_SIZE: usize = 8;

/// Buckets a lookup searches before giving up, and so the furthest from its
/// own bucket an entry is worth placing.
const BUCKETS_PROBED: usize = 5;

/// Pack a block index and the upper 32 bits of the hash into a single u64.
/// Stores index+1 so that EMPTY (0) is unambiguous; [`table_layout`]
/// refuses texts of `u32::MAX` blocks or more, so it fits.
#[inline(always)]
fn pack_entry(index: u32, hash: u64) -> u64 {
    (hash & 0xFFFF_FFFF_0000_0000) | (u64::from(index) + 1)
}

/// Extract the block index from a packed entry.
#[inline(always)]
fn entry_index(entry: u64) -> u32 {
    (entry as u32).wrapping_sub(1)
}

/// Extract the 32-bit tag from a packed entry.
#[inline(always)]
fn entry_tag(entry: u64) -> u32 {
    (entry >> 32) as u32
}

#[inline(always)]
fn wymix(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

/// wyhash-style hash for a block of bytes.
/// Fast path for 32-byte blocks (4 u64 reads + 2 wide multiplies),
/// fallback to FNV-1a for smaller blocks.
#[inline(always)]
fn hash_block(data: &[u8]) -> u64 {
    let Some(d) = data.first_chunk::<32>() else {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        return h;
    };
    let a = u64::from_ne_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    let b = u64::from_ne_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
    let c = u64::from_ne_bytes([d[16], d[17], d[18], d[19], d[20], d[21], d[22], d[23]]);
    let d = u64::from_ne_bytes([d[24], d[25], d[26], d[27], d[28], d[29], d[30], d[31]]);
    wymix(a ^ 0xa0761d6478bd642f, b ^ 0xe7037ed1a0b428db)
        ^ wymix(c ^ 0x8ebc6af09c88c6e3, d ^ 0x1d8e4e27c47d124f)
}

#[inline(always)]
fn prefetch(slot: Option<&AtomicU64>) {
    #[cfg(target_arch = "x86_64")]
    if let Some(slot) = slot {
        // SAFETY: Prefetch is a CPU hint that cannot cause UB, and the
        // address is in the table.
        unsafe {
            std::arch::x86_64::_mm_prefetch(
                slot.as_ptr().cast::<i8>().cast_const(),
                std::arch::x86_64::_MM_HINT_T0,
            );
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = slot;
}

/// The slots of the table for `text_len` bytes in `block_size`-byte blocks,
/// and the mask that picks a bucket from a hash.
fn table_layout(text_len: usize, block_size: usize) -> io::Result<(usize, usize)> {
    if block_size < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block size cannot be less than 4",
        ));
    }
    let blocks = text_len / block_size;
    if blocks >= u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the old file has {blocks} blocks, more than the index holds; use a larger block size"
            ),
        ));
    }
    // Bucket mask: num_buckets is a power of 2, mask = num_buckets - 1.
    // 50% load factor: num_entries * 2 total slots, divided into buckets.
    // An empty text still gets one bucket.
    let buckets = (blocks * 2)
        .div_ceil(BUCKET_SIZE)
        .checked_next_power_of_two()
        .filter(|b| b.checked_mul(BUCKET_SIZE * size_of::<u64>()).is_some())
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "hash table too large"))?;
    Ok((buckets * BUCKET_SIZE, buckets - 1))
}

/// Bytes of the hash table [`HashIndex::new`] makes for a text of `text_len`
/// bytes, or the error it would refuse the text or `block_size` with.
pub fn table_bytes(text_len: usize, block_size: usize) -> io::Result<usize> {
    Ok(table_layout(text_len, block_size)?.0 * size_of::<u64>())
}

impl<'a> HashIndex<'a> {
    /// Build a hash index over `text` with the given block size, on up to
    /// `threads` threads.
    ///
    /// The block size controls the granularity of matching. Smaller blocks find
    /// more matches but use more memory. It must be at least 4; 32 bytes is a
    /// good default. The table, [`table_bytes`] long, is a file in `dir`, or
    /// in RAM when `dir` is `None`.
    pub fn new(
        text: &'a [u8],
        block_size: usize,
        dir: Option<&Path>,
        threads: usize,
    ) -> io::Result<Self> {
        let (slots, mask) = table_layout(text.len(), block_size)?;
        let index = Self {
            text,
            block_size,
            table: MmapTable::new(slots, dir)?,
            mask,
        };
        index.populate(threads)?;
        Ok(index)
    }

    /// Insert all block-aligned positions into the hash table, each thread
    /// taking a run of blocks. CAS-based insertion lets them share it.
    fn populate(&self, threads: usize) -> io::Result<()> {
        let blocks = self.text.len() / self.block_size;
        let per_thread = blocks.div_ceil(threads.max(1)).max(1);
        threads::run(blocks.div_ceil(per_thread), |run| {
            let first = run * per_thread;
            let text = self.text.get(first * self.block_size..).unwrap_or_default();
            for (i, block) in text
                .chunks_exact(self.block_size)
                .take(per_thread)
                .enumerate()
            {
                // Fewer than u32::MAX blocks, as table_layout checked.
                self.insert((first + i) as u32, block);
            }
            Ok(())
        })
    }

    /// The slots of bucket `bucket`.
    #[inline(always)]
    fn bucket(&self, bucket: usize) -> &[AtomicU64] {
        let start = bucket * BUCKET_SIZE;
        self.table
            .slots()
            .get(start..start + BUCKET_SIZE)
            .unwrap_or_default()
    }

    /// The text of block `index`.
    #[inline(always)]
    fn block(&self, index: u32) -> Option<&'a [u8]> {
        let start = index as usize * self.block_size;
        self.text.get(start..start + self.block_size)
    }

    fn insert(&self, index: u32, block: &[u8]) {
        let h = hash_block(block);
        let packed = pack_entry(index, h);
        let needle_tag = entry_tag(packed);

        let mut bucket = h as usize & self.mask;
        for _ in 0..BUCKETS_PROBED {
            for slot in self.bucket(bucket) {
                let mut entry = slot.load(Ordering::Relaxed);
                if entry == EMPTY {
                    match slot.compare_exchange(EMPTY, packed, Ordering::Relaxed, Ordering::Relaxed)
                    {
                        Ok(_) => return,
                        // Slot claimed by another thread — check duplicate
                        Err(existing) => entry = existing,
                    }
                }
                if entry_tag(entry) == needle_tag && self.block(entry_index(entry)) == Some(block) {
                    // Keep the earliest of equal blocks.
                    if index < entry_index(entry) {
                        let _ = slot.compare_exchange(
                            entry,
                            packed,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                    return;
                }
                // Not a duplicate — continue scanning bucket
            }
            // Bucket full, overflow to next bucket
            bucket = (bucket + 1) & self.mask;
        }
    }

    /// Look up a block using a pre-computed hash, returning its offset in
    /// the text if found. Uses a 32-bit hash tag to reject non-matching probes
    /// without accessing the text, avoiding expensive cache misses and memcmp
    /// on most probes.
    ///
    /// Bucket hashing: hash → bucket index, scan all 8 entries in the bucket
    /// (1 cache line = 1 DRAM fetch). Entries are packed from the front, so
    /// the first EMPTY slot means the entry isn't in this or any later bucket.
    #[inline(always)]
    fn lookup_with_hash(&self, block: &[u8], h: u64) -> Option<usize> {
        let needle_tag = (h >> 32) as u32;
        let mut bucket = h as usize & self.mask;
        for _ in 0..BUCKETS_PROBED {
            for slot in self.bucket(bucket) {
                let entry = slot.load(Ordering::Relaxed);
                if entry == EMPTY {
                    return None;
                }
                if entry_tag(entry) == needle_tag {
                    let index = entry_index(entry);
                    if self.block(index) == Some(block) {
                        return Some(index as usize * self.block_size);
                    }
                }
            }
            // Bucket full with no match — probe next bucket (rare at 50% load)
            bucket = (bucket + 1) & self.mask;
        }
        None
    }

    /// Prefetch the hash table slot for a block, so a subsequent lookup is faster.
    /// Returns the computed hash so it can be reused by
    /// [`longest_substring_match_with_hash`](Self::longest_substring_match_with_hash).
    #[inline(always)]
    pub fn prefetch_block(&self, data: &[u8]) -> Option<u64> {
        let h = hash_block(data.get(..self.block_size)?);
        let bucket = h as usize & self.mask;
        prefetch(self.table.slots().get(bucket * BUCKET_SIZE));
        Some(h)
    }

    pub fn longest_substring_match(&self, needle: &[u8]) -> LongestCommonSubstring {
        match needle.get(..self.block_size) {
            Some(block) => self.extend(needle, block, hash_block(block)),
            None => NO_MATCH,
        }
    }

    /// Like longest_substring_match but uses a pre-computed hash from prefetch_block.
    pub fn longest_substring_match_with_hash(
        &self,
        needle: &[u8],
        h: u64,
    ) -> LongestCommonSubstring {
        match needle.get(..self.block_size) {
            Some(block) => self.extend(needle, block, h),
            None => NO_MATCH,
        }
    }

    /// Find `block`, the start of `needle`, and extend the match forward
    /// using common_prefix_len.
    #[inline(always)]
    fn extend(&self, needle: &[u8], block: &[u8], h: u64) -> LongestCommonSubstring {
        let Some(start) = self.lookup_with_hash(block, h) else {
            return NO_MATCH;
        };
        // Skip block_size bytes: lookup already verified they match.
        let text = self.text.get(start + self.block_size..).unwrap_or_default();
        let needle = needle.get(self.block_size..).unwrap_or_default();
        LongestCommonSubstring {
            start,
            len: self.block_size + common_prefix_len(text, needle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &[u8]) -> HashIndex<'_> {
        HashIndex::new(text, 4, None, 2).unwrap()
    }

    #[test]
    fn basic_match() {
        let text = b"hello world, hello rust!";
        let idx = index(text);
        // "hell" appears at offset 0 and 13. The hash table stores only one
        // entry per unique block, so we get whichever one was stored. The match
        // extends forward from that point. Either way we get a valid match.
        let result = idx.longest_substring_match(b"hello rust");
        assert!(
            result.len >= 5,
            "expected match of at least 5, got {} at offset {}",
            result.len,
            result.start
        );
    }

    #[test]
    fn no_match() {
        let text = b"abcdefghijklmnop";
        let idx = index(text);
        let result = idx.longest_substring_match(b"xyzw1234");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn empty_text() {
        let text = b"";
        let idx = index(text);
        let result = idx.longest_substring_match(b"hello");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn short_needle() {
        let text = b"abcdefghijklmnop";
        let idx = index(text);
        // Needle shorter than block_size — can't hash
        let result = idx.longest_substring_match(b"ab");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn aligned_match() {
        // "the " starts at offset 0 (aligned to block_size=4),
        // so "the lazy" should find a match starting there or at offset 31.
        let text = b"the quick brown fox jumps over the lazy dog!";
        let idx = index(text);
        let result = idx.longest_substring_match(b"the lazy dog!");
        // "the " at offset 32 is block-aligned (32/4=8), should match
        assert!(result.len >= 4, "expected match >= 4, got {}", result.len);
    }

    #[test]
    fn keeps_the_earliest_of_equal_blocks() {
        let text = b"abcdXXXXabcdYYYYabcd";
        for threads in [1, 3] {
            let idx = HashIndex::new(text, 4, None, threads).unwrap();
            assert_eq!(idx.longest_substring_match(b"abcdZ").start, 0);
        }
    }

    #[test]
    fn refuses_small_blocks() {
        let err = HashIndex::new(b"abcdefgh", 3, None, 1).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn common_prefix() {
        let a = b"0123456789abcdefXYZ";
        for len in 0..a.len() {
            let mut b = a.to_vec();
            b[len] = b'!';
            assert_eq!(common_prefix_len(a, &b), len);
            assert_eq!(common_prefix_len(&a[..len], a), len);
        }
        assert_eq!(common_prefix_len(a, a), a.len());
    }

    #[test]
    fn table_sizes() {
        assert_eq!(table_bytes(0, 32).unwrap(), 64);
        assert_eq!(table_bytes(31, 32).unwrap(), 64);
        // 1000 blocks need 2000 slots: 250 buckets, rounded up to 256.
        assert_eq!(table_bytes(32_000, 32).unwrap(), 256 * 64);
    }
}
