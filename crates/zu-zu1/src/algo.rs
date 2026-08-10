//! Whole-graph table function kernels: pagerank, wcc, sssp, louvain
//! (docs/07 §4, TableFunction). Each kernel runs directly on a rel
//! table's CSR through [`GraphReader`], one sequential sweep per
//! iteration so the per-direction group cache decodes every group once
//! per pass, and returns one value per node in dense row order. The
//! query layer surfaces these through CALL; nothing here parses or
//! plans.
//!
//! Determinism: every kernel sweeps nodes in row order with fixed
//! iteration or convergence rules, so the same file produces the same
//! output on every run and every machine, which is what lets tests and
//! the Graphalytics harness assert exact values.

use crate::file::Zu1File;
use crate::graph::{Direction, GraphReader};
use zu_common::Result;

/// The damping factor and iteration count Graphalytics fixes for
/// comparable runs.
pub const PAGERANK_DAMPING: f64 = 0.85;
pub const PAGERANK_ITERATIONS: usize = 20;

/// PageRank by power iteration over the forward CSR: fixed iteration
/// count, dangling mass redistributed uniformly, ranks summing to one.
pub fn pagerank(db: &mut Zu1File, reader: &mut GraphReader, iterations: usize) -> Result<Vec<f64>> {
    let n = reader.directory().node_count as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut outdeg = vec![0u64; n];
    for (node, deg) in outdeg.iter_mut().enumerate() {
        *deg = reader.neighbors_dir(db, node as u64, Direction::Fwd)?.len() as u64;
    }
    let mut rank = vec![1.0 / n as f64; n];
    let mut next = vec![0.0f64; n];
    for _ in 0..iterations {
        let mut dangling = 0.0;
        for (node, deg) in outdeg.iter().enumerate() {
            if *deg == 0 {
                dangling += rank[node];
            }
        }
        let base = (1.0 - PAGERANK_DAMPING + PAGERANK_DAMPING * dangling) / n as f64;
        next.iter_mut().for_each(|v| *v = base);
        for node in 0..n {
            if outdeg[node] == 0 {
                continue;
            }
            let share = PAGERANK_DAMPING * rank[node] / outdeg[node] as f64;
            for &dst in reader.neighbors_dir(db, node as u64, Direction::Fwd)? {
                next[dst as usize] += share;
            }
        }
        std::mem::swap(&mut rank, &mut next);
    }
    Ok(rank)
}

/// Weakly connected components over the undirected view of the rel
/// table: union-find over the forward lists, which name every edge
/// once. Each node's component id is the smallest row in it, the
/// Graphalytics convention.
pub fn wcc(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<u64>> {
    let n = reader.directory().node_count as usize;
    let mut parent: Vec<u64> = (0..n as u64).collect();
    fn find(parent: &mut [u64], mut x: u64) -> u64 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }
    for node in 0..n {
        let nbrs = reader.neighbors_dir(db, node as u64, Direction::Fwd)?;
        for &dst in nbrs {
            let (a, b) = (find(&mut parent, node as u64), find(&mut parent, dst));
            // Union by minimum keeps every root the smallest row seen,
            // so no relabeling pass is needed at the end.
            if a < b {
                parent[b as usize] = a;
            } else {
                parent[a as usize] = b;
            }
        }
    }
    Ok((0..n as u64).map(|x| find(&mut parent, x)).collect())
}

