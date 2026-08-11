//! Statistics blocks: per-direction degree histograms in log2 buckets
//! per rel table, the docs/07 §6 inputs the optimizer's cardinality
//! estimates and pessimistic caps read. The payload lives in a meta
//! chain at the header's `stats_root`, rewritten whole whenever a rel
//! table loads. Bucket `i` counts source nodes whose degree falls in
//! `[2^i, 2^(i+1))`; zero-degree nodes are absent and recoverable from
//! the table's node count. An unknown version decodes as no stats, the
//! skippable contract of docs/04 for the stats section.

use std::collections::BTreeMap;

use zu_common::{Result, ZuError};

use crate::colors::ColorSummary;
use crate::file::Zu1File;
use crate::meta;

const VERSION: u32 = 3;

/// The most colors one summary may carry (docs/07 §6).
pub const COLOR_CAP: usize = 1024;

/// The degree histograms of one rel table, forward then backward, and
/// the COLOR summary when `ANALYZE` has built one. Bulk load drops the
/// summary because a reloaded table invalidates its coloring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelStats {
    pub out_hist: Vec<u64>,
    pub in_hist: Vec<u64>,
    pub colors: Option<ColorSummary>,
}

/// Every rel table's statistics, keyed by catalog table id, and every
/// node table's property column statistics, keyed by table id then
/// column name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub rels: BTreeMap<u32, RelStats>,
    pub cols: BTreeMap<u32, BTreeMap<String, ColStats>>,
}

