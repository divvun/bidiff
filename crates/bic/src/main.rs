use comde::{Compressor, Decompressor};
use crossbeam_utils::thread;
use failure::{err_msg, Fallible};
use log::*;
use size::Size;
use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::Path,
    str::FromStr,
    time::Instant,
};

/// Command-line arguments to bic
struct Args {
    free: Vec<String>,
    partitions: usize,
    method: Method,
    chunk_size: Option<usize>,
}

/// Compression method used
#[derive(Debug, Clone, Copy)]
pub enum Method {
    Stored,
    Deflate,
    Brotli,
    Snappy,
    Zstd,
}

impl Default for Method {
    fn default() -> Self {
        Self::Stored
    }
}

impl Method {
    fn compress<W: Write + Seek, R: Read>(
        &self,
        writer: &mut W,
        reader: &mut R,
    ) -> io::Result<comde::ByteCount> {
        match self {
            Self::Stored => comde::stored::StoredCompressor::new().compress(writer, reader),
            Self::Deflate => comde::deflate::DeflateCompressor::new().compress(writer, reader),
            Self::Brotli => comde::brotli::BrotliCompressor::new().compress(writer, reader),
            Self::Snappy => comde::snappy::SnappyCompressor::new().compress(writer, reader),
            Self::Zstd => comde::zstd::ZstdCompressor::new().compress(writer, reader),
        }
    }

    fn decompress<W: Write, R: Read>(&self, reader: R, writer: W) -> io::Result<u64> {
        match self {
            Self::Stored => comde::stored::StoredDecompressor::new().copy(reader, writer),
            Self::Deflate => comde::deflate::DeflateDecompressor::new().copy(reader, writer),
            Self::Brotli => comde::brotli::BrotliDecompressor::new().copy(reader, writer),
            Self::Snappy => comde::snappy::SnappyDecompressor::new().copy(reader, writer),
            Self::Zstd => comde::zstd::ZstdDecompressor::new().copy(reader, writer),
        }
    }
}

impl FromStr for Method {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stored" => Ok(Method::Stored),
            "deflate" => Ok(Method::Deflate),
            "brotli" => Ok(Method::Brotli),
            "snappy" => Ok(Method::Snappy),
            "zstd" => Ok(Method::Zstd),
            _ => Err(format!("Unknown compression method {}", s)),
        }
    }
}

impl Args {
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
        partitions: args.opt_value_from_str("--partitions")?.unwrap_or(1),
        chunk_size: args.opt_value_from_str("--chunk-size")?,
        method: args.opt_value_from_str("--method")?.unwrap_or_default(),
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
    info!("Reading older and newer in memory...");
    let (older, newer) = (older.as_ref(), newer.as_ref());
    let (older, newer) = (fs::read(older)?, fs::read(newer)?);

    info!(
        "Before {}, After {}",
        Size::Bytes(older.len()),
        Size::Bytes(newer.len()),
    );

    let mut compatch = Vec::new();
    let before_diff = Instant::now();

    {
        let mut compatch_w = io::Cursor::new(&mut compatch);

        let (mut patch_r, mut patch_w) = pipe::pipe();
        thread::scope(|s| {
            s.spawn(|_| {
                bidiff::simple_diff_with_params(
                    &older[..],
                    &newer[..],
                    &mut patch_w,
                    &args.diff_params(),
                )
                .unwrap();
                // this is important for `.compress()` to finish.
                // since we're using scoped threads, it's never dropped
                // otherwise.
                drop(patch_w);
            });
            args.method.compress(&mut compatch_w, &mut patch_r).unwrap();
        })
        .unwrap();
    }

    let diff_duration = before_diff.elapsed();

    let ratio = (compatch.len() as f64) / (newer.len() as f64);

    let mut fresh = Vec::new();
    let before_patch = Instant::now();
    {
        let mut older = io::Cursor::new(&older[..]);

        let method = args.method;
        let (patch_r, patch_w) = pipe::pipe();

        thread::scope(|s| {
            s.spawn(|_| {
                method.decompress(&compatch[..], patch_w).unwrap();
            });

            let mut r = bipatch::Reader::new(patch_r, &mut older).unwrap();
            let fresh_size = io::copy(&mut r, &mut fresh).unwrap();

            assert_eq!(fresh_size as usize, newer.len());
        })
        .unwrap();
    }
    let patch_duration = before_patch.elapsed();

    let newer_hash = hmac_sha256::Hash::hash(&newer[..]);
    let fresh_hash = hmac_sha256::Hash::hash(&fresh[..]);

    if newer_hash != fresh_hash {
        return Err(err_msg("Hash mismatch!"));
    }

    let cm = format!("{:?}", args.method);
    let cp = format!("patch {}", Size::Bytes(compatch.len()));
    let cr = format!("{:.3}x of {}", ratio, Size::Bytes(newer.len()));
    let cdd = format!("diffed in {:?}", diff_duration);
    let cpd = format!("patched in {:?}", patch_duration);
    println!("{:12} {:20} {:27} {:20} {:20}", cm, cp, cr, cdd, cpd);

    Ok(())
}

fn do_patch<P, O, U>(args: &Args, patch: P, older: O, output: U) -> Fallible<()>
where
    P: AsRef<Path>,
    O: AsRef<Path>,
    U: AsRef<Path>,
{
    println!("Using method {:?}", args.method);
    let start = Instant::now();

    let compatch_r = File::open(patch)?;
    let (patch_r, patch_w) = pipe::pipe();
    let method = args.method;

    std::thread::spawn(move || {
        method.decompress(compatch_r, patch_w).unwrap();
    });

    let older_r = File::open(older)?;
    let mut fresh_r = bipatch::Reader::new(patch_r, older_r)?;
    let mut output_w = File::create(output)?;
    io::copy(&mut fresh_r, &mut output_w)?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}

fn do_diff<O, N, P>(args: &Args, older: O, newer: N, patch: P) -> Fallible<()>
where
    O: AsRef<Path>,
    N: AsRef<Path>,
    P: AsRef<Path>,
{
    println!("Using method {:?}", args.method);
    let start = Instant::now();

    let older_contents = fs::read(older)?;
    let newer_contents = fs::read(newer)?;

    let (mut patch_r, mut patch_w) = pipe::pipe();
    let diff_params = args.diff_params();
    std::thread::spawn(move || {
        bidiff::simple_diff_with_params(
            &older_contents[..],
            &newer_contents[..],
            &mut patch_w,
            &diff_params,
        )
        .unwrap();
    });

    let mut compatch_w = File::create(patch)?;
    args.method.compress(&mut compatch_w, &mut patch_r)?;
    compatch_w.flush()?;

    info!("Completed in {:?}", start.elapsed());

    Ok(())
}
