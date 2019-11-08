#![allow(unused)]
use anyhow::anyhow;
use async_std::{fs, future::try_join, prelude::*, task};
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

        let [older, newer, _patch] = {
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

        let mut translator = bidiff::Translator::new(
            &obuf[..],
            &nbuf[..],
            |control| -> Result<(), std::io::Error> {
                println!("control = {:?}", control);
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

        info!("Completed in {:?}", start.elapsed());

        Ok(())
    });
    res?;

    Ok(())
}