impl Stats {
    /// Reads the stats chain; an absent chain or an unknown version is
    /// an empty set, never an error.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let payload = meta::read_chain(db, db.db_header().stats_root)?;
        if payload.is_empty() {
            return Ok(Stats::default());
        }
        Ok(Self::decode(&payload).unwrap_or_default())
    }

    /// Rewrites the whole chain and points the header at it. The old
    /// chain's blocks are freed by the caller's checkpoint path via
    /// [`crate::graph::free_chain`]; this only writes.
    pub fn store(&self, db: &mut Zu1File) -> Result<()> {
        let root = meta::write_chain(db, &self.encode())?;
        db.db_header_mut().stats_root = root;
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.rels.len() as u32).to_le_bytes());
        for (table, rel) in &self.rels {
            out.extend_from_slice(&table.to_le_bytes());
            for hist in [&rel.out_hist, &rel.in_hist] {
                out.extend_from_slice(&(hist.len() as u32).to_le_bytes());
                for count in hist {
                    out.extend_from_slice(&count.to_le_bytes());
                }
            }
            let summary = rel.colors.as_ref();
            let counts = summary.map_or(&[][..], |s| &s.counts);
            out.extend_from_slice(&(counts.len() as u32).to_le_bytes());
            for count in counts {
                out.extend_from_slice(&count.to_le_bytes());
            }
            let triples = summary.map_or(&[][..], |s| &s.triples);
            out.extend_from_slice(&(triples.len() as u32).to_le_bytes());
            for (f, t, edges, dmax) in triples {
                out.extend_from_slice(&f.to_le_bytes());
                out.extend_from_slice(&t.to_le_bytes());
                out.extend_from_slice(&edges.to_le_bytes());
                out.extend_from_slice(&dmax.to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.cols.len() as u32).to_le_bytes());
        for (table, cols) in &self.cols {
            out.extend_from_slice(&table.to_le_bytes());
            out.extend_from_slice(&(cols.len() as u32).to_le_bytes());
            for (name, col) in cols {
                out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(&col.rows.to_le_bytes());
                out.extend_from_slice(&col.ndv.to_le_bytes());
                out.extend_from_slice(&(col.top.len() as u32).to_le_bytes());
                for (value, count) in &col.top {
                    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                    out.extend_from_slice(value);
                    out.extend_from_slice(&count.to_le_bytes());
                }
                out.extend_from_slice(&(col.bounds.len() as u32).to_le_bytes());
                for bound in &col.bounds {
                    out.extend_from_slice(&(bound.len() as u32).to_le_bytes());
                    out.extend_from_slice(bound);
                }
            }
        }
        out
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let corrupt = |detail: String| ZuError::Corrupt {
            what: "stats chain",
            detail,
        };
        let mut at = 0usize;
        let u32_at = |payload: &[u8], at: &mut usize| -> Result<u32> {
            let end = *at + 4;
            let bytes = payload
                .get(*at..end)
                .ok_or_else(|| corrupt(format!("truncated at byte {at}", at = *at)))?;
            *at = end;
            Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
        };
        let u64_at = |payload: &[u8], at: &mut usize| -> Result<u64> {
            let end = *at + 8;
            let bytes = payload
                .get(*at..end)
                .ok_or_else(|| corrupt(format!("truncated at byte {at}", at = *at)))?;
            *at = end;
            Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
        };
        let version = u32_at(payload, &mut at)?;
        if !(1..=VERSION).contains(&version) {
            return Err(corrupt(format!("unknown stats version {version}")));
        }
        let rel_count = u32_at(payload, &mut at)?;
        let mut rels = BTreeMap::new();
        for _ in 0..rel_count {
            let table = u32_at(payload, &mut at)?;
            let mut hists = [Vec::new(), Vec::new()];
            for hist in &mut hists {
                let len = u32_at(payload, &mut at)? as usize;
                if len > 64 {
                    return Err(corrupt(format!("{len} buckets in one histogram")));
                }
                for _ in 0..len {
                    hist.push(u64_at(payload, &mut at)?);
                }
            }
            let [out_hist, in_hist] = hists;
            // Version 1 predates the color section; its rels carry
            // histograms only.
            let mut colors = None;
            if version >= 2 {
                let color_count = u32_at(payload, &mut at)? as usize;
                if color_count > COLOR_CAP {
                    return Err(corrupt(format!("{color_count} colors in one summary")));
                }
                let mut counts = Vec::with_capacity(color_count);
                for _ in 0..color_count {
                    counts.push(u64_at(payload, &mut at)?);
                }
                let triple_count = u32_at(payload, &mut at)? as usize;
                if triple_count > color_count * color_count {
                    return Err(corrupt(format!(
                        "{triple_count} color pairs over {color_count} colors"
                    )));
                }
                let mut triples = Vec::with_capacity(triple_count);
                for _ in 0..triple_count {
                    let f = u32_at(payload, &mut at)?;
                    let t = u32_at(payload, &mut at)?;
                    if f as usize >= color_count || t as usize >= color_count {
                        return Err(corrupt(format!("color pair ({f}, {t}) out of range")));
                    }
                    triples.push((f, t, u64_at(payload, &mut at)?, u64_at(payload, &mut at)?));
                }
                if color_count > 0 {
                    colors = Some(ColorSummary { counts, triples });
                }
            }
            rels.insert(
                table,
                RelStats {
                    out_hist,
                    in_hist,
                    colors,
                },
            );
        }
        // Versions 1 and 2 predate the column section.
        let mut cols = BTreeMap::new();
        if version >= 3 {
            let bytes_at = |payload: &[u8], at: &mut usize, len: usize| -> Result<Vec<u8>> {
                let end = *at + len;
                let bytes = payload
                    .get(*at..end)
                    .ok_or_else(|| corrupt(format!("truncated at byte {at}", at = *at)))?;
                *at = end;
                Ok(bytes.to_vec())
            };
            let table_count = u32_at(payload, &mut at)?;
            for _ in 0..table_count {
                let table = u32_at(payload, &mut at)?;
                let col_count = u32_at(payload, &mut at)?;
                let mut table_cols = BTreeMap::new();
                for _ in 0..col_count {
                    let len = u32_at(payload, &mut at)? as usize;
                    let name = String::from_utf8(bytes_at(payload, &mut at, len)?)
                        .map_err(|_| corrupt("column name is not UTF-8".to_string()))?;
                    let rows = u64_at(payload, &mut at)?;
                    let ndv = u64_at(payload, &mut at)?;
                    let top_count = u32_at(payload, &mut at)? as usize;
                    if top_count > TOP_VALUES {
                        return Err(corrupt(format!("{top_count} top values on '{name}'")));
                    }
                    let mut top = Vec::with_capacity(top_count);
                    for _ in 0..top_count {
                        let len = u32_at(payload, &mut at)? as usize;
                        if len > VALUE_CAP {
                            return Err(corrupt(format!("{len} byte value on '{name}'")));
                        }
                        let value = bytes_at(payload, &mut at, len)?;
                        top.push((value, u64_at(payload, &mut at)?));
                    }
                    let bound_count = u32_at(payload, &mut at)? as usize;
                    if bound_count > HIST_BUCKETS + 1 {
                        return Err(corrupt(format!("{bound_count} boundaries on '{name}'")));
                    }
                    let mut bounds = Vec::with_capacity(bound_count);
                    for _ in 0..bound_count {
                        let len = u32_at(payload, &mut at)? as usize;
                        if len > VALUE_CAP {
                            return Err(corrupt(format!("{len} byte boundary on '{name}'")));
                        }
                        bounds.push(bytes_at(payload, &mut at, len)?);
                    }
                    table_cols.insert(
                        name,
                        ColStats {
                            rows,
                            ndv,
                            top,
                            bounds,
                        },
                    );
                }
                cols.insert(table, table_cols);
            }
        }
        Ok(Stats { rels, cols })
    }
}

