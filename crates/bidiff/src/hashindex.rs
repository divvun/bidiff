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
        let a_chunk = u64::from_ne_bytes([
            a[i],
            a[i + 1],
            a[i + 2],
            a[i + 3],
            a[i + 4],
            a[i + 5],
            a[i + 6],
            a[i + 7],
        ]);
        let b_chunk = u64::from_ne_bytes([
            b[i],
            b[i + 1],
            b[i + 2],
            b[i + 3],
            b[i + 4],
            b[i + 5],
            b[i + 6],
            b[i + 7],
        ]);
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
// MmapTable: disk-backed hash table storage
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod mmap_table {
    use std::fs;
    use std::io;
    use std::os::unix::io::AsRawFd;

    /// A u32 array backed by a temp file on `/var/tmp` (disk-backed, not tmpfs).
    /// Pages are evictable under memory pressure, keeping RssAnon low.
    pub struct MmapTable {
        ptr: *mut u32,
        len: usize,
        _file: fs::File,
    }

    // MmapTable is effectively a &mut [u32] with sole ownership — safe to send/share.
    unsafe impl Send for MmapTable {}
    unsafe impl Sync for MmapTable {}

    impl MmapTable {
        /// Create a new table of `len` u32 slots, all initialized to `fill`.
        pub fn new(len: usize, fill: u32) -> io::Result<Self> {
            let byte_len = len * std::mem::size_of::<u32>();

            // Create temp file on /var/tmp (usually a real filesystem, not tmpfs)
            let file = tempfile()?;

            // Size the file
            file.set_len(byte_len as u64)?;

            // mmap it MAP_SHARED so pages can be evicted to disk
            let ptr = unsafe {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    byte_len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                );
                if p == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                p as *mut u32
            };

            // Fill with sentinel value
            let table = Self {
                ptr,
                len,
                _file: file,
            };
            for i in 0..len {
                table.set(i, fill);
            }

            Ok(table)
        }

        #[inline(always)]
        pub fn get(&self, i: usize) -> u32 {
            debug_assert!(i < self.len);
            unsafe { self.ptr.add(i).read() }
        }

        #[inline(always)]
        pub fn set(&self, i: usize, v: u32) {
            debug_assert!(i < self.len);
            unsafe {
                self.ptr.add(i).write(v);
            }
        }
    }

    impl Drop for MmapTable {
        fn drop(&mut self) {
            let byte_len = self.len * std::mem::size_of::<u32>();
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, byte_len);
            }
        }
    }

    /// Create an anonymous temp file on /var/tmp. The file is unlinked immediately
    /// so it is cleaned up automatically when the fd is closed.
    fn tempfile() -> io::Result<fs::File> {
        use std::ffi::CString;
        use std::os::unix::io::FromRawFd;

        let template = CString::new("/var/tmp/bidiff-XXXXXX")
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut buf = template.into_bytes_with_nul();
        let fd = unsafe { libc::mkstemp(buf.as_mut_ptr() as *mut libc::c_char) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Unlink immediately — the file stays alive via the fd
        unsafe {
            libc::unlink(buf.as_ptr() as *const libc::c_char);
        }
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

#[cfg(not(unix))]
mod mmap_table {
    use std::io;

    /// Fallback: plain Vec<u32> on non-unix platforms.
    pub struct MmapTable {
        data: Vec<u32>,
    }

    impl MmapTable {
        pub fn new(len: usize, fill: u32) -> io::Result<Self> {
            Ok(Self {
                data: vec![fill; len],
            })
        }

        #[inline(always)]
        pub fn get(&self, i: usize) -> u32 {
            self.data[i]
        }

        #[inline(always)]
        pub fn set(&self, i: usize, v: u32) {
            // Safety: we need interior mutability for the uniform API.
            // This is only called during single-threaded construction.
            #[allow(mutable_transmutes)]
            let slot = unsafe { &mut *(&self.data[i] as *const u32 as *mut u32) };
            *slot = v;
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
/// On unix, the hash table is backed by a disk-based temp file via mmap,
/// so its pages are evictable under memory pressure and don't count toward
/// anonymous RSS.
///
/// The index stores every block-aligned position in the old text. When queried,
/// it checks if the first `block_size` bytes of the needle match any indexed
/// block, then extends the match forward. This works correctly with
/// BsdiffIterator which calls `longest_substring_match` at each scan position.
pub struct HashIndex<'a> {
    text: &'a [u8],
    block_size: usize,
    /// Hash table using open addressing with linear probing.
    /// Each slot stores a u32 offset (EMPTY = empty).
    table: MmapTable,
    mask: usize,
}

const EMPTY: u32 = u32::MAX;

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
        assert!(block_size >= 4, "block_size must be at least 4");

        if text.len() < block_size {
            return Self {
                text,
                block_size,
                table: MmapTable::new(2, EMPTY).expect("failed to allocate hash table"),
                mask: 1,
            };
        }

        // Index every block_size-th byte position (block-aligned).
        let num_entries = text.len() / block_size;
        if num_entries == 0 {
            return Self {
                text,
                block_size,
                table: MmapTable::new(2, EMPTY).expect("failed to allocate hash table"),
                mask: 1,
            };
        }

        // Size table to ~1.5x the number of entries (~67% load factor).
        // With linear probing this gives average probe length ~2.0, well
        // within the 32-probe limit.
        let table_size = (num_entries * 3 / 2).next_power_of_two().max(2);
        let mask = table_size - 1;
        let table = MmapTable::new(table_size, EMPTY).expect("failed to allocate hash table");

        // Insert block-aligned positions. Iterate backwards so that earlier
        // offsets overwrite later ones with the same hash, biasing toward
        // matches earlier in the file.
        let mut i = num_entries;
        while i > 0 {
            i -= 1;
            let offset = i * block_size;
            let block = &text[offset..offset + block_size];
            let h = hash_block(block) as usize & mask;

            // Linear probe to find an empty slot
            let mut slot = h;
            loop {
                if table.get(slot) == EMPTY {
                    table.set(slot, offset as u32);
                    break;
                }
                // If existing entry has the same block content, overwrite
                let existing = table.get(slot) as usize;
                if &text[existing..existing + block_size] == block {
                    table.set(slot, offset as u32);
                    break;
                }
                slot = (slot + 1) & mask;
            }
        }

        Self {
            text,
            block_size,
            table,
            mask,
        }
    }

    /// Look up a block in the hash table, returning the offset if found.
    #[inline(always)]
    fn lookup(&self, block: &[u8]) -> Option<usize> {
        let h = hash_block(block) as usize & self.mask;
        let mut slot = h;
        let mut probes = 0;
        loop {
            let offset = self.table.get(slot);
            if offset == EMPTY {
                return None;
            }
            let o = offset as usize;
            if &self.text[o..o + self.block_size] == block {
                return Some(o);
            }
            probes += 1;
            if probes > 32 {
                return None; // give up after too many probes
            }
            slot = (slot + 1) & self.mask;
        }
    }
}

impl<'a> HashIndex<'a> {
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
            // Found a match — extend it forward using common_prefix_len
            let match_len = common_prefix_len(&self.text[text_offset..], needle);
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
