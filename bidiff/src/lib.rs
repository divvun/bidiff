#![allow(unused)]

use async_std::{future::try_join, io::Read, prelude::*};
use log::*;
use std::pin::Pin;

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
    add: &'a [u8],
    copy: &'a [u8],
    seek: isize,
}

pub struct Translator<'a, F, E>
where
    F: Fn(&Control) -> Result<(), E>,
    E: std::error::Error,
{
    obuf: &'a [u8],
    nbuf: &'a [u8],
    prev_match: Option<Match>,
    buf: Vec<u8>,
    on_control: F,
}

impl<'a, F, E> Translator<'a, F, E>
where
    F: Fn(&Control) -> Result<(), E>,
    E: std::error::Error,
{
    pub fn new(obuf: &'a [u8], nbuf: &'a [u8], on_control: F) -> Self {
        Self {
            obuf,
            nbuf,
            buf: Vec::with_capacity(16 * 1024),
            prev_match: None,
            on_control,
        }
    }

    fn send_control(&mut self, m: Option<&Match>) -> Result<(), E> {
        if let Some(pm) = self.prev_match.take() {
            (self.on_control)(&Control {
                add: &self.buf[..pm.add_length],
                copy: &self.nbuf[pm.copy_start()..pm.copy_end],
                seek: if let Some(m) = m {
                    m.add_old_start as isize - (pm.add_old_start + pm.add_length) as isize
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
                .push(self.nbuf[m.add_new_start + i] - self.obuf[m.add_old_start + i]);
        }

        self.prev_match = Some(m);
        Ok(())
    }

    pub fn close(mut self) -> Result<(), E> {
        self.send_control(None)?;
        Ok(())
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
    let sa = oipss::SuffixArray::new(&obuf[..]);

    info!("scanning...");
    {
        use std::cmp::min;

        let mut scan = 0_usize;
        let mut pos = 0_usize;
        let mut length = 0_usize;
        let mut lastscan = 0_usize;
        let mut lastpos = 0_usize;

        let mut lastoffset = 0_isize;

        'outer: while scan < nbuflen {
            scan += length;
            let mut oldscore = 0_usize;

            info!("scan = {}", scan);

            let mut scsc = scan;
            'inner: while scan < nbuflen {
                let res = sa.search(&nbuf[scan..]);
                pos = res.start();
                length = res.len();

                {
                    let bound1 = scan + length;
                    let bound2 = (obuflen as isize - lastoffset) as usize;

                    for scsc in scan..min(bound1, bound2) {
                        let oi = (scsc as isize + lastoffset) as usize;
                        if obuf[oi] == nbuf[scsc] {
                            oldscore += 1;
                        }
                    }
                }

                let significantly_better = length > oldscore + 8;
                let same_length = (length == oldscore && length != 0);

                if same_length || significantly_better {
                    println!("break");
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
            println!(
                "length={} oldscore={} scan={} buflen={}",
                length, oldscore, scan, nbuflen
            );
            if length != oldscore || done_scanning {
                for mut i in 0..10 {
                    i += 1;
                }

                // length forward from lastscan
                let mut lenf = {
                    let (mut s, mut sf, mut lenf) = (0, 0, 0);

                    for mut i in 0..min(scan - lastscan, obuflen - lastpos) {
                        if obuf[lastpos + i] == nbuf[lastscan + i] {
                            s += 1;
                        }

                        {
                            // the original Go code has an `i++` in the
                            // middle of what's essentially a while loop.
                            let i = i + 1;
                            if s * 2 - i > sf * 2 - lenf {
                                sf = s;
                                lenf = i;
                            }
                        }
                    }
                    lenf
                };
                println!("lenf={}", lenf);

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
                println!("lenb={}", lenb);

                let lastscan_was_better = lastscan + lenf > scan - lenb;
                if lastscan_was_better {
                    // if our last scan went forward more than
                    // our current scan went back, figure out how much
                    // of our current scan to crop based on scoring
                    let overlap = (lastscan + lenf) - (scan - lenb);
                    println!("overlap={}", overlap);

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

                    println!("lenf={}", lenf);
                    println!("lens={}", lens);
                    println!("overlap={}", overlap);
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
                lastoffset = (pos as isize - scan as isize);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good
    }

    Ok(())
}
