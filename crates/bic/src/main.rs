use failure::{err_msg, Fallible};
use log::*;
use size::Size;
use std::{
    fs::{self, File},
    io::{self},
    path::Path,
    time::Instant,
};

struct Args {
    free: Vec<String>,
    quality: i32,
    partitions: usize,
    chunk_size: Option<usize>,
}

impl Args {
    fn writer_params(&self) -> bidiff::enc::WriterParams {
        let mut params: bidiff::enc::WriterParams = Default::default();
        params.brotli_params.quality = self.quality;
        params
    }

    fn diff_params(&self) -> bidiff::DiffParams {
        bidiff::DiffParams {
            sort_partitions: self.partitions,
            scan_chunk_size: self.chunk_size,
        }
    }
}

fn main() -> Fallible<()> {
    #[cfg(debug_assertions)]
    std::env::set_var("RUST_BACKTRACE", "1");

    env_logger::builder().init();

    let mut args = pico_args::Arguments::from_env();
    let args = Args {
        quality: args.opt_value_from_str(["--quality", "-q"])?.unwrap_or(9),
        partitions: args.opt_value_from_str("--partitions")?.unwrap_or(1),
        chunk_size: args.opt_value_from_str("--chunk-size")?,
        free: args.free()?,
    };

    let cmd = args
        .free
        .get(0)
        .expect("Usage: bic diff|patch|cycle (1)")
        .as_ref();
    match cmd {
        "diff" => {
            let [older, newer, patch] = {
                let f = &args.free[1..];
                if f.len() != 3 {
                    return Err(err_msg("Usage: bic diff OLDER NEWER PATCH"));
                }
                [&f[0], &f[1], &f[2]]
            };
            do_diff(&args, older, newer, patch)?;
        }
        "patch" => {
            let [patch, older, output] = {
                let f = &args.free[1..];
                if f.len() != 3 {
                    return Err(err_msg("Usage: bic patch PATCH OLDER OUTPUT"));
                }
                [&f[0], &f[1], &f[2]]
            };
            do_patch(&args, patch, older, output)?;
        }
        "cycle" => {
            let [older, newer] = {
                let f = &args.free[1..];
                if f.len() != 2 {
                    return Err(err_msg("Usage: bic cycle OLDER NEWER"));
                }
                [&f[0], &f[1]]
            };
            do_cycle(&args, older, newer)?;
        }
        _ => return Err(err_msg("Usage: bic diff|patch|cycle")),
    }

    Ok(())
}

fn do_cycle<O, N>(args: &Args, older: O, newer: N) -> Fallible<()>
where
    O: AsRef<Path>,
    N: AsRef<Path>,
{
    println!("reading older and newer in memory...");
    let (older, newer) = (older.as_ref(), newer.as_ref());
    let (older, newer) = (fs::read(older)?, fs::read(newer)?);

    println!(
        "before {}, after {}",
        Size::Bytes(older.len()),
        Size::Bytes(newer.len()),
    );

    let mut patch = Vec::new();
    let before_diff = Instant::now();
    bidiff::simple_diff_with_params(
        &older[..],
        &newer[..],
        &mut patch,
        &args.diff_params(),
        &args.writer_params(),
    )?;
    println!("diffed in {:?}", before_diff.elapsed());

    let ratio = (patch.len() as f64) / (newer.len() as f64);
    println!(
        "patch size: {} ({:.2}% of newer)",
        Size::Bytes(patch.len()),
        ratio * 100.0
    );

    let mut fresh = Vec::new();
    {
        let before_patch = Instant::now();
        let mut older = io::Cursor::new(&older[..]);
        let mut r = bipatch::Reader::new(&patch[..], &mut older)?;
        let fresh_size = io::copy(&mut r, &mut fresh)?;
        println!("patched in {:?}", before_patch.elapsed());

        assert_eq!(fresh_size as usize, newer.len());
    }

    let newer_hash = hmac_sha256::Hash::hash(&newer[..]);
    let fresh_hash = hmac_sha256::Hash::hash(&fresh[..]);

    if newer_hash != fresh_hash {
        return Err(err_msg("hash mismatch!"));
    }

    Ok(())
}

fn do_patch<P, O, U>(_args: &Args, patch: P, older: O, output: U) -> Fallible<()>
where
    P: AsRef<Path>,
    O: AsRef<Path>,
    U: AsRef<Path>,
{
    let start = Instant::now();

    let older = File::open(older)?;
    let patch = File::open(patch)?;
    let mut reader = bipatch::Reader::new(patch, older)?;

    let mut output = File::create(output)?;
    io::copy(&mut reader, &mut output)?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}

fn do_diff<O, N, P>(args: &Args, older: O, newer: N, patch: P) -> Fallible<()>
where
    O: AsRef<Path>,
    N: AsRef<Path>,
    P: AsRef<Path>,
{
    let start = Instant::now();

    let older = fs::read(older)?;
    let newer = fs::read(newer)?;
    let mut patch = File::create(patch)?;

    bidiff::simple_diff_with_params(
        &older[..],
        &newer[..],
        &mut patch,
        &args.diff_params(),
        &args.writer_params(),
    )?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}
