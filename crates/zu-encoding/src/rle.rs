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

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. The ceiling bounds the run streams (every run carries at
/// least one value) and the materialized total, and the total is summed
/// checked so hostile run lengths cannot overflow it past the wall.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "rle",
        detail,
    };
    let header = bytes
        .get(..8)
        .ok_or_else(|| corrupt("truncated header".into()))?;
    let run_count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let values_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if run_count > max_values {
        return Err(corrupt(format!(
            "claims {run_count} runs, caller allows {max_values} values"
        )));
    }
    let values_bytes = bytes
        .get(8..8 + values_len)
        .ok_or_else(|| corrupt("truncated values".into()))?;
    let lengths_bytes = bytes
        .get(8 + values_len..)
        .ok_or_else(|| corrupt("truncated lengths".into()))?;
    let mut run_values = Vec::new();
    let mut run_lengths = Vec::new();
    for_bitpack::decode(values_bytes, max_values, &mut run_values)?;
    for_bitpack::decode(lengths_bytes, max_values, &mut run_lengths)?;
    if run_values.len() != run_count || run_lengths.len() != run_count {
        return Err(corrupt("stream length mismatch".into()));
    }
    let mut total = 0u64;
    for &len in &run_lengths {
        total = total
            .checked_add(len)
            .ok_or_else(|| corrupt("total length overflow".into()))?;
    }
    if total > max_values as u64 {
        return Err(corrupt(format!(
            "claims {total} values, caller allows {max_values}"
        )));
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
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn roundtrip_no_runs() {
        let values: Vec<u64> = (0..2000).collect();
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn empty_and_corrupt() {
        let mut buf = Vec::new();
        encode(&[], &mut buf);
        let mut out = Vec::new();
        decode(&buf, 0, &mut out).unwrap();
        assert!(out.is_empty());
        assert!(decode(&buf[..3], 16, &mut out).is_err());
    }

    #[test]
    fn hostile_lengths_cannot_overflow_or_flood() {
        // Two runs whose lengths sum past u64: the checked sum must
        // reject them instead of wrapping into a small reservation.
        let mut buf = 2u32.to_le_bytes().to_vec();
        let mut values_buf = Vec::new();
        for_bitpack::encode(&[1, 2], &mut values_buf);
        buf.extend_from_slice(&(values_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&values_buf);
        for_bitpack::encode(&[u64::MAX, 2], &mut buf);
        let mut out = Vec::new();
        assert!(decode(&buf, 1 << 20, &mut out).is_err());

        // A single run claiming a billion values dies on the ceiling,
        // not in the allocator.
        let mut buf = 1u32.to_le_bytes().to_vec();
        let mut values_buf = Vec::new();
        for_bitpack::encode(&[7], &mut values_buf);
        buf.extend_from_slice(&(values_buf.len() as u32).to_le_bytes());
        buf.extend_from_slice(&values_buf);
        for_bitpack::encode(&[1_000_000_000], &mut buf);
        assert!(decode(&buf, 1 << 20, &mut out).is_err());
    }
}
