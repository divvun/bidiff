#![cfg(all(feature = "diff", feature = "patch"))]

mod common;

use bidiff::patch::{Error, Malformed, apply_chunk, read_patch};
use common::*;

fn sample() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (old, new) = related_pair(&mut Rng(42), 4096);
    let patch = make_patch(&old, &new, Some(1024), 8);
    (old, new, patch)
}

fn malformed(r: Result<impl Sized, Error>) -> Malformed {
    match r {
        Err(Error::Malformed(m)) => m,
        Err(e) => panic!("expected Malformed, got {e:?}"),
        Ok(_) => panic!("expected Malformed, got Ok"),
    }
}

#[test]
fn wrong_magic() {
    let (_, _, mut patch) = sample();
    patch[0] ^= 1;
    assert!(matches!(read_patch(&patch), Err(Error::WrongMagic(_))));
}

#[test]
fn other_versions_are_unsupported() {
    let (_, _, mut patch) = sample();
    for v in [0x1000, 0x2000, bidiff::VERSION + 1, u32::MAX] {
        if v == bidiff::VERSION {
            continue;
        }
        put_u32(&mut patch, layout::VERSION, v);
        match read_patch(&patch) {
            Err(Error::UnsupportedVersion(got)) => assert_eq!(got, v),
            Err(e) => panic!("version {v:#x}: {e:?}"),
            Ok(_) => panic!("version {v:#x} accepted"),
        }
    }
}

#[test]
fn every_truncation_is_an_error() {
    let (old, _, patch) = sample();
    for len in 0..patch.len() {
        let cut = &patch[..len];
        assert!(read_patch(cut).is_err(), "prefix of {len} bytes parsed");
        assert!(apply_stream(&old, cut).is_err());
    }
}

#[test]
fn trailing_data() {
    let (_, _, mut patch) = sample();
    patch.push(0);
    assert!(matches!(
        malformed(read_patch(&patch)),
        Malformed::TrailingData { .. }
    ));
}

#[test]
fn forged_chunk_count_does_not_allocate() {
    let (_, _, mut patch) = sample();
    put_u32(&mut patch, layout::NUM_CHUNKS, u32::MAX);
    assert!(matches!(
        malformed(read_patch(&patch)),
        Malformed::ChunkCount { .. }
    ));
}

#[test]
fn forged_new_size() {
    let (_, _, mut patch) = sample();
    for v in [0, 1, 1 << 60, u64::MAX] {
        put_u64(&mut patch, layout::NEW_SIZE, v);
        assert!(read_patch(&patch).is_err(), "new_size {v}");
    }
}

#[test]
fn overlapping_or_gapped_chunks() {
    let (_, _, patch) = sample();
    let chunks = chunk_offsets(&patch);
    assert!(chunks.len() >= 3);
    for delta in [-1i64, 1] {
        let mut p = patch.clone();
        let at = chunks[1] + layout::NEW_START;
        let v = get_u64(&p, at).wrapping_add_signed(delta);
        put_u64(&mut p, at, v);
        assert!(matches!(
            malformed(read_patch(&p)),
            Malformed::ChunkLayout { chunk: 1 }
        ));
    }
    // A chunk whose end wraps around u64.
    let mut p = patch.clone();
    put_u64(&mut p, chunks[0] + layout::NEW_LEN, u64::MAX);
    assert!(matches!(
        malformed(read_patch(&p)),
        Malformed::ChunkLayout { chunk: 0 }
    ));
}

#[test]
fn forged_data_len() {
    let (_, _, patch) = sample();
    let at = chunk_offsets(&patch)[0] + layout::DATA_LEN;
    for v in [u64::MAX, u64::MAX / 2, patch.len() as u64] {
        let mut p = patch.clone();
        put_u64(&mut p, at, v);
        assert!(read_patch(&p).is_err());
    }
}

