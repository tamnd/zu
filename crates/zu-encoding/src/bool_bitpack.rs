//! Boolean bitpacking (encoding id 9).
//!
//! Values must be 0 or 1; each becomes one bit, LSB first within a byte.
//! Layout: `count: u32 LE`, then `ceil(count / 8)` packed bytes. The
//! cascade only offers this encoding when the whole input is binary, so
//! the encoder asserts that instead of masking, which would break the
//! roundtrip silently.

use zu_common::{Result, ZuError};

/// Encodes `values` into `out`, returning the encoded byte length.
/// Every value must be 0 or 1.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for chunk in values.chunks(8) {
        let mut byte = 0u8;
        for (bit, &v) in chunk.iter().enumerate() {
            debug_assert!(v <= 1, "bool_bitpack fed a non-binary value");
            byte |= (v as u8) << bit;
        }
        out.push(byte);
    }
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. The count is a claim until checked against the ceiling.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "bool_bitpack",
        detail: detail.to_string(),
    };
    let count = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| corrupt("truncated count"))?
            .try_into()
            .unwrap(),
    ) as usize;
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
    let body = bytes
        .get(4..4 + count.div_ceil(8))
        .ok_or_else(|| corrupt("truncated body"))?;
    out.reserve(count);
    let full = count / 8;
    for &byte in &body[..full] {
        for bit in 0..8 {
            out.push(u64::from(byte >> bit & 1));
        }
    }
    for bit in 0..count % 8 {
        out.push(u64::from(body[full] >> bit & 1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let values: Vec<u64> = (0..10_000u64).map(|i| (i * i / 7) & 1).collect();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert_eq!(len, 4 + 1250);
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_ragged_tail() {
        for n in [0usize, 1, 7, 8, 9, 15, 17] {
            let values: Vec<u64> = (0..n as u64).map(|i| i & 1).collect();
            let mut buf = Vec::new();
            encode(&values, &mut buf);
            let mut out = Vec::new();
            decode(&buf, n, &mut out).unwrap();
            assert_eq!(values, out, "count {n}");
        }
    }

    #[test]
    fn corrupt_and_hostile() {
        let mut out = Vec::new();
        assert!(decode(&[1, 0], 16, &mut out).is_err());
        // A count past the body must read as truncation, not zeros.
        let buf = [100u32.to_le_bytes().as_slice(), &[0xFF; 3]].concat();
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        // A flood claim dies on the ceiling before the reserve.
        let buf = [u32::MAX.to_le_bytes().as_slice(), &[0xFF; 8]].concat();
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
        assert!(out.is_empty());
    }
}
