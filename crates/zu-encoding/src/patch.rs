//! Patched bit packing, the exception container behind encoding id 12.
//!
//! Plain per-chunk packing pays the width of the widest value for all
//! 1024 values, which is exactly how adjacency deltas die: neighbor lists
//! average about 14 values on social graphs, a chunk crosses about 70
//! lists, and nearly every chunk holds one hub-sized gap that prices the
//! whole chunk at 23 bits. Here the encoder picks the packed width that
//! minimizes total bytes; values wider than the pick become exceptions,
//! stored at the chunk's max width behind a presence bitmap while their
//! packed slots hold zero.
//!
//! Layout: `count: u32 LE`, then per 1024-value chunk a header of
//! `width: u8, exc_width: u8`, a packed body of
//! `packed_bytes(width, take)` bytes, a presence bitmap of
//! `packed_bytes(1, take)` bytes, and the exceptions in position order,
//! `packed_bytes(exc_width, popcount)` bytes.

use zu_common::{Result, ZuError};

use crate::bitpack::{self, CHUNK};
use crate::bits_needed;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let mut low = [0u64; CHUNK];
    let mut flags = [0u64; CHUNK];
    let mut excs = Vec::with_capacity(64);
    for chunk in values.chunks(CHUNK) {
        let take = chunk.len();
        // Width histogram, then a reverse suffix walk prices every
        // candidate width in one pass.
        let mut hist = [0usize; 65];
        let mut max_width = 0u32;
        for &v in chunk {
            let w = bits_needed(v);
            hist[w as usize] += 1;
            max_width = max_width.max(w);
        }
        let mut width = max_width;
        let mut best = bitpack::packed_bytes(max_width, take) + bitpack::packed_bytes(1, take);
        let mut exc_count = 0usize;
        for w in (0..max_width).rev() {
            exc_count += hist[w as usize + 1];
            let cost = bitpack::packed_bytes(w, take)
                + bitpack::packed_bytes(1, take)
                + bitpack::packed_bytes(max_width, exc_count);
            if cost < best {
                best = cost;
                width = w;
            }
        }

        excs.clear();
        for (i, &v) in chunk.iter().enumerate() {
            if bits_needed(v) > width {
                low[i] = 0;
                flags[i] = 1;
                excs.push(v);
            } else {
                low[i] = v;
                flags[i] = 0;
            }
        }
        out.push(width as u8);
        out.push(max_width as u8);
        bitpack::pack(&low[..take], width, out);
        bitpack::pack(&flags[..take], 1, out);
        bitpack::pack(&excs, max_width, out);
    }
    out.len() - start
}

/// Parses the container and hands each patched chunk to `sink` as
/// `(values, take)`. Shared with the delta-patch decoder.
pub(crate) fn decode_chunks(bytes: &[u8], mut sink: impl FnMut(&[u64], usize)) -> Result<usize> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "patch",
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
    let mut low = [0u64; CHUNK];
    let mut flags = [0u64; CHUNK];
    let mut excs = [0u64; CHUNK];
    let mut remaining = count;
    while remaining > 0 {
        let header = bytes
            .get(pos..pos + 2)
            .ok_or_else(|| corrupt("truncated chunk header"))?;
        let (width, exc_width) = (u32::from(header[0]), u32::from(header[1]));
        if width > 64 || exc_width > 64 {
            return Err(corrupt("width > 64"));
        }
        pos += 2;
        let take = remaining.min(CHUNK);

        let plen = bitpack::packed_bytes(width, take);
        let packed = bytes
            .get(pos..pos + plen)
            .ok_or_else(|| corrupt("truncated chunk body"))?;
        bitpack::unpack(packed, width, &mut low);
        pos += plen;

        let blen = bitpack::packed_bytes(1, take);
        let bitmap = bytes
            .get(pos..pos + blen)
            .ok_or_else(|| corrupt("truncated bitmap"))?;
        bitpack::unpack(bitmap, 1, &mut flags);
        pos += blen;

        let exc_count = flags[..take].iter().map(|&f| f as usize).sum();
        let elen = bitpack::packed_bytes(exc_width, exc_count);
        let packed_excs = bytes
            .get(pos..pos + elen)
            .ok_or_else(|| corrupt("truncated exceptions"))?;
        bitpack::unpack(packed_excs, exc_width, &mut excs);
        pos += elen;

        let mut next = 0usize;
        for i in 0..take {
            if flags[i] != 0 {
                low[i] = excs[next];
                next += 1;
            }
        }
        sink(&low, take);
        remaining -= take;
    }
    Ok(count)
}

/// Decodes an encoded buffer, appending the values to `out`.
pub fn decode(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    decode_chunks(bytes, |values, take| {
        out.extend_from_slice(&values[..take]);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u64]) -> usize {
        let mut buf = Vec::new();
        let len = encode(values, &mut buf);
        assert_eq!(len, buf.len());
        let mut out = Vec::new();
        decode(&buf, &mut out).unwrap();
        assert_eq!(values, out.as_slice());
        len
    }

    #[test]
    fn outliers_stop_pricing_the_chunk() {
        // 1% hub-sized values in a sea of 4-bit gaps. Plain packing pays
        // 24 bits each; patched must land under 7 bits per value.
        let values: Vec<u64> = (0..10_000u64)
            .map(|i| if i % 100 == 7 { 10_000_000 + i } else { i % 13 })
            .collect();
        let len = roundtrip(&values);
        assert!(len * 8 < values.len() * 7, "got {} bytes", len);
        let mut plain = Vec::new();
        crate::for_bitpack::encode(&values, &mut plain);
        assert!(len * 2 < plain.len());
    }

    #[test]
    fn uniform_width_needs_no_exceptions() {
        let values: Vec<u64> = (0..2048u64).map(|i| i % 256).collect();
        let len = roundtrip(&values);
        // 8-bit body plus 1-bit bitmap plus headers, nothing more.
        assert!(len <= 4 + 2 * (2 + 1024 + 128));
    }

    #[test]
    fn extremes_and_short_tails() {
        roundtrip(&[]);
        roundtrip(&[u64::MAX]);
        roundtrip(&[0, u64::MAX, 0, 1]);
        let values: Vec<u64> = (0..CHUNK as u64 + 1).collect();
        roundtrip(&values);
    }

    #[test]
    fn hostile_input_rejected() {
        let mut out = Vec::new();
        assert!(decode(&[1, 0], &mut out).is_err());
        // Count says one value, then a chunk header claiming width 65.
        assert!(decode(&[1, 0, 0, 0, 65, 0], &mut out).is_err());
        // Valid header, truncated body.
        assert!(decode(&[100, 0, 0, 0, 8, 8, 1], &mut out).is_err());
    }
}
