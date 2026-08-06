//! Graph kernels over the in-memory CSR: BFS, bidirectional shortest
//! path, triangle counting, PageRank, and weakly connected components.
//!
//! These are the M4 table-function kernels (`docs/07-query-engine.md`,
//! operator table) in their single-threaded, in-memory form. Everything
//! is frontier or pass based, no recursion, deterministic output.

use std::cmp::Ordering;

use crate::csr::{Csr, CsrBuilder};

/// When one sorted list is this many times longer than the other, the
/// intersection gallops through the long list instead of merging.
const GALLOP_RATIO: usize = 16;

/// Frontier-based BFS returning the hop level of every node from
/// `source`; unreached nodes hold `u32::MAX`.
pub fn bfs(csr: &Csr, source: u32) -> Vec<u32> {
    let mut level = vec![u32::MAX; csr.node_count() as usize];
    level[source as usize] = 0;
    let mut frontier = vec![source];
    let mut next = Vec::new();
    let mut depth = 0u32;
    while !frontier.is_empty() {
        depth += 1;
        for &v in &frontier {
            for &w in csr.neighbors(v) {
                if level[w as usize] == u32::MAX {
                    level[w as usize] = depth;
                    next.push(w);
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next);
        next.clear();
    }
    level
}

/// Single-source shortest paths in hop counts; on an unweighted graph
/// this is exactly BFS.
pub fn sssp_hops(csr: &Csr, source: u32) -> Vec<u32> {
    bfs(csr, source)
}

/// Point-to-point hop count from `a` to `b` by bidirectional BFS,
/// expanding the smaller frontier each step; `rev` must be the reversed
/// build of `csr`. Returns `None` when no path exists.
pub fn bidirectional_shortest_path(csr: &Csr, rev: &Csr, a: u32, b: u32) -> Option<u32> {
    if a == b {
        return Some(0);
    }
    let n = csr.node_count() as usize;
    let mut dist_f = vec![u32::MAX; n];
    let mut dist_b = vec![u32::MAX; n];
    dist_f[a as usize] = 0;
    dist_b[b as usize] = 0;
    let mut front_f = vec![a];
    let mut front_b = vec![b];
    let (mut level_f, mut level_b) = (0u32, 0u32);
    let mut best = u32::MAX;
    let mut next = Vec::new();
    // Once both sides are fully expanded to depths (lf, lb), any path
    // longer than lf + lb would already have produced a meeting, so
    // best <= lf + lb is a safe stopping point.
    while !front_f.is_empty() && !front_b.is_empty() && best > level_f + level_b {
        if front_f.len() <= front_b.len() {
            level_f += 1;
            expand(
                csr,
                &mut front_f,
                &mut next,
                &mut dist_f,
                &dist_b,
                level_f,
                &mut best,
            );
        } else {
            level_b += 1;
            expand(
                rev,
                &mut front_b,
                &mut next,
                &mut dist_b,
                &dist_f,
                level_b,
                &mut best,
            );
        }
    }
    (best != u32::MAX).then_some(best)
}

/// Advances one bidirectional BFS side a full level, recording the best
/// meeting distance seen against the opposite side.
fn expand(
    graph: &Csr,
    frontier: &mut Vec<u32>,
    next: &mut Vec<u32>,
    dist_this: &mut [u32],
    dist_other: &[u32],
    depth: u32,
    best: &mut u32,
) {
    next.clear();
    for &v in frontier.iter() {
        for &w in graph.neighbors(v) {
            let wi = w as usize;
            if dist_other[wi] != u32::MAX {
                *best = (*best).min(depth + dist_other[wi]);
            }
            if dist_this[wi] == u32::MAX {
                dist_this[wi] = depth;
                next.push(w);
            }
        }
    }
    std::mem::swap(frontier, next);
}

/// Counts triangles in the undirected view of `csr`, each exactly once.
///
/// Every undirected edge is oriented toward its higher (degree, id)
/// endpoint, so a triangle is found only at its lowest-order corner and
/// forward lists stay short; per edge the two sorted forward lists are
/// intersected, galloping when their lengths are lopsided.
pub fn triangle_count(csr: &Csr) -> u64 {
    let n = csr.node_count();
    let mut edges: Vec<(u32, u32)> = Vec::with_capacity(csr.edge_count() as usize);
    for v in 0..n {
        for &w in csr.neighbors(v) {
            if v != w {
                edges.push((v.min(w), v.max(w)));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let mut deg = vec![0u32; n as usize];
    for &(u, v) in &edges {
        deg[u as usize] += 1;
        deg[v as usize] += 1;
    }
    let rank = |v: u32| (deg[v as usize], v);
    let mut fwd = CsrBuilder::new(n);
    for &(u, v) in &edges {
        if rank(u) < rank(v) {
            fwd.push(u, v);
        } else {
            fwd.push(v, u);
        }
    }
    let fwd = fwd.build();
    let mut count = 0u64;
    for u in 0..n {
        let ahead = fwd.neighbors(u);
        for &v in ahead {
            count += intersect_count(ahead, fwd.neighbors(v));
        }
    }
    count
}

/// Size of the intersection of two sorted, deduplicated lists.
fn intersect_count(a: &[u32], b: &[u32]) -> u64 {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.is_empty() {
        return 0;
    }
    if long.len() / short.len() >= GALLOP_RATIO {
        return gallop_count(short, long);
    }
    let (mut i, mut j, mut count) = (0, 0, 0u64);
    while i < short.len() && j < long.len() {
        match short[i].cmp(&long[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
        }
    }
    count
}

/// Galloping intersection: for each element of the short list, an
/// exponential probe into the remaining suffix of the long list bounds a
/// binary search, so runtime is O(short * log(long / short)).
fn gallop_count(short: &[u32], long: &[u32]) -> u64 {
    let mut rest = long;
    let mut count = 0u64;
    for &x in short {
        let mut hi = 1usize;
        while hi < rest.len() && rest[hi - 1] < x {
            hi <<= 1;
        }
        let hi = hi.min(rest.len());
        let pos = rest[..hi].partition_point(|&y| y < x);
        if pos < rest.len() && rest[pos] == x {
            count += 1;
        }
        rest = &rest[pos..];
        if rest.is_empty() {
            break;
        }
    }
    count
}

/// Push-based power iteration with uniform teleport; dangling mass is
/// redistributed uniformly so scores sum to one after every iteration.
pub fn pagerank(csr: &Csr, damping: f64, iters: u32) -> Vec<f64> {
    let n = csr.node_count() as usize;
    if n == 0 {
        return Vec::new();
    }
    let uniform = 1.0 / n as f64;
    let mut cur = vec![uniform; n];
    let mut next = vec![0.0f64; n];
    for _ in 0..iters {
        next.fill(0.0);
        let mut dangling = 0.0;
        for v in 0..csr.node_count() {
            let nbrs = csr.neighbors(v);
            let score = cur[v as usize];
            if nbrs.is_empty() {
                dangling += score;
            } else {
                let share = score / nbrs.len() as f64;
                for &w in nbrs {
                    next[w as usize] += share;
                }
            }
        }
        let base = (1.0 - damping) * uniform + damping * dangling * uniform;
        for x in &mut next {
            *x = base + damping * *x;
        }
        std::mem::swap(&mut cur, &mut next);
    }
    cur
}

/// Weakly connected components: edge direction is ignored and every node
/// is labeled with the smallest node id in its component.
///
/// Union-find with path halving; unions root at the smaller id, which is
/// what makes the min-id labeling fall out of the final `find` pass.
pub fn wcc(csr: &Csr) -> Vec<u32> {
    let n = csr.node_count();
    let mut parent: Vec<u32> = (0..n).collect();
    for v in 0..n {
        for &w in csr.neighbors(v) {
            let a = find(&mut parent, v);
            let b = find(&mut parent, w);
            match a.cmp(&b) {
                Ordering::Less => parent[b as usize] = a,
                Ordering::Greater => parent[a as usize] = b,
                Ordering::Equal => {}
            }
        }
    }
    (0..n).map(|v| find(&mut parent, v)).collect()
}

/// Root lookup with path halving: every visited node is repointed to its
/// grandparent on the way up.
fn find(parent: &mut [u32], mut v: u32) -> u32 {
    while parent[v as usize] != v {
        let grand = parent[parent[v as usize] as usize];
        parent[v as usize] = grand;
        v = grand;
    }
    v
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Deterministic xorshift edge list with endpoints below `n`.
    fn random_edges(n: u32, m: usize, mut rng: u64) -> Vec<(u32, u32)> {
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        (0..m)
            .map(|_| {
                (
                    (next() % u64::from(n)) as u32,
                    (next() % u64::from(n)) as u32,
                )
            })
            .collect()
    }

    /// Mirrors every edge so the graph is symmetric.
    fn symmetric(edges: &[(u32, u32)]) -> Vec<(u32, u32)> {
        edges.iter().flat_map(|&(a, b)| [(a, b), (b, a)]).collect()
    }

    /// Reference BFS with an explicit queue, no frontier batching.
    fn naive_bfs(csr: &Csr, source: u32) -> Vec<u32> {
        let mut level = vec![u32::MAX; csr.node_count() as usize];
        level[source as usize] = 0;
        let mut queue = VecDeque::from([source]);
        while let Some(v) = queue.pop_front() {
            for &w in csr.neighbors(v) {
                if level[w as usize] == u32::MAX {
                    level[w as usize] = level[v as usize] + 1;
                    queue.push_back(w);
                }
            }
        }
        level
    }

    #[test]
    fn bfs_directed_path() {
        let csr = Csr::from_edges(5, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
        assert_eq!(bfs(&csr, 0), vec![0, 1, 2, 3, 4]);
        // From the middle, everything behind the source is unreached.
        assert_eq!(bfs(&csr, 2), vec![u32::MAX, u32::MAX, 0, 1, 2]);
        assert_eq!(sssp_hops(&csr, 0), bfs(&csr, 0));
    }

    #[test]
    fn bfs_cycle_and_star() {
        let cycle = Csr::from_edges(4, vec![(0, 1), (1, 2), (2, 3), (3, 0)]);
        assert_eq!(bfs(&cycle, 0), vec![0, 1, 2, 3]);

        let star = Csr::from_edges(5, symmetric(&[(0, 1), (0, 2), (0, 3), (0, 4)]));
        assert_eq!(bfs(&star, 0), vec![0, 1, 1, 1, 1]);
        assert_eq!(bfs(&star, 3), vec![1, 2, 2, 0, 2]);
    }

    #[test]
    fn bfs_matches_naive_on_random_graph() {
        let csr = Csr::from_edges(1000, random_edges(1000, 8000, 0xA5A5_5A5A_1234_5678));
        for source in [0u32, 137, 999] {
            assert_eq!(
                bfs(&csr, source),
                naive_bfs(&csr, source),
                "source {source}"
            );
        }
    }

    #[test]
    fn bidirectional_matches_bfs_on_random_graph() {
        let csr = Csr::from_edges(1000, random_edges(1000, 8000, 0xDEAD_BEEF_CAFE_F00D));
        let rev = csr.reversed();
        for i in 0..40u32 {
            let (a, b) = (i * 37 % 1000, i * 61 % 1000);
            let level = bfs(&csr, a)[b as usize];
            let expect = (level != u32::MAX).then_some(level);
            assert_eq!(
                bidirectional_shortest_path(&csr, &rev, a, b),
                expect,
                "pair ({a}, {b})"
            );
        }
    }

    #[test]
    fn bidirectional_trivial_and_disconnected() {
        // Two directed paths with no cross edges.
        let csr = Csr::from_edges(6, vec![(0, 1), (1, 2), (3, 4), (4, 5)]);
        let rev = csr.reversed();
        assert_eq!(bidirectional_shortest_path(&csr, &rev, 2, 2), Some(0));
        assert_eq!(bidirectional_shortest_path(&csr, &rev, 0, 2), Some(2));
        assert_eq!(bidirectional_shortest_path(&csr, &rev, 0, 5), None);
        // Direction matters: the path is one-way.
        assert_eq!(bidirectional_shortest_path(&csr, &rev, 2, 0), None);
    }

    #[test]
    fn triangles_in_clique_of_five() {
        // Directed edges only from lower to higher id; the undirected
        // view is K5 with C(5, 3) = 10 triangles.
        let mut edges = Vec::new();
        for i in 0..5u32 {
            for j in (i + 1)..5 {
                edges.push((i, j));
            }
        }
        assert_eq!(triangle_count(&Csr::from_edges(5, edges.clone())), 10);
        assert_eq!(triangle_count(&Csr::from_edges(5, symmetric(&edges))), 10);
    }

    #[test]
    fn triangles_in_simple_shapes() {
        let path = Csr::from_edges(4, symmetric(&[(0, 1), (1, 2), (2, 3)]));
        assert_eq!(triangle_count(&path), 0);
        let star = Csr::from_edges(5, symmetric(&[(0, 1), (0, 2), (0, 3), (0, 4)]));
        assert_eq!(triangle_count(&star), 0);
        let tri = Csr::from_edges(3, symmetric(&[(0, 1), (1, 2), (2, 0)]));
        assert_eq!(triangle_count(&tri), 1);
        let square = Csr::from_edges(4, symmetric(&[(0, 1), (1, 2), (2, 3), (3, 0)]));
        assert_eq!(triangle_count(&square), 0);
        // Self loops and parallel edges must not manufacture triangles.
        let loopy = Csr::from_edges(3, vec![(0, 0), (0, 1), (0, 1), (1, 2), (2, 0), (1, 0)]);
        assert_eq!(triangle_count(&loopy), 1);
    }

    #[test]
    fn triangles_match_brute_force_on_subsample() {
        // Fold the 1000-node random graph onto 60 nodes so the sample is
        // dense enough to actually contain triangles.
        const SUB: usize = 60;
        let edges = random_edges(1000, 8000, 0x1357_9BDF_2468_ACE0);
        let folded: Vec<(u32, u32)> = edges
            .iter()
            .map(|&(a, b)| (a % SUB as u32, b % SUB as u32))
            .collect();

        let mut adj = vec![[false; SUB]; SUB];
        for &(a, b) in &folded {
            if a != b {
                adj[a as usize][b as usize] = true;
                adj[b as usize][a as usize] = true;
            }
        }
        let mut expect = 0u64;
        for (i, row) in adj.iter().enumerate() {
            for j in (i + 1)..SUB {
                if !row[j] {
                    continue;
                }
                for k in (j + 1)..SUB {
                    if row[k] && adj[j][k] {
                        expect += 1;
                    }
                }
            }
        }
        assert!(expect > 0, "sample too sparse to be a meaningful check");
        assert_eq!(triangle_count(&Csr::from_edges(SUB as u32, folded)), expect);
    }

    #[test]
    fn pagerank_star_closed_form() {
        // Symmetric star, center 0 with k = 4 leaves. At the fixpoint
        // the center holds (1 + d * k) / ((k + 1) * (1 + d)) and the
        // leaves split the remainder evenly.
        let star = Csr::from_edges(5, symmetric(&[(0, 1), (0, 2), (0, 3), (0, 4)]));
        let damping = 0.85;
        let pr = pagerank(&star, damping, 100);
        let center = (1.0 + damping * 4.0) / (5.0 * (1.0 + damping));
        let leaf = (1.0 - center) / 4.0;
        assert!(
            (pr[0] - center).abs() < 1e-6,
            "center {} vs {center}",
            pr[0]
        );
        for &score in &pr[1..] {
            assert!((score - leaf).abs() < 1e-6, "leaf {score} vs {leaf}");
        }
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "sum {sum}");
    }

    #[test]
    fn pagerank_handles_dangling_nodes() {
        // Node 2 is a sink; without dangling redistribution the mass
        // would leak below one.
        let csr = Csr::from_edges(3, vec![(0, 1), (1, 2)]);
        let pr = pagerank(&csr, 0.85, 50);
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "sum {sum}");
        assert!(pr[2] > pr[1] && pr[1] > pr[0], "scores {pr:?}");
    }

    #[test]
    fn wcc_two_components() {
        // A directed path and a directed cycle, no cross edges.
        let csr = Csr::from_edges(6, vec![(1, 0), (1, 2), (3, 4), (4, 5), (5, 3)]);
        assert_eq!(wcc(&csr), vec![0, 0, 0, 3, 3, 3]);

        let star = Csr::from_edges(5, vec![(0, 1), (0, 2), (0, 3), (0, 4)]);
        assert_eq!(wcc(&star), vec![0; 5]);
    }

    #[test]
    fn wcc_matches_bfs_flood_on_random_graph() {
        let edges = random_edges(1000, 1500, 0x0F0F_F0F0_5555_AAAA);
        let directed = Csr::from_edges(1000, edges.clone());
        let undirected = Csr::from_edges(1000, symmetric(&edges));
        let labels = wcc(&directed);

        // Flood fill on the undirected view; the first unseen node id is
        // the component's expected min-id label.
        let mut expect = vec![u32::MAX; 1000];
        for v in 0..1000u32 {
            if expect[v as usize] != u32::MAX {
                continue;
            }
            for (w, &level) in bfs(&undirected, v).iter().enumerate() {
                if level != u32::MAX {
                    expect[w] = v;
                }
            }
        }
        assert_eq!(labels, expect);
        let mut roots: Vec<u32> = labels.clone();
        roots.sort_unstable();
        roots.dedup();
        assert!(roots.len() > 1, "graph too dense for a component check");
    }
}
