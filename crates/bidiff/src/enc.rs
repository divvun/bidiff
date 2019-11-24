use super::Control;
use byteorder::{LittleEndian, WriteBytesExt};
use integer_encoding::VarIntWriter;
use std::io::{self, Write};

pub const MAGIC: u32 = 0xB1DF;
pub const VERSION: u32 = 0x1000;

pub struct Writer<W>
where
    W: Write,
{
    w: W,
}

impl<W> Writer<W>
where
    W: Write,
{
    pub fn new(mut w: W) -> Result<Self, io::Error> {
        w.write_u32::<LittleEndian>(MAGIC)?;
        w.write_u32::<LittleEndian>(VERSION)?;

        Ok(Self { w })
    }

    pub fn write(&mut self, c: &Control) -> Result<(), io::Error> {
        let w = &mut self.w;

        w.write_varint(c.add.len())?;
        w.write_all(c.add)?;

        w.write_varint(c.copy.len())?;
        w.write_all(c.copy)?;

        w.write_varint(c.seek)?;

        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), io::Error> {
        self.w.flush()
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}
