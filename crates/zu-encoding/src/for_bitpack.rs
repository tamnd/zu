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

/// A pull cursor over a container's chunks, for decoders that walk two
/// streams in lockstep or stream chunks straight into typed output.
/// `next` unpacks the following chunk into `scratch` without the frame
/// minimum applied and returns `(min, take)`, or None past the end.
/// The claimed count is rejected against `max_values` up front: width-0
/// chunks mean 9 bytes of input can claim 1024 values, so nothing may
/// scale with the claim before that wall.
pub(crate) struct ChunkCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
    remaining: usize,
    count: usize,
}

impl<'a> ChunkCursor<'a> {
    pub(crate) fn new(bytes: &'a [u8], max_values: usize) -> Result<Self> {
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
        Ok(Self {
            bytes,
            pos: 4,
            remaining: count,
            count,
        })
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }

    pub(crate) fn next(&mut self, scratch: &mut [u64; CHUNK]) -> Result<Option<(u64, usize)>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "for_bitpack",
            detail: detail.to_string(),
        };
        let header = self
            .bytes
            .get(self.pos..self.pos + 9)
            .ok_or_else(|| corrupt("truncated chunk header"))?;
        let min = u64::from_le_bytes(header[..8].try_into().unwrap());
        let width = u32::from(header[8]);
        if width > 64 {
            return Err(corrupt("width > 64"));
        }
        self.pos += 9;
        let take = self.remaining.min(CHUNK);
        let plen = bitpack::packed_bytes(width, take);
        let packed = self
            .bytes
            .get(self.pos..self.pos + plen)
            .ok_or_else(|| corrupt("truncated chunk body"))?;
        bitpack::unpack(packed, width, scratch);
        self.pos += plen;
        self.remaining -= take;
        Ok(Some((min, take)))
    }
}

/// Parses the container and hands each decoded chunk to `sink` as
/// `(min, values, take)`. Shared with the delta decoder so both stay
/// single pass.
pub(crate) fn decode_chunks(
    bytes: &[u8],
    max_values: usize,
    mut sink: impl FnMut(u64, &[u64], usize),
) -> Result<usize> {
    let mut cursor = ChunkCursor::new(bytes, max_values)?;
    let mut scratch = [0u64; CHUNK];
    while let Some((min, take)) = cursor.next(&mut scratch)? {
        sink(min, &scratch, take);
    }
    Ok(cursor.count())
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
