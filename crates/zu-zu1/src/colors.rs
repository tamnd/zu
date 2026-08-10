//! COLOR summaries: quasi-stable coloring for cardinality estimation
//! (docs/07 §6). Nodes split into at most `cap` colors so that nodes
//! sharing a color have near-identical per-color degree profiles, then
//! each rel table stores one sparse summary per color pair: the edge
//! count and the largest single-node degree across the pair. The
//! optimizer walks these matrices to estimate multi-hop cardinalities
//! where a global mean cannot see that hubs dominate a frontier.
//!
//! The coloring is iterated signature refinement. A node's signature is
//! its neighbor-count per color in both directions, bucketed by log2 so
//! nodes within a factor of two of each other stay together; that
//! tolerance is what makes the coloring quasi-stable rather than the
//! exponentially large stable refinement. Refinement stops at the
//! fixpoint or right before a split would exceed the cap, keeping the
//! last coloring that fit.

use std::collections::BTreeMap;

use zu_common::Result;

use crate::catalog::Catalog;
use crate::file::Zu1File;
use crate::graph::{Direction, GraphReader, free_chain};
use crate::stats::{COLOR_CAP, Stats};

/// The sparse per-color-pair summary of one rel table's edges.
/// `counts[c]` is the number of nodes holding color `c`; each triple is
/// `(from_color, to_color, edge_count, max_degree)` where `max_degree`
/// is the largest number of edges one source in `from_color` holds into
/// `to_color`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColorSummary {
    pub counts: Vec<u64>,
    pub triples: Vec<(u32, u32, u64, u64)>,
}

/// Colors `n` nodes over `edges` (sorted by source) into at most `cap`
/// colors. Returns one color per node, colors dense from zero and
/// assigned in first-seen node order, so the result is deterministic.
pub fn quasi_stable_coloring(n: u32, edges: &[(u32, u32)], cap: usize) -> Vec<u32> {
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let mut colors = vec![0u32; n as usize];
    if n == 0 {
        return colors;
    }
    let mut palette = 1usize;
    // Each round recolors every node by (old color, bucketed per-color
    // neighbor counts both ways). The palette only grows, so the loop
    // ends at a fixpoint or at the cap; log2 buckets keep rounds few.
    loop {
        let mut sig = vec![(0u32, Vec::new()); n as usize];
        for (node, s) in sig.iter_mut().enumerate() {
            s.0 = colors[node];
        }
        for (dir, list) in [(0u8, edges), (1u8, &rev[..])] {
            let mut i = 0;
            while i < list.len() {
                let src = list[i].0 as usize;
                let mut per_color: BTreeMap<u32, u64> = BTreeMap::new();
                while i < list.len() && list[i].0 as usize == src {
                    *per_color.entry(colors[list[i].1 as usize]).or_default() += 1;
                    i += 1;
                }
                for (color, count) in per_color {
                    sig[src].1.push((dir, color, count.ilog2()));
                }
            }
        }
        let mut next = BTreeMap::new();
        let mut recolored = vec![0u32; n as usize];
        for node in 0..n as usize {
            let id = next.len() as u32;
            recolored[node] = *next.entry(sig[node].clone()).or_insert(id);
        }
        if next.len() == palette {
            return colors;
        }
        if next.len() > cap {
            return colors;
        }
        palette = next.len();
        colors = recolored;
    }
}

/// Builds the sparse summary of `edges` (sorted by source) under
/// `colors`, one run-length pass per source.
pub fn summarize(colors: &[u32], edges: &[(u32, u32)]) -> ColorSummary {
    let mut counts = Vec::new();
    for &c in colors {
        let c = c as usize;
        if counts.len() <= c {
            counts.resize(c + 1, 0);
        }
        counts[c] += 1;
    }
    let mut pairs: BTreeMap<(u32, u32), (u64, u64)> = BTreeMap::new();
    let mut i = 0;
    while i < edges.len() {
        let src = edges[i].0;
        let mut per_color: BTreeMap<u32, u64> = BTreeMap::new();
        while i < edges.len() && edges[i].0 == src {
            *per_color.entry(colors[edges[i].1 as usize]).or_default() += 1;
            i += 1;
        }
        for (to_color, deg) in per_color {
            let entry = pairs.entry((colors[src as usize], to_color)).or_default();
            entry.0 += deg;
            entry.1 = entry.1.max(deg);
        }
    }
    ColorSummary {
        counts,
        triples: pairs
            .into_iter()
            .map(|((f, t), (edges, dmax))| (f, t, edges, dmax))
            .collect(),
    }
}

