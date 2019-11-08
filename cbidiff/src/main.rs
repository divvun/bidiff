#![allow(unused)]
use async_std::{fs, task};
use anyhow::anyhow;
use std::time::Instant;
use log::*;

struct Args {
    free: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    env_logger::builder().filter(None, LevelFilter::Info).init();
    
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
        // info!("Older file: {}", older);
        // info!("Newer file: {}", newer);
        let mut older = Box::pin(fs::File::open(older).await?);
        let mut newer = Box::pin(fs::File::open(newer).await?);

        bidiff::diff(older.as_mut(), newer.as_mut()).await?;
        // info!("Completed in {:?}", start.elapsed());

        Ok(())
    });
    res?;

    Ok(())
}
