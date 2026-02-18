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
        self.write_extended(c, None)
    }

    /// Write a control with optional COPY_OLD optimization.
    ///
    /// If `copy_old` is `Some(old_pos)`, the COPY region is encoded as a reference
    /// to `old[old_pos..old_pos+copy.len()]` instead of literal bytes.
    ///
    /// Wire format for copy_tag varint:
    /// - LSB = 0: literal copy, length = tag >> 1, followed by literal bytes
    /// - LSB = 1: copy-from-old, length = tag >> 1, followed by old_pos varint
    pub fn write_extended(
        &mut self,
        c: &Control,
        copy_old: Option<usize>,
    ) -> Result<(), io::Error> {
        let w = &mut self.w;

        let all_zero = c.add.iter().all(|&b| b == 0);
        if all_zero {
            w.write_varint(c.add.len() * 2 + 1)?; // LSB=1: zero-copy from old
        } else {
            w.write_varint(c.add.len() * 2)?; // LSB=0: normal ADD with delta
            w.write_all(c.add)?;
        }

        match copy_old {
            None => {
                w.write_varint(c.copy.len() * 2)?;
                w.write_all(c.copy)?;
            }
            Some(old_pos) => {
                w.write_varint(c.copy.len() * 2 + 1)?;
                w.write_varint(old_pos)?;
            }
        }

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
