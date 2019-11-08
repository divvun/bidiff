#![allow(unused)]
#![allow(nonstandard_style)]
use log::*;

trait Offset {
    fn offset(&self, second: &Self) -> usize;
}

impl<'a> Offset for &'a [u16] {
    fn offset(&self, second: &Self) -> usize {
        let fst = self.as_ptr();
        let snd = second.as_ptr();

        (snd as usize - fst as usize) / 2
    }
}

pub struct Workspace {}

// cf. https://arxiv.org/pdf/1610.08305.pdf

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    S,
    L,
}

impl Workspace {
    pub fn new(input: &[u8]) -> Self {
        // transform &[u8] into &[u16] so we can have '0' as marker value.
        // TODO: get rid of marker value if possible
        let mut T = Vec::<u16>::new();
        for &c in input {
            T.push(1 + c as u16);
        }
        T.push(0);
        let n = T.len();

        // returns the suffix starting at `i` in `T`
        let Empty = n;
        let suf = |i: usize| -> &[u16] { &T[i..] };

        const alphabet_size: usize = 257;
        let mut bucket_sizes = [0usize; 257];

        // buckets contain all suffixes that start with a given character
        // (there are `alphabet_size` buckets in total)
        // compute buckets and determine whether sequences are S-type or L-type
        // in a single go.
        let mut Type = vec![Type::S; T.len()];
        for i in 0..n {
            bucket_sizes[T[i] as usize] += 1;

            Type[i] = if suf(i) < suf(i + 1) {
                Type::S
            } else {
                Type::L
            }
        }

        // note: T[n-1] is S-type by definition, but we let
        // the previous for loop iterate until `n-1` included,
        // so that `bucket_sizes` is filled properly.
        Type[n - 1] = Type::S;

        // leftmost-free position, per bucket
        let mut lf = vec![0 as usize; alphabet_size];
        // rightmost-free position, per bucket
        let mut rf = vec![0 as usize; alphabet_size];

        {
            let mut pos = 0usize;
            for character in 0..alphabet_size {
                lf[character] = std::cmp::min(n - 1, pos);
                rf[character] = pos + bucket_sizes[character] - 1;
                pos += bucket_sizes[character];
            }
        }

        // Convenience function (for debug) that returns
        // which bucket a given index of SA corresponds to;
        let bucket_at = |i: usize| -> usize {
            let mut pos = 0usize;
            let mut bucket_number = 0;
            for bucket_size in &bucket_sizes[..] {
                if pos + bucket_size > i {
                    return bucket_number;
                }
                bucket_number += 1;
                pos += bucket_size;
            }
            bucket_number
        };

        /// Suffix array
        let mut SA = vec![Empty; T.len()];

        // Insert unsorted S-suffixes at tail of their buckets
        for i in 0..n {
            if Type[i] == Type::S {
                // insert at rf in relevant bucket
                let pos = rf[T[i] as usize];
                SA[pos] = i;

                if pos > 0 {
                    rf[T[i] as usize] -= 1;
                } else {
                    // well rf is gonna be 0 instead of -1 now,
                    // but that's the price of using usize I guess?
                }
            } else {
                // do not insert L-type suffixes yet
            }
        }

        // Sort S-suffixes
        for character in 0..alphabet_size {
            let l = rf[character] + 1;
            let r = if character == alphabet_size - 1 {
                SA.len()
            } else {
                lf[character + 1]
            };
            if l >= SA.len() {
                // empty bucket, ignore
                continue;
            }
            let s_type_suffixes = &mut SA[l..r];
            s_type_suffixes.sort_by(|&a, &b| suf(a).cmp(suf(b)));
        }

        // Induced sorting all L-suffixes sorting from the sorted S-suffixes
        // Scan SA from left to right
        for i in 0..n {
            if (SA[i] == 0) {
                continue;
            }
            let j = SA[i] - 1;
            // If suf(j) is an L-suffix (indicated by the type array)
            if Type[j] == Type::L {
                let bucket = T[j] as usize;

                // we place the index of suf(j) (ie. j)
                // into the LF-entry of bucket T[j]
                SA[lf[bucket]] = j;
                lf[bucket] += 1; // move leftmost-free one to the right
            }
        }

        for win in SA.windows(2) {
            if let &[i, j] = win {
                if !(suf(i) <= suf(j)) {
                    panic!("Sequence {} > {}", i, j);
                }
            } else {
                unreachable!()
            }
        }

        println!(
            "{:^8} = {}",
            "Index",
            (0..n)
                .map(|x| format!("{:3}", x))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "{:^8} = {}",
            "T",
            T.iter()
                .map(|x| format!("{:3}", x))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "{:^8} = {}",
            "Type",
            Type.iter()
                .map(|x| format!("{:?}", x))
                .map(|x| format!("{:>3}", x))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "{:^8} = {}",
            "SA",
            SA.iter()
                .map(|&x| if x == n {
                    "  E".to_owned()
                } else {
                    format!("{:3}", x)
                })
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "{:^8} = {}",
            "Bucket",
            (0..n)
                .map(|x| format!("{:3}", bucket_at(x)))
                .collect::<Vec<_>>()
                .join(" ")
        );

        Workspace {}
    }
}

#[cfg(test)]
mod tests {
    use crate::Workspace;

    #[test]
    fn it_works() {
        let input: &[u8] = &[1, 0, 0, 2, 2, 0, 0, 2, 2, 0, 1, 0];
        Workspace::new(input);
    }
}
