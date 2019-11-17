use failure::{err_msg, Fallible};
use log::*;
use size::Size;
use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::Instant,
};

struct Args {
    free: Vec<String>,
}

fn main() -> Fallible<()> {
    #[cfg(debug_assertions)]
    std::env::set_var("RUST_BACKTRACE", "1");

    env_logger::builder().init();

    let args = pico_args::Arguments::from_env();
    let args = Args { free: args.free()? };

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
            do_diff(older, newer, patch)?;
        }
        "patch" => {
            let [patch, older, output] = {
                let f = &args.free[1..];
                if f.len() != 3 {
                    return Err(err_msg("Usage: bic patch PATCH OLDER OUTPUT"));
                }
                [&f[0], &f[1], &f[2]]
            };
            do_patch(patch, older, output)?;
        }
        "cycle" => {
            let [older, newer] = {
                let f = &args.free[1..];
                if f.len() != 2 {
                    return Err(err_msg("Usage: bic cycle OLDER NEWER"));
                }
                [&f[0], &f[1]]
            };
            do_cycle(older, newer)?;
        }
        _ => return Err(err_msg("Usage: bic diff|patch|cycle")),
    }

    Ok(())
}

fn do_cycle<O, N>(older: O, newer: N) -> Fallible<()>
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
    bidiff::simple_diff(&older[..], &newer[..], &mut patch)?;
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

fn do_patch<P, O, U>(patch: P, older: O, output: U) -> Fallible<()>
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

fn do_diff<O, N, P>(older: O, newer: N, patch: P) -> Fallible<()>
where
    O: AsRef<Path>,
    N: AsRef<Path>,
    P: AsRef<Path>,
{
    let start = Instant::now();
    let mut older = File::open(older)?;
    let mut newer = File::open(newer)?;

    let mut obuf = Vec::new();
    let mut nbuf = Vec::new();

    older.read_to_end(&mut obuf)?;
    newer.read_to_end(&mut nbuf)?;

    let patch = File::create(patch)?;
    let mut patch = bidiff::enc::Writer::new(patch)?;

    let mut translator =
        bidiff::Translator::new(&obuf[..], &nbuf[..], |control| patch.write(control));

    bidiff::diff(&obuf[..], &nbuf[..], |m| -> Result<(), std::io::Error> {
        translator.translate(m)?;
        Ok(())
    })?;

    translator.close()?;
    patch.flush()?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}
