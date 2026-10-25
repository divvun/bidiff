//! Write valid inputs for the `apply` fuzz target into the given directory.

use bidiff::DiffParams;
use std::{fs, path::PathBuf};

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: seeds <corpus dir>"));
    fs::create_dir_all(&dir).unwrap();
    let mut rng = 0x5eed_u64;
    let mut written = 0;
    for (len, scan_chunk, block) in [
        (0usize, None, 4usize),
        (16, Some(4), 4),
        (300, None, 8),
        (1000, Some(256), 4),
        (2000, Some(700), 16),
        (4000, Some(1024), 32),
    ] {
        let old: Vec<u8> = (0..len).map(|_| xorshift(&mut rng) as u8).collect();
        for variant in 0..4 {
            let mut new = old.clone();
            match variant {
                0 => {}
                1 => new.extend((0..50).map(|_| xorshift(&mut rng) as u8)),
                2 => {
                    for b in new.iter_mut().step_by(97) {
                        *b ^= 0x5a;
                    }
                }
                _ => {
                    new = (0..len / 2 + 10)
                        .map(|_| xorshift(&mut rng) as u8)
                        .collect()
                }
            }
            let mut params = DiffParams::new(block, scan_chunk).unwrap();
            params.use_ram = true;
            let mut patch = Vec::new();
            bidiff::simple_diff_chunked_with_params(&old, &new, &mut patch, &params, 3).unwrap();

            let mut input = (old.len() as u16).to_le_bytes().to_vec();
            input.extend_from_slice(&old);
            input.extend_from_slice(&patch);
            fs::write(dir.join(format!("seed-{len}-{variant}")), input).unwrap();
            written += 1;
        }
    }
    eprintln!("wrote {written} seeds to {}", dir.display());
}
