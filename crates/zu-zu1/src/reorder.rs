//! NodeId relabeling before bulk load, `docs/04-storage-zu1-format.md` §5.
//!
//! `COPY ... WITH (REORDER = degree | bfs | none)` relabels nodes so that
//! neighbors land on nearby ids, which is what lets delta plus bitpack
//! reach the bits/edge budget: gap sizes track id locality, and one wide
//! gap per 1024-value chunk sets the width for the whole chunk. BFS from
//! max-degree roots is the default in the spec; degree ordering is the
//! cheaper fallback. The mapping is a bijection over `0..node_count`, so
//! the relabeled graph is isomorphic to the input.

/// Node order at COPY, `REORDER = degree | bfs | none`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reorder {
    None,
    Degree,
    Bfs,
}

impl Reorder {
    /// Parses the spec spelling, lowercase.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "degree" => Some(Self::Degree),
            "bfs" => Some(Self::Bfs),
            _ => None,
        }
    }
}

/// Undirected degree per node. Self loops count once.
fn degrees(node_count: usize, edges: &[(u32, u32)]) -> Vec<u32> {
    let mut deg = vec![0u32; node_count];
    for &(s, d) in edges {
        deg[s as usize] += 1;
        if s != d {
            deg[d as usize] += 1;
        }
    }
    deg
}

/// Old id to new id, densest nodes first. Ties break toward the lower old
/// id so the mapping is deterministic.
pub fn degree_order(node_count: u64, edges: &[(u32, u32)]) -> Vec<u32> {
    let n = usize::try_from(node_count).expect("node_count fits usize");
    let deg = degrees(n, edges);
    let mut by_degree: Vec<u32> = (0..n as u32).collect();
    by_degree.sort_by_key(|&v| (std::cmp::Reverse(deg[v as usize]), v));
    invert(&by_degree)
}

/// Old id to new id in BFS dequeue order, restarting from the unvisited
/// node of highest degree until every component (and every isolated node)
/// is numbered. Neighbor lists are walked in sorted id order, so the
/// mapping is deterministic.
pub fn bfs_order(node_count: u64, edges: &[(u32, u32)]) -> Vec<u32> {
    let n = usize::try_from(node_count).expect("node_count fits usize");
    let deg = degrees(n, edges);

    // Undirected CSR over u32 ids; counting sort keeps this linear.
    let mut offsets = vec![0u32; n + 1];
    for (v, &d) in deg.iter().enumerate() {
        offsets[v + 1] = offsets[v] + d;
    }
    let mut cursor = offsets.clone();
    let mut adj = vec![0u32; offsets[n] as usize];
    for &(s, d) in edges {
        adj[cursor[s as usize] as usize] = d;
        cursor[s as usize] += 1;
        if s != d {
            adj[cursor[d as usize] as usize] = s;
            cursor[d as usize] += 1;
        }
    }
    for v in 0..n {
        adj[offsets[v] as usize..offsets[v + 1] as usize].sort_unstable();
    }

    let mut roots: Vec<u32> = (0..n as u32).collect();
    roots.sort_by_key(|&v| (std::cmp::Reverse(deg[v as usize]), v));

    let mut map = vec![u32::MAX; n];
    let mut next = 0u32;
    let mut queue = std::collections::VecDeque::new();
    for root in roots {
        if map[root as usize] != u32::MAX {
            continue;
        }
        map[root as usize] = next;
        next += 1;
        queue.push_back(root);
        while let Some(v) = queue.pop_front() {
            let (lo, hi) = (
                offsets[v as usize] as usize,
                offsets[v as usize + 1] as usize,
            );
            for &w in &adj[lo..hi] {
                if map[w as usize] == u32::MAX {
                    map[w as usize] = next;
                    next += 1;
                    queue.push_back(w);
                }
            }
        }
    }
    debug_assert_eq!(next as usize, n);
    map
}

/// Applies an old-to-new map in place. The caller re-sorts afterward.
pub fn relabel(edges: &mut [(u32, u32)], map: &[u32]) {
    for e in edges {
        *e = (map[e.0 as usize], map[e.1 as usize]);
    }
}

