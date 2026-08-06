//! Frame-of-reference plus bit packing (encoding id 4).
//!
//! Layout: `count: u32 LE`, then per 1024-value chunk a header of
//! `min: u64 LE, width: u8` followed by the packed body, which is
//! `bitpack::packed_bytes(width, chunk_len)` bytes. Full chunks are
//! independently decodable at fixed offsets, which is what makes
//! chunk-granular point reads cheap; only the final chunk can be short.

use zu_common::{Result, ZuError};

use crate::bitpack::{self, CHUNK};
use crate::bits_needed;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let mut scratch = [0u64; CHUNK];
    for chunk in values.chunks(CHUNK) {
        let min = chunk.iter().copied().min().unwrap_or(0);
        let max = chunk.iter().copied().max().unwrap_or(0);
        let width = bits_needed(max - min);
        out.extend_from_slice(&min.to_le_bytes());
        out.push(width as u8);
        for (slot, &v) in scratch.iter_mut().zip(chunk) {
            *slot = v - min;
        }
        bitpack::pack(&scratch[..chunk.len()], width, out);
    }
    out.len() - start
}

/// Parses the container and hands each decoded chunk to `sink` as
/// `(min, values, take)`. Shared with the delta decoder so both stay
/// single pass. `max_values` is the caller's ceiling: width-0 chunks
/// mean 9 bytes of input can claim 1024 values, so the claimed count
/// must be rejected against what the caller expects before any work
/// scales with it.
pub(crate) fn decode_chunks(
    bytes: &[u8],
    max_values: usize,
    mut sink: impl FnMut(u64, &[u64], usize),
) -> Result<usize> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "for_bitpack",
        detail,
    };
    let count = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| corrupt("truncated count".into()))?
            .try_into()
            .unwrap(),
    ) as usize;
    if count > max_values {
        return Err(corrupt(format!(
            "claims {count} values, caller allows {max_values}"
        )));
    }
    let mut pos = 4usize;
    let mut scratch = [0u64; CHUNK];
    let mut remaining = count;
    while remaining > 0 {
        let header = bytes
            .get(pos..pos + 9)
            .ok_or_else(|| corrupt("truncated chunk header".into()))?;
        let min = u64::from_le_bytes(header[..8].try_into().unwrap());
        let width = u32::from(header[8]);
        if width > 64 {
            return Err(corrupt("width > 64".into()));
        }
        pos += 9;
        let take = remaining.min(CHUNK);
        let plen = bitpack::packed_bytes(width, take);
        let packed = bytes
            .get(pos..pos + plen)
            .ok_or_else(|| corrupt("truncated chunk body".into()))?;
        bitpack::unpack(packed, width, &mut scratch);
        pos += plen;
        sink(min, &scratch, take);
        remaining -= take;
    }
    Ok(count)
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`. A container claiming more than the caller's ceiling is
/// rejected before anything is allocated for it.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    decode_chunks(bytes, max_values, |min, scratch, take| {
        out.extend(scratch[..take].iter().map(|&v| min.wrapping_add(v)));
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_multi_chunk() {
        let values: Vec<u64> = (0..3000u64).map(|i| 1_000_000 + i * 3).collect();
        let mut buf = Vec::new();
        let len = encode(&values, &mut buf);
        assert_eq!(len, buf.len());
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn constant_chunk_is_tiny() {
        let values = vec![42u64; CHUNK];
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        assert_eq!(buf.len(), 4 + 9);
    }

    #[test]
    fn empty_and_truncated() {
        let mut buf = Vec::new();
        encode(&[], &mut buf);
        let mut out = Vec::new();
        decode(&buf, 0, &mut out).unwrap();
        assert!(out.is_empty());
        assert!(decode(&[1, 0], 16, &mut out).is_err());
        assert!(decode(&[10, 0, 0, 0, 1], 16, &mut out).is_err());
    }

    #[test]
    fn hostile_count_is_rejected_against_the_ceiling() {
        // Width-0 chunks make the container 9 bytes per claimed 1024
        // values, so the count wall is what stands between a small
        // input and an allocation the caller never asked for.
        let values = vec![7u64; 100];
        let mut buf = Vec::new();
        encode(&values, &mut buf);
        let mut out = Vec::new();
        assert!(decode(&buf, 99, &mut out).is_err());
        decode(&buf, 100, &mut out).unwrap();
        assert_eq!(out, values);
        let mut hostile = u32::MAX.to_le_bytes().to_vec();
        hostile.extend_from_slice(&[0u8; 9]);
        assert!(decode(&hostile, 1 << 20, &mut out).is_err());
    }
}
