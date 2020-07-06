use byteorder::{LittleEndian, ReadBytesExt};
use integer_encoding::VarIntReader;
use std::{
    cmp::min,
    error::Error as StdError,
    fmt,
    io::{self, ErrorKind, Read, Seek, SeekFrom},
};

pub const MAGIC: u32 = 0xB1DF;
pub const VERSION: u32 = 0x1000;

#[derive(Debug)]
pub enum DecodeError {
    IO(io::Error),
    WrongMagic(u32),
    WrongVersion(u32),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DecodeError::IO(_) => write!(f, "I/O error"),
            DecodeError::WrongMagic(e) => {
                write!(f, "wrong magic: expected `{:X}`, got `{:X}`", MAGIC, e)
            }
            DecodeError::WrongVersion(e) => {
                write!(f, "wrong version: expected `{:X}`, got `{:X}`", VERSION, e)
            }
        }
    }
}

impl StdError for DecodeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DecodeError::IO(e) => Some(e),
            DecodeError::WrongMagic { .. } => None,
            DecodeError::WrongVersion { .. } => None,
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(source: io::Error) -> Self {
        DecodeError::IO(source)
    }
}

pub struct Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    patch: R,
    old: RS,
    state: ReaderState,
    buf: Vec<u8>,
}

#[derive(Debug)]
enum ReaderState {
    Initial,
    Add(usize),
    Copy(usize),
    Final,
}

impl<R, RS> Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    pub fn new(mut patch: R, old: RS) -> Result<Self, DecodeError> {
        let magic = patch.read_u32::<LittleEndian>()?;
        if magic != MAGIC {
            return Err(DecodeError::WrongMagic(magic));
        }

        let version = patch.read_u32::<LittleEndian>()?;
        if version != VERSION {
            return Err(DecodeError::WrongMagic(version));
        }

        Ok(Self {
            patch,
            old,
            state: ReaderState::Initial,
            buf: vec![0u8; 4096],
        })
    }
}

impl<R, RS> Read for Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut read: usize = 0;

        while !buf.is_empty() {
            let processed = match self.state {
                ReaderState::Initial => match self.patch.read_varint() {
                    Ok(add_len) => {
                        self.state = ReaderState::Add(add_len);
                        0
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::UnexpectedEof => {
                            self.state = ReaderState::Final;
                            0
                        }
                        _ => {
                            return Err(e);
                        }
                    },
                },
                ReaderState::Add(add_len) => {
                    let n = min(min(add_len, buf.len()), self.buf.len());

                    let out = &mut buf[..n];
                    self.old.read_exact(out)?;

                    let dif = &mut self.buf[..n];
                    self.patch.read_exact(dif)?;

                    for i in 0..n {
                        out[i] = out[i].wrapping_add(dif[i]);
                    }

                    if add_len == n {
                        let copy_len: usize = self.patch.read_varint()?;
                        self.state = ReaderState::Copy(copy_len)
                    } else {
                        self.state = ReaderState::Add(add_len - n);
                    }

                    n
                }
                ReaderState::Copy(copy_len) => {
                    let n = min(copy_len, buf.len());

                    let out = &mut buf[..n];
                    self.patch.read_exact(out)?;

                    if copy_len == n {
                        let seek: i64 = self.patch.read_varint()?;
                        self.old.seek(SeekFrom::Current(seek))?;
                        self.state = ReaderState::Initial;
                    } else {
                        self.state = ReaderState::Copy(copy_len - n);
                    }

                    n
                }
                ReaderState::Final => {
                    break;
                }
            };
            read += processed;
            buf = &mut buf[processed..];
        }

        Ok(read)
    }
}