#[test]
fn huge_declared_sizes_do_not_allocate() {
    // One chunk claiming 2^60 output bytes, consistent with the header, so
    // parsing succeeds and only applying can notice.
    let old = vec![7u8; 64];
    let mut stream = Vec::new();
    varint(&mut stream, 64 * 2 + 1);
    varint(&mut stream, 0);
    varint(&mut stream, zigzag(-64));
    let mut patch = single_chunk_patch(&old, &[0; 64], 0, &stream);
    let huge = 1u64 << 60;
    put_u64(&mut patch, layout::NEW_SIZE, huge);
    put_u64(&mut patch, layout::HEADER_LEN + layout::NEW_LEN, huge);
    let p = read_patch(&patch).unwrap();
    assert_eq!(p.new_size(), huge);

    assert!(matches!(
        malformed(apply_stream(&old, &patch)),
        Malformed::Underrun { expected, actual: 64, .. } if expected == huge
    ));
    // The caller cannot provide a matching slice; a short one is refused.
    let mut out = [0u8; 64];
    assert!(matches!(
        apply_chunk(&p.chunks()[0], &old, &mut out),
        Err(Error::Io(_))
    ));
    assert!(matches!(
        p.apply_to_slice(&old, &mut out),
        Err(Error::Io(_))
    ));

    // A declared instruction length of 2^60 is never allocated either.
    let mut patch = single_chunk_patch(&old, &old, 0, &stream);
    put_u64(&mut patch, layout::HEADER_LEN + layout::RAW_LEN, huge);
    assert!(matches!(
        malformed(apply_stream(&old, &patch)),
        Malformed::InstructionLength { expected, .. } if expected == huge
    ));
}

#[test]
fn declared_instruction_length_is_enforced() {
    let (old, _, patch) = sample();
    let at = chunk_offsets(&patch)[0] + layout::RAW_LEN;
    let raw = get_u64(&patch, at);
    for v in [0, raw - 1, raw + 1] {
        let mut p = patch.clone();
        put_u64(&mut p, at, v);
        assert!(matches!(
            malformed(apply_stream(&old, &p)),
            Malformed::InstructionLength { chunk: 0, .. }
        ));
    }
}

fn apply_raw(old: &[u8], new_len: usize, stream: &[u8]) -> Result<Vec<u8>, Error> {
    let patch = single_chunk_patch(old, &vec![0; new_len], 0, stream);
    apply_stream(old, &patch)
}

fn control(add_tag: u64, add: &[u8], copy_tag: u64, copy: &[u8], seek: i64) -> Vec<u8> {
    let mut s = Vec::new();
    varint(&mut s, add_tag);
    s.extend_from_slice(add);
    varint(&mut s, copy_tag);
    s.extend_from_slice(copy);
    varint(&mut s, zigzag(seek));
    s
}

#[test]
fn instructions_out_of_range() {
    let old = vec![1u8; 100];

    // ADD past the end of the base, by delta and by zero-copy.
    let r = apply_raw(&old, 101, &control(101 * 2, &[0; 101], 0, &[], 0));
    assert!(matches!(malformed(r), Malformed::BaseRange { .. }));
    let r = apply_raw(&old, 101, &control(101 * 2 + 1, &[], 0, &[], 0));
    assert!(matches!(malformed(r), Malformed::BaseRange { .. }));

    // ADD whose length overflows when added to the cursor.
    let r = apply_raw(&old, 1, &control(u64::MAX, &[], 0, &[], 0));
    assert!(matches!(malformed(r), Malformed::BaseRange { .. }));

    // COPY_OLD past the end, and with an offset that overflows.
    let mut s = Vec::new();
    varint(&mut s, 0);
    varint(&mut s, 10 * 2 + 1);
    varint(&mut s, 95);
    varint(&mut s, 0);
    assert!(matches!(
        malformed(apply_raw(&old, 10, &s)),
        Malformed::BaseRange { .. }
    ));
    let mut s = Vec::new();
    varint(&mut s, 0);
    varint(&mut s, 10 * 2 + 1);
    varint(&mut s, u64::MAX);
    varint(&mut s, 0);
    assert!(matches!(
        malformed(apply_raw(&old, 10, &s)),
        Malformed::BaseRange { .. }
    ));

    // SEEK before the start and past the end.
    for seek in [-1, 101, i64::MIN, i64::MAX] {
        let r = apply_raw(&old, 0, &control(0, &[], 0, &[], seek));
        assert!(
            matches!(malformed(r), Malformed::BaseRange { .. }),
            "seek {seek}"
        );
    }

    // A chunk whose starting cursor is past the base.
    let patch = single_chunk_patch(&old, &[], 101, &control(0, &[], 0, &[], 0));
    assert!(matches!(
        malformed(apply_stream(&old, &patch)),
        Malformed::BaseRange { .. }
    ));
}

