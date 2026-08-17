//! Whole-graph table function kernels: bfs, pagerank, wcc, sssp, cdlp,
//! lcc, louvain (docs/07 §4, TableFunction). Each kernel runs directly
//! on a rel table's CSR through [`GraphReader`], one sequential sweep per
//! iteration so the per-direction group cache decodes every group once
//! per pass, and returns one value per node in dense row order. The
//! query layer surfaces these through CALL; nothing here parses or
//! plans.
//!
//! The traversal kernels share one frontier: a visited bitmap, rounds
//! that pin a group's CSR once and expand every frontier node in it
//! from the pinned arrays, and a direction-optimizing switch that goes
//! bottom-up when the frontier is about to touch more edges than are
//! left unexplored.
//!
//! Determinism: every kernel sweeps nodes in row order with fixed
//! iteration or convergence rules, so the same file produces the same
//! output on every run and every machine, which is what lets tests and
//! the Graphalytics harness assert exact values. The frontier is no
//! exception: a node's level is the round it was first claimed in, and
//! rounds are barriers, so which direction a round ran cannot move it.

use crate::file::Zu1File;
use crate::graph::{Direction, GraphReader};
use zu_common::{GROUP_ROWS, Result};

/// A frontier about to touch more than this fraction of the edges
/// still unexplored is cheaper scanned from the other end. GAP's
/// alpha: the classic direction-optimizing tuning constant, and the
/// one docs/06 fixes.
const ALPHA: u64 = 14;

/// Coming back the other way is a separate decision with its own
/// constant: a frontier holding under `n / BETA` nodes is sparse
/// enough that scanning every unvisited node again would waste the
/// pass. GAP's beta.
const BETA: u64 = 24;

/// Which adjacency a traversal walks. `Out` follows stored edge
/// direction, which is what BFS levels mean on a directed graph.
/// `Both` treats the rel table as undirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    Out,
    Both,
}

impl Walk {
    /// The directions a top-down round expands.
    fn forward(self) -> &'static [Direction] {
        match self {
            Walk::Out => &[Direction::Fwd],
            Walk::Both => &[Direction::Fwd, Direction::Bwd],
        }
    }

    /// The directions a bottom-up round scans, which is the reverse of
    /// what it would have expanded: a node is claimed this round when
    /// an edge that would have arrived at it starts in the frontier.
    fn backward(self) -> &'static [Direction] {
        match self {
            Walk::Out => &[Direction::Bwd],
            Walk::Both => &[Direction::Fwd, Direction::Bwd],
        }
    }
}

fn word_and_bit(node: u64) -> (usize, u64) {
    ((node / 64) as usize, 1u64 << (node % 64))
}

/// The group a row sits in and its offset inside that group.
fn locate(node: u64) -> (usize, usize) {
    (
        (node / GROUP_ROWS as u64) as usize,
        (node % GROUP_ROWS as u64) as usize,
    )
}

/// The damping factor and iteration count Graphalytics fixes for
/// comparable runs.
pub const PAGERANK_DAMPING: f64 = 0.85;
pub const PAGERANK_ITERATIONS: usize = 20;

/// The round count Graphalytics fixes for CDLP, so two runs of the
/// same graph are the same partition however far propagation has left
/// to go.
pub const CDLP_ROUNDS: usize = 10;

