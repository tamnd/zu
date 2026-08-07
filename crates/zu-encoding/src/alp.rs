//! ALP for doubles (encoding id 6), the Afroozeh and Boncz adaptive
//! lossless floating-point scheme: a double that is really a decimal,
//! like a price or a coordinate, becomes `round(d * 10^e / 10^f)`, an
//! integer that frame-of-reference packs to a fraction of eight bytes.
//! Values the transform cannot reproduce bit-exactly ride as patched
//! exceptions, so NaN, infinities, negative zero and genuine
//! full-mantissa doubles all survive.
//!
//! The exponent pair is chosen on a sample: every legal `(e, f)` combo
//! is scored by the packed width of the digits it produces plus the
//! exceptions it fails on, and the smallest estimate wins. Decode is
//! two multiplies per value after the digit stream unpacks, which is
//! what keeps it on the 1 GB/s floor. Decode streams the digit chunks
//! straight into the output, so scratch stays one MiniBlock chunk of
//! 1024 values no matter how large the container is.
//!
//! Layout: `count: u32 LE`, `e: u8`, `f: u8`, `exc_count: u32 LE`,
//! `positions_len: u32 LE`, positions container, exception bit patterns
//! (8 bytes LE each), then the zigzagged digits container.

use zu_common::{Result, ZuError};

use crate::for_bitpack;

/// 10^0 through 10^18, the largest power exact in an f64 kept small
/// enough that digit magnitudes stay far from the 2^53 integer edge.
const MAX_EXPONENT: usize = 18;

fn pow10() -> [f64; MAX_EXPONENT + 1] {
    let mut p = [1.0; MAX_EXPONENT + 1];
    let mut i = 1;
    while i <= MAX_EXPONENT {
        p[i] = p[i - 1] * 10.0;
        i += 1;
    }
    p
}

fn inv_pow10() -> [f64; MAX_EXPONENT + 1] {
    let mut p = [1.0; MAX_EXPONENT + 1];
    let mut i = 1;
    while i <= MAX_EXPONENT {
        p[i] = 10.0_f64.powi(-(i as i32));
        i += 1;
    }
    p
}

/// Digits stay well inside the exactly-representable integer range so
/// the verification multiply cannot lose bits to rounding.
const MAX_DIGITS: f64 = (1u64 << 51) as f64;

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// The transform for one value under `(e, f)`: the digits if the value
/// decodes back bit-exactly, otherwise None.
#[inline]
fn digits_for(d: f64, e: usize, f: usize, pow: &[f64], inv: &[f64]) -> Option<i64> {
    let scaled = d * pow[e] * inv[f];
    if !(-MAX_DIGITS..=MAX_DIGITS).contains(&scaled) {
        return None;
    }
    let digits = scaled.round() as i64;
    let decoded = (digits as f64) * pow[f] * inv[e];
    (decoded.to_bits() == d.to_bits()).then_some(digits)
}