/// Single-source hop distances over the undirected view, a frontier
/// BFS. Rel tables carry no edge weights yet, so this is SSSP under
/// unit weights, which is also exactly the Graphalytics BFS kernel;
/// the weighted variant arrives with rel properties. Unreachable
/// nodes get `u64::MAX`.
pub fn sssp(db: &mut Zu1File, reader: &mut GraphReader, source: u64) -> Result<Vec<u64>> {
    let n = reader.directory().node_count;
    let mut dist = vec![u64::MAX; n as usize];
    if source >= n {
        return Ok(dist);
    }
    dist[source as usize] = 0;
    let mut frontier = vec![source];
    let mut depth = 0u64;
    while !frontier.is_empty() {
        depth += 1;
        let mut next = Vec::new();
        for &node in &frontier {
            for dir in [Direction::Fwd, Direction::Bwd] {
                for &far in reader.neighbors_dir(db, node, dir)? {
                    if dist[far as usize] == u64::MAX {
                        dist[far as usize] = depth;
                        next.push(far);
                    }
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        frontier = next;
    }
    Ok(dist)
}

/// Louvain community detection over the undirected view: repeated
/// local-move sweeps in row order until a full sweep moves nothing,
/// then one aggregation level, repeated until aggregation stops
/// merging. Communities are relabeled to the smallest member row so
/// the output is deterministic.
pub fn louvain(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<u64>> {
    let n = reader.directory().node_count as usize;
    // Undirected weighted adjacency in memory: forward lists name each
    // stored edge once, both endpoints get it. A self loop sits on one
    // list with weight two, the standard convention that keeps degree
    // sums and the aggregation's halving uniform.
    let mut adj: Vec<Vec<(u32, f64)>> = vec![Vec::new(); n];
    let mut scratch = Vec::new();
    for node in 0..n {
        scratch.clear();
        scratch.extend_from_slice(reader.neighbors_dir(db, node as u64, Direction::Fwd)?);
        for &dst in &scratch {
            let dst = dst as usize;
            if dst == node {
                adj[node].push((dst as u32, 2.0));
            } else {
                adj[node].push((dst as u32, 1.0));
                adj[dst].push((node as u32, 1.0));
            }
        }
    }
    // members[c] lists the original nodes inside supernode c of the
    // current level, so the final labels map back to rows.
    let mut members: Vec<Vec<u32>> = (0..n as u32).map(|v| vec![v]).collect();
    loop {
        let level = local_moves(&adj);
        let communities: usize = {
            let mut seen = vec![false; adj.len()];
            level.iter().for_each(|&c| seen[c as usize] = true);
            seen.iter().filter(|s| **s).count()
        };
        if communities == adj.len() {
            let mut labels = vec![0u64; n];
            for group in &members {
                let root = group.iter().min().copied().unwrap_or(0) as u64;
                for &v in group {
                    labels[v as usize] = root;
                }
            }
            // The last level moved nothing, so each supernode is its
            // own community and members already holds the answer.
            return Ok(labels);
        }
        // Aggregate: one supernode per community, edge weights summed,
        // community ids compacted in first-seen row order.
        let mut compact = std::collections::BTreeMap::new();
        for &c in &level {
            let id = compact.len() as u32;
            compact.entry(c).or_insert(id);
        }
        let m = compact.len();
        let mut next_adj: Vec<std::collections::BTreeMap<u32, f64>> = vec![Default::default(); m];
        for (node, list) in adj.iter().enumerate() {
            let a = compact[&level[node]];
            for &(dst, w) in list {
                let b = compact[&level[dst as usize]];
                // Each undirected edge appears in both lists; halve on
                // aggregation so weights stay in stored-edge units.
                *next_adj[a as usize].entry(b).or_default() += w / 2.0;
            }
        }
        let mut next_members: Vec<Vec<u32>> = vec![Vec::new(); m];
        for (node, group) in members.iter().enumerate() {
            next_members[compact[&level[node]] as usize].extend_from_slice(group);
        }
        adj = next_adj
            .into_iter()
            .enumerate()
            .map(|(a, row)| {
                row.into_iter()
                    .flat_map(|(b, w)| {
                        // Restore the two-sided representation, self
                        // loops single-sided with doubled weight.
                        if b as usize == a {
                            vec![(b, w * 2.0)]
                        } else {
                            vec![(b, w)]
                        }
                    })
                    .collect()
            })
            .collect();
        members = next_members;
    }
}

/// One Louvain level: sweeps nodes in row order, moving each to the
/// neighbor community with the best positive modularity gain, until a
/// full sweep moves nothing. Returns each node's community.
fn local_moves(adj: &[Vec<(u32, f64)>]) -> Vec<u32> {
    let n = adj.len();
    let two_m: f64 = adj
        .iter()
        .map(|l| l.iter().map(|(_, w)| w).sum::<f64>())
        .sum();
    if two_m == 0.0 {
        return (0..n as u32).collect();
    }
    let degree: Vec<f64> = adj.iter().map(|l| l.iter().map(|(_, w)| w).sum()).collect();
    let mut community: Vec<u32> = (0..n as u32).collect();
    let mut total: Vec<f64> = degree.clone();
    let mut moved = true;
    while moved {
        moved = false;
        for node in 0..n {
            let own = community[node];
            // Self loops move with the node and cancel in every gain
            // comparison, so only real neighbors count as links.
            let mut links: std::collections::BTreeMap<u32, f64> = Default::default();
            for &(dst, w) in &adj[node] {
                if dst as usize != node {
                    *links.entry(community[dst as usize]).or_default() += w;
                }
            }
            total[own as usize] -= degree[node];
            let stay = links.get(&own).copied().unwrap_or(0.0);
            let gain = |c: u32, link: f64| link - total[c as usize] * degree[node] / two_m;
            let mut best = (own, gain(own, stay));
            for (&c, &link) in &links {
                let g = gain(c, link);
                // Strictly better only, so ties keep the smallest
                // community id BTreeMap order visits first.
                if g > best.1 + 1e-12 {
                    best = (c, g);
                }
            }
            total[best.0 as usize] += degree[node];
            if best.0 != own {
                community[node] = best.0;
                moved = true;
            }
        }
    }
    community
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::bulk_load_as;

    fn open_with(edges: &[(u32, u32)], nodes: u64) -> (tempfile::TempDir, Zu1File, GraphReader) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("algo.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut sorted = edges.to_vec();
        sorted.sort_unstable();
        bulk_load_as(&mut db, "n", "e", nodes, &sorted).expect("load");
        let reader = GraphReader::load(&mut db).expect("reader");
        (dir, db, reader)
    }

    #[test]
    fn pagerank_ranks_the_sink_of_a_chain_highest() {
        // 0 -> 1 -> 2: rank flows down the chain and 2 absorbs the
        // most, with the exact closed form checkable per iteration.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (1, 2)], 3);
        let ranks = pagerank(&mut db, &mut reader, 20).expect("pagerank");
        assert!(
            (ranks.iter().sum::<f64>() - 1.0).abs() < 1e-9,
            "ranks sum to one"
        );
        assert!(ranks[2] > ranks[1] && ranks[1] > ranks[0], "{ranks:?}");
    }

    #[test]
    fn wcc_labels_components_by_their_smallest_row() {
        // Two components, 0-1-2 and 3-4, direction ignored.
        let (_dir, mut db, mut reader) = open_with(&[(1, 0), (1, 2), (4, 3)], 5);
        let labels = wcc(&mut db, &mut reader).expect("wcc");
        assert_eq!(labels, [0, 0, 0, 3, 3]);
    }

    #[test]
    fn sssp_walks_both_directions_and_marks_the_unreachable() {
        // 0 -> 1 <- 2, 3 isolated: from 0 the undirected view reaches
        // 2 in two hops.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (2, 1)], 4);
        let dist = sssp(&mut db, &mut reader, 0).expect("sssp");
        assert_eq!(dist, [0, 1, 2, u64::MAX]);
    }

    #[test]
    fn sssp_from_an_out_of_range_source_reaches_nothing() {
        let (_dir, mut db, mut reader) = open_with(&[(0, 1)], 2);
        let dist = sssp(&mut db, &mut reader, 9).expect("sssp");
        assert_eq!(dist, [u64::MAX, u64::MAX]);
    }

    #[test]
    fn louvain_splits_two_cliques_on_their_bridge() {
        // Two 4-cliques joined by one edge: the canonical two
        // communities, labeled by smallest member.
        let mut edges = Vec::new();
        for group in [0u32, 4] {
            for i in group..group + 4 {
                for j in i + 1..group + 4 {
                    edges.push((i, j));
                }
            }
        }
        edges.push((3, 4));
        let (_dir, mut db, mut reader) = open_with(&edges, 8);
        let labels = louvain(&mut db, &mut reader).expect("louvain");
        assert_eq!(labels[..4], [0, 0, 0, 0]);
        assert_eq!(labels[4..], [4, 4, 4, 4]);
    }

    #[test]
    fn empty_graphs_run_every_kernel_cleanly() {
        let (_dir, mut db, mut reader) = open_with(&[], 0);
        assert!(
            pagerank(&mut db, &mut reader, 5)
                .expect("pagerank")
                .is_empty()
        );
        assert!(wcc(&mut db, &mut reader).expect("wcc").is_empty());
        assert!(sssp(&mut db, &mut reader, 0).expect("sssp").is_empty());
        assert!(louvain(&mut db, &mut reader).expect("louvain").is_empty());
    }
}
