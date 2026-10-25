#![cfg(all(feature = "diff", feature = "patch"))]

mod common;

use bidiff::patch::read_patch;
use common::*;
use proptest::prelude::*;

fn check_round_trip(old: &[u8], new: &[u8], scan_chunk: Option<usize>, block: usize) {
    let patch = make_patch(old, new, scan_chunk, block);
    let p = read_patch(&patch).unwrap();
    assert_eq!(p.old_size(), old.len() as u64);
    assert_eq!(p.old_hash(), blake3::hash(old));
    assert_eq!(p.new_size(), new.len() as u64);
    assert_eq!(p.new_hash(), blake3::hash(new));

    let streamed = apply_stream(old, &patch).unwrap();
    assert!(streamed == new, "streamed output differs");

    let sliced = apply_slice(old, &patch, u64::MAX).unwrap().unwrap();
    assert!(sliced == new, "sliced output differs");
}

fn scan_chunk() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![Just(None), (1usize..300).prop_map(Some)]
}

proptest! {
    #[test]
    fn unrelated_pairs(
        old in proptest::collection::vec(any::<u8>(), 0..2048),
        new in proptest::collection::vec(any::<u8>(), 0..2048),
        scan_chunk in scan_chunk(),
        block in 4usize..40,
    ) {
        check_round_trip(&old, &new, scan_chunk, block);
    }

    #[test]
    fn related_pairs(
        seed in any::<u64>(),
        len in 0usize..8192,
        scan_chunk in scan_chunk(),
        block in 4usize..40,
    ) {
        let (old, new) = related_pair(&mut Rng(seed | 1), len);
        check_round_trip(&old, &new, scan_chunk, block);
    }
}

#[test]
fn empty_inputs() {
    for scan_chunk in [None, Some(1), Some(1024)] {
        check_round_trip(b"", b"", scan_chunk, 4);
        check_round_trip(b"", b"some new bytes", scan_chunk, 4);
        check_round_trip(b"some old bytes", b"", scan_chunk, 4);
    }
}

#[test]
fn spans_many_instruction_buffers() {
    // Larger than the decoder's instruction window and the writer's buffer.
    let mut rng = Rng(7);
    let (old, new) = related_pair(&mut rng, 3 << 20);
    check_round_trip(&old, &new, None, 32);
    check_round_trip(&old, &new, Some(1 << 20), 32);
    let unrelated = rng.bytes(3 << 20);
    check_round_trip(&old, &unrelated, Some(1 << 20), 32);
}

#[test]
fn copy_from_old_instructions() {
    // The diff never emits COPY_OLD, so drive it through the encoder directly.
    let old: Vec<u8> = (0..=255).collect();
    let ctl = |add, copy, seek| bidiff::Control { add, copy, seek };
    let mut w = bidiff::enc::Writer::new(Vec::new());
    w.write_extended(&ctl(&[0, 0, 0, 0], &[], 0), Some(200))
        .unwrap();
    w.write_extended(&ctl(&[1, 1], &old[10..20], -6), Some(10))
        .unwrap();
    w.write_extended(&ctl(&[], b"lit", 0), None).unwrap();
    let stream = w.into_inner();

    let mut expected = old[0..4].to_vec();
    expected.extend([old[4] + 1, old[5] + 1]);
    expected.extend_from_slice(&old[10..20]);
    expected.extend_from_slice(b"lit");

    let patch = single_chunk_patch(&old, &expected, 0, &stream);
    assert_eq!(apply_stream(&old, &patch).unwrap(), expected);
}

#[test]
fn hashed_on_helper_threads() {
    // Results over 4 MiB are hashed in 1 MiB blocks on helper threads; these
    // land on, just under and just over block boundaries.
    let mib = 1 << 20;
    let mut rng = Rng(23);
    let (old, base_new) = related_pair(&mut rng, 6 * mib + 4096);
    for len in [4 * mib + 1, 5 * mib - 1, 5 * mib, 6 * mib + 1] {
        let new = &base_new[..len];
        check_round_trip(&old, new, Some(mib), 32);
    }
}