/// Scores every `(e, f)` pair on an evenly spaced sample and returns
/// the winner: estimated packed bits per value plus the exception tax.
fn choose_exponents(values: &[f64], pow: &[f64], inv: &[f64]) -> (usize, usize) {
    let step = (values.len() / 1024).max(1);
    let sample: Vec<f64> = values.iter().step_by(step).copied().collect();
    let mut best = (0usize, 0usize);
    let mut best_score = u64::MAX;
    for e in 0..=MAX_EXPONENT {
        for f in 0..=e {
            let mut lo = i64::MAX;
            let mut hi = i64::MIN;
            let mut exceptions = 0u64;
            for &d in &sample {
                match digits_for(d, e, f, pow, inv) {
                    Some(digits) => {
                        lo = lo.min(digits);
                        hi = hi.max(digits);
                    }
                    None => exceptions += 1,
                }
            }
            let width = if lo > hi {
                64
            } else {
                u64::from(crate::bits_needed((hi as i128 - lo as i128) as u64))
            };
            let score = sample.len() as u64 * width + exceptions * 96;
            if score < best_score {
                best_score = score;
                best = (e, f);
            }
        }
    }
    best
}

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[f64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let pow = pow10();
    let inv = inv_pow10();
    let (e, f) = choose_exponents(values, &pow, &inv);
    let mut digits = Vec::with_capacity(values.len());
    let mut positions = Vec::new();
    let mut patterns = Vec::new();
    for (i, &d) in values.iter().enumerate() {
        match digits_for(d, e, f, &pow, &inv) {
            Some(dig) => digits.push(zigzag(dig)),
            None => {
                // Exceptions still occupy a slot in the digit stream so
                // positions stay aligned; zero packs for free.
                digits.push(0);
                positions.push(i as u64);
                patterns.push(d.to_bits());
            }
        }
    }
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.push(e as u8);
    out.push(f as u8);
    out.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    let mut positions_buf = Vec::new();
    for_bitpack::encode(&positions, &mut positions_buf);
    out.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&positions_buf);
    for &p in &patterns {
        out.extend_from_slice(&p.to_le_bytes());
    }
    for_bitpack::encode(&digits, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. Every count is a claim checked against the caller ceiling and
/// the streams, and a rejected payload leaves `out` untouched.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<f64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "alp",
        detail: detail.to_string(),
    };
    let header = bytes.get(..14).ok_or_else(|| corrupt("truncated header"))?;
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let e = header[4] as usize;
    let f = header[5] as usize;
    let exc_count = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let positions_len = u32::from_le_bytes(header[10..14].try_into().unwrap()) as usize;
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
    if e > MAX_EXPONENT || f > e {
        return Err(corrupt("exponent pair outside the table"));
    }
    if exc_count > count {
        return Err(corrupt("more exceptions than values"));
    }
    let positions_bytes = bytes
        .get(14..14 + positions_len)
        .ok_or_else(|| corrupt("truncated positions"))?;
    let patterns_end = 14 + positions_len + exc_count * 8;
    let patterns_bytes = bytes
        .get(14 + positions_len..patterns_end)
        .ok_or_else(|| corrupt("truncated exception patterns"))?;
    let digits_bytes = bytes
        .get(patterns_end..)
        .ok_or_else(|| corrupt("truncated digits"))?;
    let mut positions = Vec::new();
    for_bitpack::decode(positions_bytes, exc_count, &mut positions)?;
    if positions.len() != exc_count {
        return Err(corrupt("position stream length mismatch"));
    }
    if positions.iter().any(|&p| p >= count as u64) {
        return Err(corrupt("position past the value count"));
    }
    let pow = pow10();
    let inv = inv_pow10();
    let scale_f = pow[f];
    let scale_e = inv[e];
    let base = out.len();
    let decoded = for_bitpack::decode_chunks(digits_bytes, count, |min, scratch, take| {
        out.extend(
            scratch[..take]
                .iter()
                .map(|&v| (unzigzag(min.wrapping_add(v)) as f64) * scale_f * scale_e),
        );
    });
    match decoded {
        Ok(n) if n == count => {}
        Ok(_) => {
            out.truncate(base);
            return Err(corrupt("digit stream length mismatch"));
        }
        Err(e) => {
            out.truncate(base);
            return Err(e);
        }
    }
    for (&pos, chunk) in positions.iter().zip(patterns_bytes.chunks_exact(8)) {
        out[base + pos as usize] = f64::from_bits(u64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords() -> Vec<f64> {
        // Five-decimal-digit coordinates, the shape ALP is for. Seven
        // digits over the full degree range packs to 33 bits and sits
        // exactly at the 2x boundary; the bench on real check-ins is
        // where that case is measured.
        let mut rng = 0x9E3779B97F4A7C15u64;
        (0..10_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let hundred_micro = (rng % 36_000_000) as i64 - 9_000_000;
                hundred_micro as f64 / 1e5
            })
            .collect()
    }

    #[test]
    fn roundtrip_compresses_decimals() {
        let values = coords();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert!(
            len * 2 < values.len() * 8,
            "seven-digit decimals should at least halve, got {len} from {}",
            values.len() * 8
        );
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(
            values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            out.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn roundtrip_hostile_values_ride_as_exceptions() {
        let values = [
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE / 2.0,
            std::f64::consts::PI,
            1e300,
            -273.15,
            2.5,
        ];
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(
            values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            out.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn roundtrip_edges() {
        for values in [vec![], vec![42.5f64], vec![-0.001; 300]] {
            let mut buf = Vec::new();
            encode(&values, &mut buf);
            let mut out = Vec::new();
            decode(&buf, values.len(), &mut out).unwrap();
            assert_eq!(
                values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                out.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn corrupt_and_hostile() {
        let mut out = vec![9.0f64];
        assert!(decode(&[1, 2, 3], 16, &mut out).is_err());
        // A flood claim dies on the ceiling before anything materializes.
        let mut buf = u32::MAX.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 10]);
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        // Exponent pairs outside the table are rejected.
        for (e, f) in [(19u8, 0u8), (5, 6)] {
            let mut buf = 1u32.to_le_bytes().to_vec();
            buf.push(e);
            buf.push(f);
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            assert!(decode(&buf, 16, &mut out).is_err());
        }
        // An exception position past the count is rejected.
        let mut buf = 1u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&1u32.to_le_bytes());
        let mut positions_buf = Vec::new();
        for_bitpack::encode(&[5], &mut positions_buf);
        buf.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&positions_buf);
        buf.extend_from_slice(&7.5f64.to_bits().to_le_bytes());
        for_bitpack::encode(&[0], &mut buf);
        assert!(decode(&buf, 16, &mut out).is_err());
        assert_eq!(out, [9.0], "a rejected payload must not touch out");
    }
}
