#![cfg(all(feature = "diff", feature = "mmap"))]

mod common;

use bidiff::patch::{Error, read_patch};
use common::*;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
};

fn sample(len: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (old, new) = related_pair(&mut Rng(77), len);
    let patch = make_patch(&old, &new, Some(64 << 10), 16);
    (old, new, patch)
}

fn read_write(path: &std::path::Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

#[test]
fn round_trips_and_returns_the_hash() {
    for len in [0, 1, 4096, 300_000, (5 << 20) + 3] {
        let (old, new, patch) = sample(len);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let hash = read_patch(&patch)
            .unwrap()
            .apply_to_file(&old, tmp.as_file())
            .unwrap();
        assert_eq!(hash, blake3::hash(&new));
        assert!(fs::read(tmp.path()).unwrap() == new, "len {len}");
    }
}

#[test]
fn empty_results() {
    for old in [&b""[..], b"some base"] {
        let patch = make_patch(old, b"", Some(1024), 4);
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"previous").unwrap();
        let file = read_write(tmp.path());
        let hash = read_patch(&patch)
            .unwrap()
            .apply_to_file(old, &file)
            .unwrap();
        assert_eq!(hash, blake3::hash(b""));
        assert_eq!(fs::read(tmp.path()).unwrap(), b"");
    }
}

#[test]
fn replaces_longer_existing_content() {
    let (old, new, patch) = sample(100_000);
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&vec![0xEE; 3 << 20]).unwrap();
    let file = read_write(tmp.path());
    read_patch(&patch)
        .unwrap()
        .apply_to_file(&old, &file)
        .unwrap();
    assert!(fs::read(tmp.path()).unwrap() == new);
}

#[test]
fn wrong_base_leaves_the_file_untouched() {
    let (old, _, patch) = sample(100_000);
    let mut wrong = old.clone();
    wrong[10] ^= 1;
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(b"previous").unwrap();
    let file = read_write(tmp.path());
    assert!(matches!(
        read_patch(&patch).unwrap().apply_to_file(&wrong, &file),
        Err(Error::BaseMismatch { .. })
    ));
    assert_eq!(fs::read(tmp.path()).unwrap(), b"previous");
}

#[test]
fn wrong_result_is_refused_and_left_for_the_caller() {
    let (old, new, patch) = sample(100_000);
    let mut p = patch.clone();
    p[layout::NEW_HASH] ^= 1;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    match read_patch(&p).unwrap().apply_to_file(&old, tmp.as_file()) {
        Err(Error::ResultMismatch { actual, .. }) => assert_eq!(actual, blake3::hash(&new)),
        other => panic!("expected ResultMismatch, got {other:?}"),
    }
    // The bytes are there for the caller to delete.
    assert_eq!(fs::metadata(tmp.path()).unwrap().len(), new.len() as u64);
}

#[test]
fn a_file_opened_write_only_is_an_error() {
    let (old, _, patch) = sample(4096);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let write_only = File::create(tmp.path()).unwrap();
    assert!(matches!(
        read_patch(&patch).unwrap().apply_to_file(&old, &write_only),
        Err(Error::Io(_))
    ));
}
