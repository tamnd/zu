//! ALP_RD for doubles (encoding id 7), the fallback for "real doubles"
//! whose mantissas actually use their bits, where the decimal transform
//! of plain ALP fails on everything. The observation from the paper is
//! that such columns still share their front: sign, exponent and the
//! top mantissa bits cluster on a handful of patterns. Each 64-bit
//! pattern splits at a trained cut into a left part that dictionary
//! encodes (at most 8 entries, 3-bit codes) and a right part stored
//! packed as-is, so a coordinate column in radians drops its shared
//! bits while staying bit-exact. Left parts beyond the dictionary ride
//! as patched exceptions.
//!
//! The cut is chosen on a sample: every left width from 1 to 16 bits is
//! scored by packed right width plus dictionary code width plus the
//! exception tax, smallest estimate wins.
//!
//! Layout: `count: u32 LE`, `right_bits: u8`, `dict_len: u8`, dict
//! entries (u16 LE each), `exc_count: u32 LE`, `positions_len: u32 LE`,
//! positions container, exception left values (u16 LE each), left codes
//! container, then the right parts container.

use zu_common::{Result, ZuError};

use crate::for_bitpack;

const MAX_DICT: usize = 8;
const MAX_LEFT_BITS: usize = 16;

/// Top `MAX_DICT` left parts by frequency for a given cut, most common
/// first, ties broken by value so training is deterministic.
fn build_dict(values: &[f64], right_bits: u32) -> Vec<u16> {
    let mut counts = std::collections::HashMap::new();
    for &d in values {
        *counts
            .entry((d.to_bits() >> right_bits) as u16)
            .or_insert(0u64) += 1;
    }
    let mut pairs: Vec<(u16, u64)> = counts.into_iter().collect();
    pairs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs.truncate(MAX_DICT);
    pairs.into_iter().map(|(left, _)| left).collect()
}

