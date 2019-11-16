use brotli::Decompressor;
use byteorder::{LittleEndian, ReadBytesExt};
use integer_encoding::VarIntReader;
use std::io::{self, Read, Seek};
use thiserror::Error;

pub const MAGIC: u32 = 0xB1CC;
pub const VERSION: u32 = 0x1000;
const BROTLI_BUFFER_SIZE: usize = 4096;

#[derive(Error, Debug)]
pub enum DecodeError {
    #[error("I/O error")]
    IO(#[from] io::Error),
    #[error("wrong magic: expected B1CC, got `{0:X}`")]
    WrongMagic(u32),
    #[error("wrong magic: expected 1000, got `{0:X}`")]
    WrongVersion(u32),
}

pub struct Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    r: Decompressor<R>,
    old: RS,
}

impl<R, RS> Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    pub fn new(mut patch: R, old: RS) -> Result<Self, DecodeError> {
        let magic = patch.read_u32::<LittleEndian>()?;
        if magic != MAGIC {
            Err(DecodeError::WrongMagic(magic))?;
        }

        let version = patch.read_u32::<LittleEndian>()?;
        if version != VERSION {
            Err(DecodeError::WrongMagic(version))?;
        }

        let r = Decompressor::new(patch, BROTLI_BUFFER_SIZE);
        Ok(Self { r, old })
    }
}

impl<R, RS> Read for Reader<R, RS>
where
    R: Read,
    RS: Read + Seek,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
