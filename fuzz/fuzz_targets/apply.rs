//! Input: a little-endian u16 base length, the base, then the patch.
//!
//! Seed the corpus with valid patches first, or the fuzzer spends its time
//! rediscovering zstd frames:
//!
//! ```sh
//! cargo run --manifest-path fuzz/Cargo.toml --example seeds -- fuzz/corpus/apply
//! cargo +nightly fuzz run apply
//! ```

#![no_main]

use bidiff::patch::{apply_chunk, read_patch};
use libfuzzer_sys::fuzz_target;
use std::io::{self, Write};

const OUTPUT_CAP: u64 = 1 << 22;

struct Capped(Vec<u8>);

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if (self.0.len() + buf.len()) as u64 > OUTPUT_CAP {
            return Err(io::Error::other("output cap exceeded"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((len, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let Some((old, patch)) = rest.split_at_checked(u16::from_le_bytes(*len) as usize) else {
        return;
    };
    let Ok(p) = read_patch(patch) else {
        return;
    };

    let mut streamed = Capped(Vec::new());
    let streamed_ok = p.apply(old, &mut streamed).is_ok();
    if streamed_ok {
        assert_eq!(blake3::hash(&streamed.0), p.new_hash());
    }

    if p.new_size() <= OUTPUT_CAP {
        let mut sliced = vec![0; p.new_size() as usize];
        let sliced_ok = p.apply_to_slice(old, &mut sliced).is_ok();
        assert_eq!(streamed_ok, sliced_ok);
        if sliced_ok {
            assert_eq!(sliced, streamed.0);
        }
    }

    // apply_chunk skips the base check, so the decoder also meets bases the
    // patch was not made for.
    for chunk in p.chunks() {
        if chunk.new_len() <= OUTPUT_CAP {
            let mut out = vec![0; chunk.new_len() as usize];
            let _ = apply_chunk(chunk, old, &mut out);
        }
    }
});