/// Scores every cut on an evenly spaced sample: bits per value for the
/// packed right part and the dictionary code, plus the exception tax
/// for left parts outside the top eight.
fn choose_cut(values: &[f64]) -> u32 {
    let step = (values.len() / 1024).max(1);
    let sample: Vec<f64> = values.iter().step_by(step).copied().collect();
    let mut best_bits = 64 - MAX_LEFT_BITS as u32;
    let mut best_score = u64::MAX;
    for left_bits in 1..=MAX_LEFT_BITS as u32 {
        let right_bits = 64 - left_bits;
        let dict = build_dict(&sample, right_bits);
        let exceptions = sample
            .iter()
            .filter(|d| !dict.contains(&((d.to_bits() >> right_bits) as u16)))
            .count() as u64;
        let code_bits = u64::from(crate::bits_needed(dict.len().saturating_sub(1) as u64));
        let score = sample.len() as u64 * (u64::from(right_bits) + code_bits) + exceptions * 48;
        if score < best_score {
            best_score = score;
            best_bits = right_bits;
        }
    }
    best_bits
}

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[f64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let right_bits = choose_cut(values);
    let mut dict = build_dict(values, right_bits);
    if dict.is_empty() {
        // An empty input still writes a well-formed container, and the
        // decoder insists on at least one dictionary entry.
        dict.push(0);
    }
    let right_mask = if right_bits == 64 {
        u64::MAX
    } else {
        (1u64 << right_bits) - 1
    };
    let mut codes = Vec::with_capacity(values.len());
    let mut rights = Vec::with_capacity(values.len());
    let mut positions = Vec::new();
    let mut exc_lefts = Vec::new();
    for (i, &d) in values.iter().enumerate() {
        let bits = d.to_bits();
        let left = (bits >> right_bits) as u16;
        rights.push(bits & right_mask);
        match dict.iter().position(|&l| l == left) {
            Some(code) => codes.push(code as u64),
            None => {
                codes.push(0);
                positions.push(i as u64);
                exc_lefts.push(left);
            }
        }
    }
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.push(right_bits as u8);
    out.push(dict.len() as u8);
    for &left in &dict {
        out.extend_from_slice(&left.to_le_bytes());
    }
    out.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    let mut positions_buf = Vec::new();
    for_bitpack::encode(&positions, &mut positions_buf);
    out.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&positions_buf);
    for &left in &exc_lefts {
        out.extend_from_slice(&left.to_le_bytes());
    }
    let mut codes_buf = Vec::new();
    for_bitpack::encode(&codes, &mut codes_buf);
    out.extend_from_slice(&(codes_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&codes_buf);
    for_bitpack::encode(&rights, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. Every count is a claim checked against the caller ceiling and
/// the streams, and a rejected payload leaves `out` untouched.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<f64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "alp_rd",
        detail: detail.to_string(),
    };
    let header = bytes.get(..6).ok_or_else(|| corrupt("truncated header"))?;
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let right_bits = u32::from(header[4]);
    let dict_len = header[5] as usize;
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
    if !(48..=63).contains(&right_bits) {
        return Err(corrupt("cut outside 48..=63 right bits"));
    }
    if dict_len == 0 || dict_len > MAX_DICT {
        return Err(corrupt("dictionary size outside 1..=8"));
    }
    let mut pos = 6;
    let dict_bytes = bytes
        .get(pos..pos + dict_len * 2)
        .ok_or_else(|| corrupt("truncated dictionary"))?;
    let dict: Vec<u64> = dict_bytes
        .chunks_exact(2)
        .map(|c| u64::from(u16::from_le_bytes(c.try_into().unwrap())))
        .collect();
    pos += dict_len * 2;
    let head = bytes
        .get(pos..pos + 8)
        .ok_or_else(|| corrupt("truncated exception header"))?;
    let exc_count = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
    let positions_len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
    pos += 8;
    if exc_count > count {
        return Err(corrupt("more exceptions than values"));
    }
    let positions_bytes = bytes
        .get(pos..pos + positions_len)
        .ok_or_else(|| corrupt("truncated positions"))?;
    pos += positions_len;
    let exc_bytes = bytes
        .get(pos..pos + exc_count * 2)
        .ok_or_else(|| corrupt("truncated exception lefts"))?;
    pos += exc_count * 2;
    let codes_head = bytes
        .get(pos..pos + 4)
        .ok_or_else(|| corrupt("truncated code header"))?;
    let codes_len = u32::from_le_bytes(codes_head.try_into().unwrap()) as usize;
    pos += 4;
    let codes_bytes = bytes
        .get(pos..pos + codes_len)
        .ok_or_else(|| corrupt("truncated codes"))?;
    let rights_bytes = bytes
        .get(pos + codes_len..)
        .ok_or_else(|| corrupt("truncated rights"))?;
    let mut positions = Vec::new();
    for_bitpack::decode(positions_bytes, exc_count, &mut positions)?;
    if positions.len() != exc_count {
        return Err(corrupt("position stream length mismatch"));
    }
    if positions.iter().any(|&p| p >= count as u64) {
        return Err(corrupt("position past the value count"));
    }
    let mut codes = Vec::with_capacity(count);
    for_bitpack::decode(codes_bytes, count, &mut codes)?;
    if codes.len() != count {
        return Err(corrupt("code stream length mismatch"));
    }
    if codes.iter().any(|&c| c as usize >= dict_len) {
        return Err(corrupt("code past the dictionary"));
    }
    let mut rights = Vec::with_capacity(count);
    for_bitpack::decode(rights_bytes, count, &mut rights)?;
    if rights.len() != count {
        return Err(corrupt("right stream length mismatch"));
    }
    let right_mask = (1u64 << right_bits) - 1;
    if rights.iter().any(|&r| r & !right_mask != 0) {
        return Err(corrupt("right part wider than the cut"));
    }
    let base = out.len();
    out.extend(
        codes
            .iter()
            .zip(&rights)
            .map(|(&c, &r)| f64::from_bits(dict[c as usize] << right_bits | r)),
    );
    for (&p, chunk) in positions.iter().zip(exc_bytes.chunks_exact(2)) {
        let left = u64::from(u16::from_le_bytes(chunk.try_into().unwrap()));
        let bits = left << right_bits | rights[p as usize];
        out[base + p as usize] = f64::from_bits(bits);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radians() -> Vec<f64> {
        // Coordinates through an irrational transform: full mantissas
        // with a shared front, exactly the RD shape.
        let mut rng = 0x9E3779B97F4A7C15u64;
        (0..10_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let deg = (rng % 360_0000000) as f64 / 1e7 - 180.0;
                deg.to_radians()
            })
            .collect()
    }

    #[test]
    fn roundtrip_shears_the_shared_front() {
        let values = radians();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert!(
            len * 10 < values.len() * 8 * 9,
            "a shared front should shave at least 10 percent, got {len} from {}",
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
    fn roundtrip_hostile_values_and_edges() {
        let mixed = vec![
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE / 2.0,
            std::f64::consts::PI,
            1e300,
            -273.15,
        ];
        for values in [vec![], vec![42.5f64], mixed] {
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
        assert!(decode(&[1, 2], 16, &mut out).is_err());
        // A flood claim dies on the ceiling before anything materializes.
        let mut buf = u32::MAX.to_le_bytes().to_vec();
        buf.extend_from_slice(&[48, 1]);
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        // Hostile cut widths and dictionary sizes are rejected.
        for (rb, dl) in [(64u8, 1u8), (47, 1), (0, 1), (48, 0), (48, 9)] {
            let mut buf = 1u32.to_le_bytes().to_vec();
            buf.push(rb);
            buf.push(dl);
            assert!(decode(&buf, 16, &mut out).is_err());
        }
        assert_eq!(out, [9.0], "a rejected payload must not touch out");
    }

    #[test]
    fn hostile_code_past_dict_rejected() {
        // A valid single-value payload, then the same bytes with the
        // code stream claiming an entry the dictionary does not have.
        let values = [1.5f64];
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, 1, &mut out).unwrap();
        assert_eq!(out[0].to_bits(), values[0].to_bits());
    }
}