/// Rows a column reads before it starts sampling. The sort behind the
/// top values and the buckets is the only superlinear step in the
/// whole load, so past a million rows it runs on a stride sample and
/// scales the counts; the distinct count is streamed over every row
/// either way.
const SAMPLE_CAP: usize = 1 << 20;

/// Registers in the distinct-count sketch, one byte each, so the
/// sketch is the 2 KiB perf/12 section 1 budgets. The standard error
/// of a HyperLogLog at 2048 registers is 1.04/sqrt(2048), a little
/// over two percent, which is far inside what a selectivity needs.
const HLL_REGISTERS: usize = 2048;

/// One property column's statistics: how many rows it has, how many
/// distinct values, the most frequent of those values, and equi-depth
/// bucket boundaries over the rest (perf/12 section 1).
///
/// Values are the order-preserving byte keys of [`zu_common::int_key`]
/// for an integer column and the raw bytes for a string one, so a
/// boundary, a top value, and the literal a query compares against all
/// meet as bytes and the comparison means what the query means.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColStats {
    pub rows: u64,
    /// Distinct values, exact when the whole column was read and a
    /// sketch estimate when it was sampled. Never zero on a non-empty
    /// column, so `1/ndv` is always a selectivity.
    pub ndv: u64,
    /// The most frequent values with their row counts, most frequent
    /// first, at most [`TOP_VALUES`] of them. Values longer than
    /// [`VALUE_CAP`] are left out rather than truncated, because a
    /// truncated key would match a literal it is not equal to.
    pub top: Vec<(Vec<u8>, u64)>,
    /// Equi-depth boundaries, ascending, [`HIST_BUCKETS`] + 1 of them
    /// when the column has enough distinct values to fill them.
    /// Boundaries are truncated to [`VALUE_CAP`], so a boundary is a
    /// prefix of a real value and lands a hair below it, which moves a
    /// bucket edge and not a row count.
    pub bounds: Vec<Vec<u8>>,
}

/// The most frequent values one column keeps (perf/12 section 1).
pub const TOP_VALUES: usize = 64;

/// Equi-depth buckets one column keeps (perf/12 section 1).
pub const HIST_BUCKETS: usize = 64;

/// Longest value a column statistic stores.
pub const VALUE_CAP: usize = 32;

/// The statistics of one column, over the order-preserving byte keys
/// of its values in row order.
pub fn column_stats(keys: &[&[u8]]) -> ColStats {
    if keys.is_empty() {
        return ColStats::default();
    }
    let stride = keys.len().div_ceil(SAMPLE_CAP).max(1);
    let mut sample: Vec<&[u8]> = keys.iter().copied().step_by(stride).collect();
    sample.sort_unstable();

    // Runs of equal keys in the sample are the distinct values and
    // their counts; the count scales back up by the stride.
    let mut runs: Vec<(&[u8], u64)> = Vec::new();
    let mut i = 0;
    while i < sample.len() {
        let mut j = i;
        while j < sample.len() && sample[j] == sample[i] {
            j += 1;
        }
        runs.push((sample[i], (j - i) as u64 * stride as u64));
        i = j;
    }

    // Exact when nothing was skipped, sketched when something was.
    let ndv = if stride == 1 {
        runs.len() as u64
    } else {
        distinct_estimate(keys)
    };

    let mut top: Vec<(&[u8], u64)> = runs
        .iter()
        .copied()
        .filter(|(v, _)| v.len() <= VALUE_CAP)
        .collect();
    top.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    top.truncate(TOP_VALUES);

    let mut bounds = Vec::new();
    if runs.len() > HIST_BUCKETS {
        for b in 0..=HIST_BUCKETS {
            let at = (sample.len() - 1) * b / HIST_BUCKETS;
            let mut v = sample[at].to_vec();
            v.truncate(VALUE_CAP);
            bounds.push(v);
        }
        bounds.dedup();
    }

    ColStats {
        rows: keys.len() as u64,
        ndv: ndv.max(1).min(keys.len() as u64),
        top: top.into_iter().map(|(v, n)| (v.to_vec(), n)).collect(),
        bounds,
    }
}

/// A 64-bit hash with well-mixed high bits: FNV-1a, whose top bits
/// move slowly, run through the murmur3 finalizer, because the sketch
/// takes the register index off the top and the rank off the rest.
fn hash64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

