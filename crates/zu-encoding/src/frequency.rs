//! Frequency encoding (encoding id 10), the BtrBlocks shape for columns
//! where one value dominates but the rest are wide and scattered, so
//! neither RLE (short runs) nor Dict (high exception cardinality) wins.
//!
//! The dominant value is stored once; everything else becomes an
//! exception patched back in by position. Both exception streams ride
//! the frame-of-reference container.
//! Layout: `count: u32 LE`, `top: u64 LE`, `positions_len: u32 LE`,
//! positions container, values container.

use zu_common::{Result, ZuError};

use crate::for_bitpack;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let mut counts = std::collections::HashMap::new();
    for &v in values {
        *counts.entry(v).or_insert(0usize) += 1;
    }
    let top = counts
        .into_iter()
        .max_by_key(|&(_, n)| n)
        .map_or(0, |(v, _)| v);
    let mut positions = Vec::new();
    let mut exceptions = Vec::new();
    for (i, &v) in values.iter().enumerate() {
        if v != top {
            positions.push(i as u64);
            exceptions.push(v);
        }
    }
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.extend_from_slice(&top.to_le_bytes());
    let mut positions_buf = Vec::new();
    for_bitpack::encode(&positions, &mut positions_buf);
    out.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&positions_buf);
    for_bitpack::encode(&exceptions, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. The count and both exception streams are claims until checked:
/// the count against the caller ceiling, the streams against the count,
/// and every position against the materialized range.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "frequency",
        detail: detail.to_string(),
    };
    let header = bytes.get(..16).ok_or_else(|| corrupt("truncated header"))?;
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let top = u64::from_le_bytes(header[4..12].try_into().unwrap());
    let positions_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
    let positions_bytes = bytes
        .get(16..16 + positions_len)
        .ok_or_else(|| corrupt("truncated positions"))?;
    let values_bytes = bytes
        .get(16 + positions_len..)
        .ok_or_else(|| corrupt("truncated exception values"))?;
    let mut positions = Vec::new();
    let mut exceptions = Vec::new();
    for_bitpack::decode(positions_bytes, count, &mut positions)?;
    for_bitpack::decode(values_bytes, count, &mut exceptions)?;
    if positions.len() != exceptions.len() {
        return Err(corrupt("stream length mismatch"));
    }
    if positions.iter().any(|&p| p >= count as u64) {
        return Err(corrupt("position past the value count"));
    }
    let base = out.len();
    out.extend(std::iter::repeat_n(top, count));
    for (&pos, &v) in positions.iter().zip(&exceptions) {
        out[base + pos as usize] = v;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_dominant_scattered() {
        let mut rng = 0x9E3779B97F4A7C15u64;
        let values: Vec<u64> = (0..10_000)
            .map(|i| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if i % 10 == 3 { rng } else { 777 }
            })
            .collect();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert!(
            len < values.len() * 2,
            "10% exceptions should encode near a tenth of raw, got {len}"
        );
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_edges() {
        for values in [vec![], vec![5u64], vec![1, 2, 3, 4], vec![0; 100]] {
            let mut buf = Vec::new();
            encode(&values, &mut buf);
            let mut out = Vec::new();
            decode(&buf, values.len(), &mut out).unwrap();
            assert_eq!(values, out);
        }
    }

    #[test]
    fn corrupt_and_hostile() {
        let mut out = Vec::new();
        assert!(decode(&[1, 2, 3], 16, &mut out).is_err());
        // A flood claim dies on the ceiling before anything materializes.
        let mut buf = u32::MAX.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0; 12]);
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn hostile_position_past_count_rejected() {
        // A valid-looking payload whose exception position lands outside
        // the materialized range must be a Corrupt error, not a write
        // into whatever the caller already had in `out`.
        let mut buf = 4u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&7u64.to_le_bytes());
        let mut positions_buf = Vec::new();
        for_bitpack::encode(&[3], &mut positions_buf);
        buf.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&positions_buf);
        for_bitpack::encode(&[99], &mut buf);
        let mut out = Vec::new();
        decode(&buf, 16, &mut out).unwrap();
        assert_eq!(out, [7, 7, 7, 99]);
        // Same payload, position 4 in a count of 4.
        let mut buf = 4u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&7u64.to_le_bytes());
        let mut positions_buf = Vec::new();
        for_bitpack::encode(&[4], &mut positions_buf);
        buf.extend_from_slice(&(positions_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&positions_buf);
        for_bitpack::encode(&[99], &mut buf);
        let mut out = vec![1, 2, 3];
        assert!(decode(&buf, 16, &mut out).is_err());
        assert_eq!(out, [1, 2, 3], "a rejected payload must not touch out");
    }
}
