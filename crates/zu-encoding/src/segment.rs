//! Segment-level encoding selection, the BtrBlocks-style sampled cascade.
//!
//! `encode_auto` samples 8 runs of 128 values (about 0.8% of a full node
//! group), sizes each legal candidate on the sample, encodes with the
//! winner, and falls back to Plain if the winner loses to Plain on the
//! full input. The payload is prefixed with one stable `EncodingId` byte,
//! so `decode_any` needs no side channel.

use zu_common::{Result, ZuError};

use crate::{EncodingId, delta, dict, for_bitpack, rle};

const SAMPLE_RUNS: usize = 8;
const SAMPLE_RUN_LEN: usize = 128;

/// Encodes `values` with the estimated-best encoding, returning the id used.
pub fn encode_auto(values: &[u64], out: &mut Vec<u8>) -> EncodingId {
    let id = choose(values);
    let start = out.len();
    out.push(id as u8);
    encode_with(id, values, out);
    // The sample can mislead; never ship a segment larger than Plain.
    if out.len() - start > 9 + values.len() * 8 {
        out.truncate(start);
        out.push(EncodingId::Plain as u8);
        encode_with(EncodingId::Plain, values, out);
        return EncodingId::Plain;
    }
    id
}

/// Decodes a segment produced by `encode_auto`, appending to `out`.
pub fn decode_any(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    let (&id, payload) = bytes.split_first().ok_or_else(|| corrupt("empty"))?;
    match EncodingId::try_from(id)? {
        EncodingId::Plain => decode_plain(payload, out),
        EncodingId::Constant => decode_constant(payload, out),
        EncodingId::Rle => rle::decode(payload, out),
        EncodingId::Dict => dict::decode(payload, out),
        EncodingId::ForBitPack => for_bitpack::decode(payload, out),
        EncodingId::DeltaBitPack => delta::decode(payload, out),
        other => Err(ZuError::Unsupported {
            what: "segment encoding",
            id: other as u8 as u32,
        }),
    }
}

fn encode_with(id: EncodingId, values: &[u64], out: &mut Vec<u8>) {
    match id {
        EncodingId::Plain => encode_plain(values, out),
        EncodingId::Constant => encode_constant(values, out),
        EncodingId::Rle => {
            rle::encode(values, out);
        }
        EncodingId::Dict => {
            dict::encode(values, out);
        }
        EncodingId::ForBitPack => {
            for_bitpack::encode(values, out);
        }
        EncodingId::DeltaBitPack => {
            delta::encode(values, out);
        }
        _ => unreachable!("choose never returns other ids"),
    }
}

fn choose(values: &[u64]) -> EncodingId {
    if values.is_empty() {
        return EncodingId::Plain;
    }
    let first = values[0];
    if values.iter().all(|&v| v == first) {
        return EncodingId::Constant;
    }
    let sample = sample(values);
    // Candidate order breaks ties toward the shallower cascade.
    let candidates = [
        EncodingId::ForBitPack,
        EncodingId::DeltaBitPack,
        EncodingId::Rle,
        EncodingId::Dict,
    ];
    let mut best = EncodingId::Plain;
    let mut best_size = 4 + sample.len() * 8;
    let mut buf = Vec::new();
    for id in candidates {
        buf.clear();
        encode_with(id, &sample, &mut buf);
        if buf.len() < best_size {
            best = id;
            best_size = buf.len();
        }
    }
    best
}

/// Even-spaced runs so ordered data keeps its local structure in the sample.
fn sample(values: &[u64]) -> Vec<u64> {
    let want = SAMPLE_RUNS * SAMPLE_RUN_LEN;
    if values.len() <= want {
        return values.to_vec();
    }
    let stride = values.len() / SAMPLE_RUNS;
    let mut out = Vec::with_capacity(want);
    for run in 0..SAMPLE_RUNS {
        let start = run * stride;
        out.extend_from_slice(&values[start..start + SAMPLE_RUN_LEN]);
    }
    out
}

fn encode_plain(values: &[u64], out: &mut Vec<u8>) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn decode_plain(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "plain",
        detail: detail.to_string(),
    };
    let count = u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or_else(|| corrupt("truncated count"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let body = bytes
        .get(4..4 + count * 8)
        .ok_or_else(|| corrupt("truncated body"))?;
    out.reserve(count);
    out.extend(
        body.chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap())),
    );
    Ok(())
}

fn encode_constant(values: &[u64], out: &mut Vec<u8>) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    out.extend_from_slice(&values[0].to_le_bytes());
}

fn decode_constant(bytes: &[u8], out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "constant",
        detail: detail.to_string(),
    };
    let body = bytes.get(..12).ok_or_else(|| corrupt("truncated"))?;
    let count = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    let value = u64::from_le_bytes(body[4..12].try_into().unwrap());
    out.extend(std::iter::repeat_n(value, count));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u64]) -> EncodingId {
        let mut buf = Vec::new();
        let id = encode_auto(values, &mut buf);
        let mut out = Vec::new();
        decode_any(&buf, &mut out).unwrap();
        assert_eq!(values, out.as_slice());
        id
    }

    #[test]
    fn picks_constant() {
        assert_eq!(roundtrip(&vec![9u64; 5000]), EncodingId::Constant);
    }

    #[test]
    fn picks_delta_for_sorted() {
        let values: Vec<u64> = (0..10_000u64).map(|i| 1_000_000 + i * 3).collect();
        assert_eq!(roundtrip(&values), EncodingId::DeltaBitPack);
    }

    #[test]
    fn picks_for_on_small_range_noise() {
        let mut rng = 0xABCDEFu64;
        let values: Vec<u64> = (0..10_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                5_000_000 + rng % 1000
            })
            .collect();
        assert_eq!(roundtrip(&values), EncodingId::ForBitPack);
    }

    #[test]
    fn picks_rle_for_runs() {
        let mut values = Vec::new();
        for run in 0..40u64 {
            values.extend(std::iter::repeat_n(run * 1_000_000_007, 500));
        }
        assert_eq!(roundtrip(&values), EncodingId::Rle);
    }

    #[test]
    fn picks_dict_for_scattered_low_cardinality() {
        let pool: Vec<u64> = (0..16u64)
            .map(|i| i.wrapping_mul(0x9E3779B97F4A7C15))
            .collect();
        let values: Vec<u64> = (0..10_000).map(|i| pool[(i * 7) % pool.len()]).collect();
        assert_eq!(roundtrip(&values), EncodingId::Dict);
    }

    #[test]
    fn random_wide_data_stays_decodable() {
        let mut rng = 0x123456789u64;
        let values: Vec<u64> = (0..5000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            })
            .collect();
        roundtrip(&values);
    }

    #[test]
    fn empty_input() {
        assert_eq!(roundtrip(&[]), EncodingId::Plain);
    }

    #[test]
    fn unknown_id_rejected() {
        let mut out = Vec::new();
        assert!(decode_any(&[200, 0, 0], &mut out).is_err());
        assert!(decode_any(&[], &mut out).is_err());
        // FSST is a known id without a shipped decoder yet; it must error
        // by name, not panic.
        assert!(decode_any(&[8, 1, 2], &mut out).is_err());
    }
}
