#![cfg(all(feature = "diff", feature = "patch"))]

mod common;

use bidiff::DiffParams;
use common::*;
use std::io;

const LEVEL: i32 = 3;

fn params(chunk: usize) -> DiffParams {
    DiffParams::with_threads(16, Some(chunk), Some(1)).unwrap()
}

fn diff(old: &[u8], new: &[u8], params: &DiffParams) -> io::Result<Vec<u8>> {
    let mut patch = Vec::new();
    bidiff::simple_diff_chunked_with_params(old, new, &mut patch, params, LEVEL).map(|()| patch)
}

fn least_limit(old: &[u8], new: &[u8], params: &DiffParams) -> usize {
    let (mut low, mut high) = (0, 1 << 30);
    while low < high {
        let mid = (low + high) / 2;
        match diff(old, new, &params.clone().with_memory_limit(mid)) {
            Ok(_) => high = mid,
            Err(e) if e.kind() == io::ErrorKind::OutOfMemory => low = mid + 1,
            Err(e) => panic!("{e}"),
        }
    }
    low
}

#[test]
fn spilling_to_disk_makes_the_same_patch() {
    let (old, new) = related_pair(&mut Rng(5), 300_000);
    let p = params(64 << 10);
    let in_ram = diff(&old, &new, &p).unwrap();

    let least = least_limit(&old, &new, &p);
    let dir = tempfile::tempdir().unwrap();
    let on_disk = diff(
        &old,
        &new,
        &p.clone().with_memory_limit(least).with_temp_dir(dir.path()),
    )
    .unwrap();
    assert!(on_disk == in_ram);
    assert_eq!(apply_stream(&old, &on_disk).unwrap(), new);
    // The table's and the chunks' files are unnamed, so none is left.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_limit_too_small_fails_before_writing() {
    let (old, new) = related_pair(&mut Rng(6), 50_000);
    let p = params(16 << 10);
    let least = least_limit(&old, &new, &p);
    let mut patch = Vec::new();
    let err = bidiff::simple_diff_chunked_with_params(
        &old,
        &new,
        &mut patch,
        &p.with_memory_limit(least - 1),
        LEVEL,
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
    assert!(patch.is_empty());
}

#[test]
fn a_missing_temp_dir_is_an_error() {
    let (old, new) = related_pair(&mut Rng(7), 50_000);
    let dir = tempfile::tempdir().unwrap();
    let p = params(16 << 10);
    let least = least_limit(&old, &new, &p);
    let p = p
        .with_memory_limit(least)
        .with_temp_dir(dir.path().join("missing"));
    assert_eq!(
        diff(&old, &new, &p).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn a_failing_writer_stops_the_diff() {
    let (old, new) = related_pair(&mut Rng(8), 1 << 20);
    let p = DiffParams::with_threads(16, Some(4 << 10), Some(4)).unwrap();
    let whole = diff(&old, &new, &p).unwrap();
    for limit in [0, 200, whole.len() / 2] {
        let mut out = Capped {
            data: Vec::new(),
            limit,
        };
        let err =
            bidiff::simple_diff_chunked_with_params(&old, &new, &mut out, &p, LEVEL).unwrap_err();
        assert_eq!(err.to_string(), "output cap exceeded");
    }
}

#[test]
fn small_blocks_are_refused() {
    let p = DiffParams::default();
    assert!(DiffParams::new(3, None).is_err());
    assert!(diff(b"abc", b"abd", &p).is_ok());
}
