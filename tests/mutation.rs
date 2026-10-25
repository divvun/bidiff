#![cfg(all(feature = "diff", feature = "patch"))]

//! Applies thousands of mutated patches and checks that none panics.
//!
//! `BIDIFF_MUTATIONS` sets the number of mutants (default 20000) and
//! `BIDIFF_MUTATION_SEED` the RNG seed, to replay or extend a run.

mod common;

use bidiff::patch::{apply_chunk, read_patch};
use common::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

const OUTPUT_CAP: usize = 16 << 20;

const INTERESTING_U64: &[u64] = &[
    0,
    1,
    2,
    7,
    8,
    0x7f,
    0x80,
    0xff,
    0x100,
    0xffff,
    0x1_0000,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    0x1_0000_0000,
    1 << 60,
    i64::MAX as u64,
    1 << 63,
    u64::MAX - 1,
    u64::MAX,
];

struct Case {
    old: Vec<u8>,
    new: Vec<u8>,
    patch: Vec<u8>,
}

fn cases(rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    for (len, scan_chunk, block) in [
        (0, None, 4),
        (64, Some(16), 4),
        (512, None, 8),
        (2048, Some(256), 4),
        (4096, Some(1024), 16),
        (6000, Some(997), 32),
    ] {
        let (old, new) = related_pair(rng, len);
        let patch = make_patch(&old, &new, scan_chunk, block);
        cases.push(Case { old, new, patch });
    }
    let old = rng.bytes(1500);
    let new = rng.bytes(1700);
    let patch = make_patch(&old, &new, Some(500), 4);
    cases.push(Case { old, new, patch });
    cases
}

fn mutate_bytes(rng: &mut Rng, p: &mut Vec<u8>) {
    if p.is_empty() {
        p.push(rng.next() as u8);
        return;
    }
    match rng.below(7) {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(p.len());
                p[i] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let i = rng.below(p.len());
            p[i] = rng.next() as u8;
        }
        2 => {
            let n = rng.below(p.len());
            p.truncate(n);
        }
        3 => {
            let i = rng.below(p.len() + 1);
            let n = 1 + rng.below(16);
            let ins = rng.bytes(n);
            p.splice(i..i, ins);
        }
        4 => {
            let i = rng.below(p.len());
            let end = (i + 1 + rng.below(16)).min(p.len());
            p.drain(i..end);
        }
        5 => {
            // Overwrite an aligned or unaligned 8-byte field.
            if p.len() >= 8 {
                let i = rng.below(p.len() - 7);
                let v = INTERESTING_U64[rng.below(INTERESTING_U64.len())];
                p[i..i + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => {
            let i = rng.below(p.len());
            let j = rng.below(p.len());
            p.swap(i, j);
        }
    }
}

fn interesting(rng: &mut Rng, current: u64) -> u64 {
    match rng.below(3) {
        0 => INTERESTING_U64[rng.below(INTERESTING_U64.len())],
        1 => current.wrapping_add(1 + rng.below(4) as u64),
        _ => current.wrapping_sub(1 + rng.below(4) as u64),
    }
}

/// One mutation of a well-formed patch, aimed at a specific layer.
fn mutate(rng: &mut Rng, patch: &[u8]) -> Vec<u8> {
    let mut p = patch.to_vec();
    let chunks = chunk_offsets(patch);
    match rng.below(4) {
        0 => {
            for _ in 0..1 + rng.below(3) {
                mutate_bytes(rng, &mut p);
            }
        }
        1 => {
            // A header field.
            let fields = [
                (layout::VERSION, 4),
                (layout::OLD_SIZE, 8),
                (layout::NEW_SIZE, 8),
                (layout::NUM_CHUNKS, 4),
            ];
            let (at, width) = fields[rng.below(fields.len())];
            if width == 4 {
                let v = interesting(rng, get_u32(&p, at) as u64) as u32;
                put_u32(&mut p, at, v);
            } else {
                let v = interesting(rng, get_u64(&p, at));
                put_u64(&mut p, at, v);
            }
        }
        2 if !chunks.is_empty() => {
            // A chunk metadata field.
            let chunk = chunks[rng.below(chunks.len())];
            let fields = [
                layout::OLD_START,
                layout::NEW_START,
                layout::NEW_LEN,
                layout::RAW_LEN,
                layout::DATA_LEN,
            ];
            let at = chunk + fields[rng.below(fields.len())];
            let v = interesting(rng, get_u64(&p, at));
            put_u64(&mut p, at, v);
        }
        _ if !chunks.is_empty() => {
            // The decompressed instructions, re-compressed so zstd accepts
            // them and the instruction decoder sees the damage.
            let chunk = chunks[rng.below(chunks.len())];
            let mut stream = instructions_of(&p, chunk);
            for _ in 0..1 + rng.below(4) {
                mutate_bytes(rng, &mut stream);
            }
            p = replace_instructions(&p, chunk, &stream);
        }
        _ => mutate_bytes(rng, &mut p),
    }
    p
}

#[test]
fn mutated_patches_never_panic() {
    let n: usize = std::env::var("BIDIFF_MUTATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let seed: u64 = std::env::var("BIDIFF_MUTATION_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_b1df);
    let mut rng = Rng(seed | 1);
    let cases = cases(&mut rng);

    let (mut errors, mut ok) = (0usize, 0usize);
    #[cfg(feature = "mmap")]
    let file = tempfile::tempfile().unwrap();
    for i in 0..n {
        let case = &cases[i % cases.len()];
        let mutant = mutate(&mut rng, &case.patch);
        // One mutant in eight gets another case's base.
        let wrong_base;
        let old: &[u8] = if rng.below(8) == 0 {
            wrong_base = cases[rng.below(cases.len())].old.clone();
            &wrong_base
        } else {
            &case.old
        };

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let streamed = apply_stream_capped(old, &mutant, OUTPUT_CAP);
            let sliced = apply_slice(old, &mutant, OUTPUT_CAP as u64);
            #[cfg(feature = "mmap")]
            if let Ok(p) = read_patch(&mutant)
                && p.new_size() <= OUTPUT_CAP as u64
            {
                let filed = p.apply_to_file(old, &file);
                assert_eq!(
                    filed.is_ok(),
                    streamed.is_ok(),
                    "mutant {i}: file path disagrees"
                );
            }
            // apply_chunk skips the base check, so the decoder also meets
            // bases the patch was not made for.
            if let Ok(p) = read_patch(&mutant) {
                for chunk in p.chunks() {
                    if chunk.new_len() <= OUTPUT_CAP as u64 {
                        let mut out = vec![0; chunk.new_len() as usize];
                        let _ = apply_chunk(chunk, old, &mut out);
                    }
                }
            }
            (streamed, sliced)
        }));
        let Ok((streamed, sliced)) = outcome else {
            panic!(
                "mutant {i} (seed {seed}) panicked; patch bytes: {:02x?}",
                mutant
            );
        };
        // Output never exceeds new_size, so under the cap the two paths
        // must agree.
        if let Some(sliced) = sliced {
            match (&streamed, &sliced) {
                (Ok(a), Ok(b)) => assert!(a == b, "mutant {i}: outputs differ"),
                (Err(_), Err(_)) => {}
                _ => panic!("mutant {i}: streamed {streamed:?} but sliced {sliced:?}"),
            }
        }
        // The result hash makes any accepted output the real one.
        match streamed {
            Ok(out) => {
                assert!(out == case.new, "mutant {i}: accepted with wrong output");
                ok += 1;
            }
            Err(_) => errors += 1,
        }
    }
    eprintln!("{n} mutants: {errors} rejected, {ok} applied");
}
