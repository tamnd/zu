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

const VERSION: u32 = 2;

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

/// Every rel table's statistics, keyed by catalog table id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub rels: BTreeMap<u32, RelStats>,
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
        if version != 1 && version != VERSION {
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
        Ok(Stats { rels })
    }
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
    fn unknown_versions_and_garbage_do_not_decode() {
        let mut payload = Stats::default().encode();
        payload[0] = 9;
        assert!(Stats::decode(&payload).is_err());
        assert!(Stats::decode(&[1, 0]).is_err());
    }
}