/// Turns a new-id-to-old-id order into an old-id-to-new-id map.
fn invert(order: &[u32]) -> Vec<u32> {
    let mut map = vec![0u32; order.len()];
    for (new, &old) in order.iter().enumerate() {
        map[old as usize] = new as u32;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_bijection(map: &[u32]) -> bool {
        let mut seen = vec![false; map.len()];
        map.iter().all(|&v| {
            let hit = (v as usize) < seen.len() && !seen[v as usize];
            if hit {
                seen[v as usize] = true;
            }
            hit
        })
    }

    #[test]
    fn degree_puts_hubs_first() {
        // Node 3 touches everyone, node 0 touches nothing.
        let edges = [(3, 1), (3, 2), (3, 4), (1, 2)];
        let map = degree_order(5, &edges);
        assert!(is_bijection(&map));
        assert_eq!(map[3], 0);
        assert_eq!(map[0], 4);
    }

    #[test]
    fn bfs_numbers_from_the_hub() {
        // Star around 2 plus a separate pair and an isolated node.
        let edges = [(2, 0), (2, 4), (2, 5), (1, 3)];
        let map = bfs_order(7, &edges);
        assert!(is_bijection(&map));
        assert_eq!(map[2], 0);
        // The star fills 1..=3 in sorted neighbor order.
        assert_eq!((map[0], map[4], map[5]), (1, 2, 3));
        // The pair component starts after the star, isolated node 6 last.
        assert_eq!((map[1], map[3]), (4, 5));
        assert_eq!(map[6], 6);
    }

    #[test]
    fn bfs_handles_self_loops_and_dups_in_direction() {
        let edges = [(0, 0), (0, 1), (1, 0), (1, 2)];
        let map = bfs_order(3, &edges);
        assert!(is_bijection(&map));
    }

    #[test]
    fn relabel_preserves_structure() {
        let mut edges = vec![(0u32, 1u32), (1, 2), (2, 0), (3, 3)];
        let original = edges.clone();
        let map = bfs_order(4, &edges);
        relabel(&mut edges, &map);
        // Mapping back with the inverse recovers the original multiset.
        let mut inverse = vec![0u32; map.len()];
        for (old, &new) in map.iter().enumerate() {
            inverse[new as usize] = old as u32;
        }
        let mut back: Vec<(u32, u32)> = edges
            .iter()
            .map(|&(s, d)| (inverse[s as usize], inverse[d as usize]))
            .collect();
        back.sort_unstable();
        let mut want = original;
        want.sort_unstable();
        assert_eq!(back, want);
    }

    #[test]
    fn parse_accepts_spec_spellings() {
        assert_eq!(Reorder::parse("bfs"), Some(Reorder::Bfs));
        assert_eq!(Reorder::parse("degree"), Some(Reorder::Degree));
        assert_eq!(Reorder::parse("none"), Some(Reorder::None));
        assert_eq!(Reorder::parse("llp"), None);
    }

    #[test]
    fn bfs_tightens_gaps_on_a_ring_of_cliques() {
        // 16 cliques of 8 wired in a ring, labels shuffled by a stride
        // permutation. BFS must bring clique members back together and
        // shrink the average absolute neighbor gap.
        let k = 8u32;
        let cliques = 16u32;
        let n = (k * cliques) as u64;
        let shuffle = |v: u32| (v * 37) % (k * cliques);
        let mut edges = Vec::new();
        for c in 0..cliques {
            let base = c * k;
            for a in 0..k {
                for b in (a + 1)..k {
                    edges.push((shuffle(base + a), shuffle(base + b)));
                }
            }
            let next = ((c + 1) % cliques) * k;
            edges.push((shuffle(base), shuffle(next)));
        }
        let gap_sum = |es: &[(u32, u32)]| -> u64 {
            es.iter()
                .map(|&(s, d)| u64::from(s.abs_diff(d)))
                .sum::<u64>()
        };
        let before = gap_sum(&edges);
        let map = bfs_order(n, &edges);
        assert!(is_bijection(&map));
        let mut relabeled = edges.clone();
        relabel(&mut relabeled, &map);
        assert!(gap_sum(&relabeled) * 2 < before);
    }
}