/// ANALYZE: rebuilds every rel table's COLOR summary from its stored
/// CSR and publishes them in the stats chain with a checkpoint. The
/// degree histograms written at load stay as they are. Coloring reads
/// each forward group once in row order, so the pass is one sequential
/// sweep per table plus the in-memory refinement.
pub fn analyze(db: &mut Zu1File) -> Result<()> {
    let catalog = Catalog::load(db)?;
    let mut stats = Stats::load(db)?;
    for rel in catalog.rel_tables() {
        let mut reader = GraphReader::load_table(db, &rel.name)?;
        let n = reader.directory().node_count;
        if n > u32::MAX as u64 {
            continue;
        }
        let mut edges = Vec::with_capacity(reader.directory().edge_count as usize);
        for node in 0..n {
            for &dst in reader.neighbors_dir(db, node, Direction::Fwd)? {
                edges.push((node as u32, dst as u32));
            }
        }
        let colors = quasi_stable_coloring(n as u32, &edges, COLOR_CAP);
        let summary = summarize(&colors, &edges);
        stats.rels.entry(rel.id).or_default().colors = Some(summary);
    }
    free_chain(db, db.db_header().stats_root)?;
    stats.store(db)?;
    db.checkpoint()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A star: node 0 points at 1..=4, which point nowhere.
    fn star() -> Vec<(u32, u32)> {
        (1..=4).map(|d| (0, d)).collect()
    }

    #[test]
    fn coloring_separates_the_hub_from_the_spokes() {
        let colors = quasi_stable_coloring(5, &star(), 1024);
        assert_ne!(colors[0], colors[1], "hub and spoke profiles differ");
        assert_eq!(&colors[1..], [colors[1]; 4], "spokes are identical");
    }

    #[test]
    fn the_cap_keeps_the_last_coloring_that_fit() {
        // Degrees 0..8 give distinct signatures past any log2 bucket
        // for at least four colors; a cap of 2 must stop the first
        // split that would pass it and keep what fit.
        let mut edges = Vec::new();
        for src in 0u32..8 {
            for d in 0..src {
                edges.push((src, 8 + d));
            }
        }
        edges.sort_unstable();
        let capped = quasi_stable_coloring(16, &edges, 2);
        let max = capped.iter().max().copied().unwrap_or(0) as usize;
        assert!(max < 2, "{} colors exceed the cap", max + 1);
        let free = quasi_stable_coloring(16, &edges, 1024);
        let free_max = free.iter().max().copied().unwrap() as usize;
        assert!(free_max + 1 > 2, "uncapped refinement splits further");
    }

    #[test]
    fn summaries_count_edges_and_track_the_worst_source() {
        let colors = quasi_stable_coloring(5, &star(), 1024);
        let summary = summarize(&colors, &star());
        assert_eq!(summary.counts.iter().sum::<u64>(), 5);
        assert_eq!(summary.triples.len(), 1, "hub color into spoke color");
        let (f, t, edges, dmax) = summary.triples[0];
        assert_eq!((f, t), (colors[0], colors[1]));
        assert_eq!((edges, dmax), (4, 4));
    }

    #[test]
    fn analyze_stores_summaries_the_next_open_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("colors.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let edges = star();
        crate::graph::bulk_load_as(&mut db, "person", "knows", 5, &edges).expect("load");
        analyze(&mut db).expect("analyze");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");
        let stats = Stats::load(&mut db).expect("stats");
        let rel = stats.rels.values().next().expect("one rel table");
        assert_eq!(rel.in_hist, [4], "histograms survive the analyze");
        let summary = rel.colors.as_ref().expect("summary stored");
        assert_eq!(summary.counts.iter().sum::<u64>(), 5);
        let [(_, _, edge_count, dmax)] = summary.triples[..] else {
            panic!("one color pair, got {:?}", summary.triples);
        };
        assert_eq!((edge_count, dmax), (4, 4));
    }

    #[test]
    fn empty_graphs_color_and_summarize_cleanly() {
        assert!(quasi_stable_coloring(0, &[], 1024).is_empty());
        let summary = summarize(&[0, 0], &[]);
        assert_eq!(summary.counts, [2]);
        assert!(summary.triples.is_empty());
    }
}