/// PageRank by power iteration over the forward CSR: fixed iteration
/// count, dangling mass redistributed uniformly, ranks summing to one.
pub fn pagerank(db: &mut Zu1File, reader: &mut GraphReader, iterations: usize) -> Result<Vec<f64>> {
    let n = reader.directory().one_domain()? as usize;
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
    let n = reader.directory().one_domain()? as usize;
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

/// Single-source hop distances over the undirected view. Rel tables
/// carry no edge weights yet, so this is SSSP under unit weights; the
/// weighted variant arrives with rel properties. Unreachable nodes get
/// `u64::MAX`.
pub fn sssp(db: &mut Zu1File, reader: &mut GraphReader, source: u64) -> Result<Vec<u64>> {
    levels(db, reader, source, Walk::Both)
}

/// Breadth-first levels from `source` following stored edge direction,
/// the Graphalytics and Graph500 BFS kernel. Unreachable nodes get
/// `u64::MAX`; the source is level zero.
pub fn bfs(db: &mut Zu1File, reader: &mut GraphReader, source: u64) -> Result<Vec<u64>> {
    levels(db, reader, source, Walk::Out)
}

/// The frontier both traversal kernels run on: a visited bitmap, one
/// group pin per group per round, and a per-round choice between
/// expanding the frontier and scanning for it.
///
/// The bitmap is the memory story. Claiming through `dist` would mean
/// eight bytes a node in the hot set; a bit a node is sixty four times
/// less, which is the difference between a working set that fits in
/// cache on a ten million node graph and one that does not.
fn levels(db: &mut Zu1File, reader: &mut GraphReader, source: u64, walk: Walk) -> Result<Vec<u64>> {
    let n = reader.directory().one_domain()?;
    let mut dist = vec![u64::MAX; n as usize];
    if source >= n {
        return Ok(dist);
    }
    let words = (n as usize).div_ceil(64);
    let mut visited = vec![0u64; words];
    let (word, bit) = word_and_bit(source);
    visited[word] |= bit;
    dist[source as usize] = 0;

    // The unexplored edge count the alpha rule compares against. Every
    // stored edge is one traversal in Out and two in Both, which is
    // what makes the same rule fit both walks.
    let stored = reader.directory().edge_count;
    let mut unexplored = match walk {
        Walk::Out => stored,
        Walk::Both => stored * 2,
    };

    let mut frontier = vec![source];
    let mut frontier_bits: Vec<u64> = Vec::new();
    let mut depth = 0u64;
    let mut bottom_up = false;
    while !frontier.is_empty() {
        depth += 1;
        // The switch is decided per round, from this round's frontier.
        // Going down: the frontier is about to read mf edges and only
        // mu are left to find anything in, so scanning from the other
        // end reads less. Coming back up: the frontier has thinned to
        // where a full scan costs more than the expansion.
        frontier.sort_unstable();
        if bottom_up {
            bottom_up = (frontier.len() as u64) > n / BETA;
        } else {
            let mut mf = 0u64;
            for &dir in walk.forward() {
                mf += reader.degree_batch(db, &frontier, dir)?;
            }
            bottom_up = mf > unexplored / ALPHA;
        }
        let next = if bottom_up {
            frontier_bits.clear();
            frontier_bits.resize(words, 0);
            for &v in &frontier {
                let (word, bit) = word_and_bit(v);
                frontier_bits[word] |= bit;
            }
            scan_round(db, reader, n, &frontier_bits, &mut visited, walk)?
        } else {
            expand_round(db, reader, &frontier, &mut visited, walk, &mut unexplored)?
        };
        for &v in &next {
            dist[v as usize] = depth;
        }
        frontier = next;
    }
    Ok(dist)
}

/// One top-down round. The frontier arrives sorted, so nodes of the
/// same group are one run: the run pins that group's CSR once and
/// reads every list in it out of the pinned arrays, rather than
/// letting each node decode a group of its own.
fn expand_round(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    frontier: &[u64],
    visited: &mut [u64],
    walk: Walk,
    unexplored: &mut u64,
) -> Result<Vec<u64>> {
    let mut next = Vec::new();
    for &dir in walk.forward() {
        let mut at = 0;
        while at < frontier.len() {
            let (group, _) = locate(frontier[at]);
            let mut end = at + 1;
            while end < frontier.len() && locate(frontier[end]).0 == group {
                end += 1;
            }
            let (offsets, nbrs) = reader.csr_group(db, group, dir)?;
            for &v in &frontier[at..end] {
                let (_, row) = locate(v);
                let (lo, hi) = (offsets[row] as usize, offsets[row + 1] as usize);
                *unexplored = unexplored.saturating_sub((hi - lo) as u64);
                for &far in &nbrs[lo..hi] {
                    let (word, bit) = word_and_bit(far);
                    if visited[word] & bit == 0 {
                        visited[word] |= bit;
                        next.push(far);
                    }
                }
            }
            at = end;
        }
    }
    Ok(next)
}

/// One bottom-up round: every unvisited node scans the edges arriving
/// at it and stops at the first one that starts in the frontier. On a
/// round where the frontier holds most of the graph this reads a
/// fraction of the lists a top-down round would, because a node needs
/// one hit and not the whole list.
///
/// Groups are the unit here too, in row order, so the pass decodes
/// each group's arrays once and node order stays the file's.
fn scan_round(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    n: u64,
    frontier_bits: &[u64],
    visited: &mut [u64],
    walk: Walk,
) -> Result<Vec<u64>> {
    let mut next = Vec::new();
    let groups = (n as usize).div_ceil(GROUP_ROWS as usize);
    let dirs = walk.backward();
    let mut pinned = Vec::with_capacity(dirs.len());
    for group in 0..groups {
        pinned.clear();
        for &dir in dirs {
            pinned.push(reader.csr_group(db, group, dir)?);
        }
        let first = group as u64 * GROUP_ROWS as u64;
        let rows = (n - first).min(GROUP_ROWS as u64) as usize;
        for row in 0..rows {
            let node = first + row as u64;
            let (word, bit) = word_and_bit(node);
            if visited[word] & bit != 0 {
                continue;
            }
            let hit = pinned.iter().any(|(offsets, nbrs)| {
                let (lo, hi) = (offsets[row] as usize, offsets[row + 1] as usize);
                nbrs[lo..hi].iter().any(|&near| {
                    let (w, b) = word_and_bit(near);
                    frontier_bits[w] & b != 0
                })
            });
            if hit {
                visited[word] |= bit;
                next.push(node);
            }
        }
    }
    Ok(next)
}

/// Community detection by label propagation: `rounds` synchronous
/// sweeps, each node taking the label most common among its neighbors
/// in both directions, a bidirectional pair voting once per direction,
/// the smallest label winning a tie. A node with no neighbors keeps
/// what it has.
///
/// Labels start at the node's original key rather than at its row,
/// because the tie-break reads label values and a graph loaded with
/// `REORDER` has rows the dataset never named. That makes the answer a
/// statement about ids, which is what the caller asked, and it cannot
/// be repaired after the fact the way a component id can: relabeling a
/// finished run would have to undo ties that were already decided the
/// wrong way. A file with no key index is its own key space, so rows
/// serve.
pub fn cdlp(db: &mut Zu1File, reader: &mut GraphReader, rounds: usize) -> Result<Vec<u64>> {
    let n = reader.directory().one_domain()? as usize;
    let index = reader.directory().keys.clone();
    let mut label = match index {
        Some(index) => crate::keys::key_by_row(db, &index)?,
        None => (0..n as u64).collect(),
    };
    if label.len() != n {
        return Err(zu_common::ZuError::Corrupt {
            what: "key index",
            detail: format!("{} keys over {n} nodes", label.len()),
        });
    }
    let mut next = label.clone();
    // One scratch vector for the whole sweep: the votes of one node are
    // read and forgotten inside its iteration, and a fresh allocation
    // per node would be the kernel's largest cost on a sparse graph.
    let mut votes: Vec<u64> = Vec::new();
    for _ in 0..rounds {
        for node in 0..n {
            votes.clear();
            for dir in [Direction::Fwd, Direction::Bwd] {
                for &far in reader.neighbors_dir(db, node as u64, dir)? {
                    votes.push(label[far as usize]);
                }
            }
            if votes.is_empty() {
                next[node] = label[node];
                continue;
            }
            // Sorting groups equal labels, so the winner is the longest
            // run. Scanning ascending and taking a new best only on a
            // strictly longer run is the smallest-label tie-break.
            votes.sort_unstable();
            let (mut best, mut best_len) = (votes[0], 0usize);
            let (mut run, mut run_len) = (votes[0], 0usize);
            for &v in &votes {
                if v == run {
                    run_len += 1;
                    continue;
                }
                if run_len > best_len {
                    (best, best_len) = (run, run_len);
                }
                (run, run_len) = (v, 1);
            }
            if run_len > best_len {
                best = run;
            }
            next[node] = best;
        }
        std::mem::swap(&mut label, &mut next);
    }
    Ok(label)
}

/// Local clustering coefficient over the directed graph: for a node
/// whose neighbor set is the union of its out and in neighbors without
/// itself, the number of stored edges running from one neighbor to
/// another over `d * (d - 1)`, the count of ordered pairs. Fewer than
/// two neighbors scores zero, there being no pair to close.
///
/// Parallel edges count as many times as they are stored, and a self
/// loop on a neighbor closes nothing, which is what the Graphalytics
/// reference counts.
pub fn lcc(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<f64>> {
    let n = reader.directory().one_domain()? as usize;
    let mut coeff = Vec::with_capacity(n);
    let mut nbrs: Vec<u64> = Vec::new();
    // Membership as a dense flag array rather than a search of the
    // neighbor list: the inner loop asks the question once per edge out
    // of the neighborhood, and the array is cleared through the same
    // list that set it, so the cost stays with the node's degree rather
    // than with the graph.
    let mut member = vec![false; n];
    for node in 0..n {
        nbrs.clear();
        for dir in [Direction::Fwd, Direction::Bwd] {
            nbrs.extend_from_slice(reader.neighbors_dir(db, node as u64, dir)?);
        }
        nbrs.retain(|&far| far != node as u64);
        nbrs.sort_unstable();
        nbrs.dedup();
        let d = nbrs.len();
        if d < 2 {
            coeff.push(0.0);
            continue;
        }
        for &far in &nbrs {
            member[far as usize] = true;
        }
        let mut links = 0u64;
        for &near in &nbrs {
            for &far in reader.neighbors_dir(db, near, Direction::Fwd)? {
                if far != near && member[far as usize] {
                    links += 1;
                }
            }
        }
        for &far in &nbrs {
            member[far as usize] = false;
        }
        coeff.push(links as f64 / (d as f64 * (d as f64 - 1.0)));
    }
    Ok(coeff)
}

/// Louvain community detection over the undirected view: repeated
/// local-move sweeps in row order until a full sweep moves nothing,
/// then one aggregation level, repeated until aggregation stops
/// merging. Communities are relabeled to the smallest member row so
/// the output is deterministic.
pub fn louvain(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<u64>> {
    let n = reader.directory().one_domain()? as usize;
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
    fn bfs_follows_edge_direction_where_sssp_ignores_it() {
        // 0 -> 1 <- 2: following the arrows from 0 stops at 1, while
        // the undirected view walks back out of 1 and reaches 2.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (2, 1)], 4);
        assert_eq!(
            bfs(&mut db, &mut reader, 0).expect("bfs"),
            [0, 1, u64::MAX, u64::MAX]
        );
        assert_eq!(
            sssp(&mut db, &mut reader, 0).expect("sssp"),
            [0, 1, 2, u64::MAX]
        );
    }

    #[test]
    fn the_scan_round_finds_what_the_expand_round_would_have() {
        // A star with a hub big enough that the round after the hub
        // has a frontier over the alpha threshold, so this graph takes
        // the bottom-up path and has to agree with the levels a plain
        // expansion gives: hub at one, every other spoke at two.
        let mut edges = vec![(0u32, 1u32)];
        for spoke in 2..600u32 {
            edges.push((1, spoke));
        }
        let (_dir, mut db, mut reader) = open_with(&edges, 600);
        let got = sssp(&mut db, &mut reader, 0).expect("sssp");
        assert_eq!(got[0], 0);
        assert_eq!(got[1], 1);
        assert!(got[2..].iter().all(|&d| d == 2), "{:?}", &got[2..10]);
    }

    #[test]
    fn a_ring_levels_the_same_both_ways_round() {
        // Every node of a ring is reachable in both directions, so the
        // undirected levels are the distance to the nearer arc and the
        // directed ones go the whole way around. Neither is allowed to
        // depend on which round switched direction.
        let n = 400u32;
        let edges: Vec<(u32, u32)> = (0..n).map(|v| (v, (v + 1) % n)).collect();
        let (_dir, mut db, mut reader) = open_with(&edges, n as u64);
        let both = sssp(&mut db, &mut reader, 0).expect("sssp");
        let out = bfs(&mut db, &mut reader, 0).expect("bfs");
        for v in 0..n as usize {
            let forward = v as u64;
            assert_eq!(out[v], forward, "directed level of {v}");
            assert_eq!(
                both[v],
                forward.min(n as u64 - forward),
                "undirected level of {v}"
            );
        }
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
    fn cdlp_settles_a_clique_on_its_smallest_label() {
        // A 4-clique and an isolated node: every member sees the same
        // three labels once each, so the tie-break decides, and after
        // one round of that the whole clique agrees on 0. The isolate
        // has nobody to hear from and stays itself.
        let mut edges = Vec::new();
        for i in 0u32..4 {
            for j in i + 1..4 {
                edges.push((i, j));
            }
        }
        let (_dir, mut db, mut reader) = open_with(&edges, 5);
        let labels = cdlp(&mut db, &mut reader, CDLP_ROUNDS).expect("cdlp");
        assert_eq!(labels, [0, 0, 0, 0, 4]);
    }

    #[test]
    fn cdlp_counts_a_bidirectional_pair_once_per_direction() {
        // 1 -> 0 and 0 -> 1 sit on both of 0's lists, so label 1 votes
        // twice against 2's single vote from 2 -> 0, and 0 takes 1
        // rather than the smaller label a single count would hand it.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (1, 0), (2, 0)], 3);
        let labels = cdlp(&mut db, &mut reader, 1).expect("cdlp");
        assert_eq!(labels[0], 1);
    }

    #[test]
    fn cdlp_propagates_the_keys_a_reordered_load_stored() {
        // Rows 0..3 hold keys 30, 10, 20, 40: the clique 0-1-2 agrees
        // on the smallest key among them, 10, which is no row's number.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = Zu1File::create(&dir.path().join("keyed.zu1")).expect("create");
        let edges = [(0u32, 1u32), (0, 2), (1, 2)];
        crate::graph::bulk_load_keyed(&mut db, "n", "e", 4, &edges, Some(&[30, 10, 20, 40]))
            .expect("load");
        let mut reader = GraphReader::load(&mut db).expect("reader");
        let labels = cdlp(&mut db, &mut reader, CDLP_ROUNDS).expect("cdlp");
        assert_eq!(labels, [10, 10, 10, 40]);
    }

    #[test]
    fn lcc_scores_a_closed_triangle_one_and_an_open_wedge_zero() {
        // 0's neighbors are 1 and 2 in both graphs; the edge 1 -> 2
        // closes one of the two ordered pairs in the first and none in
        // the second. Nodes 3 and 4 have too few neighbors to score.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2), (1, 2), (3, 4)], 5);
        let coeff = lcc(&mut db, &mut reader).expect("lcc");
        assert_eq!(coeff[0], 0.5);
        assert_eq!(coeff[3], 0.0);
        assert_eq!(coeff[4], 0.0);
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2)], 3);
        assert_eq!(lcc(&mut db, &mut reader).expect("lcc")[0], 0.0);
    }

    #[test]
    fn lcc_ignores_a_self_loop_on_a_neighbor() {
        // 1 -> 1 runs between two neighbors of 0 only if a node counts
        // as its own neighbor, which the LDBC definition denies.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2), (1, 1)], 3);
        assert_eq!(lcc(&mut db, &mut reader).expect("lcc")[0], 0.0);
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
        assert!(bfs(&mut db, &mut reader, 0).expect("bfs").is_empty());
        assert!(
            cdlp(&mut db, &mut reader, CDLP_ROUNDS)
                .expect("cdlp")
                .is_empty()
        );
        assert!(lcc(&mut db, &mut reader).expect("lcc").is_empty());
        assert!(louvain(&mut db, &mut reader).expect("louvain").is_empty());
    }
}
