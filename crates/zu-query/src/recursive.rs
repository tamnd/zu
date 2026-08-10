//! RecursiveBFS: the frontier fixpoint behind variable-length and
//! RPQ-style patterns (`docs/07-query-engine.md` §5).
//!
//! Two switches happen at runtime, both driven by frontier statistics.
//! Within one BFS the frontier flips between a sparse id list and a
//! dense bitmap, and between top-down expansion and bottom-up scanning
//! over the reversed adjacency, whenever the frontier grows past a
//! fixed fraction of the graph. Across BFS runs the hybrid morsel
//! policy from the spec picks the unit of parallelism: few sources run
//! one BFS at a time with each level's frontier partitioned into
//! morsels across threads, many sources run as multi-source morsels,
//! 64 sources sharing one pass through the adjacency with a bitmask
//! lane per source.
//!
//! Levels are deterministic in every mode: a node's hop count is the
//! round it is first claimed in, and rounds are barriers, so thread
//! scheduling can reorder discovery within a round but never across
//! rounds.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;

use crate::csr::Csr;

/// Lanes in one multi-source morsel: one bit per source in a `u64`.
pub const MSBFS_LANES: usize = 64;

/// Below this many sources per thread the hybrid policy runs
/// frontier-partitioned single BFS; at or above it, multi-source
/// morsels amortize the adjacency pass across a whole lane batch.
pub const FEW_SOURCES_PER_THREAD: usize = 8;

/// A frontier larger than `node_count / DENSE_FRACTION` switches to
/// the dense bitmap and, when a reversed CSR is at hand, to bottom-up
/// scanning. The classic direction-optimizing threshold shape.
const DENSE_FRACTION: usize = 16;

/// Frontier morsels smaller than this run inline; spawning threads for
/// a hundred nodes costs more than the expansion.
const MIN_PARALLEL_FRONTIER: usize = 4096;

fn word_and_bit(node: u32) -> (usize, u64) {
    ((node / 64) as usize, 1u64 << (node % 64))
}

/// Min-hop levels from any of `sources` to every node, `u32::MAX` for
/// nodes unreached within `max_hops`. Pass the reversed CSR to enable
/// bottom-up levels on huge frontiers; without it every level runs
/// top-down. `threads == 0` picks the available parallelism.
pub fn recursive_bfs(
    csr: &Csr,
    rev: Option<&Csr>,
    sources: &[u32],
    max_hops: u32,
    threads: usize,
) -> Vec<u32> {
    let n = csr.node_count() as usize;
    let threads = effective_threads(threads);
    let mut levels = vec![u32::MAX; n];
    let visited: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
    let mut frontier: Vec<u32> = Vec::new();
    for &s in sources {
        let (word, bit) = word_and_bit(s);
        if visited[word].fetch_or(bit, Relaxed) & bit == 0 {
            levels[s as usize] = 0;
            frontier.push(s);
        }
    }
    let mut frontier_bits = vec![0u64; n.div_ceil(64)];
    let mut depth = 0u32;
    while !frontier.is_empty() && depth < max_hops {
        depth += 1;
        let bottom_up = rev.is_some() && frontier.len() > n / DENSE_FRACTION;
        let next = if let (true, Some(rev)) = (bottom_up, rev) {
            // Dense round: the frontier becomes a bitmap and every
            // unvisited node scans its in-edges for a frontier member.
            for w in frontier_bits.iter_mut() {
                *w = 0;
            }
            for &v in &frontier {
                let (word, bit) = word_and_bit(v);
                frontier_bits[word] |= bit;
            }
            bottom_up_level(rev, &frontier_bits, &visited, threads)
        } else {
            top_down_level(csr, &frontier, &visited, threads)
        };
        for &w in &next {
            levels[w as usize] = depth;
        }
        frontier = next;
    }
    levels
}

