//! Segment-level encoding selection, the BtrBlocks-style cascade.
//!
//! `encode_auto` reads the chunk once, prices every legal candidate off
//! what that pass found, encodes with the winner, and falls back to
//! Plain if the winner loses to Plain on the full input. The payload is
//! prefixed with one stable `EncodingId` byte, so `decode_any` needs no
//! side channel.
//!
//! Pricing is arithmetic rather than a trial encode, and that is the
//! whole of what makes a write cheap. Sizing seven candidates by
//! encoding each of them costs seven encodes of every chunk a column
//! rewrite touches, and a fold is column rewrites: profiling a stream of
//! single row writes put two thirds of the engine's processor time
//! inside the encoders, nearly all of it in candidates that lost. The
//! containers are simple enough to price exactly instead. Frame of
//! reference is a minimum and a width per 1024 values, the patched
//! container prices its own width off a histogram this pass already
//! builds, and the rest are a count of runs, a count of distinct values
//! and the count of the value that dominates. Only the winner is
//! encoded, and the Plain guard below catches an estimate that was
//! wrong about the shape rather than about the arithmetic.

use zu_common::{Result, ZuError};

use crate::bitpack::{CHUNK, packed_bytes};
use crate::counts;
use crate::delta::zigzag;
use crate::{
    EncodingId, bits_needed, bool_bitpack, delta, delta_patch, dict, for_bitpack, frequency, rle,
};

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
    let shape = Shape::read(values);
    if shape.constant {
        return EncodingId::Constant;
    }
    // Candidate order breaks ties toward the shallower cascade.
    // BoolBitpack is legal only when the whole input is binary, and Dict
    // only under the format cap on distinct values, both of which this
    // pass answers for every value rather than for a sample of them.
    let mut best = EncodingId::Plain;
    let mut best_size = 4 + values.len() * 8;
    let candidates = [
        EncodingId::BoolBitpack,
        EncodingId::ForBitPack,
        EncodingId::DeltaBitPack,
        EncodingId::DeltaPatch,
        EncodingId::Rle,
        EncodingId::Dict,
        EncodingId::Frequency,
    ];
    for id in candidates {
        let Some(size) = shape.price(id) else {
            continue;
        };
        if size < best_size {
            best = id;
            best_size = size;
        }
    }
    best
}

/// What one pass over a chunk says, which is enough to price every
/// candidate the cascade offers.
///
/// The three container sizes are exact: frame of reference stores a
/// minimum and a width per 1024 values, and the patched container picks
/// its width off the same width histogram its encoder builds, by the
/// same suffix walk. The rest are priced off counts, and the only
/// looseness is the width the exception streams pack at, which is taken
/// as the chunk's own width rather than the exceptions' own. That
/// overprices Frequency slightly and never underprices it.
struct Shape {
    len: usize,
    min: u64,
    max: u64,
    /// Every value the same, which Constant says in twelve bytes.
    constant: bool,
    /// Every value 0 or 1, which is what makes BoolBitpack legal.
    binary: bool,
    /// The frame of reference container over the values themselves.
    for_bytes: usize,
    /// The same container over the zigzag deltas, which is DeltaBitPack
    /// below its eight byte base.
    delta_bytes: usize,
    /// The patched container over those deltas, which is DeltaPatch
    /// below the same base.
    patch_bytes: usize,
    runs: usize,
    run_len_min: u64,
    run_len_max: u64,
    /// Distinct values, or None past the Dict cap, which is what says
    /// Dict is not legal here.
    distinct: Option<usize>,
    /// How often the most common value appears, which is everything
    /// Frequency is not an exception of.
    top_count: usize,
}

