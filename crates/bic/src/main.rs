use failure::{err_msg, Fallible};
use integer_encoding::VarIntReader;
use log::*;
use size::Size;
use std::{
    cmp::min,
    fs,
    io::{Read, Seek, SeekFrom, Write},
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
                    Err(err_msg("Usage: bic diff OLDER NEWER PATCH"))?;
                }
                [&f[0], &f[1], &f[2]]
            };
            do_diff(older, newer, patch)?;
        }
        "patch" => {
            let [patch, older, output] = {
                let f = &args.free[1..];
                if f.len() != 3 {
                    Err(err_msg("Usage: bic patch PATCH OLDER OUTPUT"))?;
                }
                [&f[0], &f[1], &f[2]]
            };
            do_patch(patch, older, output)?;
        }
        "cycle" => {
            let [older, newer] = {
                let f = &args.free[1..];
                if f.len() != 2 {
                    Err(err_msg("Usage: bic cycle OLDER NEWER"))?;
                }
                [&f[0], &f[1]]
            };
            do_cycle(older, newer)?;
        }
        _ => Err(err_msg("Usage: bic diff|patch|cycle"))?,
    }

    Ok(())
}

fn do_cycle<O, N>(older: O, newer: N) -> Fallible<()>
where
    O: AsRef<Path>,
    N: AsRef<Path>,
{
    let (older, newer) = (older.as_ref(), newer.as_ref());

    let tmp = std::env::temp_dir();
    let patch = tmp.join("patch");
    let fresh = tmp.join("fresh");

    {
        let older_size = std::fs::metadata(older)?.len();
        let newer_size = std::fs::metadata(newer)?.len();

        println!(
            "before {}, after {}",
            Size::Bytes(older_size),
            Size::Bytes(newer_size),
        );

        let older_hash = hmac_sha256::Hash::hash(&std::fs::read(older)?);

        do_diff(older, newer, &patch)?;
        do_patch(&patch, older, fresh)?;

        let patch_size = std::fs::metadata(patch)?.len();
        let ratio = (patch_size as f64) / (newer_size as f64);
        println!(
            "patch size: {} ({:.2}% of newer)",
            Size::Bytes(patch_size),
            ratio * 100.0
        );

        let fresh_hash = hmac_sha256::Hash::hash(&std::fs::read(older)?);

        if older_hash != fresh_hash {
            return Err(err_msg("hash mismatch!"));
        }
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

    let mut older = std::fs::File::open(older)?;
    let patch = std::fs::File::open(patch)?;
    let mut output = std::fs::File::create(output)?;

    let mut patch = brotli::Decompressor::new(patch, 64 * 1024);

    let mut buf1 = vec![0u8; 64 * 1024];
    let mut buf2 = vec![0u8; 64 * 1024];

    'read: loop {
        match read_control(&mut buf1, &mut buf2, &mut patch, &mut output, &mut older) {
            Err(e) => {
                match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => {
                        // all good!
                        break 'read;
                    }
                    _ => {
                        println!("in do_patch, got err {:?}", e);
                        Err(e)?;
                    }
                }
            }
            _ => {}
        }
    }

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
    let mut older = fs::File::open(older)?;
    let mut newer = fs::File::open(newer)?;

    let mut obuf = Vec::new();
    let mut nbuf = Vec::new();

    older.read_to_end(&mut obuf)?;
    newer.read_to_end(&mut nbuf)?;

    let patch = std::fs::File::create(patch)?;
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

trait ReadSeek: Read + Seek {}

impl<T> ReadSeek for T where T: Read + Seek {}

fn read_control(
    buf1: &mut [u8],
    buf2: &mut [u8],
    mut patch: &mut dyn Read,
    output: &mut dyn Write,
    older: &mut dyn ReadSeek,
) -> Result<(), std::io::Error> {
    let buflen = buf1.len();
    assert_eq!(buf1.len(), buf2.len());

    let add_len: usize = patch.read_varint()?;

    {
        let mut remain = add_len;
        while remain > 0 {
            let buf1 = &mut buf1[..min(buflen, remain)];
            let buf2 = &mut buf2[..buf1.len()];

            patch.read_exact(buf1)?;
            older.read_exact(buf2)?;
            for i in 0..buf1.len() {
                buf1[i] = buf1[i].wrapping_add(buf2[i]);
            }
            output.write_all(buf1)?;

            remain -= buf1.len();
        }
    }

    let copy_len: usize = patch.read_varint()?;
    {
        let mut remain = copy_len;
        while remain > 0 {
            let buf = &mut buf1[..min(buflen, remain)];
            patch.read_exact(buf)?;
            output.write_all(buf)?;
            remain -= buf.len();
        }
    }

    let seek: i64 = patch.read_varint()?;
    if seek != 0 {
        older.seek(SeekFrom::Current(seek))?;
    }

    Ok(())
}
