use log::*;
use std::{
    cmp::min,
    io::{self, Write},
    time::Instant,
};

#[cfg(feature = "enc")]
pub mod enc;

#[derive(Debug)]
pub struct Match {
    pub add_old_start: usize,
    pub add_new_start: usize,
    pub add_length: usize,
    pub copy_end: usize,
}

impl Match {
    #[inline(always)]
    pub fn copy_start(&self) -> usize {
        self.add_new_start + self.add_length
    }
}

#[derive(Debug, Clone)]
pub struct Control<'a> {
    pub add: &'a [u8],
    pub copy: &'a [u8],
    pub seek: i64,
}

pub struct Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: std::error::Error,
{
    obuf: &'a [u8],
    nbuf: &'a [u8],
    prev_match: Option<Match>,
    buf: Vec<u8>,
    on_control: F,
    closed: bool,
}

impl<'a, F, E> Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: std::error::Error,
{
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], on_control: F) -> Self {
        Self {
            obuf,
            nbuf,
            buf: Vec::with_capacity(16 * 1024),
            prev_match: None,
            on_control,
            closed: false,
        }
    }

    fn send_control(&mut self, m: Option<&Match>) -> Result<(), E> {
        if let Some(pm) = self.prev_match.take() {
            (self.on_control)(&Control {
                add: &self.buf[..pm.add_length],
                copy: &self.nbuf[pm.copy_start()..pm.copy_end],
                seek: if let Some(m) = m {
                    m.add_old_start as i64 - (pm.add_old_start + pm.add_length) as i64
                } else {
                    0
                },
            })?;
        }
        Ok(())
    }

    pub fn translate(&mut self, m: Match) -> Result<(), E> {
        self.send_control(Some(&m))?;

        self.buf.clear();
        self.buf.reserve(m.add_length);
        for i in 0..m.add_length {
            self.buf
                .push(self.nbuf[m.add_new_start + i].wrapping_sub(self.obuf[m.add_old_start + i]));
        }

        self.prev_match = Some(m);
        Ok(())
    }

    pub fn close(mut self) -> Result<(), E> {
        self.do_close()
    }

    fn do_close(&mut self) -> Result<(), E> {
        if !self.closed {
            self.send_control(None)?;
            self.closed = true;
        }
        Ok(())
    }
}

impl<'a, F, E> Drop for Translator<'a, F, E>
where
    F: FnMut(&Control) -> Result<(), E>,
    E: std::error::Error,
{
    fn drop(&mut self) {
        // dropping a Translator ignores errors on purpose,
        // just like File does
        self.do_close().unwrap_or_else(|_| {});
    }
}

struct BsdiffIterator<'a> {
    scan: usize,
    pos: usize,
    length: usize,
    lastscan: usize,
    lastpos: usize,
    lastoffset: isize,

    obuf: &'a [u8],
    nbuf: &'a [u8],
    sa: &'a sacabase::SuffixArray<'a, i32>,
}

impl<'a> BsdiffIterator<'a> {
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], sa: &'a sacabase::SuffixArray<'a, i32>) -> Self {
        Self {
            scan: 0,
            pos: 0,
            length: 0,
            lastscan: 0,
            lastpos: 0,
            lastoffset: 0,
            obuf,
            nbuf,
            sa,
        }
    }
}

impl<'a> Iterator for BsdiffIterator<'a> {
    type Item = Match;
    fn next(&mut self) -> Option<Self::Item> {
        let obuflen = self.obuf.len();
        let nbuflen = self.nbuf.len();

        'outer: while self.scan < nbuflen {
            let mut oldscore = 0_usize;
            self.scan += self.length;

            let mut scsc = self.scan;
            'inner: while self.scan < nbuflen {
                let res = self.sa.longest_substring_match(&self.nbuf[self.scan..]);
                self.pos = res.start();
                self.length = res.len();

                {
                    while scsc < self.scan + self.length {
                        let oi = (scsc as isize + self.lastoffset) as usize;
                        if oi < obuflen && self.obuf[oi] == self.nbuf[scsc] {
                            oldscore += 1;
                        }
                        scsc += 1;
                    }
                }

                let significantly_better = self.length > oldscore + 8;
                let same_length = self.length == oldscore && self.length != 0;

                if same_length || significantly_better {
                    break 'inner;
                }

                {
                    let oi = (self.scan as isize + self.lastoffset) as usize;
                    if oi < obuflen && self.obuf[oi] == self.nbuf[self.scan] {
                        oldscore -= 1;
                    }
                }

                self.scan += 1;
            } // 'inner