/// One top-down round: expand every frontier node's out-edges, claim
/// unvisited targets through the shared bitmap. The frontier splits
/// into per-thread morsels when it is large enough to pay for them.
fn top_down_level(csr: &Csr, frontier: &[u32], visited: &[AtomicU64], threads: usize) -> Vec<u32> {
    let expand = |part: &[u32]| {
        let mut next = Vec::new();
        for &v in part {
            for &w in csr.neighbors(v) {
                let (word, bit) = word_and_bit(w);
                if visited[word].fetch_or(bit, Relaxed) & bit == 0 {
                    next.push(w);
                }
            }
        }
        next
    };
    if threads <= 1 || frontier.len() < MIN_PARALLEL_FRONTIER {
        return expand(frontier);
    }
    let chunk = frontier.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = frontier
            .chunks(chunk)
            .map(|part| scope.spawn(move || expand(part)))
            .collect();
        let mut next = Vec::new();
        for handle in handles {
            next.extend(handle.join().expect("bfs morsel panicked"));
        }
        next
    })
}

/// One bottom-up round: every unvisited node scans its in-neighbors
/// for a frontier bit. Node ranges are the morsels here, aligned to
/// bitmap words so each thread owns the words it writes.
fn bottom_up_level(
    rev: &Csr,
    frontier_bits: &[u64],
    visited: &[AtomicU64],
    threads: usize,
) -> Vec<u32> {
    let n = rev.node_count();
    let scan = |lo: u32, hi: u32| {
        let mut next = Vec::new();
        for w in lo..hi {
            let (word, bit) = word_and_bit(w);
            if visited[word].load(Relaxed) & bit != 0 {
                continue;
            }
            for &u in rev.neighbors(w) {
                let (uw, ub) = word_and_bit(u);
                if frontier_bits[uw] & ub != 0 {
                    visited[word].fetch_or(bit, Relaxed);
                    next.push(w);
                    break;
                }
            }
        }
        next
    };
    if threads <= 1 || (n as usize) < MIN_PARALLEL_FRONTIER {
        return scan(0, n);
    }
    // Ranges rounded to 64-node boundaries so no two morsels share a
    // visited word they both write.
    let per = ((n as usize).div_ceil(threads).div_ceil(64) * 64) as u32;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .filter_map(|t| {
                let lo = (t as u32).checked_mul(per)?;
                if lo >= n {
                    return None;
                }
                let hi = (lo + per).min(n);
                Some(scope.spawn(move || scan(lo, hi)))
            })
            .collect();
        let mut next = Vec::new();
        for handle in handles {
            next.extend(handle.join().expect("bfs morsel panicked"));
        }
        next
    })
}

/// One multi-source morsel: up to [`MSBFS_LANES`] sources advance
/// through a single pass per level, a bitmask lane per source. Calls
/// `emit(lane, node, level)` once for every (source, node) pair
/// reached within `max_hops`, sources included at level 0.
pub fn multi_source_bfs(
    csr: &Csr,
    sources: &[u32],
    max_hops: u32,
    emit: &mut impl FnMut(u32, u32, u32),
) {
    assert!(
        sources.len() <= MSBFS_LANES,
        "one multi-source morsel holds at most {MSBFS_LANES} sources"
    );
    let n = csr.node_count() as usize;
    let mut seen = vec![0u64; n];
    let mut frontier_mask = vec![0u64; n];
    let mut next_mask = vec![0u64; n];
    let mut active: Vec<u32> = Vec::new();
    for (lane, &s) in sources.iter().enumerate() {
        if frontier_mask[s as usize] == 0 {
            active.push(s);
        }
        seen[s as usize] |= 1 << lane;
        frontier_mask[s as usize] |= 1 << lane;
        emit(lane as u32, s, 0);
    }
    let mut depth = 0u32;
    while !active.is_empty() && depth < max_hops {
        depth += 1;
        let mut next_active: Vec<u32> = Vec::new();
        for &v in &active {
            let mask = frontier_mask[v as usize];
            for &w in csr.neighbors(v) {
                let new = mask & !seen[w as usize];
                if new != 0 {
                    if next_mask[w as usize] == 0 {
                        next_active.push(w);
                    }
                    next_mask[w as usize] |= new;
                }
            }
        }
        for &v in &active {
            frontier_mask[v as usize] = 0;
        }
        for &w in &next_active {
            let mut new = next_mask[w as usize];
            next_mask[w as usize] = 0;
            seen[w as usize] |= new;
            frontier_mask[w as usize] = new;
            while new != 0 {
                let lane = new.trailing_zeros();
                emit(lane, w, depth);
                new &= new - 1;
            }
        }
        active = next_active;
    }
}

