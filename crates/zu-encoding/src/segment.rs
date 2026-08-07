//! Segment-level encoding selection, the BtrBlocks-style sampled cascade.
//!
//! `encode_auto` samples 8 runs of 128 values (about 0.8% of a full node
//! group), sizes each legal candidate on the sample, encodes with the
//! winner, and falls back to Plain if the winner loses to Plain on the
//! full input. The payload is prefixed with one stable `EncodingId` byte,
//! so `decode_any` needs no side channel.

use zu_common::{Result, ZuError};

use crate::{EncodingId, bool_bitpack, delta, delta_patch, dict, for_bitpack, frequency, rle};

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

/// Decodes a segment produced by `encode_auto`, appending at most
/// `max_values` values to `out`. Every encoding here amplifies, that
/// is the point of compression, so decoded counts are claims until the
/// caller vouches for them: a container claiming more than the ceiling
/// is rejected before anything is allocated for it. Callers always
/// know the bound, the row count their segment meta or chunk index
/// promised.
pub fn decode_any(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "segment",
        detail: detail.to_string(),
    };
    let (&id, payload) = bytes.split_first().ok_or_else(|| corrupt("empty"))?;
    match EncodingId::try_from(id)? {
        EncodingId::Plain => decode_plain(payload, max_values, out),
        EncodingId::Constant => decode_constant(payload, max_values, out),
        EncodingId::Rle => rle::decode(payload, max_values, out),
        EncodingId::Dict => dict::decode(payload, max_values, out),
        EncodingId::ForBitPack => for_bitpack::decode(payload, max_values, out),
        EncodingId::DeltaBitPack => delta::decode(payload, max_values, out),
        EncodingId::DeltaPatch => delta_patch::decode(payload, max_values, out),
        EncodingId::BoolBitpack => bool_bitpack::decode(payload, max_values, out),
        EncodingId::Frequency => frequency::decode(payload, max_values, out),
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
        EncodingId::DeltaPatch => {
            delta_patch::encode(values, out);
        }
        EncodingId::BoolBitpack => {
            bool_bitpack::encode(values, out);
        }
        EncodingId::Frequency => {
            frequency::encode(values, out);
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
    let runs = sample_runs(values);
    let concat: Vec<u64> = runs.concat();
    // Candidate order breaks ties toward the shallower cascade.
    // BoolBitpack is only legal when the whole input is binary, not just
    // the sample, or the encoder would corrupt the values it never saw.
    let mut candidates = Vec::with_capacity(7);
    if values.iter().all(|&v| v <= 1) {
        candidates.push(EncodingId::BoolBitpack);
    }
    candidates.extend([
        EncodingId::ForBitPack,
        EncodingId::DeltaBitPack,
        EncodingId::DeltaPatch,
        EncodingId::Rle,
        EncodingId::Dict,
        EncodingId::Frequency,
    ]);
    let best = size_and_pick(&candidates, &runs, &concat);
    // Dict is only legal under the format cap on distinct values, a
    // property the sample cannot vouch for. The proof scans the full
    // input, so it runs only when Dict actually won the sizing; every
    // other column skips it, and a failed proof re-picks without Dict.
    if best == EncodingId::Dict && !dict_legal(values) {
        let rest: Vec<EncodingId> = candidates
            .into_iter()
            .filter(|&id| id != EncodingId::Dict)
            .collect();
        return size_and_pick(&rest, &runs, &concat);
    }
    best
}

fn size_and_pick(candidates: &[EncodingId], runs: &[&[u64]], concat: &[u64]) -> EncodingId {
    let mut best = EncodingId::Plain;
    let mut best_size = 4 + concat.len() * 8;
    let mut buf = Vec::new();
    for &id in candidates {
        // Delta candidates are sized per run and summed: concatenating the
        // runs fabricates one wide delta per boundary, which reads as
        // outliers on data that has none and skews the pick toward the
        // patched encoding. Value-distribution candidates see the
        // concatenated sample, since their costs do not depend on
        // adjacency and the mix of runs is what a real chunk contains.
        let per_run = matches!(id, EncodingId::DeltaBitPack | EncodingId::DeltaPatch);
        let mut size = 0usize;
        if per_run {
            for run in runs {
                buf.clear();
                encode_with(id, run, &mut buf);
                size += buf.len();
            }
        } else {
            buf.clear();
            encode_with(id, concat, &mut buf);
            size = buf.len();
        }
        if size < best_size {
            best = id;
            best_size = size;
        }
    }
    best
}

/// Whether the whole input stays under the Dict cardinality cap. Early
/// exit keeps the scan cheap exactly when Dict is hopeless anyway.
fn dict_legal(values: &[u64]) -> bool {
    let mut seen = std::collections::HashSet::with_capacity(1024);
    for &v in values {
        seen.insert(v);
        if seen.len() > dict::MAX_ENTRIES {
            return false;
        }
    }
    true
}

/// Even-spaced runs so ordered data keeps its local structure in the sample.
fn sample_runs(values: &[u64]) -> Vec<&[u64]> {
    let want = SAMPLE_RUNS * SAMPLE_RUN_LEN;
    if values.len() <= want {
        return vec![values];
    }
    let stride = values.len() / SAMPLE_RUNS;
    (0..SAMPLE_RUNS)
        .map(|run| &values[run * stride..run * stride + SAMPLE_RUN_LEN])
        .collect()
}

fn encode_plain(values: &[u64], out: &mut Vec<u8>) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn decode_plain(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
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
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
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

fn decode_constant(bytes: &[u8], max_values: usize, out: &mut Vec<u64>) -> Result<()> {
    let corrupt = |detail: &str| ZuError::Corrupt {
        what: "constant",
        detail: detail.to_string(),
    };
    let body = bytes.get(..12).ok_or_else(|| corrupt("truncated"))?;
    let count = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    // Twelve bytes claim any count at all; this wall is the whole
    // defense, and the first thing the fuzzer found without it.
    if count > max_values {
        return Err(corrupt("count above the caller ceiling"));
    }
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
        decode_any(&buf, values.len(), &mut out).unwrap();
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
    fn picks_delta_patch_for_adjacency() {
        // Concatenated sorted neighbor lists: tight in-list gaps with a
        // wide restart and hub jump per list. The wide deltas inside any
        // 128-value sample run are real outliers, so the patched delta
        // must win over plain delta's max-width chunks.
        let mut rng = 0x2545F4914F6CDD1Du64;
        let mut values = Vec::new();
        for list in 0..2_000u64 {
            let anchor = (list.wrapping_mul(0x9E3779B97F4A7C15) % 4_000_000).max(1);
            values.push(anchor % 1000);
            let mut v = anchor;
            for _ in 0..13 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                v += 1 + rng % 30;
                values.push(v);
            }
        }
        assert_eq!(roundtrip(&values), EncodingId::DeltaPatch);
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
    fn picks_bool_for_binary() {
        // Mixed 0/1 noise: Constant does not apply, and BoolBitpack must
        // beat width-1 FOR on the tie because it sits first in line.
        let values: Vec<u64> = (0..10_000u64).map(|i| (i * i / 7) & 1).collect();
        assert_eq!(roundtrip(&values), EncodingId::BoolBitpack);
    }

    #[test]
    fn bool_never_picked_when_a_single_value_is_wider() {
        // Binary everywhere except one value the sample misses: the
        // legality check scans the whole input, so BoolBitpack must not
        // be offered at all.
        let mut values: Vec<u64> = (0..10_000u64).map(|i| i & 1).collect();
        values[9_999] = 2;
        let id = roundtrip(&values);
        assert_ne!(id, EncodingId::BoolBitpack);
    }

    #[test]
    fn dict_never_offered_past_the_distinct_cap() {
        // Sixteen scattered wide values inside any sample window, but a
        // fresh sixteen every 64 rows: the sample prices Dict as a clear
        // winner while the full input holds ten thousand distinct
        // values, past the format cap. The legality gate has to keep
        // Dict out, or the encoder would ship a dictionary no reader
        // accepts.
        let values: Vec<u64> = (0..40_000usize)
            .map(|i| {
                let pool = (i / 64 * 16 + (i * 7) % 16) as u64;
                pool.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            })
            .collect();
        let mut buf = Vec::new();
        let id = encode_auto(&values, &mut buf);
        assert_ne!(id, EncodingId::Dict);
        let mut out = Vec::new();
        decode_any(&buf, values.len(), &mut out).unwrap();
        assert_eq!(values, out);
    }

    #[test]
    fn picks_frequency_for_dominant_scattered() {
        // 90% one value, 10% wide random exceptions: runs are too short
        // for RLE, exception cardinality too high for Dict, values too
        // wide for FOR, and the exceptions are cheap as patches.
        let mut rng = 0x9E3779B97F4A7C15u64;
        let values: Vec<u64> = (0..10_000)
            .map(|i| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if i % 10 == 3 { rng } else { 777 }
            })
            .collect();
        assert_eq!(roundtrip(&values), EncodingId::Frequency);
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
        assert!(decode_any(&[200, 0, 0], 16, &mut out).is_err());
        assert!(decode_any(&[], 16, &mut out).is_err());
        // FSST is a known id without a shipped decoder yet; it must error
        // by name, not panic.
        assert!(decode_any(&[8, 1, 2], 16, &mut out).is_err());
    }

    #[test]
    fn fuzzer_found_constant_flood_rejected() {
        // The exact payload libFuzzer produced in 21 execs against the
        // unguarded decoder: Constant claiming 0xFFF607D0 values, which
        // was a 34 GB allocation from a 14 byte input. The ceiling turns
        // it into a Corrupt error.
        let bytes = [1, 208, 7, 246, 255, 88, 42, 0, 0, 0, 0, 16, 0, 0];
        let mut out = Vec::new();
        assert!(decode_any(&bytes, 2048, &mut out).is_err());
        assert!(out.is_empty());
    }
}
