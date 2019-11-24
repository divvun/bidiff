use log::*;
use rayon::prelude::*;
use sacabase::StringIndex;
use sacapart::PartitionedSuffixArray;
use std::{
    cmp::min,
    io::{self, Write},
    time::Instant,
};

#[cfg(feature = "enc")]
pub mod enc;

#[cfg(any(test, feature = "instructions"))]
pub mod instructions;

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
    sa: &'a dyn StringIndex<'a>,
}

impl<'a> BsdiffIterator<'a> {
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], sa: &'a dyn StringIndex<'a>) -> Self {
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
                self.pos = res.start;
                self.length = res.len;

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
                let mut lenb = if self.scan >= nbuflen {
                    0
                } else {
                    let (mut s, mut sb, mut lenb) = (0_isize, 0_isize, 0_isize);

                    for i in 1..=min(self.scan - self.lastscan, self.pos) {
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
                                // point goes to last scan
                                s += 1;
                            }
                            if self.nbuf[self.scan - lenb + i] == self.obuf[self.pos - lenb + i] {
                                // point goes to current scan
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

                let m = Match {
                    add_old_start: self.lastpos,
                    add_new_start: self.lastscan,
                    add_length: lenf,
                    copy_end: self.scan - lenb,
                };

                self.lastscan = self.scan - lenb;
                self.lastpos = self.pos - lenb;
                self.lastoffset = self.pos as isize - self.scan as isize;

                return Some(m);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good

        None
    }
}

pub struct DiffParams {
    // Number of partitions to use for suffix sorting.
    // Increase this number increases parallelism but produces slightly worse patches.
    pub sort_partitions: usize,

    // Size of chunks to use for scanning. When None, treat the
    // input as a single chunk. Smaller chunks increase parallelism but
    // produce slightly worse patches.
    pub scan_chunk_size: Option<usize>,
}

impl Default for DiffParams {
    fn default() -> Self {
        Self {
            sort_partitions: 1,
            scan_chunk_size: None,
        }
    }
}

/// Diff two files
pub fn diff<F, E>(obuf: &[u8], nbuf: &[u8], params: &DiffParams, mut on_match: F) -> Result<(), E>
where
    F: FnMut(Match) -> Result<(), E>,
{
    info!("building suffix array...");
    let before_suffix = Instant::now();
    let sa = PartitionedSuffixArray::new(&obuf[..], params.sort_partitions, divsufsort::sort);
    info!(
        "sorting took {}",
        DurationSpeed(obuf.len() as u64, before_suffix.elapsed())
    );

    let before_scan = Instant::now();
    if let Some(chunk_size) = params.scan_chunk_size {
        // +1 to make sure we don't have > num_partitions
        let num_chunks = (nbuf.len() + chunk_size - 1) / chunk_size;

        info!(
            "scanning with {}B chunks... ({} chunks total)",
            chunk_size, num_chunks
        );

        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..num_chunks {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<Match>>();
            txs.push(tx);
            rxs.push(rx);
        }

        nbuf.par_chunks(chunk_size).zip(txs).for_each(|(nbuf, tx)| {
            let iter = BsdiffIterator::new(obuf, nbuf, &sa);
            tx.send(iter.collect()).expect("should send results");
        });

        for (i, rx) in rxs.into_iter().enumerate() {
            let offset = i * chunk_size;
            let v = rx.recv().expect("should receive results");
            for mut m in v {
                // if m.add_length == 0 && m.copy_end == m.copy_start() {
                //     continue;
                // }

                m.add_new_start += offset;
                m.copy_end += offset;
                on_match(m)?;
            }
        }
    } else {
        for m in BsdiffIterator::new(obuf, nbuf, &sa) {
            on_match(m)?
        }
    }

    info!(
        "scanning took {}",
        DurationSpeed(obuf.len() as u64, before_scan.elapsed())
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
    diff_params: &DiffParams,
) -> Result<(), io::Error> {
    let mut w = enc::Writer::new(out)?;

    let mut translator = Translator::new(older, newer, |control| w.write(control));
    diff(older, newer, diff_params, |m| translator.translate(m))?;
    translator.close()?;

    Ok(())
}

pub fn assert_cycle(older: &[u8], newer: &[u8]) {
    let mut older_pos = 0_usize;
    let mut newer_pos = 0_usize;

    let mut translator = Translator::new(older, newer, |control| -> Result<(), std::io::Error> {
        for &ab in control.add {
            let fb = ab.wrapping_add(older[older_pos]);
            older_pos += 1;

            let nb = newer[newer_pos];
            newer_pos += 1;

            assert_eq!(fb, nb);
        }

        for &cb in control.copy {
            let nb = newer[newer_pos];
            newer_pos += 1;

            assert_eq!(cb, nb);
        }

        older_pos = (older_pos as i64 + control.seek) as usize;

        Ok(())
    });

    diff(older, newer, &Default::default(), |m| {
        translator.translate(m)
    })
    .unwrap();

    translator.close().unwrap();

    assert_eq!(
        newer_pos,
        newer.len(),
        "fresh should have same length as newer"
    );
}

#[cfg(test)]
mod tests {
    use super::instructions::apply_instructions;
    use proptest::prelude::*;

    #[test]
    fn short_patch() {
        let older = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            1, 2, 0,
        ];
        let instructions = [
            12, 16, 5, 40, 132, 1, 47, 43, 20, 86, 150, 0, 150, 0, 150, 0, 115, 31, 0, 0, 0, 0, 0,
            0, 0, 1, 38, 188, 128, 0, 150, 0,
        ];
        let newer = apply_instructions(&older[..], &instructions[..]);

        super::assert_cycle(&older[..], &newer[..]);
    }

    proptest! {
        #[test]
        fn cycle(older: [u8; 32], instructions: [u8; 32]) {
            let newer = apply_instructions(&older[..], &instructions[..]);
            println!("{} => {}", older.len(), newer.len());
            super::assert_cycle(&older[..], &newer[..]);
        }
    }
}
