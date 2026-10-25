//! Just enough of the zstd frame header (RFC 8878, section 3.1.1) to learn
//! the window a frame asks the decoder for, before it allocates one.

const ZSTD_MAGIC: u32 = 0xFD2F_B528;

/// The window size `frame` declares, computed as zstd's decoder does, or
/// `None` if `frame` does not begin with a complete zstd frame header.
pub(crate) fn window_size(frame: &[u8]) -> Option<u64> {
    let magic = u32::from_le_bytes(frame.get(..4)?.try_into().ok()?);
    if magic != ZSTD_MAGIC {
        return None;
    }
    let descriptor = *frame.get(4)?;
    let single_segment = descriptor & 0x20 != 0;
    if !single_segment {
        let window = *frame.get(5)?;
        let base = 1u64 << (10 + u32::from(window >> 3));
        return Some(base + base / 8 * u64::from(window & 7));
    }
    // A single-segment frame's window is its content size.
    let dictionary_id_len = [0, 1, 2, 4][usize::from(descriptor & 3)];
    let at = 5 + dictionary_id_len;
    Some(match descriptor >> 6 {
        0 => u64::from(*frame.get(at)?),
        1 => u64::from(u16::from_le_bytes(frame.get(at..at + 2)?.try_into().ok()?)) + 256,
        2 => u64::from(u32::from_le_bytes(frame.get(at..at + 4)?.try_into().ok()?)),
        _ => u64::from_le_bytes(frame.get(at..at + 8)?.try_into().ok()?),
    })
}

/// A frame whose header declares a window of `window_log` bits (plus
/// `mantissa` eighths), holding `payload` in one raw block.
#[cfg(test)]
pub(crate) fn raw_frame(window_log: u32, mantissa: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = ZSTD_MAGIC.to_le_bytes().to_vec();
    f.push(0);
    f.push(((window_log - 10) << 3) as u8 | mantissa);
    let block_header = (payload.len() as u32) << 3 | 1;
    f.extend_from_slice(&block_header.to_le_bytes()[..3]);
    f.extend_from_slice(payload);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether zstd's streaming decoder, limited to `2^log` bytes of window,
    /// decodes `frame`.
    fn zstd_accepts(frame: &[u8], log: u32) -> bool {
        use zstd::zstd_safe::{DCtx, DParameter, InBuffer, OutBuffer};
        let mut d = DCtx::create();
        d.set_parameter(DParameter::WindowLogMax(log)).unwrap();
        let mut input = InBuffer::around(frame);
        let mut buf = vec![0; 1 << 16];
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            match d.decompress_stream(&mut output, &mut input) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn matches_zstds_window_limit() {
        for len in [
            0usize,
            1,
            255,
            256,
            300,
            70_000,
            1 << 20,
            (1 << 20) + 1,
            3 << 20,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            for frame in [
                zstd::bulk::compress(&data, 3).unwrap(),
                zstd::stream::encode_all(&data[..], 3).unwrap(),
            ] {
                let window = window_size(&frame).unwrap().max(1 << 10);
                let log = 64 - (window - 1).leading_zeros();
                assert!(zstd_accepts(&frame, log), "len {len}: window {window}");
                if log > 10 {
                    assert!(!zstd_accepts(&frame, log - 1), "len {len}: window {window}");
                }
            }
        }
        for (log, mantissa) in [(20, 0), (20, 1), (21, 7)] {
            let frame = raw_frame(log, mantissa, b"payload");
            assert!(zstd_accepts(&frame, log + u32::from(mantissa > 0)));
            assert!(!zstd_accepts(&frame, log - u32::from(mantissa == 0)));
        }
    }

    #[test]
    fn reads_window_descriptors() {
        assert_eq!(window_size(&raw_frame(27, 0, b"x")), Some(1 << 27));
        assert_eq!(
            window_size(&raw_frame(20, 3, b"x")),
            Some((1 << 20) + 3 * (1 << 17))
        );
        assert_eq!(window_size(&raw_frame(10, 0, b"")), Some(1 << 10));
    }

    #[test]
    fn short_or_foreign_headers() {
        let frame = raw_frame(20, 0, b"x");
        for len in 0..6 {
            assert_eq!(window_size(&frame[..len]), None);
        }
        assert_eq!(window_size(b"\x50\x2a\x4d\x18\0\0\0\0"), None);
    }
}
