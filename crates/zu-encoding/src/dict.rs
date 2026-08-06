//! Dictionary encoding (encoding id 3).
//!
//! Distinct values are sorted into a dictionary and rows become codes, both
//! stored in the frame-of-reference container. Sorted dictionaries keep
//! range predicates cheap and delta-compress well.
//! Layout: `dict_len: u32 LE`, dictionary container, codes container.

use zu_common::{Result, ZuError};

use crate::for_bitpack;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let mut dict: Vec<u64> = values.to_vec();
    dict.sort_unstable();
    dict.dedup();
    let codes: Vec<u64> = values
        .iter()
        .map(|v| dict.binary_search(v).unwrap() as u64)
        .collect();
    let mut dict_buf = Vec::new();
    for_bitpack::encode(&dict, &mut dict_buf);
    out.extend_from_slice(&(dict_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&dict_buf);
    for_bitpack::encode(&codes, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. The ceiling bounds both streams: a real dictionary never
/// holds more distinct values than the rows it codes.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "dict",
        detail: detail.to_string(),
    };
    let dict_len = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| corrupt("truncated header"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let dict_bytes = bytes
        .get(4..4 + dict_len)
        .ok_or_else(|| corrupt("truncated dictionary"))?;
    let codes_bytes = bytes
        .get(4 + dict_len..)
        .ok_or_else(|| corrupt("truncated codes"))?;
    let mut dict = Vec::new();
    for_bitpack::decode(dict_bytes, max_values, &mut dict)?;
    let start = out.len();
    for_bitpack::decode(codes_bytes, max_values, out)?;
    for slot in &mut out[start..] {
        let code = *slot as usize;
        *slot = *dict.get(code).ok_or_else(|| corrupt("code out of range"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_low_cardinality() {
        let cities = [90_001u64, 10_115, 75_000, 90_001];
        let values: Vec<u64> = (0..10_000).map(|i| cities[i % cities.len()]).collect();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert!(
            len < values.len(),
            "3 distinct values should pack below 1 B/row"
        );
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_all_distinct() {
        let values: Vec<u64> = (0..3000u64).rev().map(|i| i * 17).collect();
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn corrupt_code_rejected() {
        let mut buf = Vec::new();
        encode(&[5, 6, 5], &mut buf);
        let mut out = Vec::new();
        assert!(decode(&buf[..5], 16, &mut out).is_err());
    }
}
