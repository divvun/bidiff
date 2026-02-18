use std::cmp::min;
use std::fmt;

/// Default block size for hashing (32 bytes)
pub const DEFAULT_BLOCK_SIZE: usize = 32;

// ---------------------------------------------------------------------------
// Inlined from sacabase: common_prefix_len, LongestCommonSubstring
// ---------------------------------------------------------------------------

pub struct LongestCommonSubstring<'a> {
    pub text: &'a [u8],
    pub start: usize,
    pub len: usize,
}

impl<'a> fmt::Debug for LongestCommonSubstring<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "T[{}..{}]", self.start, self.start + self.len)
    }
}

pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = min(a.len(), b.len());
    let mut i = 0;

    // Compare 8 bytes at a time using u64 XOR
    while i + 8 <= n {
        let a_chunk = u64::from_ne_bytes(a[i..i + 8].try_into().unwrap());
        let b_chunk = u64::from_ne_bytes(b[i..i + 8].try_into().unwrap());
        let xor = a_chunk ^ b_chunk;
        if xor != 0 {
            #[cfg(target_endian = "little")]
            {
                return i + (xor.trailing_zeros() as usize / 8);
            }
            #[cfg(target_endian = "big")]
            {
                return i + (xor.leading_zeros() as usize / 8);
            }
        }
        i += 8;
    }

    // Scalar tail for remaining bytes
    while i < n {
        if a[i] != b[i] {
            return i;
        }
        i += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// MmapTable: disk-backed hash table storage via memmap2
// ---------------------------------------------------------------------------

mod mmap_table {
    use memmap2::{MmapMut, MmapOptions};
    use std::io;

    /// A u64 array backed by a file-backed mmap (via tempfile + memmap2).
    /// The kernel can page out entries to disk under memory pressure.
    /// Works cross-platform (Linux, macOS, Windows).
    pub struct MmapTable {
        mmap: MmapMut,
        len: usize,
    }

    // SAFETY: MmapTable has sole ownership of the mapping (private tempfile,
    // no external references). The backing memory is never aliased.
    unsafe impl Send for MmapTable {}
    // SAFETY: Concurrent reads are plain loads (no tearing for aligned u64).
    // Concurrent writes during parallel construction use CAS (atomic).
    // Serial construction is single-threaded (no concurrent writes).
    unsafe impl Sync for MmapTable {}

    impl MmapTable {
        /// Create a new table of `len` u64 slots, all initialized to EMPTY (u64::MAX).
        pub fn new(len: usize) -> io::Result<Self> {
            let byte_len = len * std::mem::size_of::<u64>();
            let file = tempfile::tempfile()?;
            file.set_len(byte_len as u64)?;
            // SAFETY: Private tempfile — no other process can modify the mapping.
            // The file handle is dropped after mmap creation; the mapping persists.
            let mmap = unsafe { MmapOptions::new().len(byte_len).map_mut(&file)? };
            // File handle dropped here — mapping persists, kernel can page to disk.

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

        #[inline(always)]
        pub fn get(&self, i: usize) -> u64 {
            debug_assert!(i < self.len);
            // SAFETY: i < self.len (debug_assert above), mmap is len*8 bytes,
            // so pointer offset is within the allocation. Aligned u64 read.
            unsafe { (self.mmap.as_ptr() as *const u64).add(i).read() }
        }

        #[inline(always)]
        #[cfg_attr(feature = "parallel", allow(dead_code))]
        pub fn set(&self, i: usize, v: u64) {
            debug_assert!(i < self.len);
            // SAFETY: i < self.len (debug_assert above). Only called from the
            // serial construction path (single-threaded, no concurrent access).
            unsafe {
                (self.mmap.as_ptr() as *mut u64).add(i).write(v);
            }
        }

        /// Compare-and-swap for lock-free parallel insertion.
        /// AtomicU64 has identical size/alignment to u64, so the cast is safe
        /// on the page-aligned mmap memory.
        #[inline(always)]
        pub fn cas(&self, i: usize, expected: u64, new: u64) -> Result<u64, u64> {
            debug_assert!(i < self.len);
            use std::sync::atomic::{AtomicU64, Ordering};
            // SAFETY: AtomicU64 has identical size (8) and alignment (8) as u64.
            // The mmap is page-aligned. i < self.len (debug_assert above).
            let atom = unsafe { &*(self.mmap.as_ptr() as *const AtomicU64).add(i) };
            atom.compare_exchange(expected, new, Ordering::Relaxed, Ordering::Relaxed)
        }

        #[inline(always)]
        pub fn prefetch(&self, i: usize) {
            debug_assert!(i < self.len);
            // SAFETY: i < self.len (debug_assert above), pointer is within allocation.
            let ptr = unsafe { (self.mmap.as_ptr() as *const u64).add(i) };
            #[cfg(target_arch = "x86_64")]
            // SAFETY: Prefetch is a CPU hint that cannot cause UB.
            // Invalid/unmapped addresses are silently ignored by the processor.
            unsafe {
                std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
            }
            #[cfg(target_arch = "aarch64")]
            // SAFETY: Same as x86_64 — PRFM is a hint, cannot fault or cause UB.
            unsafe {
                std::arch::aarch64::_prefetch(ptr as *const i8, 0, 3);
            }
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
/// The hash table is backed by a file-backed mmap (via tempfile + memmap2),
/// so the kernel can page out entries under memory pressure. On Linux,
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
/// Valid entries always have lower 32 bits >= 1 (we store offset+1).
const EMPTY: u64 = 0;

/// 8 entries per bucket = 8 × 8 bytes = 64 bytes = 1 cache line.
/// A single DRAM fetch loads the entire bucket's probe sequence.
const BUCKET_SIZE: usize = 8;

/// Pack a u32 offset and the upper 32 bits of the hash into a single u64.
/// Stores offset+1 so that EMPTY (0) is unambiguous.
#[inline(always)]
fn pack_entry(offset: u32, hash: u64) -> u64 {
    (hash & 0xFFFF_FFFF_0000_0000) | (offset as u64 + 1)
}

/// Extract the u32 offset from a packed entry.
#[inline(always)]
fn entry_offset(entry: u64) -> u32 {
    (entry as u32) - 1
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
    if data.len() < 32 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        return h;
    }
    let a = u64::from_ne_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let b = u64::from_ne_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    let c = u64::from_ne_bytes([
        data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
    ]);
    let d = u64::from_ne_bytes([
        data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
    ]);
    wymix(a ^ 0xa0761d6478bd642f, b ^ 0xe7037ed1a0b428db)
        ^ wymix(c ^ 0x8ebc6af09c88c6e3, d ^ 0x1d8e4e27c47d124f)
}

impl<'a> HashIndex<'a> {
    /// Build a hash index over `text` with the given block size.
    ///
    /// The block size controls the granularity of matching. Smaller blocks find
    /// more matches but use more memory. 32 bytes is a good default.
    pub fn new(text: &'a [u8], block_size: usize) -> Self {
        let index = Self::new_empty(text, block_size);
        index.populate();
        index
    }

    /// Allocate an empty hash index (table created but no entries inserted).
    /// Call `populate()` to insert entries. Lookups on an unpopulated index
    /// return no matches.
    pub fn new_empty(text: &'a [u8], block_size: usize) -> Self {
        assert!(block_size >= 4, "block_size must be at least 4");

        if text.len() < block_size {
            return Self {
                text,
                block_size,
                table: MmapTable::new(BUCKET_SIZE).expect("failed to allocate hash table"),
                mask: 0, // 1 bucket
            };
        }

        let num_entries = text.len() / block_size;
        if num_entries == 0 {
            return Self {
                text,
                block_size,
                table: MmapTable::new(BUCKET_SIZE).expect("failed to allocate hash table"),
                mask: 0, // 1 bucket
            };
        }

        // Bucket mask: num_buckets is a power of 2, mask = num_buckets - 1.
        // 50% load factor: num_entries * 2 total slots, divided into buckets.
        let num_buckets = (num_entries * 2)
            .div_ceil(BUCKET_SIZE)
            .next_power_of_two()
            .max(1);
        let table_size = num_buckets * BUCKET_SIZE;
        let mask = num_buckets - 1;
        let table = MmapTable::new(table_size).expect("failed to allocate hash table");

        Self {
            text,
            block_size,
            table,
            mask,
        }
    }

    /// Insert all block-aligned positions into the hash table.
    ///
    /// Safe to call concurrently with lookups: the CAS-based insertion ensures
    /// consistent slot states. Lookups during population may miss entries not yet
    /// inserted (resulting in missed matches, not wrong matches).
    pub fn populate(&self) {
        let num_entries = self.text.len() / self.block_size;
        if num_entries == 0 {
            return;
        }

        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            (0..num_entries).into_par_iter().for_each(|i| {
                let offset = i * self.block_size;
                let h = hash_block(&self.text[offset..offset + self.block_size]);
                let packed = pack_entry(offset as u32, h);
                let needle_tag = (h >> 32) as u32;
                let block = &self.text[offset..offset + self.block_size];

                let mut bucket = h as usize & self.mask;
                loop {
                    let base = bucket * BUCKET_SIZE;
                    for slot in base..base + BUCKET_SIZE {
                        let entry = self.table.get(slot);
                        if entry == EMPTY {
                            match self.table.cas(slot, EMPTY, packed) {
                                Ok(_) => return,
                                Err(existing) => {
                                    // Slot claimed by another thread — check duplicate
                                    if entry_tag(existing) == needle_tag {
                                        let existing_off = entry_offset(existing) as usize;
                                        if &self.text[existing_off..existing_off + self.block_size]
                                            == block
                                        {
                                            if offset < existing_off {
                                                let _ = self.table.cas(slot, existing, packed);
                                            }
                                            return;
                                        }
                                    }
                                    // Not a duplicate — continue scanning bucket
                                }
                            }
                        } else if entry_tag(entry) == needle_tag {
                            let existing_off = entry_offset(entry) as usize;
                            if &self.text[existing_off..existing_off + self.block_size] == block {
                                if offset < existing_off {
                                    let _ = self.table.cas(slot, entry, packed);
                                }
                                return;
                            }
                        }
                    }
                    // Bucket full, overflow to next bucket
                    bucket = (bucket + 1) & self.mask;
                }
            });
        }

        #[cfg(not(feature = "parallel"))]
        {
            const PIPE_DEPTH: usize = 8;
            let prefill = min(PIPE_DEPTH, num_entries);
            let mut pipe_hash = [0u64; PIPE_DEPTH];
            let mut pipe_offset = [0u32; PIPE_DEPTH];

            for k in 0..prefill {
                let idx = num_entries - 1 - k;
                let offset = idx * self.block_size;
                let h = hash_block(&self.text[offset..offset + self.block_size]);
                self.table.prefetch((h as usize & self.mask) * BUCKET_SIZE);
                pipe_hash[k] = h;
                pipe_offset[k] = offset as u32;
            }

            let mut head = 0;
            let mut next_idx = num_entries.saturating_sub(PIPE_DEPTH);
            for _ in 0..num_entries {
                let h = pipe_hash[head];
                let offset = pipe_offset[head] as usize;
                let packed = pack_entry(offset as u32, h);
                let needle_tag = (h >> 32) as u32;
                let block = &self.text[offset..offset + self.block_size];

                let mut bucket = h as usize & self.mask;
                'insert: loop {
                    let base = bucket * BUCKET_SIZE;
                    for slot in base..base + BUCKET_SIZE {
                        let entry = self.table.get(slot);
                        if entry == EMPTY {
                            self.table.set(slot, packed);
                            break 'insert;
                        }
                        if entry_tag(entry) == needle_tag {
                            let existing = entry_offset(entry) as usize;
                            if &self.text[existing..existing + self.block_size] == block {
                                self.table.set(slot, packed);
                                break 'insert;
                            }
                        }
                    }
                    // Bucket full, overflow to next
                    bucket = (bucket + 1) & self.mask;
                    self.table.prefetch(bucket * BUCKET_SIZE);
                }

                if next_idx > 0 {
                    next_idx -= 1;
                    let offset = next_idx * self.block_size;
                    let h = hash_block(&self.text[offset..offset + self.block_size]);
                    self.table.prefetch((h as usize & self.mask) * BUCKET_SIZE);
                    pipe_hash[head] = h;
                    pipe_offset[head] = offset as u32;
                }
                head = (head + 1) % PIPE_DEPTH;
            }
        }
    }

    /// Look up a block in the hash table, returning the offset if found.
    /// Uses a 32-bit hash tag to reject non-matching probes without accessing
    /// the text, avoiding expensive cache misses and memcmp on most probes.
    #[inline(always)]
    fn lookup(&self, block: &[u8]) -> Option<usize> {
        let h = hash_block(block);
        self.lookup_with_hash(block, h)
    }

    /// Look up a block using a pre-computed hash, avoiding redundant hashing
    /// when the hash was already computed by prefetch_block.
    ///
    /// Bucket hashing: hash → bucket index, scan all 8 entries in the bucket
    /// (1 cache line = 1 DRAM fetch). Entries are packed from the front, so
    /// the first EMPTY slot means the entry isn't in this or any later bucket.
    #[inline(always)]
    fn lookup_with_hash(&self, block: &[u8], h: u64) -> Option<usize> {
        let needle_tag = (h >> 32) as u32;
        let mut bucket = h as usize & self.mask;
        let mut probes = 0;
        loop {
            let base = bucket * BUCKET_SIZE;
            for i in 0..BUCKET_SIZE {
                let entry = self.table.get(base + i);
                if entry == EMPTY {
                    return None;
                }
                if entry_tag(entry) == needle_tag {
                    let o = entry_offset(entry) as usize;
                    if &self.text[o..o + self.block_size] == block {
                        return Some(o);
                    }
                }
            }
            // Bucket full with no match — probe next bucket (rare at 50% load)
            probes += 1;
            if probes > 4 {
                return None;
            }
            bucket = (bucket + 1) & self.mask;
            self.table.prefetch(bucket * BUCKET_SIZE);
        }
    }
}

impl<'a> HashIndex<'a> {
    /// Prefetch the hash table slot for a block, so a subsequent lookup is faster.
    /// Returns the computed hash so it can be reused by lookup_with_hash.
    #[inline(always)]
    pub fn prefetch_block(&self, data: &[u8]) -> Option<u64> {
        if data.len() >= self.block_size {
            let h = hash_block(&data[..self.block_size]);
            let bucket = h as usize & self.mask;
            self.table.prefetch(bucket * BUCKET_SIZE);
            Some(h)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn longest_substring_match(&self, needle: &[u8]) -> LongestCommonSubstring<'a> {
        // If needle is shorter than block_size, we can't hash it
        if needle.len() < self.block_size || self.text.len() < self.block_size {
            return LongestCommonSubstring {
                text: self.text,
                start: 0,
                len: 0,
            };
        }

        // Hash the first block_size bytes of the needle and look up
        let block = &needle[..self.block_size];
        if let Some(text_offset) = self.lookup(block) {
            // Found a match — extend it forward using common_prefix_len.
            // Skip block_size bytes: lookup already verified they match.
            let match_len = self.block_size
                + common_prefix_len(
                    &self.text[text_offset + self.block_size..],
                    &needle[self.block_size..],
                );
            LongestCommonSubstring {
                text: self.text,
                start: text_offset,
                len: match_len,
            }
        } else {
            LongestCommonSubstring {
                text: self.text,
                start: 0,
                len: 0,
            }
        }
    }

    /// Like longest_substring_match but uses a pre-computed hash from prefetch_block.
    pub fn longest_substring_match_with_hash(
        &self,
        needle: &[u8],
        h: u64,
    ) -> LongestCommonSubstring<'a> {
        if needle.len() < self.block_size || self.text.len() < self.block_size {
            return LongestCommonSubstring {
                text: self.text,
                start: 0,
                len: 0,
            };
        }

        let block = &needle[..self.block_size];
        if let Some(text_offset) = self.lookup_with_hash(block, h) {
            // Skip block_size bytes: lookup already verified they match.
            let match_len = self.block_size
                + common_prefix_len(
                    &self.text[text_offset + self.block_size..],
                    &needle[self.block_size..],
                );
            LongestCommonSubstring {
                text: self.text,
                start: text_offset,
                len: match_len,
            }
        } else {
            LongestCommonSubstring {
                text: self.text,
                start: 0,
                len: 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_match() {
        let text = b"hello world, hello rust!";
        let idx = HashIndex::new(text, 4);
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
        let idx = HashIndex::new(text, 4);
        let result = idx.longest_substring_match(b"xyzw1234");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn empty_text() {
        let text = b"";
        let idx = HashIndex::new(text, 4);
        let result = idx.longest_substring_match(b"hello");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn short_needle() {
        let text = b"abcdefghijklmnop";
        let idx = HashIndex::new(text, 4);
        // Needle shorter than block_size — can't hash
        let result = idx.longest_substring_match(b"ab");
        assert_eq!(result.len, 0);
    }

    #[test]
    fn aligned_match() {
        // "the " starts at offset 0 (aligned to block_size=4),
        // so "the lazy" should find a match starting there or at offset 31.
        let text = b"the quick brown fox jumps over the lazy dog!";
        let idx = HashIndex::new(text, 4);
        let result = idx.longest_substring_match(b"the lazy dog!");
        // "the " at offset 32 is block-aligned (32/4=8), should match
        assert!(result.len >= 4, "expected match >= 4, got {}", result.len);
    }
}
