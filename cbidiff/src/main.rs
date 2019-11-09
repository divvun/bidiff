#![allow(unused)]
use anyhow::anyhow;
use async_std::{fs, future::try_join, prelude::*, task};
use byteorder::{LittleEndian, WriteBytesExt};
use integer_encoding::VarInt;
use log::*;
use std::time::Instant;

struct Args {
    free: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    env_logger::builder().init();

    let res: anyhow::Result<()> = task::block_on(async {
        let args = pico_args::Arguments::from_env();
        let args = Args { free: args.free()? };

        let [older, newer, patch] = {
            let f = &args.free[..];
            if f.len() != 3 {
                return Err(anyhow!("Usage: cbidiff OLDER NEWER PATCH"));
            }
            [&f[0], &f[1], &f[2]]
        };

        let start = Instant::now();
        let mut older = Box::pin(fs::File::open(older).await?);
        let mut newer = Box::pin(fs::File::open(newer).await?);

        let mut obuf = Vec::new();
        let mut nbuf = Vec::new();

        {
            let a = older.read_to_end(&mut obuf);
            let b = newer.read_to_end(&mut nbuf);
            try_join!(a, b).await?;
        }

        let mut patch = std::fs::File::create(patch)?;
        let mut patch = brotli::CompressorWriter::new(patch, 4096, 9, 19);

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
    });
    res?;

    Ok(())
}

use std::io::Write;

fn write_control(w: &mut dyn Write, c: &bidiff::Control) -> Result<(), std::io::Error> {
    let mut buf = [0u8; 8];

    let l = (c.add.len() as u64).encode_var(&mut buf[..]);
    w.write_all(&buf[..l]);
    w.write_all(c.add)?;

    let l = (c.copy.len() as u64).encode_var(&mut buf[..]);
    w.write_all(&buf[..l]);
    w.write_all(c.copy)?;

    let l = (c.seek as i64).encode_var(&mut buf[..]);
    w.write_all(&buf[..l]);

    Ok(())
}

