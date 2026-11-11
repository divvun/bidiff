/// Generate a "newer" input from an "older" input and a set of instructions
pub fn apply_instructions(older: &[u8], instructions: &[u8]) -> Vec<u8> {
    let mut newer = older.to_vec();

    for &[i, j] in instructions.as_chunks::<2>().0 {
        if i < 128 {
            // Repeat up to j bytes from a position in what is there so far.
            let Some(pos) = (i as usize).checked_rem(newer.len()) else {
                continue;
            };
            let end = newer.len().min(pos + j as usize);
            newer.extend_from_within(pos..end);
        } else if i < 150 {
            newer.extend(std::iter::repeat_n(j, (i - 128) as usize));
        } else {
            let Some(a) = (j as usize).checked_rem(newer.len()) else {
                continue;
            };
            let b = (a + 1) % newer.len();
            newer.swap(a, b);
        }
    }
    newer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_older() {
        assert_eq!(apply_instructions(b"", &[1, 4, 130, 7, 200, 0]), [7, 7]);
    }
}
