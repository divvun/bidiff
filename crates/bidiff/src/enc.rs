use super::Control;
use integer_encoding::VarIntWriter;
use std::io::{self, Write};

pub const MAGIC: u32 = 0xB1DF;
pub const VERSION: u32 = 0x2000;

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
    pub fn new(mut w: W, new_size: u64) -> Result<Self, io::Error> {
        w.write_all(&MAGIC.to_le_bytes())?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&new_size.to_le_bytes())?;

        Ok(Self { w })
    }

    /// Create a Writer that skips the header — for writing sub-patch Control streams.
    pub fn new_raw(w: W) -> Self {
        Self { w }
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
