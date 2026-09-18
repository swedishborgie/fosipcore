//! G.711 μ-law / A-law → 16-bit linear PCM decoder (ITU-T G.711).
//!
//! The camera sends PCMU (μ-law, RTP payload type 0) at 8 kHz mono.
//! A-law (PT 8) is supported defensively in case a unit is configured
//! for it.
//!
//! The decode functions are a direct port of the canonical Sun
//! Microsystems `g711.c` (`ulaw2linear` / `alaw2linear`) —
//! <https://github.com/escrichov/G711/blob/master/g711.c> — which is
//! the reference implementation reproduced in countless audio stacks
//! (`ffmpeg`, `libsndfile`, `SoX`). The algorithm and its comments are kept
//! verbatim.

/// Sign bit for a G.711 byte.
const SIGN_BIT: u8 = 0x80;
/// Quantization field mask.
const QUANT_MASK: u8 = 0x0F;
/// Left shift for the A-law segment number.
const SEG_SHIFT: u32 = 4;
/// Segment field mask.
const SEG_MASK: u8 = 0x70;
/// Bias for the μ-law linear code.
const BIAS: i32 = 0x84;

/// Convert a μ-law byte to a 16-bit linear PCM sample.
///
/// Port of `ulaw2linear()` from Sun's `g711.c`.
///
/// First, a biased linear code is derived from the code word. An unbiased
/// output can then be obtained by subtracting 33 from the biased code.
///
/// Note that this function expects to be passed the complement of the
/// original code word. This is in keeping with ISDN conventions.
///
/// The output is guaranteed to fit `i16` (G.711 μ-law max is ±32124).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // G.711 output is bounded to i16 range
pub fn ulaw2linear(u_val: u8) -> i16 {
    /* Complement to obtain normal u-law value. */
    let u_val = !u_val;

    /*
     * Extract and bias the quantization bits. Then
     * shift up by the segment number and subtract out the bias.
     */
    let mut t = (i32::from(u_val & QUANT_MASK) << 3) + BIAS;
    t <<= (i32::from(u_val & SEG_MASK)) >> SEG_SHIFT;

    if u_val & SIGN_BIT != 0 {
        (BIAS - t) as i16
    } else {
        (t - BIAS) as i16
    }
}

/// Convert an A-law byte to a 16-bit linear PCM sample.
///
/// Port of `alaw2linear()` from Sun's `g711.c`.
///
/// The output is guaranteed to fit `i16` (A-law full scale is ±32256,
/// bounded inside the `i16` range).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // G.711 output is bounded to i16 range
pub fn alaw2linear(a_val: u8) -> i16 {
    let a_val = a_val ^ 0x55;

    let mut t = i32::from(a_val & QUANT_MASK) << 4;
    let seg = i32::from(a_val & SEG_MASK) >> SEG_SHIFT;
    if seg == 0 {
        t += 8;
    } else {
        t += 0x108;
        if seg > 1 {
            t <<= seg - 1;
        }
    }

    if a_val & SIGN_BIT != 0 {
        t as i16
    } else {
        (-t) as i16
    }
}

/// Decode a slice of μ-law bytes to 16-bit linear PCM.
///
/// # Errors
///
/// `Err(())` if `output` is shorter than `input`.
pub fn decode_mulaw(input: &[u8], output: &mut [i16]) -> Result<(), ()> {
    if output.len() < input.len() {
        return Err(());
    }
    for (out, &b) in output.iter_mut().zip(input.iter()) {
        *out = ulaw2linear(b);
    }
    Ok(())
}

