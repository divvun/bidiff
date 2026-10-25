#![cfg(all(feature = "diff", feature = "patch"))]

mod common;

use bidiff::patch::{Error, read_patch};
use common::*;
use std::io::{self, Write};

fn sample() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (old, new) = related_pair(&mut Rng(99), 20_000);
    let patch = make_patch(&old, &new, Some(4096), 8);
    (old, new, patch)
}

/// Records every write so a test can check that nothing was written.
#[derive(Default)]
struct Recorder {
    writes: usize,
    data: Vec<u8>,
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn header_declares_both_hashes() {
    let (old, new, patch) = sample();
    let p = read_patch(&patch).unwrap();
    assert_eq!(p.old_hash(), blake3::hash(&old));
    assert_eq!(p.new_hash(), blake3::hash(&new));
    assert_eq!(
        &patch[layout::OLD_HASH..][..32],
        blake3::hash(&old).as_bytes()
    );
    assert_eq!(
        &patch[layout::NEW_HASH..][..32],
        blake3::hash(&new).as_bytes()
    );
}

#[test]
fn wrong_base_of_the_same_size_writes_nothing() {
    let (old, _, patch) = sample();
    let p = read_patch(&patch).unwrap();
    let mut wrong = old.clone();
    wrong[old.len() / 2] ^= 0x40;

    let mut out = Recorder::default();
    match p.apply(&wrong, &mut out) {
        Err(Error::BaseMismatch {
            expected_hash,
            actual_hash: Some(actual),
            expected_size,
            actual_size,
        }) => {
            assert_eq!(expected_hash, blake3::hash(&old));
            assert_eq!(actual, blake3::hash(&wrong));
            assert_eq!(expected_size, actual_size);
        }
        other => panic!("expected BaseMismatch, got {other:?}"),
    }
    assert_eq!(out.writes, 0);

    let mut slice = vec![0xAA; p.new_size() as usize];
    assert!(matches!(
        p.apply_to_slice(&wrong, &mut slice),
        Err(Error::BaseMismatch { .. })
    ));
    assert!(slice.iter().all(|&b| b == 0xAA), "output slice was touched");
}

#[test]
fn wrong_base_size_is_refused_without_hashing() {
    let (old, _, patch) = sample();
    let p = read_patch(&patch).unwrap();
    for wrong in [&old[..old.len() - 1], &[][..]] {
        let mut out = Recorder::default();
        match p.apply(wrong, &mut out) {
            Err(Error::BaseMismatch {
                actual_hash: None,
                expected_size,
                actual_size,
                ..
            }) => {
                assert_eq!(expected_size, old.len() as u64);
                assert_eq!(actual_size, wrong.len() as u64);
            }
            other => panic!("expected BaseMismatch, got {other:?}"),
        }
        assert_eq!(out.writes, 0);
    }
}

#[test]
fn forged_base_hash_is_refused() {
    let (old, _, mut patch) = sample();
    patch[layout::OLD_HASH] ^= 1;
    let mut out = Recorder::default();
    assert!(matches!(
        read_patch(&patch).unwrap().apply(&old, &mut out),
        Err(Error::BaseMismatch {
            actual_hash: Some(_),
            ..
        })
    ));
    assert_eq!(out.writes, 0);
}

/// Offset of the first delta or literal byte in an instruction stream, so a
/// test can change output bytes without changing how the stream parses.
fn first_data_byte(stream: &[u8]) -> Option<usize> {
    fn varint(s: &[u8], pos: &mut usize) -> u64 {
        let mut v = 0u64;
        for i in 0.. {
            let b = s[*pos];
            *pos += 1;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return v;
            }
        }
        unreachable!()
    }
    let mut pos = 0;
    while pos < stream.len() {
        let add = varint(stream, &mut pos);
        if add & 1 == 0 && add >> 1 > 0 {
            return Some(pos);
        }
        let copy = varint(stream, &mut pos);
        if copy & 1 == 0 && copy >> 1 > 0 {
            return Some(pos);
        }
        if copy & 1 == 1 {
            varint(stream, &mut pos);
        }
        varint(stream, &mut pos);
    }
    None
}

#[test]
fn altered_instruction_data_is_a_result_mismatch() {
    let (old, new, patch) = sample();
    let mut altered = None;
    for chunk in chunk_offsets(&patch) {
        let mut stream = instructions_of(&patch, chunk);
        if let Some(at) = first_data_byte(&stream) {
            stream[at] ^= 0x01;
            altered = Some(replace_instructions(&patch, chunk, &stream));
            break;
        }
    }
    let altered = altered.expect("no chunk carries delta or literal bytes");

    // Streaming writes the whole damaged result before the hash can be
    // checked; the error tells the caller to throw it away.
    let p = read_patch(&altered).unwrap();
    let mut out = Recorder::default();
    match p.apply(&old, &mut out) {
        Err(Error::ResultMismatch { expected, actual }) => {
            assert_eq!(expected, blake3::hash(&new));
            assert_eq!(actual, blake3::hash(&out.data));
        }
        other => panic!("expected ResultMismatch, got {other:?}"),
    }
    assert_eq!(out.data.len(), new.len());
    assert_ne!(out.data, new);

    let mut slice = vec![0; new.len()];
    assert!(matches!(
        p.apply_to_slice(&old, &mut slice),
        Err(Error::ResultMismatch { .. })
    ));
}

#[test]
fn declared_result_hash_is_checked() {
    let (old, new, mut patch) = sample();
    patch[layout::NEW_HASH + 31] ^= 0x80;
    let p = read_patch(&patch).unwrap();
    assert_ne!(p.new_hash(), blake3::hash(&new));
    match apply_stream(&old, &patch) {
        Err(Error::ResultMismatch { expected, actual }) => {
            assert_eq!(expected, p.new_hash());
            assert_eq!(actual, blake3::hash(&new));
        }
        other => panic!("expected ResultMismatch, got {other:?}"),
    }
}

#[test]
fn result_hash_is_known_before_applying() {
    // A caller holding an attested hash can reject a patch for the wrong
    // target without touching the base.
    let (_, new, patch) = sample();
    let attested = blake3::hash(&new);
    assert_eq!(read_patch(&patch).unwrap().new_hash(), attested);
}

#[test]
fn verify_base_then_apply_both_ways() {
    let (old, new, patch) = sample();
    let p = read_patch(&patch).unwrap();
    let base = p.verify_base(&old).unwrap();
    let mut out = Recorder::default();
    base.apply(&mut out).unwrap();
    assert_eq!(out.data, new);
    let mut slice = vec![0; new.len()];
    base.apply_to_slice(&mut slice).unwrap();
    assert_eq!(slice, new);
}

#[test]
fn version_0x2000_patches_are_refused() {
    // The 0x2000 layout: magic, version, new_size, num_chunks, then chunks.
    let mut v2000 = Vec::new();
    v2000.extend_from_slice(&bidiff::MAGIC.to_le_bytes());
    v2000.extend_from_slice(&0x2000u32.to_le_bytes());
    v2000.extend_from_slice(&0u64.to_le_bytes());
    v2000.extend_from_slice(&0u32.to_le_bytes());
    match read_patch(&v2000) {
        Err(e @ Error::UnsupportedVersion(0x2000)) => {
            assert!(e.to_string().contains("unsupported patch format version"));
        }
        Err(e) => panic!("expected UnsupportedVersion, got {e:?}"),
        Ok(_) => panic!("0x2000 patch accepted"),
    }
}