/// Runs one BFS per source under the hybrid morsel policy and calls
/// `emit(source_index, node, level)` for every (source, node) pair
/// reached within `max_hops`. Fewer than [`FEW_SOURCES_PER_THREAD`]
/// sources per thread run one at a time with frontier-partitioned
/// morsels; more run as lane batches of [`MSBFS_LANES`] distributed
/// across threads. Emission order is unspecified across pairs.
pub fn hybrid_bfs(
    csr: &Csr,
    rev: Option<&Csr>,
    sources: &[u32],
    max_hops: u32,
    threads: usize,
    emit: &mut impl FnMut(u32, u32, u32),
) {
    let threads = effective_threads(threads);
    if sources.len() < FEW_SOURCES_PER_THREAD * threads {
        for (si, &s) in sources.iter().enumerate() {
            let levels = recursive_bfs(csr, rev, &[s], max_hops, threads);
            for (w, &level) in levels.iter().enumerate() {
                if level != u32::MAX {
                    emit(si as u32, w as u32, level);
                }
            }
        }
        return;
    }
    let batches: Vec<&[u32]> = sources.chunks(MSBFS_LANES).collect();
    let cursor = AtomicUsize::new(0);
    let (tx, rx) = mpsc::sync_channel::<Vec<(u32, u32, u32)>>(threads * 2);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(batches.len()) {
            let tx = tx.clone();
            let (batches, cursor) = (&batches, &cursor);
            scope.spawn(move || {
                loop {
                    let b = cursor.fetch_add(1, Relaxed);
                    let Some(batch) = batches.get(b) else { break };
                    let base = (b * MSBFS_LANES) as u32;
                    let mut events = Vec::new();
                    multi_source_bfs(csr, batch, max_hops, &mut |lane, node, level| {
                        events.push((base + lane, node, level));
                        if events.len() >= 1 << 16 {
                            tx.send(std::mem::take(&mut events)).expect("emit sink");
                        }
                    });
                    tx.send(events).expect("emit sink");
                }
            });
        }
        drop(tx);
        for chunk in rx {
            for (source, node, level) in chunk {
                emit(source, node, level);
            }
        }
    });
}

