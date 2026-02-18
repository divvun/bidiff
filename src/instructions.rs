/// Generate a "newer" input from an "older" input and a set of instructions
pub fn apply_instructions(older: &[u8], instructions: &[u8]) -> Vec<u8> {
    use std::cmp::min;
    let mut newer: Vec<_> = older.iter().map(|x| *x).collect();

    for couple in instructions.chunks(2) {
        if couple.len() != 2 {
            break;
        }
        let (i, j) = (couple[0], couple[1]);

        if i < 128 {
            let pos = (i as usize) % newer.len();
            let len = j as usize;
            let data: Vec<u8> = (&newer[pos..min(pos + len, newer.len())])
                .iter()
                .map(|x| *x)
                .collect();
            for c in data {
                newer.push(c);
            }
        } else if i < 150 {
            for _ in 0..(i - 128) {
                newer.push(j);
            }
        } else {
            let a = (j as usize) % newer.len();
            let b = (a + 1) % newer.len();
            newer.swap(a, b);
        }
    }
    newer
}
