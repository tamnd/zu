//! Delta plus patched bit packing (encoding id 12).
//!
//! Same front end as encoding id 5: `base: u64 LE`, then zigzag deltas
//! with the running previous seeded from the base. The difference is the
//! container: deltas go through `patch` instead of `for_bitpack`, so a
//! rare wide gap becomes an exception instead of setting the packed
//! width for its whole 1024-value chunk. This is the encoding CSR
//! adjacency wants: within a neighbor list the gaps are small, and the
//! hub jumps and list restarts that used to price every chunk at 23 bits
//! on LiveJournal move into the exception stream.

use zu_common::{Result, ZuError};

use crate::delta::{unzigzag, zigzag};
use crate::patch;

/// Encodes `values` into `out`, returning the encoded byte length.
pub fn encode(values: &[u64], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    let base = values.first().copied().unwrap_or(0);
    out.extend_from_slice(&base.to_le_bytes());
    let mut deltas = Vec::with_capacity(values.len());
    let mut prev = base;
    for &v in values {
        deltas.push(zigzag(v.wrapping_sub(prev) as i64));
        prev = v;
    }
    patch::encode(&deltas, out);
    out.len() - start
}

/// Decodes an encoded buffer, appending at most `max_values` values to
/// `out`; a container claiming more is rejected before allocation.
pub fn decode(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let base_bytes = bytes.get(..8).ok_or_else(|| ZuError::Corrupt {
        what: "delta_patch",
        detail: "truncated base".to_string(),
    })?;
    let mut prev = u64::from_le_bytes(base_bytes.try_into().unwrap());
    patch::decode_chunks(&bytes[8..], max_values, |deltas, take| {
        out.extend(deltas[..take].iter().map(|&d| {
            prev = prev.wrapping_add(unzigzag(d) as u64);
            prev
        }));
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u64]) -> usize {
        let mut buf = Vec::new();
        let len = encode(values, &mut buf);
        let mut out = Vec::new();
        decode(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out.as_slice());
        len
    }

    #[test]
    fn adjacency_shape_beats_plain_delta() {
        // Concatenated sorted neighbor lists: 14 tight ids per list, one
        // hub near zero, list starts scattered over 4M ids. Every list
        // carries two wide deltas (the restart and the hub jump), so the
        // floor for this shape is about 10 bits; plain delta pays ~23 for
        // every value.
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut values = Vec::new();
        for list in 0..10_000u64 {
            let anchor = (list.wrapping_mul(0x2545F4914F6CDD1D) % 4_000_000).max(1);
            values.push(anchor % 1000); // hub neighbor, low id
            let mut v = anchor;
            for _ in 0..13 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v += 1 + rng % 30;
                values.push(v);
            }
        }
        let len = roundtrip(&values);
        assert!(
            len * 8 < values.len() * 11,
            "got {} bits/value",
            len * 8 / values.len()
        );
        let mut plain = Vec::new();
        crate::delta::encode(&values, &mut plain);
        assert!(
            len * 2 < plain.len(),
            "patched {} vs delta {}",
            len,
            plain.len()
        );
    }

    #[test]
    fn unsorted_extremes_roundtrip() {
        roundtrip(&[]);
        roundtrip(&[u64::MAX, 0, 1, u64::MAX / 2, 3, 3, 2]);
    }

    #[test]
    fn truncated_base_rejected() {
        let mut out = Vec::new();
        assert!(decode(&[1, 2, 3], 16, &mut out).is_err());
    }
}
