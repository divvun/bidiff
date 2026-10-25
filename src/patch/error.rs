use crate::{MAGIC, VERSION};
use blake3::Hash;
use std::{error::Error as StdError, fmt, io};

/// Why a patch could not be read or applied.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The data does not start with [`MAGIC`].
    WrongMagic(u32),
    /// The patch uses a format version other than [`VERSION`].
    UnsupportedVersion(u32),
    /// The patch is truncated, inconsistent, or its instructions go out of range.
    Malformed(Malformed),
    /// The base is not the file the patch was made from. Nothing was written.
    ///
    /// `actual_hash` is `None` when the size already differs and the base was
    /// not hashed.
    BaseMismatch {
        expected_size: u64,
        actual_size: u64,
        expected_hash: Hash,
        actual_hash: Option<Hash>,
    },
    /// The result does not have the hash the patch declares. When streaming,
    /// all of it has already been written and must be discarded.
    ResultMismatch { expected: Hash, actual: Hash },
    /// Writing the output failed, or the caller passed an output of the wrong size.
    Io(io::Error),
}

/// What is wrong with a malformed patch.
///
/// Chunk-level `offset`s count bytes of the chunk's decompressed instruction
/// stream; header-level ones count bytes of the patch itself.
#[derive(Debug)]
#[non_exhaustive]
pub enum Malformed {
    /// The patch ends inside its header or a chunk.
    Truncated { offset: usize },
    /// The header declares more chunks than the patch has room for.
    ChunkCount { declared: u32 },
    /// The chunk does not start where the previous one ended, or runs past `new_size`.
    ChunkLayout { chunk: usize },
    /// The chunks end before `new_size`.
    ChunkCoverage { covered: u64, new_size: u64 },
    /// Bytes follow the last chunk.
    TrailingData { offset: usize },
    /// The chunk's data is not exactly one valid zstd frame.
    Compression { chunk: usize, source: io::Error },
    /// The chunk's zstd frame asks for a larger window than the format
    /// allows, [`MAX_WINDOW_LOG`](super::MAX_WINDOW_LOG). Nothing was
    /// allocated for it.
    WindowTooLarge { chunk: usize, window: u64 },
    /// The instruction stream ends inside an instruction, or a varint is over-long.
    Instruction { chunk: usize, offset: u64 },
    /// An instruction reads or seeks outside the base.
    BaseRange { chunk: usize, offset: u64 },
    /// The instructions produce more bytes than the chunk declares.
    Overrun { chunk: usize, offset: u64 },
    /// The instructions produce fewer bytes than the chunk declares.
    Underrun {
        chunk: usize,
        expected: u64,
        actual: u64,
    },
    /// The instruction stream's length differs from the length the chunk declares.
    InstructionLength {
        chunk: usize,
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::WrongMagic(m) => write!(
                f,
                "not a bidiff patch: magic is {m:#X}, expected {MAGIC:#X}"
            ),
            Error::UnsupportedVersion(v) => write!(
                f,
                "unsupported patch format version {v:#X}; this library reads only {VERSION:#X}"
            ),
            Error::Malformed(m) => write!(f, "malformed patch: {m}"),
            Error::BaseMismatch {
                expected_size,
                actual_size,
                expected_hash,
                actual_hash: None,
            } => write!(
                f,
                "wrong base file: it is {actual_size} bytes, but the patch was made from \
                 {expected_size} bytes with BLAKE3 {expected_hash}"
            ),
            Error::BaseMismatch {
                expected_hash,
                actual_hash: Some(actual),
                ..
            } => write!(
                f,
                "wrong base file: its BLAKE3 is {actual}, but the patch was made from {expected_hash}"
            ),
            Error::ResultMismatch { expected, actual } => write!(
                f,
                "patched result has BLAKE3 {actual}, but the patch declares {expected}"
            ),
            Error::Io(_) => write!(f, "I/O error while applying patch"),
        }
    }
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Malformed::Truncated { offset } => write!(f, "truncated at byte {offset}"),
            Malformed::ChunkCount { declared } => {
                write!(
                    f,
                    "declares {declared} chunks, more than the patch can hold"
                )
            }
            Malformed::ChunkLayout { chunk } => write!(
                f,
                "chunk {chunk} does not start where the previous chunk ends or runs past the output"
            ),
            Malformed::ChunkCoverage { covered, new_size } => {
                write!(f, "chunks cover {covered} of {new_size} output bytes")
            }
            Malformed::TrailingData { offset } => {
                write!(f, "unexpected data after the last chunk at byte {offset}")
            }
            Malformed::Compression { chunk, source } => {
                write!(f, "chunk {chunk} has bad compressed data: {source}")
            }
            Malformed::WindowTooLarge { chunk, window } => write!(
                f,
                "chunk {chunk} needs a {window}-byte zstd window; the format allows {}",
                1u64 << super::MAX_WINDOW_LOG
            ),
            Malformed::Instruction { chunk, offset } => write!(
                f,
                "chunk {chunk} has a truncated or invalid instruction at offset {offset}"
            ),
            Malformed::BaseRange { chunk, offset } => write!(
                f,
                "chunk {chunk} instruction at offset {offset} goes outside the base file"
            ),
            Malformed::Overrun { chunk, offset } => write!(
                f,
                "chunk {chunk} instruction at offset {offset} writes past the chunk's output"
            ),
            Malformed::Underrun {
                chunk,
                expected,
                actual,
            } => write!(
                f,
                "chunk {chunk} produces {actual} bytes but declares {expected}"
            ),
            Malformed::InstructionLength {
                chunk,
                expected,
                actual,
            } => write!(
                f,
                "chunk {chunk} instruction stream is {actual} bytes but declares {expected}"
            ),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Error::Malformed(m) => Some(m),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl StdError for Malformed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Malformed::Compression { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<Malformed> for Error {
    fn from(m: Malformed) -> Self {
        Error::Malformed(m)
    }
}
