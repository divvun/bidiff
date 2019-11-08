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
    pub eoc: bool,
}

/// Diff two files
pub async fn diff<F>(
    mut older: Pin<&mut dyn Read>,
    mut newer: Pin<&mut dyn Read>,
    on_match: F,
) -> Result<(), async_std::io::Error>
where
    F: Fn(Match),
{
    let mut obuf = Vec::new();
    let mut nbuf = Vec::new();

    {
        let a = older.read_to_end(&mut obuf);
        let b = newer.read_to_end(&mut nbuf);
        try_join!(a, b).await?;
    }

    let obuflen = obuf.len();
    let nbuflen = nbuf.len();

    info!("building suffix array...");
    let sa = oipss::SuffixArray::new(&obuf[..]);

    info!("scanning...");
    {
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

                oldscore += {
                    let ostart = (scan as isize + lastoffset) as usize;
                    oipss::matchlen(&obuf[ostart..], &nbuf[scan..])
                };

                let significantly_better = length > oldscore + 8;
                let same_length = (length == oldscore && length != 0);

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
            println!(
                "length={} oldscore={} scan={} buflen={}",
                length, oldscore, scan, nbuflen
            );
            if length != oldscore || done_scanning {
                use std::cmp::min;

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
                    eoc: false,
                });

                if done_scanning {
                    break 'outer;
                }

                lastscan = scan - lenb;
                lastpos = pos - lenb;
                lastoffset = (pos as isize - scan as isize);
            } // interesting score, or done scanning
        } // 'outer - done scanning for good
    }

    on_match(Match {
        add_old_start: 0,
        add_new_start: 0,
        add_length: 0,
        copy_end: 0,
        eoc: true,
    });

    Ok(())
}