fn effective_threads(threads: usize) -> usize {
    if threads > 0 {
        return threads;
    }
    std::thread::available_parallelism().map_or(1, |p| p.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels::bfs;

    fn random_graph(seed: u64, nodes: u32, edges: usize) -> Csr {
        let mut rng = seed | 1;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let list: Vec<(u32, u32)> = (0..edges)
            .map(|_| (next() as u32 % nodes, next() as u32 % nodes))
            .collect();
        Csr::from_edges(nodes, list)
    }

    fn clamp(levels: &[u32], max_hops: u32) -> Vec<u32> {
        levels
            .iter()
            .map(|&l| if l > max_hops { u32::MAX } else { l })
            .collect()
    }

    #[test]
    fn single_source_matches_plain_bfs_in_every_mode() {
        for seed in [3, 7, 11] {
            let csr = random_graph(seed, 500, 3000);
            let rev = csr.reversed();
            let want = bfs(&csr, 0);
            for threads in [1, 4] {
                assert_eq!(recursive_bfs(&csr, None, &[0], u32::MAX, threads), want);
                assert_eq!(
                    recursive_bfs(&csr, Some(&rev), &[0], u32::MAX, threads),
                    want,
                    "bottom-up rounds must not change levels"
                );
            }
        }
    }

    #[test]
    fn multi_source_union_is_the_elementwise_min() {
        let csr = random_graph(5, 400, 2400);
        let sources = [0u32, 17, 200];
        let mut want = vec![u32::MAX; 400];
        for &s in &sources {
            for (w, &l) in bfs(&csr, s).iter().enumerate() {
                want[w] = want[w].min(l);
            }
        }
        assert_eq!(recursive_bfs(&csr, None, &sources, u32::MAX, 2), want);
    }

    #[test]
    fn max_hops_bounds_the_fixpoint() {
        let csr = Csr::from_edges(5, vec![(0, 1), (1, 2), (2, 3), (3, 4)]);
        assert_eq!(
            recursive_bfs(&csr, None, &[0], 2, 1),
            vec![0, 1, 2, u32::MAX, u32::MAX]
        );
        assert_eq!(
            recursive_bfs(&csr, None, &[0], 0, 1),
            clamp(&bfs(&csr, 0), 0)
        );
    }

    #[test]
    fn a_full_graph_frontier_takes_the_dense_rounds() {
        // A star: level 1 is every other node, far past n / 16, so the
        // second round (finding nobody) runs bottom-up over the rev.
        let n = 2000u32;
        let edges: Vec<(u32, u32)> = (1..n).map(|i| (0, i)).collect();
        let csr = Csr::from_edges(n, edges);
        let rev = csr.reversed();
        let got = recursive_bfs(&csr, Some(&rev), &[0], u32::MAX, 4);
        assert_eq!(got, bfs(&csr, 0));
    }

    #[test]
    fn one_multi_source_morsel_matches_per_source_bfs() {
        let csr = random_graph(9, 300, 1500);
        let sources: Vec<u32> = (0..64).map(|i| (i * 5) % 300).collect();
        let mut got: Vec<(u32, u32, u32)> = Vec::new();
        multi_source_bfs(&csr, &sources, 3, &mut |lane, node, level| {
            got.push((lane, node, level));
        });
        got.sort_unstable();
        let mut want: Vec<(u32, u32, u32)> = Vec::new();
        for (lane, &s) in sources.iter().enumerate() {
            for (w, &l) in clamp(&bfs(&csr, s), 3).iter().enumerate() {
                if l != u32::MAX {
                    want.push((lane as u32, w as u32, l));
                }
            }
        }
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn hybrid_policy_agrees_on_both_sides_of_the_threshold() {
        let csr = random_graph(13, 250, 1200);
        let rev = csr.reversed();
        // 3 sources with 2 threads stays under the threshold, 40 with
        // 1 thread crosses it; both must match per-source BFS.
        for (count, threads) in [(3usize, 2usize), (40, 1)] {
            let sources: Vec<u32> = (0..count as u32).map(|i| (i * 7) % 250).collect();
            let mut got: Vec<(u32, u32, u32)> = Vec::new();
            hybrid_bfs(&csr, Some(&rev), &sources, 4, threads, &mut |s, w, l| {
                got.push((s, w, l));
            });
            got.sort_unstable();
            let mut want: Vec<(u32, u32, u32)> = Vec::new();
            for (si, &s) in sources.iter().enumerate() {
                for (w, &l) in clamp(&bfs(&csr, s), 4).iter().enumerate() {
                    if l != u32::MAX {
                        want.push((si as u32, w as u32, l));
                    }
                }
            }
            want.sort_unstable();
            assert_eq!(got, want, "{count} sources on {threads} threads");
        }
    }

    #[test]
    fn duplicate_sources_and_empty_graphs_hold_up() {
        let empty = Csr::from_edges(0, Vec::new());
        assert_eq!(
            recursive_bfs(&empty, None, &[], u32::MAX, 2),
            Vec::<u32>::new()
        );

        let csr = Csr::from_edges(3, vec![(0, 1)]);
        assert_eq!(
            recursive_bfs(&csr, None, &[0, 0], u32::MAX, 1),
            vec![0, 1, u32::MAX]
        );
        let mut events = 0u32;
        multi_source_bfs(&csr, &[0, 0], u32::MAX, &mut |_, _, _| events += 1);
        assert_eq!(events, 4, "each lane reports its own copy of the walk");
    }
}
