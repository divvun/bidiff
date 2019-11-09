#![allow(unused)]
use anyhow::anyhow;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use integer_encoding::{VarIntReader, VarIntWriter};
use log::*;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    time::Instant,
};

struct Args {
    free: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    env_logger::builder().init();

    let args = pico_args::Arguments::from_env();
    let args = Args { free: args.free()? };

    let cmd = args.free[0].as_ref();
    match cmd {
        "diff" => diff(&args)?,
        "patch" => patch(&args)?,
        _ => unimplemented!(),
    }

    Ok(())
}

fn patch(args: &Args) -> anyhow::Result<()> {
    let [patch, older, output] = {
        let f = &args.free[1..];
        if f.len() != 3 {
            return Err(anyhow!("Usage: cbidiff OLDER NEWER PATCH"));
        }
        [&f[0], &f[1], &f[2]]
    };

    let start = Instant::now();

    let mut older = std::fs::File::open(older)?;
    let mut patch = std::fs::File::open(patch)?;
    let mut output = std::fs::File::create(output)?;

    let mut patch = brotli::Decompressor::new(patch, 64 * 1024);

    'read: loop {
        match read_control(&mut patch, &mut output, &mut older) {
            Err(e) => {
                match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => {
                        // all good!
                        break 'read;
                    }
                    _ => Err(e)?,
                }
            }
            _ => {}
        }
    }

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}

fn diff(args: &Args) -> anyhow::Result<()> {
    let [older, newer, patch] = {
        let f = &args.free[1..];
        if f.len() != 3 {
            return Err(anyhow!("Usage: cbidiff OLDER NEWER PATCH"));
        }
        [&f[0], &f[1], &f[2]]
    };

    let start = Instant::now();
    let mut older = fs::File::open(older)?;
    let mut newer = fs::File::open(newer)?;

    let mut obuf = Vec::new();
    let mut nbuf = Vec::new();

    older.read_to_end(&mut obuf)?;
    newer.read_to_end(&mut nbuf)?;

    let mut patch = std::fs::File::create(patch)?;
    let mut params = brotli::enc::BrotliEncoderInitParams();
    params.quality = 9;
    let mut patch = brotli::CompressorWriter::with_params(patch, 64 * 1024, &params);

    let mut translator = bidiff::Translator::new(
        &obuf[..],
        &nbuf[..],
        |control| -> Result<(), std::io::Error> {
            write_control(&mut patch, control)?;
            Ok(())
        },
    );

    bidiff::diff(&obuf[..], &nbuf[..], |m| -> Result<(), std::io::Error> {
        translator.translate(m)?;
        // eprintln!(
        //     "=> aos={} ans={} al={} ce={}",
        //     m.add_old_start, m.add_new_start, m.add_length, m.copy_end
        // );
        Ok(())
    })?;

    translator.close()?;
    patch.flush()?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}

fn write_control(mut w: &mut dyn Write, c: &bidiff::Control) -> Result<(), std::io::Error> {
    w.write_varint(c.add.len())?;
    w.write_all(c.add)?;

    w.write_varint(c.copy.len())?;
    w.write_all(c.copy)?;

    w.write_varint(c.seek)?;

    Ok(())
}

trait ReadSeek: Read + Seek {}

impl<T> ReadSeek for T where T: Read + Seek {}

fn read_control(
    mut patch: &mut dyn Read,
    mut output: &mut dyn Write,
    mut older: &mut dyn ReadSeek,
) -> Result<(), std::io::Error> {
    let add_len: usize = patch.read_varint()?;
    let mut add = vec![0u8; add_len];

    for i in 0..add_len {
        let a = patch.read_u8()?;
        let b = older.read_u8()?;
        let c = a.wrapping_add(b);
        output.write_all(&[c])?;
    }

    let copy_len: usize = patch.read_varint()?;
    for i in 0..copy_len {
        // this is slow, but should be correct
        let a = patch.read_u8()?;
        output.write_all(&[a])?;
    }

    let seek: i64 = patch.read_varint()?;
    older.seek(SeekFrom::Current(seek))?;

    Ok(())
}
