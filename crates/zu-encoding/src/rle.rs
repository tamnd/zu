//! Run-length encoding (encoding id 2).
//!
//! Runs are split into two streams, values and run lengths, each stored in
//! the frame-of-reference container so both compress with the same machinery
//! chunk-granular readers already understand.
//! Layout: `run_count: u32 LE`, `values_len: u32 LE`, values container,
//! lengths container.

use zu_common::{Result, ZuError};

use crate::for_bitpack;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let mut run_values = Vec::new();
    let mut run_lengths = Vec::new();
    let mut iter = values.iter().copied();
    if let Some(first) = iter.next() {
        let mut cur = first;
        let mut len = 1u64;
        for v in iter {
            if v == cur {
                len += 1;
            } else {
                run_values.push(cur);
                run_lengths.push(len);
                cur = v;
                len = 1;
            }
        }
        run_values.push(cur);
        run_lengths.push(len);
    }
    out.extend_from_slice(&(run_values.len() as u32).to_le_bytes());
    let mut values_buf = Vec::new();
    for_bitpack::encode(&run_values, &mut values_buf);
    out.extend_from_slice(&(values_buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&values_buf);
    for_bitpack::encode(&run_lengths, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending the values to `out`.
pub fn decode(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "rle",
        detail: detail.to_string(),
    };
    let header = bytes.get(..8).ok_or_else(|| corrupt("truncated header"))?;
    let run_count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let values_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let values_bytes = bytes
        .get(8..8 + values_len)
        .ok_or_else(|| corrupt("truncated values"))?;
    let lengths_bytes = bytes
        .get(8 + values_len..)
        .ok_or_else(|| corrupt("truncated lengths"))?;
    let mut run_values = Vec::new();
    let mut run_lengths = Vec::new();
    for_bitpack::decode(values_bytes, &mut run_values)?;
    for_bitpack::decode(lengths_bytes, &mut run_lengths)?;
    if run_values.len() != run_count || run_lengths.len() != run_count {
        return Err(corrupt("stream length mismatch"));
    }
    let total: u64 = run_lengths.iter().sum();
    if total > u32::MAX as u64 {
        return Err(corrupt("total length overflow"));
    }
    out.reserve(total as usize);
    for (&v, &len) in run_values.iter().zip(&run_lengths) {
        for _ in 0..len {
            out.push(v);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_runs() {
        let mut values = Vec::new();
        for (v, n) in [(7u64, 500usize), (0, 1), (7, 200), (u64::MAX, 3)] {
            values.extend(std::iter::repeat_n(v, n));
        }
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        assert!(
            buf.len() < 100,
            "4 runs should encode tiny, got {}",
            buf.len()
        );
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_no_runs() {
        let values: Vec<u64> = (0..2000).collect();
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn empty_and_corrupt() {
        let mut buf = Vec::new();
        encode(&[], &mut buf);
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert!(out.is_empty());
        assert!(decode(&buf[..3], &mut out).is_err());
    }
}
