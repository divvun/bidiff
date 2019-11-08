#![allow(unused)]
#![allow(nonstandard_style)]
use log::*;
use std::fmt;

const empty: usize = usize::max_value();

pub struct SuffixArray<'a> {
    text: &'a [u8],
    indices: Vec<usize>,
}

pub struct LongestCommonSubstring<'a> {
    text: &'a [u8],
    start: usize,
    len: usize,
}

impl<'a> fmt::Debug for LongestCommonSubstring<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LCS[{}..{}]", self.start, self.start + self.len)
    }
}

impl<'a> LongestCommonSubstring<'a> {
    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        &self.text[self.start..self.start + self.len]
    }

    #[inline(always)]
    pub fn start(&self) -> usize {
        self.start
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }
}

// cf. https://arxiv.org/pdf/1610.08305.pdf

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    S,
    L,
}

impl<'a> SuffixArray<'a> {
    pub fn new(text: &'a [u8]) -> Self {
        // transform &[u8] into &[u16] so we can have '0' as marker value.
        // TODO: get rid of marker value if possible
        let mut T = Vec::<u16>::new();
        for &c in text {
            T.push(1 + c as u16);
        }
        T.push(0);
        let n = T.len();

        // returns the suffix starting at `i` in `T`
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
        let mut SA = vec![empty; T.len()];

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

        Self { indices: SA, text }
    }

    pub fn check_valid(&self) {
        let suf = |i: usize| -> &[u8] { &self.text[i..] };

        for win in self.indices.windows(2) {
            if let &[i, j] = win {
                if !(suf(i) <= suf(j)) {
                    panic!("Sequence {:?} > {:?}", suf(i), suf(j));
                }
            } else {
                unreachable!()
            }
        }
    }

    pub fn search(&self, needle: &[u8]) -> LongestCommonSubstring {
        self.do_search(needle, 0, self.text.len())
    }

    fn do_search(&self, needle: &[u8], st: usize, en: usize) -> LongestCommonSubstring {
        let I = &self.indices[..];

        if en - st < 2 {
            let x = matchlen(&self.text[I[st]..], needle);
            let y = matchlen(&self.text[I[en]..], needle);

            if x > y {
                self.lcs(I[st], x)
            } else {
                self.lcs(I[en], y)
            }
        } else {
            let x = st + (en - st) / 2;

            let l = needle;
            let r = &self.text[I[x]..];
            if l > r {
                self.do_search(needle, x, en)
            } else {
                self.do_search(needle, st, x)
            }
        }
    }

    fn lcs(&self, start: usize, len: usize) -> LongestCommonSubstring {
        LongestCommonSubstring {
            text: self.text,
            start,
            len,
        }
    }
}

/// Returns the number of bytes common to a and b
pub fn matchlen(a: &[u8], b: &[u8]) -> usize {
    let l = std::cmp::min(a.len(), b.len());
    for i in 0..l {
        if a[i] != b[i] {
            return i;
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use crate::SuffixArray;
    use proptest::prelude::*;

    #[test]
    fn it_works() {
        let input: &[u8] = &[1, 0, 0, 2, 2, 0, 0, 2, 2, 0, 1, 0];
        let sa = SuffixArray::new(input);
        sa.check_valid();

        // all substrings should be found in input
        for i in 1..input.len() {
            let needle = &input[i..];
            let res = sa.search(needle);
            assert_eq!(res.len(), needle.len());
        }
    }

    proptest! {
        #[test]
        fn random_input(input_array: [u8;32]) {
            let input = &input_array[..];

            let sa = SuffixArray::new(input);
            sa.check_valid();

            // all substrings should be found in input
            for i in 1..input.len() {
                let needle = &input[i..];
                let res = sa.search(needle);
                assert_eq!(res.len(), needle.len());
            }
        }
    }
}
