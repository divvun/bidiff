use super::Control;
use brotli::enc::{backward_references::BrotliEncoderParams, writer::CompressorWriter};
use byteorder::{LittleEndian, WriteBytesExt};
use integer_encoding::VarIntWriter;
use std::io::{self, Write};

pub const MAGIC: u32 = 0xB1CC;
pub const VERSION: u32 = 0x1000;

pub struct Writer<W>
where
    W: Write,
{
    w: CompressorWriter<W>,
}

pub struct WriterParams {
    brotli_buffer_size: usize,
    brotli_params: BrotliEncoderParams,
}

impl Default for WriterParams {
    fn default() -> Self {
        let mut brotli_params = BrotliEncoderParams::default();
        brotli_params.quality = 9;
        brotli_params.lgwin = 22;

        Self {
            brotli_buffer_size: 4096,
            brotli_params,
        }
    }
}

impl<W> Writer<W>
where
    W: Write,
{
    pub fn new(w: W) -> Result<Self, io::Error> {
        Self::with_params(w, &Default::default())
    }

    pub fn with_params(mut w: W, params: &WriterParams) -> Result<Self, io::Error> {
        w.write_u32::<LittleEndian>(MAGIC)?;
        w.write_u32::<LittleEndian>(VERSION)?;

        let bw = CompressorWriter::with_params(w, params.brotli_buffer_size, &params.brotli_params);
        Ok(Self { w: bw })
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
        self.w.into_inner()
    }
}
