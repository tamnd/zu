//! Frame-of-reference plus bit packing (encoding id 4).
//!
//! Layout: `count: u32 LE`, then per 1024-value chunk a header of
//! `min: u64 LE, width: u8` followed by `bitpack::packed_bytes(width)` bytes.
//! Chunks are independently decodable, which is what makes chunk-granular
//! point reads cheap.

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
        scratch[chunk.len()..].fill(0);
        bitpack::pack(&scratch[..chunk.len()], width, out);
    }
    out.len() - start
}

/// Decodes an encoded buffer, appending the values to `out`.
pub fn decode(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "for_bitpack",
        detail: detail.to_string(),
    };
    let count = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| corrupt("truncated count"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let mut pos = 4usize;
    let mut scratch = [0u64; CHUNK];
    let mut remaining = count;
    out.reserve(count);
    while remaining > 0 {
        let header = bytes
            .get(pos..pos + 9)
            .ok_or_else(|| corrupt("truncated chunk header"))?;
        let min = u64::from_le_bytes(header[..8].try_into().unwrap());
        let width = u32::from(header[8]);
        if width > 64 {
            return Err(corrupt("width > 64"));
        }
        pos += 9;
        let plen = bitpack::packed_bytes(width);
        let packed = bytes
            .get(pos..pos + plen)
            .ok_or_else(|| corrupt("truncated chunk body"))?;
        bitpack::unpack(packed, width, &mut scratch);
        pos += plen;
        let take = remaining.min(CHUNK);
        for &v in &scratch[..take] {
            out.push(min.wrapping_add(v));
        }
        remaining -= take;
    }
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
        decode(&buf, &mut out).unwrap();
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
        decode(&buf, &mut out).unwrap();
        assert!(out.is_empty());
        assert!(decode(&[1, 0], &mut out).is_err());
        assert!(decode(&[10, 0, 0, 0, 1], &mut out).is_err());
    }
}