impl Shape {
    fn read(values: &[u64]) -> Shape {
        let mut shape = Shape {
            len: values.len(),
            min: u64::MAX,
            max: 0,
            constant: true,
            binary: true,
            for_bytes: 4,
            delta_bytes: 4,
            patch_bytes: 4,
            runs: 0,
            run_len_min: u64::MAX,
            run_len_max: 0,
            distinct: None,
            top_count: 0,
        };
        let first = values[0];
        // The delta encoders seed the running previous from the first
        // value, so the first delta is zero and no chunk pays for the
        // magnitude the column starts at.
        let mut prev = first;
        let mut run = 0u64;
        for block in values.chunks(CHUNK) {
            let (mut bmin, mut bmax) = (u64::MAX, 0u64);
            let (mut dmin, mut dmax) = (u64::MAX, 0u64);
            let mut hist = [0usize; 65];
            let mut wide = 0u32;
            for &v in block {
                bmin = bmin.min(v);
                bmax = bmax.max(v);
                shape.constant &= v == first;
                shape.binary &= v <= 1;
                let zz = zigzag(v.wrapping_sub(prev) as i64);
                dmin = dmin.min(zz);
                dmax = dmax.max(zz);
                let width = bits_needed(zz);
                hist[width as usize] += 1;
                wide = wide.max(width);
                if v == prev && run > 0 {
                    run += 1;
                } else {
                    shape.close_run(run);
                    run = 1;
                }
                prev = v;
            }
            shape.min = shape.min.min(bmin);
            shape.max = shape.max.max(bmax);
            shape.for_bytes += 9 + packed_bytes(bits_needed(bmax - bmin), block.len());
            shape.delta_bytes += 9 + packed_bytes(bits_needed(dmax - dmin), block.len());
            shape.patch_bytes += 2 + patch_body(&hist, wide, block.len());
        }
        shape.close_run(run);
        // Dict cannot hold more than the format cap, and past it the
        // count of distinct values buys nothing else, so the table stops
        // there rather than growing with the column.
        let counts = counts::count(values, dict::MAX_ENTRIES);
        shape.distinct = counts.distinct;
        shape.top_count = counts.top_count;
        shape
    }

    fn close_run(&mut self, len: u64) {
        if len > 0 {
            self.runs += 1;
            self.run_len_min = self.run_len_min.min(len);
            self.run_len_max = self.run_len_max.max(len);
        }
    }

    /// What `id` would cost on this chunk, or None when the format does
    /// not allow it here.
    fn price(&self, id: EncodingId) -> Option<usize> {
        let spread = self.max - self.min;
        match id {
            EncodingId::BoolBitpack => self.binary.then(|| 4 + self.len.div_ceil(8)),
            EncodingId::ForBitPack => Some(self.for_bytes),
            EncodingId::DeltaBitPack => Some(8 + self.delta_bytes),
            EncodingId::DeltaPatch => Some(8 + self.patch_bytes),
            EncodingId::Rle => Some(
                8 + for_bytes(self.runs, bits_needed(spread))
                    + for_bytes(self.runs, bits_needed(self.run_len_max - self.run_len_min)),
            ),
            EncodingId::Dict => self.distinct.map(|d| {
                4 + for_bytes(d, bits_needed(spread))
                    + for_bytes(self.len, bits_needed(d.saturating_sub(1) as u64))
            }),
            EncodingId::Frequency => {
                let exceptions = self.len - self.top_count;
                Some(
                    16 + for_bytes(exceptions, bits_needed(self.len as u64))
                        + for_bytes(exceptions, bits_needed(spread)),
                )
            }
            _ => None,
        }
    }
}

/// What a frame of reference container costs: its count, then a minimum
/// and a width per 1024 values, then the packed body.
fn for_bytes(count: usize, width: u32) -> usize {
    4 + count.div_ceil(CHUNK) * 9 + packed_bytes(width, count)
}

/// What the patched container's body costs for one chunk, by the walk
/// its encoder runs: every width from the widest down, paying the
/// packed body, the presence bitmap and the exceptions above it.
fn patch_body(hist: &[usize; 65], wide: u32, take: usize) -> usize {
    let mut best = packed_bytes(wide, take) + packed_bytes(1, take);
    let mut exceptions = 0usize;
    for width in (0..wide).rev() {
        exceptions += hist[width as usize + 1];
        let cost =
            packed_bytes(width, take) + packed_bytes(1, take) + packed_bytes(wide, exceptions);
        best = best.min(cost);
    }
    best
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
        body.as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c)),
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
