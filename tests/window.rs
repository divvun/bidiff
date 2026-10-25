#![cfg(all(feature = "diff", feature = "patch"))]

mod common;

use bidiff::patch::{Error, MAX_WINDOW_LOG, Malformed, read_patch};
use common::*;

/// Instructions writing `lit` as one literal.
fn literal(lit: &[u8]) -> Vec<u8> {
    let mut s = Vec::new();
    varint(&mut s, 0);
    varint(&mut s, lit.len() as u64 * 2);
    s.extend_from_slice(lit);
    varint(&mut s, 0);
    s
}

#[test]
fn frames_up_to_the_limit_decode() {
    let old = b"base";
    let stream = literal(b"hello");
    let frame = raw_frame(MAX_WINDOW_LOG, 0, &stream);
    let patch = single_frame_patch(old, b"hello", stream.len() as u64, &frame);
    assert_eq!(apply_stream(old, &patch).unwrap(), b"hello");
    assert_eq!(apply_slice(old, &patch, 64).unwrap().unwrap(), b"hello");
}

#[test]
fn larger_windows_are_refused_before_decoding() {
    let old = b"base";
    let stream = literal(b"hello");
    for (log, mantissa) in [
        (MAX_WINDOW_LOG, 1),
        (MAX_WINDOW_LOG + 1, 0),
        (27, 0),
        (41, 7),
    ] {
        let frame = raw_frame(log, mantissa, &stream);
        let patch = single_frame_patch(old, b"hello", stream.len() as u64, &frame);
        let expected = (1u64 << log) + (1u64 << log) / 8 * u64::from(mantissa);
        for r in [
            apply_stream(old, &patch),
            apply_slice(old, &patch, 64).unwrap(),
        ] {
            match r {
                Err(Error::Malformed(Malformed::WindowTooLarge { chunk: 0, window })) => {
                    assert_eq!(window, expected)
                }
                other => panic!("window 2^{log}+{mantissa}/8: {other:?}"),
            }
        }
        let p = read_patch(&patch).unwrap();
        let mut out = [0u8; 5];
        assert!(matches!(
            bidiff::patch::apply_chunk(&p.chunks()[0], old, &mut out),
            Err(Error::Malformed(Malformed::WindowTooLarge { .. }))
        ));
    }
}