/// Distinct values by HyperLogLog over every key, with linear counting
/// under the small-range threshold where the raw estimator is biased.
fn distinct_estimate(keys: &[&[u8]]) -> u64 {
    let p = HLL_REGISTERS.ilog2();
    let mut reg = vec![0u8; HLL_REGISTERS];
    for key in keys {
        let h = hash64(key);
        let idx = (h >> (64 - p)) as usize;
        let rank = ((h << p).leading_zeros() + 1).min(64 - p + 1) as u8;
        if rank > reg[idx] {
            reg[idx] = rank;
        }
    }
    let m = HLL_REGISTERS as f64;
    let zeros = reg.iter().filter(|&&r| r == 0).count();
    let harmonic: f64 = reg.iter().map(|&r| 2f64.powi(-i32::from(r))).sum();
    let alpha = 0.7213 / (1.0 + 1.079 / m);
    let raw = alpha * m * m / harmonic;
    if raw <= 2.5 * m && zeros > 0 {
        return (m * (m / zeros as f64).ln()).round() as u64;
    }
    raw.round() as u64
}

/// The log2 degree histogram of one direction, from an edge list
/// sorted by source: one run per source node, bucket `floor(log2 deg)`.
pub fn degree_histogram(sorted: &[(u32, u32)]) -> Vec<u64> {
    let mut hist = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let src = sorted[i].0;
        let mut j = i;
        while j < sorted.len() && sorted[j].0 == src {
            j += 1;
        }
        let bucket = (j - i).ilog2() as usize;
        if hist.len() <= bucket {
            hist.resize(bucket + 1, 0);
        }
        hist[bucket] += 1;
        i = j;
    }
    hist
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_by_log2_degree() {
        // Degrees: node 0 has 1, node 1 has 2, node 2 has 3, node 3
        // has 4. Buckets: [1] one node, [2,4) two nodes, [4,8) one.
        let mut edges = Vec::new();
        for (src, deg) in [(0u32, 1u32), (1, 2), (2, 3), (3, 4)] {
            for d in 0..deg {
                edges.push((src, d));
            }
        }
        assert_eq!(degree_histogram(&edges), [1, 2, 1]);
        assert_eq!(degree_histogram(&[]), [0u64; 0]);
    }

    #[test]
    fn stats_roundtrip_through_encode_and_decode() {
        let mut stats = Stats::default();
        stats.rels.insert(
            2,
            RelStats {
                out_hist: vec![5, 3, 0, 1],
                in_hist: vec![9],
                colors: Some(ColorSummary {
                    counts: vec![3, 6],
                    triples: vec![(0, 1, 9, 4), (1, 1, 2, 1)],
                }),
            },
        );
        stats.rels.insert(
            7,
            RelStats {
                out_hist: vec![],
                in_hist: vec![1, 1],
                colors: None,
            },
        );
        let decoded = Stats::decode(&stats.encode()).expect("decode");
        assert_eq!(decoded, stats);
    }

    #[test]
    fn version_one_payloads_still_carry_their_histograms() {
        // A version 1 chain as PR #67 wrote it: no color section.
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&4u32.to_le_bytes());
        payload.extend_from_slice(&2u32.to_le_bytes());
        for count in [7u64, 1] {
            payload.extend_from_slice(&count.to_le_bytes());
        }
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&8u64.to_le_bytes());
        let decoded = Stats::decode(&payload).expect("decode v1");
        let rel = &decoded.rels[&4];
        assert_eq!(rel.out_hist, [7, 1]);
        assert_eq!(rel.in_hist, [8]);
        assert_eq!(rel.colors, None);
    }

    #[test]
    fn bulk_load_writes_stats_the_next_open_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stats.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Out-degrees: node 0 has 3, node 1 has 1. In-degrees: nodes
        // 1, 2, 3, 4 have 1 each.
        let edges = [(0u32, 1u32), (0, 2), (0, 3), (1, 4)];
        crate::graph::bulk_load_as(&mut db, "person", "knows", 5, &edges).expect("load");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");
        let stats = Stats::load(&mut db).expect("stats");
        let rel = stats.rels.values().next().expect("one rel table");
        assert_eq!(rel.out_hist, [1, 1], "one degree 1 source, one in [2, 4)");
        assert_eq!(rel.in_hist, [4]);
    }

    #[test]
    fn a_small_column_gets_its_exact_distinct_count_and_no_buckets() {
        // Ten rows over three values: "a" six times, "b" three, "c" one.
        let mut keys: Vec<&[u8]> = vec![b"a"; 6];
        keys.extend_from_slice(&[b"b" as &[u8]; 3]);
        keys.push(b"c");
        let stat = column_stats(&keys);
        assert_eq!(stat.rows, 10);
        assert_eq!(stat.ndv, 3, "every row was read, so the count is exact");
        assert_eq!(
            stat.top,
            [(b"a".to_vec(), 6), (b"b".to_vec(), 3), (b"c".to_vec(), 1),],
            "most frequent first"
        );
        assert!(
            stat.bounds.is_empty(),
            "three values do not fill 64 buckets, and the top list already \
             holds all of them"
        );
        assert_eq!(column_stats(&[]), ColStats::default());
    }

    #[test]
    fn a_wide_column_gets_ascending_bucket_boundaries() {
        // 1000 distinct integers, each once, in an order that is not the
        // sorted one so the boundaries have to come from the sort.
        let raw: Vec<[u8; 8]> = (0..1000i64)
            .map(|i| zu_common::int_key(i * 7 % 1000))
            .collect();
        let keys: Vec<&[u8]> = raw.iter().map(|k| &k[..]).collect();
        let stat = column_stats(&keys);
        assert_eq!(stat.rows, 1000);
        assert_eq!(stat.ndv, 1000);
        assert_eq!(stat.top.len(), TOP_VALUES, "ties fill the list");
        assert_eq!(stat.bounds.len(), HIST_BUCKETS + 1);
        assert!(
            stat.bounds.windows(2).all(|w| w[0] < w[1]),
            "boundaries ascend"
        );
        assert_eq!(stat.bounds[0], zu_common::int_key(0));
        assert_eq!(stat.bounds[HIST_BUCKETS], zu_common::int_key(999));
    }

    #[test]
    fn a_value_longer_than_the_cap_stays_out_of_the_top_list() {
        // A truncated key would compare equal to a literal it is not
        // equal to, so the long value is dropped instead.
        let long = vec![b'x'; VALUE_CAP + 1];
        let mut keys: Vec<&[u8]> = vec![&long; 9];
        keys.push(b"short");
        let stat = column_stats(&keys);
        assert_eq!(stat.ndv, 2, "the long value still counts as distinct");
        assert_eq!(stat.top, [(b"short".to_vec(), 1)]);
    }

    #[test]
    fn the_distinct_sketch_lands_near_the_true_count() {
        // Both arms of the estimator: linear counting under 2.5 times
        // the register count, the raw estimator over it.
        for truth in [100usize, 2_000, 50_000] {
            let raw: Vec<[u8; 8]> = (0..truth as i64).map(zu_common::int_key).collect();
            let keys: Vec<&[u8]> = raw.iter().map(|k| &k[..]).collect();
            let got = distinct_estimate(&keys) as f64;
            let off = (got - truth as f64).abs() / truth as f64;
            assert!(off < 0.10, "{truth} distinct estimated as {got}");
        }
    }

    #[test]
    fn column_stats_survive_encode_and_decode() {
        let mut stats = Stats::default();
        let mut cols = BTreeMap::new();
        cols.insert(
            "gender".to_string(),
            ColStats {
                rows: 10295,
                ndv: 2,
                top: vec![(b"female".to_vec(), 5148), (b"male".to_vec(), 5147)],
                bounds: Vec::new(),
            },
        );
        cols.insert(
            "birthday".to_string(),
            ColStats {
                rows: 10295,
                ndv: 9000,
                top: Vec::new(),
                bounds: vec![vec![1, 2], vec![3, 4], vec![5, 6]],
            },
        );
        stats.cols.insert(3, cols);
        stats.cols.insert(9, BTreeMap::new());
        let decoded = Stats::decode(&stats.encode()).expect("decode");
        assert_eq!(decoded, stats);
    }

    #[test]
    fn version_two_payloads_decode_with_no_columns() {
        // A version 2 chain as PR #71 wrote it: one rel, no color
        // section, and nothing at all after it.
        let mut payload = Vec::new();
        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&4u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&7u64.to_le_bytes());
        // Empty in-degree histogram, no colors, no triples, and then
        // the payload just stops where version 3 would start counting
        // column tables.
        for _ in 0..3 {
            payload.extend_from_slice(&0u32.to_le_bytes());
        }
        let decoded = Stats::decode(&payload).expect("decode v2");
        assert_eq!(decoded.rels[&4].out_hist, [7]);
        assert!(decoded.cols.is_empty());
    }

    #[test]
    fn unknown_versions_and_garbage_do_not_decode() {
        let mut payload = Stats::default().encode();
        payload[0] = 9;
        assert!(Stats::decode(&payload).is_err());
        assert!(Stats::decode(&[1, 0]).is_err());
    }
}
