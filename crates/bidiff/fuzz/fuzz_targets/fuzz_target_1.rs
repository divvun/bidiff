#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }

    let (mid, data) = (data[0] as f64 / 255.0, &data[1..]);
    let mid = 0.5 + mid * 0.5;
    let mid = (mid * data.len() as f64) as usize;
    let (older, instr) = (&data[..mid], &data[mid..]);
    let newer = bidiff::instructions::apply_instructions(older, instr);
    bidiff::assert_cycle(older, &newer[..]);
});