#[test]
fn output_length_is_enforced() {
    let old = vec![1u8; 100];
    let lit = control(0, &[], 5 * 2, b"hello", 0);
    assert!(matches!(
        malformed(apply_raw(&old, 4, &lit)),
        Malformed::Overrun { .. }
    ));
    assert!(matches!(
        malformed(apply_raw(&old, 6, &lit)),
        Malformed::Underrun {
            expected: 6,
            actual: 5,
            ..
        }
    ));
    assert_eq!(apply_raw(&old, 5, &lit).unwrap(), b"hello");

    // A literal far longer than the stream must not be allocated or trusted.
    let huge = control(0, &[], u64::MAX - 1, b"x", 0);
    let mut patch = single_chunk_patch(&old, &[], 0, &huge);
    put_u64(&mut patch, layout::NEW_SIZE, u64::MAX >> 1);
    put_u64(
        &mut patch,
        layout::HEADER_LEN + layout::NEW_LEN,
        u64::MAX >> 1,
    );
    assert!(matches!(
        malformed(apply_stream(&old, &patch)),
        Malformed::Instruction { .. }
    ));
}

#[test]
fn bad_varints_and_truncated_instructions() {
    let old = vec![1u8; 100];
    let full = control(0, &[], 5 * 2, b"hello", 3);
    for len in 1..full.len() {
        let r = apply_raw(&old, 5, &full[..len]);
        assert!(r.is_err(), "instruction prefix {len} accepted");
    }
    let mut overlong = vec![0x80; 10];
    overlong.push(0);
    assert!(matches!(
        malformed(apply_raw(&old, 0, &overlong)),
        Malformed::Instruction { .. }
    ));
}

#[test]
fn bad_compressed_data() {
    let (old, _, patch) = sample();
    let first = chunk_offsets(&patch)[0];
    let data_at = first + layout::CHUNK_META_LEN;
    let data_len = get_u64(&patch, first + layout::DATA_LEN) as usize;

    // Chunk data that is not a zstd frame.
    let mut p = patch.clone();
    for b in &mut p[data_at..data_at + data_len] {
        *b = 0xA5;
    }
    assert!(matches!(
        malformed(apply_stream(&old, &p)),
        Malformed::Compression { chunk: 0, .. }
    ));

    // The frame cut short, with data_len shrunk to match.
    let mut p = patch[..data_at + data_len - 3].to_vec();
    p.extend_from_slice(&patch[data_at + data_len..]);
    put_u64(&mut p, first + layout::DATA_LEN, data_len as u64 - 3);
    assert!(matches!(
        malformed(apply_stream(&old, &p)),
        Malformed::Compression { chunk: 0, .. }
    ));

    // Bytes after the frame, inside the chunk.
    let mut p = patch[..data_at + data_len].to_vec();
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(&patch[data_at + data_len..]);
    put_u64(&mut p, first + layout::DATA_LEN, data_len as u64 + 4);
    assert!(matches!(
        malformed(apply_stream(&old, &p)),
        Malformed::Compression { chunk: 0, .. }
    ));
}

#[test]
fn short_base_is_an_error() {
    let (old, _, patch) = sample();
    for len in [0, 1, old.len() / 2, old.len() - 1] {
        assert!(apply_stream(&old[..len], &patch).is_err());
        assert!(apply_slice(&old[..len], &patch, u64::MAX).unwrap().is_err());
    }
}

#[test]
fn rebuilding_instructions_round_trips() {
    // The mutation test relies on these helpers producing valid patches.
    let (old, new, patch) = sample();
    let first = chunk_offsets(&patch)[0];
    let stream = instructions_of(&patch, first);
    let rebuilt = replace_instructions(&patch, first, &stream);
    assert_eq!(apply_stream(&old, &rebuilt).unwrap(), new);
}