            let done_scanning = self.scan == nbuflen;
            if self.length != oldscore || done_scanning {
                // length forward from lastscan
                let mut lenf = {
                    let (mut s, mut sf, mut lenf) = (0_isize, 0_isize, 0_isize);

                    for i in 0..min(self.scan - self.lastscan, obuflen - self.lastpos) {
                        if self.obuf[self.lastpos + i] == self.nbuf[self.lastscan + i] {
                            s += 1;
                        }

                        {
                            // the original code has an `i++` in the
                            // middle of what's essentially a while loop.
                            let i = i + 1;
                            if s * 2 - i as isize > sf * 2 - lenf {
                                sf = s;
                                lenf = i as isize;
                            }
                        }
                    }
                    lenf as usize
                };

                // length backwards from scan
                let mut lenb = {
                    let (mut s, mut sb, mut lenb) = (0_isize, 0_isize, 0_isize);

                    for i in 1..min(self.scan - self.lastscan + 1, self.pos + 1) {
                        if self.obuf[self.pos - i] == self.nbuf[self.scan - i] {
                            s += 1;
                        }

                        if (s * 2 - i as isize) > (sb * 2 - lenb) {
                            sb = s;
                            lenb = i as isize;
                        }
                    }
                    lenb as usize
                };

                let lastscan_was_better = self.lastscan + lenf > self.scan - lenb;
                if lastscan_was_better {
                    // if our last scan went forward more than
                    // our current scan went back, figure out how much
                    // of our current scan to crop based on scoring
                    let overlap = (self.lastscan + lenf) - (self.scan - lenb);

                    let lens = {
                        let (mut s, mut ss, mut lens) = (0, 0, 0);
                        for i in 0..overlap {
                            if self.nbuf[self.lastscan + lenf - overlap + i]
                                == self.obuf[self.lastpos + lenf - overlap + i]
                            {
                                // point to last scan
                                s += 1;
                            }
                            if self.nbuf[self.scan - lenb + i] == self.obuf[self.pos - lenb + i] {
                                // point to current scan
                                s -= 1;
                            }

                            // new high score for last scan?
                            if s > ss {
                                ss = s;
                                lens = i + 1;
                            }
                        }
                        lens
                    };

                    // order matters to avoid overflow
                    lenf += lens;
                    lenf -= overlap;

                    lenb -= lens;
                } // lastscan was better

                let res = Match {
                    add_old_start: self.lastpos,
                    add_new_start: self.lastscan,
                    add_length: lenf,
                    copy_end: self.scan - lenb,
                };

                if !done_scanning {
                    self.lastscan = self.scan - lenb;
                    self.lastpos = self.pos - lenb;
                    self.lastoffset = self.pos as isize - self.scan as isize;
                }

                return Some(res);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good

        None
    }
}

