use super::Control;
use integer_encoding::VarIntWriter;
use std::io::{self, Write};

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
    pub fn new(w: W) -> Self {
        Self { w }
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

    /// Write a control whose add bytes are `new` minus `old`, computed as
    /// they are written rather than held, and whose copy bytes are the
    /// literal `copy`.
    pub fn write_diff(&mut self, new: &[u8], old: &[u8], copy: &[u8], seek: i64) -> io::Result<()> {
        if new.len() != old.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the new and old bytes of an add differ in length",
            ));
        }
        let w = &mut self.w;
        if new == old {
            w.write_varint(new.len() * 2 + 1)?; // LSB=1: zero-copy from old
        } else {
            w.write_varint(new.len() * 2)?; // LSB=0: normal ADD with delta
            let mut delta = [0u8; 4096];
            for (n, o) in new.chunks(delta.len()).zip(old.chunks(delta.len())) {
                let d = delta.get_mut(..n.len()).unwrap_or_default();
                for ((d, n), o) in d.iter_mut().zip(n).zip(o) {
                    *d = n.wrapping_sub(*o);
                }
                w.write_all(d)?;
            }
        }
        w.write_varint(copy.len() * 2)?;
        w.write_all(copy)?;
        w.write_varint(seek)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), io::Error> {
        self.w.flush()
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}