/// Decode a slice of A-law bytes to 16-bit linear PCM.
///
/// # Errors
///
/// `Err(())` if `output` is shorter than `input`.
pub fn decode_alaw(input: &[u8], output: &mut [i16]) -> Result<(), ()> {
    if output.len() < input.len() {
        return Err(());
    }
    for (out, &b) in output.iter_mut().zip(input.iter()) {
        *out = alaw2linear(b);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_zeros() {
        // μ-law silence: 0x7F (positive zero) and 0xFF (negative zero).
        assert_eq!(ulaw2linear(0x7F), 0);
        assert_eq!(ulaw2linear(0xFF), 0);
    }

    #[test]
    fn ulaw_full_scale_range() {
        // Max negative (0x00) and max positive (0x80).
        assert_eq!(ulaw2linear(0x00), -32124);
        assert_eq!(ulaw2linear(0x80), 32124);
    }

    /// (byte, linear) rows taken from the reference `g711.c` (`ulaw2linear`),
    /// the exact source this module ports — an independent implementation, so
    /// a sign or segment bug in either direction is caught.
    const ULAW_VECTORS: &[(u8, i16)] = &[
        // silence / near-silence
        (0x7F, 0),
        (0xFF, 0),
        (0x7E, -8),
        (0xFE, 8),
        (0x7D, -16),
        (0xFD, 16),
        // full scale (clipping)
        (0x80, 32124),
        (0x00, -32124),
        (0x81, 31100),
        (0x01, -31100),
        // mid-scale, both signs, several segments
        (0x11, -15484),
        (0x91, 15484),
        (0x3F, -1980),
        (0xBF, 1980),
        (0x40, -1884),
        (0xC0, 1884),
        (0x08, -23932),
        (0x88, 23932),
    ];

    #[test]
    fn ulaw_matches_reference_table() {
        for (code, expected) in ULAW_VECTORS {
            assert_eq!(
                ulaw2linear(*code),
                *expected,
                "ulaw2linear(0x{code:02X})"
            );
        }
    }

    /// (byte, linear) rows from the reference `g711.c` (`alaw2linear`),
    /// spanning all eight A-law segments in both signs — the direction the
    /// earlier tests never exercised.
    const ALAW_VECTORS: &[(u8, i16)] = &[
        // segment 0 (full-scale clipping boundary)
        (0x00, -5504),
        (0x80, 5504),
        (0x01, -5248),
        (0x81, 5248),
        // segment 1
        (0x10, -2752),
        (0x90, 2752),
        // segment 2
        (0x20, -22016),
        (0xA0, 22016),
        // segment 3
        (0x30, -11008),
        (0xB0, 11008),
        // segment 4
        (0x40, -344),
        (0xC0, 344),
        // segment 5 (small signals around the A-law DC offset)
        (0x50, -88),
        (0xD0, 88),
        (0x55, -8),
        (0x54, -24),
        (0x56, -56),
        // segment 6
        (0x60, -1376),
        (0xE0, 1376),
        // segment 7
        (0x70, -688),
        (0xF0, 688),
    ];

    #[test]
    fn alaw_matches_reference_table() {
        for (code, expected) in ALAW_VECTORS {
            assert_eq!(
                alaw2linear(*code),
                *expected,
                "alaw2linear(0x{code:02X})"
            );
        }
    }

    #[test]
    fn decode_slices() {
        let input = [0x7F, 0x11, 0x91, 0x00, 0xFF];
        let mut out = [0i16; 16];
        decode_mulaw(&input, &mut out).unwrap();
        assert_eq!(out[0], 0);
        assert_eq!(out[4], 0);
        assert!(out[1] < 0); // 0x11: negative
        assert!(out[2] > 0); // 0x91: positive
        assert_eq!(out[3], -32124);
    }

    #[test]
    fn decode_alaw_success_and_odd_length() {
        // G.711 is one byte per sample, so odd/unaligned input lengths are
        // perfectly valid — decode the exact reference vectors through the
        // slice API (also covering the A-law decode loop's success path).
        let input: Vec<u8> = ALAW_VECTORS.iter().map(|(c, _)| *c).collect();
        let mut out = vec![0i16; input.len()];
        decode_alaw(&input, &mut out).unwrap();
        for (i, &(code, expected)) in ALAW_VECTORS.iter().enumerate() {
            assert_eq!(out[i], expected, "decode_alaw(0x{code:02X})");
        }

        // A genuinely odd (unaligned) length is fine too — 5 samples in, 5 out.
        let odd: &[u8] = &[0x55, 0x80, 0x20, 0xD0, 0x70];
        let mut odd_out = [0i16; 5];
        decode_alaw(odd, &mut odd_out).unwrap();
        assert_eq!(odd_out, [-8, 5504, -22016, 88, -688]);
    }

    #[test]
    fn decode_leaves_tail_untouched() {
        // Output longer than input: only the first input.len() slots are
        // written; the tail keeps its sentinel.
        let mut out = [777i16; 8];
        decode_mulaw(&[0x7F, 0x80], &mut out).unwrap();
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 32124);
        assert_eq!(&out[2..], &[777; 6]);
    }

    #[test]
    fn decode_output_too_short() {
        let mut out = [0i16; 1];
        assert!(decode_mulaw(&[0x7F, 0x7F], &mut out).is_err());
        assert!(decode_alaw(&[0x55, 0x55], &mut out).is_err());
    }
}