/// Diff two files
pub fn diff<F, E>(obuf: &[u8], nbuf: &[u8], mut on_match: F) -> Result<(), E>
where
    F: FnMut(Match) -> Result<(), E>,
{
    let obuflen = obuf.len();
    let nbuflen = nbuf.len();

    info!("building suffix array...");
    let before_suffix = Instant::now();
    let sa = divsufsort::sort(&obuf[..]);
    info!(
        "sorting took {}",
        DurationSpeed(obuf.len() as u64, before_suffix.elapsed())
    );

    {
        info!("trying parallel sort");
        use rayon::prelude::*;
        let before_parsuf = Instant::now();
        let sas: Vec<_> = obuf
            .par_chunks(obuf.len() / 4 + 1)
            .map(divsufsort::sort)
            .collect();
        info!(
            "had {} partitions, took {}",
            sas.len(),
            DurationSpeed(obuf.len() as u64, before_parsuf.elapsed())
        );
    }

    {
        info!("trying parallel scan");
        use rayon::prelude::*;
        let before_parscan = Instant::now();
        let matches: Vec<Vec<Match>> = nbuf
            .par_chunks(nbuf.len() / 12 + 1)
            .map(|nbuf| BsdiffIterator::new(obuf, nbuf, &sa).collect::<Vec<_>>())
            .collect();
        info!(
            "had {} partitions, took {}",
            matches.len(),
            DurationSpeed(obuf.len() as u64, before_parscan.elapsed())
        );
    }

    let before_scan = Instant::now();
    info!("scanning...");
    {
        let mut scan = 0_usize;
        let mut pos = 0_usize;
        let mut length = 0_usize;
        let mut lastscan = 0_usize;
        let mut lastpos = 0_usize;

        let mut lastoffset = 0_isize;

        'outer: while scan < nbuflen {
            let mut oldscore = 0_usize;
            scan += length;

            let mut scsc = scan;
            'inner: while scan < nbuflen {
                let res = sa.longest_substring_match(&nbuf[scan..]);
                pos = res.start();
                length = res.len();

                {
                    while scsc < scan + length {
                        let oi = (scsc as isize + lastoffset) as usize;
                        if oi < obuflen && obuf[oi] == nbuf[scsc] {
                            oldscore += 1;
                        }
                        scsc += 1;
                    }
                }

                let significantly_better = length > oldscore + 8;
                let same_length = length == oldscore && length != 0;

                if same_length || significantly_better {
                    break 'inner;
                }

                {
                    let oi = (scan as isize + lastoffset) as usize;
                    if oi < obuflen && obuf[oi] == nbuf[scan] {
                        oldscore -= 1;
                    }
                }

                scan += 1;
            } // 'inner

            let done_scanning = scan == nbuflen;
            if length != oldscore || done_scanning {
                // length forward from lastscan
                let mut lenf = {
                    let (mut s, mut sf, mut lenf) = (0_isize, 0_isize, 0_isize);

                    for i in 0..min(scan - lastscan, obuflen - lastpos) {
                        if obuf[lastpos + i] == nbuf[lastscan + i] {
                            s += 1;
                        }

                        {
                            // the original code has an `i++` in the
                            // middle of what's essentially a while loop.
                            let i = i + 1;
                            if s * 2 - i as isize > sf * 2 - lenf {
                                sf = s;
                                lenf = i as isize;
                            }
                        }
                    }
                    lenf as usize
                };

                // length backwards from scan
                let mut lenb = {
                    let (mut s, mut sb, mut lenb) = (0_isize, 0_isize, 0_isize);

                    for i in 1..min(scan - lastscan + 1, pos + 1) {
                        if obuf[pos - i] == nbuf[scan - i] {
                            s += 1;
                        }

                        if (s * 2 - i as isize) > (sb * 2 - lenb) {
                            sb = s;
                            lenb = i as isize;
                        }
                    }
                    lenb as usize
                };

                let lastscan_was_better = lastscan + lenf > scan - lenb;
                if lastscan_was_better {
                    // if our last scan went forward more than
                    // our current scan went back, figure out how much
                    // of our current scan to crop based on scoring
                    let overlap = (lastscan + lenf) - (scan - lenb);

                    let lens = {
                        let (mut s, mut ss, mut lens) = (0, 0, 0);
                        for i in 0..overlap {
                            if nbuf[lastscan + lenf - overlap + i]
                                == obuf[lastpos + lenf - overlap + i]
                            {
                                // point to last scan
                                s += 1;
                            }
                            if nbuf[scan - lenb + i] == obuf[pos - lenb + i] {
                                // point to current scan
                                s -= 1;
                            }

                            // new high score for last scan?
                            if s > ss {
                                ss = s;
                                lens = i + 1;
                            }
                        }
                        lens
                    };

                    // order matters to avoid overflow
                    lenf += lens;
                    lenf -= overlap;

                    lenb -= lens;
                } // lastscan was better

                on_match(Match {
                    add_old_start: lastpos,
                    add_new_start: lastscan,
                    add_length: lenf,
                    copy_end: scan - lenb,
                })?;

                if done_scanning {
                    break 'outer;
                }

                lastscan = scan - lenb;
                lastpos = pos - lenb;
                lastoffset = pos as isize - scan as isize;
            } // interesting score, or done scanning
        } // 'outer - done scanning for good
    }
    info!(
        "scanning took {}",
        DurationSpeed(nbuf.len() as u64, before_scan.elapsed())
    );

    Ok(())
}

use std::fmt;

struct DurationSpeed(u64, std::time::Duration);

impl fmt::Display for DurationSpeed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (size, duration) = (self.0, self.1);
        write!(f, "{:?} ({})", duration, Speed(size.into(), duration))
    }
}

struct Speed(u64, std::time::Duration);

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (size, duration) = (self.0, self.1);
        let per_sec = size as f64 / duration.as_secs_f64();
        write!(f, "{} / s", Size(per_sec as u64))
    }
}

struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let x = self.0;

        if x > 1024 * 1024 {
            write!(f, "{:.2} MiB", x as f64 / (1024.0 * 1024.0))
        } else if x > 1024 {
            write!(f, "{:.1} KiB", x as f64 / (1024.0))
        } else {
            write!(f, "{} B", x)
        }
    }
}

#[cfg(feature = "enc")]
pub fn simple_diff(older: &[u8], newer: &[u8], out: &mut dyn Write) -> Result<(), io::Error> {
    simple_diff_with_params(older, newer, out, &Default::default())
}

#[cfg(feature = "enc")]
pub fn simple_diff_with_params(
    older: &[u8],
    newer: &[u8],
    out: &mut dyn Write,
    params: &enc::WriterParams,
) -> Result<(), io::Error> {
    let mut w = enc::Writer::with_params(out, params)?;

    let mut translator = Translator::new(older, newer, |control| w.write(control));
    diff(older, newer, |m| translator.translate(m))?;
    translator.close()?;

    Ok(())
}
