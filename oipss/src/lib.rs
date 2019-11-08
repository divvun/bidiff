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
        let mut T = Vec::<u16>::new();
        for &c in input {
            T.push(1 + c as u16);
        }
        T.push(0);
        let n = T.len();

        let suf = |i: usize| -> &[u16] { &T[i..] };

        // let alphabet_size = 257;
        let alphabet_size = 6;
        let mut bucket_sizes = vec![0usize; alphabet_size];

        let mut Type = vec![Type::S; T.len()];
        for i in 0..n {
            bucket_sizes[T[i] as usize] += 1;

            Type[i] = if suf(i) < suf(i + 1) {
                Type::S
            } else {
                Type::L
            }
        }
        Type[n - 1] = Type::S; // T[n-1] is S-type by definition

        dbg!(&bucket_sizes);

        let mut bucket_lf = vec![0 as usize; alphabet_size];
        let mut bucket_rf = vec![0 as usize; alphabet_size];

        {
            let mut pos = 0usize;
            for character in 0..alphabet_size {
                bucket_lf[character] = std::cmp::min(n - 1, pos);
                bucket_rf[character] = pos + bucket_sizes[character] - 1;
                pos += bucket_sizes[character];
            }
        }
        dbg!(&bucket_lf);
        dbg!(&bucket_rf);

        let bucket_at = |i: usize| -> usize {
            let mut pos = 0usize;
            let mut bucket_number = 0;
            for bucket_size in &bucket_sizes {
                if pos + bucket_size > i {
                    return bucket_number;
                }
                bucket_number += 1;
                pos += bucket_size;
            }
            bucket_number
        };

        let mut SA = vec![n; T.len()];

        for i in 0..n {
            if Type[i] == Type::S {
                // insert at rf in relevant bucket
                let rf = bucket_rf[T[i] as usize];
                SA[rf] = i;

                if rf > 0 {
                    bucket_rf[T[i] as usize] -= 1;
                } else {
                    // well rf is gonna be 0 instead of -1 now,
                    // but that's the price of using usize I guess?
                }
            } else {
                // do not insert L-type suffixes yet
            }
        }

        for character in 0..alphabet_size {
            let l = bucket_rf[character] + 1;
            let r = if character == alphabet_size - 1 {
                SA.len()
            } else {
                bucket_lf[character + 1]
            };
            if l >= SA.len() {
                // empty bucket, ignore
                continue;
            }
            let s_type_suffixes = &mut SA[l..r];
            dbg!(("unsorted", character, &s_type_suffixes));

            s_type_suffixes.sort_by(|&a, &b| suf(a).cmp(suf(b)));
            dbg!(("sorted", character, &s_type_suffixes));
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

        println!("");

        Workspace {}
    }
}
