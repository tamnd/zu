//! Delta plus bit packing (encoding id 5).
//!
//! Consecutive differences are zigzag mapped to u64 and fed through the
//! frame-of-reference container, so the layout is `for_bitpack` over
//! zigzag deltas with the first delta taken from zero. Sorted inputs such
//! as CSR neighbor lists produce small non-negative deltas, which is where
//! the bits-per-edge budget comes from; unsorted input stays correct, just
//! larger.

use zu_common::Result;

use crate::for_bitpack;

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let mut deltas = Vec::with_capacity(values.len());
    let mut prev = 0u64;
    for &v in values {
        deltas.push(zigzag(v.wrapping_sub(prev) as i64));
        prev = v;
    }
    for_bitpack::encode(&deltas, out)
}

/// Decodes an encoded buffer, appending the values to `out`.
pub fn decode(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let start = out.len();
    for_bitpack::decode(bytes, out)?;
    let mut prev = 0u64;
    for slot in &mut out[start..] {
        prev = prev.wrapping_add(unzigzag(*slot) as u64);
        *slot = prev;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sorted() {
        let values: Vec<u64> = (0..5000u64).map(|i| i * 7 + (i % 3)).collect();
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_unsorted_and_extremes() {
        let values = vec![u64::MAX, 0, 1, u64::MAX / 2, 3, 3, 2];
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn sorted_ids_compress_hard() {
        // 100k sorted ids with gaps around 20: about 6 bits each after
        // zigzag, so the payload must land well under 2 bytes per value.
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut v = 0u64;
        let values: Vec<u64> = (0..100_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v += rng % 40;
                v
            })
            .collect();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert!(len < values.len() * 2, "got {} bytes", len);
    }
}
