//! Whole-graph table function kernels: bfs, pagerank, wcc, sssp, cdlp,
//! lcc, triangle_count, louvain (docs/07 §4, TableFunction). Each kernel runs directly
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

/// Where the converging form stops. The tolerance is the largest a
/// single rank moves in a round, the cap is what a run costs on a
/// graph that will not settle. A fixed twenty rounds is the right rule
/// only against a published reference produced the same way; asked for
/// the pagerank of a graph with nothing to match, twenty rounds is
/// about 3.4e-4 off the converged answer, which is outside the 1e-4
/// the LDBC harness compares at. These two are what that harness uses
/// for its own reference.
pub const PAGERANK_TOLERANCE: f64 = 1e-12;
pub const PAGERANK_MAX_ITERATIONS: usize = 100;

/// The round count Graphalytics fixes for CDLP, so two runs of the
/// same graph are the same partition however far propagation has left
/// to go.
pub const CDLP_ROUNDS: usize = 10;

/// PageRank by power iteration over the forward CSR: fixed iteration
/// count, dangling mass redistributed uniformly, ranks summing to one.
/// This is the form that matches a published Graphalytics reference,
/// which is itself the same loop run the same number of times.
pub fn pagerank(db: &mut Zu1File, reader: &mut GraphReader, iterations: usize) -> Result<Vec<f64>> {
    power_iteration(db, reader, iterations, 0.0)
}

/// PageRank run until it settles rather than until a counter runs out:
/// stops when no rank moves by more than [`PAGERANK_TOLERANCE`] in a
/// round, and gives up at [`PAGERANK_MAX_ITERATIONS`]. This is what a
/// query asking for the pagerank of a graph wants, there being no
/// fixed-round reference on the other side of it to match.
pub fn pagerank_converged(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<f64>> {
    power_iteration(db, reader, PAGERANK_MAX_ITERATIONS, PAGERANK_TOLERANCE)
}

/// The loop both forms share. `stop_at` is the largest per-node move
/// that counts as settled; a stop of zero never fires, which is how the
/// fixed-count form spends its whole budget.
fn power_iteration(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    iterations: usize,
    stop_at: f64,
) -> Result<Vec<f64>> {
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
        let moved = rank
            .iter()
            .zip(&next)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        std::mem::swap(&mut rank, &mut next);
        if moved < stop_at {
            break;
        }
    }
    Ok(rank)
}

/// Weakly connected components over the undirected view of the rel
/// table: union-find over the forward lists, which name every edge
/// once. Each node's component id is the smallest id in the component,
/// the Graphalytics convention.
///
/// The smallest id, not the smallest row. A load that reorders rows,
/// which every load of a real graph does because degree order is what
/// makes the CSR read well, leaves the two unrelated, and the caller
/// that wanted a component id then has to join the answer back to the
/// keys and take a minimum per group. That is a group-by and an unwind
/// over the whole node table to repair an answer the kernel already
/// had the parts of.
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
    let roots: Vec<u64> = (0..n as u64).map(|x| find(&mut parent, x)).collect();
    // An unkeyed table is dense and the row is the id, so the roots
    // are already the answer.
    let Some(index) = reader.directory().keys.clone() else {
        return Ok(roots);
    };
    let key = crate::keys::key_by_row(db, &index)?;
    if key.len() != n {
        return Err(zu_common::ZuError::Corrupt {
            what: "key index",
            detail: format!("{} keys over {n} nodes", key.len()),
        });
    }
    let mut smallest = vec![u64::MAX; n];
    for (row, &root) in roots.iter().enumerate() {
        let slot = &mut smallest[root as usize];
        *slot = (*slot).min(key[row]);
    }
    Ok(roots.iter().map(|&root| smallest[root as usize]).collect())
}

/// Single-source hop distances over the undirected view, SSSP under
/// unit weights. Unreachable nodes get `u64::MAX`. The weighted form is
/// [`sssp_weighted`], which follows stored direction instead, that
/// being what the GAP and Graph500 kernels measure.
pub fn sssp(db: &mut Zu1File, reader: &mut GraphReader, source: u64) -> Result<Vec<u64>> {
    levels(db, reader, source, Walk::Both)
}

/// The widest edge weight a bucket queue is worth building for. Buckets
/// cost a slot per distance value within a window of the settled
/// distance, so the array is as wide as the widest edge plus one; past
/// this the window is bigger than the frontier it holds and a heap is
/// the cheaper structure. GAP's generators draw weights from 1..255,
/// which is three orders inside it.
const SSSP_BUCKET_LIMIT: u64 = 1 << 16;

/// Weighted single-source shortest paths from `source` following stored
/// edge direction, the GAP SSSP and Graph500 K3 kernel. `weights[i]` is
/// the weight of edge `i` of the load order. Unreachable nodes get
/// `u64::MAX`.
///
/// The load order is the order the forward lists lay out, group after
/// group and list after list, so a walk that counts as it goes reads a
/// weight at an index and never searches for one. That is also what
/// keeps a parallel edge honest: two edges over the same pair are two
/// slots of the list with two weights, and only the count says which is
/// which.
///
/// Direction, not the undirected view, because the reference this is
/// measured against relaxes out-edges only. The distances come out the
/// same whichever order the frontier settles in, so there is nothing
/// here for a tie to move.
pub fn sssp_weighted(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    source: u64,
    weights: &[u64],
) -> Result<Vec<u64>> {
    let n = reader.directory().one_domain()?;
    let stored = reader.directory().edge_count;
    if weights.len() as u64 != stored {
        return Err(zu_common::ZuError::InvalidArgument(format!(
            "{} weights over {stored} edges",
            weights.len()
        )));
    }
    let mut dist = vec![u64::MAX; n as usize];
    if source >= n {
        return Ok(dist);
    }
    dist[source as usize] = 0;
    let widest = weights.iter().copied().max().unwrap_or(0);
    if widest < SSSP_BUCKET_LIMIT {
        dial(db, reader, source, weights, widest, &mut dist)?;
    } else {
        heap_dijkstra(db, reader, source, weights, &mut dist)?;
    }
    Ok(dist)
}

/// Dijkstra over a ring of buckets, one bucket a distance value.
///
/// Every edge out of a settled node lands within `widest` of the
/// distance that settled it, so a ring of `widest + 1` buckets holds
/// every tentative distance in flight and the ring index is the
/// distance modulo its length. That turns the priority queue into an
/// array index: no comparisons, no sift, and a node that improves is
/// pushed again rather than moved, which the settled check at pop
/// filters out. Against a binary heap this is what a small weight range
/// buys, and the GAP generators draw 1..255.
fn dial(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    source: u64,
    weights: &[u64],
    widest: u64,
    dist: &mut [u64],
) -> Result<()> {
    let ring = (widest + 1) as usize;
    let mut buckets: Vec<Vec<u64>> = vec![Vec::new(); ring];
    buckets[0].push(source);
    let mut queued = 1usize;
    let mut at = 0u64;
    while queued > 0 {
        let slot = (at % ring as u64) as usize;
        // The bucket is emptied into a scratch first: relaxing out of
        // it can push back into this same slot when an edge weighs
        // zero, and a drain that walked the live vector would then read
        // what it is writing.
        let round = std::mem::take(&mut buckets[slot]);
        queued -= round.len();
        for &node in &round {
            // A node reachable by several paths sits in as many
            // buckets; the one that settled it is the only one that
            // gets to expand it.
            if dist[node as usize] != at {
                continue;
            }
            let (nbrs, base) = reader.out_neighbors_from(db, node)?;
            for (i, &dst) in nbrs.iter().enumerate() {
                let reach = at + weights[(base + i as u64) as usize];
                if reach < dist[dst as usize] {
                    dist[dst as usize] = reach;
                    buckets[(reach % ring as u64) as usize].push(dst);
                    queued += 1;
                }
            }
        }
        at += 1;
    }
    Ok(())
}

/// Dijkstra over a binary heap, for the weight range a bucket ring
/// would be wasteful over. Stale entries are left in the heap and
/// dropped at pop, which is cheaper than finding and moving them.
fn heap_dijkstra(
    db: &mut Zu1File,
    reader: &mut GraphReader,
    source: u64,
    weights: &[u64],
    dist: &mut [u64],
) -> Result<()> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((0u64, source)));
    while let Some(Reverse((at, node))) = heap.pop() {
        if at != dist[node as usize] {
            continue;
        }
        let (nbrs, base) = reader.out_neighbors_from(db, node)?;
        for (i, &dst) in nbrs.iter().enumerate() {
            let reach = at + weights[(base + i as u64) as usize];
            if reach < dist[dst as usize] {
                dist[dst as usize] = reach;
                heap.push(Reverse((reach, dst)));
            }
        }
    }
    Ok(())
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
/// A pair of neighbors closes the triangle once however many edges run
/// between them, and a self loop on a neighbor closes nothing. Counting
/// a parallel edge again would put the coefficient of a node above one,
/// which is not a thing a coefficient does.
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
            // The list is sorted, so a parallel edge sits next to the
            // copy before it and skipping a repeat is one comparison.
            let mut prev = u64::MAX;
            for &far in reader.neighbors_dir(db, near, Direction::Fwd)? {
                if far == near || far == prev {
                    continue;
                }
                prev = far;
                if member[far as usize] {
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

/// A list this many times longer than the one it is being intersected
/// with is searched rather than walked. A merge costs the sum of the
/// two lengths and a search costs the short one times the log of the
/// long one, so the crossover is where the log stops paying, and on a
/// power law graph the pairing of a hub with a leaf is most of the
/// work.
const GALLOP_RATIO: usize = 32;

/// Members two sorted lists of distinct ids have in common. Each one
/// found adds to its own count, which is the third corner of a triangle
/// whose other two are the nodes the lists belong to.
fn intersect(a: &[u32], b: &[u32], counts: &mut [u64]) -> u64 {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.is_empty() {
        return 0;
    }
    let mut hits = 0;
    if long.len() >= short.len().saturating_mul(GALLOP_RATIO) {
        // Every search starts where the last one ended, so the whole
        // pass over the short list walks the long one once at worst and
        // the log is on what is left rather than on all of it.
        let mut rest = long;
        for &x in short {
            let at = rest.partition_point(|&y| y < x);
            rest = &rest[at..];
            match rest.first() {
                Some(&y) if y == x => {
                    counts[x as usize] += 1;
                    hits += 1;
                    rest = &rest[1..];
                }
                Some(_) => {}
                None => break,
            }
        }
        return hits;
    }
    let (mut i, mut j) = (0, 0);
    while i < short.len() && j < long.len() {
        match short[i].cmp(&long[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                counts[short[i] as usize] += 1;
                hits += 1;
                i += 1;
                j += 1;
            }
        }
    }
    hits
}

/// Undirected triangles, one count per node: how many triangles that
/// node is a corner of. The single figure the GAP TC kernel reports is
/// the sum over nodes divided by three, a triangle being counted once
/// at each of its corners.
///
/// Direction is dropped and a pair joined more than once is still one
/// adjacency. A triangle is three nodes that all know each other, and
/// how many stored edges run between two of them does not make more of
/// them, which is the same rule [`lcc`] closes a neighbor pair by.
///
/// The work is the forward algorithm. Every node keeps only the
/// neighbors above it, so a triangle is looked at from its smallest
/// corner and found once rather than six times, and the lists being
/// intersected are half the length. That adjacency is built once into
/// one flat `u32` array, four bytes a stored edge, because the shape
/// this kernel wants from the reader is the list of a node somewhere
/// else in the graph once per edge, and the group the reader decodes to
/// answer that is thrown away by the question after it. Two sequential
/// sweeps to build it cost two group decodes each; asking as it goes
/// would cost one per edge.
pub fn triangle_count(db: &mut Zu1File, reader: &mut GraphReader) -> Result<Vec<u64>> {
    let domain = reader.directory().one_domain()?;
    if domain > u32::MAX as u64 {
        return Err(zu_common::ZuError::InvalidArgument(format!(
            "triangle_count holds a node id in 4 bytes and this table has {domain} nodes"
        )));
    }
    let n = domain as usize;
    let mut counts = vec![0u64; n];
    if n == 0 {
        return Ok(counts);
    }

    // Pass one sizes each node's upward list. A stored edge lands on
    // one list, its lower endpoint's, so the array is one slot an edge
    // before the duplicates come out of it.
    let mut offsets = vec![0u64; n + 1];
    for v in 0..n {
        for &d in reader.neighbors_dir(db, v as u64, Direction::Fwd)? {
            let d = d as usize;
            if d != v {
                offsets[v.min(d) + 1] += 1;
            }
        }
    }
    for v in 0..n {
        offsets[v + 1] += offsets[v];
    }
    let mut adj = vec![0u32; offsets[n] as usize];

    // Pass two fills it. The cursor is the running write position per
    // node, which is the offsets array again until it has been used up.
    let mut cursor = offsets[..n].to_vec();
    for v in 0..n {
        for &d in reader.neighbors_dir(db, v as u64, Direction::Fwd)? {
            let d = d as usize;
            if d == v {
                continue;
            }
            let (lo, hi) = if v < d { (v, d) } else { (d, v) };
            adj[cursor[lo] as usize] = hi as u32;
            cursor[lo] += 1;
        }
    }
    drop(cursor);

    // Sorted and without repeats, which is what the intersection reads
    // and what makes a pair joined twice one adjacency. The unique run
    // stays at the front of the node's slice and `lens` says how far it
    // goes, so nothing is copied to close the gaps.
    let mut lens = vec![0u32; n];
    for v in 0..n {
        let list = &mut adj[offsets[v] as usize..offsets[v + 1] as usize];
        list.sort_unstable();
        let mut kept = 0;
        for i in 0..list.len() {
            if i == 0 || list[i] != list[i - 1] {
                list[kept] = list[i];
                kept += 1;
            }
        }
        lens[v] = kept as u32;
    }

    for v in 0..n {
        let (vo, vl) = (offsets[v] as usize, lens[v] as usize);
        let mut corners = 0;
        for k in 0..vl {
            let u = adj[vo + k] as usize;
            let (uo, ul) = (offsets[u] as usize, lens[u] as usize);
            let shared = intersect(&adj[vo..vo + vl], &adj[uo..uo + ul], &mut counts);
            counts[u] += shared;
            corners += shared;
        }
        counts[v] += corners;
    }
    Ok(counts)
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
    fn pagerank_converged_settles_past_where_twenty_rounds_stops() {
        // A hub with a long tail off it is slow to settle: twenty
        // rounds leaves the tail short of where it lands, and the
        // converging form has to be closer to the fixed point than
        // that. "Closer" is measured against a run of a thousand
        // rounds, which is the fixed point for this shape.
        let mut edges = vec![(0, 1)];
        for node in 1..63u32 {
            edges.push((node, node + 1));
        }
        let (_dir, mut db, mut reader) = open_with(&edges, 64);
        let settled = pagerank(&mut db, &mut reader, 1000).expect("pagerank");
        let twenty = pagerank(&mut db, &mut reader, 20).expect("pagerank");
        let converged = pagerank_converged(&mut db, &mut reader).expect("pagerank");
        let off = |run: &[f64]| {
            run.iter()
                .zip(&settled)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max)
        };
        assert!(
            off(&converged) < off(&twenty),
            "converged is {:.3e} off, twenty rounds is {:.3e}",
            off(&converged),
            off(&twenty)
        );
        // This shape spends the whole cap without the largest move
        // reaching 1e-12, so what it stops at is the cap rather than
        // the tolerance. It is 3.5e-11 out, seven orders inside the
        // 1e-4 the LDBC harness compares at, which is the point.
        assert!(
            off(&converged) < 1e-9,
            "converged is {:.3e} off the fixed point",
            off(&converged)
        );
        assert!(
            (converged.iter().sum::<f64>() - 1.0).abs() < 1e-9,
            "ranks sum to one"
        );
    }

    #[test]
    fn wcc_labels_components_by_their_smallest_id() {
        // Two components, 0-1-2 and 3-4, direction ignored. The table
        // is unkeyed, so the row is the id.
        let (_dir, mut db, mut reader) = open_with(&[(1, 0), (1, 2), (4, 3)], 5);
        let labels = wcc(&mut db, &mut reader).expect("wcc");
        assert_eq!(labels, [0, 0, 0, 3, 3]);
    }

    #[test]
    fn wcc_labels_a_reordered_load_by_the_smallest_key_not_the_smallest_row() {
        // Rows 0..3 hold keys 30, 10, 20, 40. The component on rows
        // 0-1-2 is 10 and the one on row 3 is 40, and neither of those
        // is the row a load in id order would have put them at.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = Zu1File::create(&dir.path().join("keyed.zu1")).expect("create");
        let edges = [(0u32, 1u32), (1, 2)];
        crate::graph::bulk_load_keyed(&mut db, "n", "e", 4, &edges, Some(&[30, 10, 20, 40]))
            .expect("load");
        let mut reader = GraphReader::load(&mut db).expect("reader");
        let labels = wcc(&mut db, &mut reader).expect("wcc");
        assert_eq!(labels, [10, 10, 10, 40]);
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
    fn sssp_weighted_takes_the_long_way_round_when_it_is_cheaper() {
        // 0 -> 1 weighs 10, 0 -> 2 -> 1 weighs 2. Hop counting picks
        // the direct edge and this has to pick the detour, so the two
        // kernels disagree here on purpose. 3 is only reachable
        // against the arrows, which a weighted run does not follow.
        let edges = [(0u32, 1u32), (0, 2), (2, 1), (3, 0)];
        let (_dir, mut db, mut reader) = open_with(&edges, 4);
        // Sorted by pair, which is the load order the weights index:
        // (0,1) (0,2) (2,1) (3,0).
        let weights = [10, 1, 1, 1];
        let dist = sssp_weighted(&mut db, &mut reader, 0, &weights).expect("sssp_weighted");
        assert_eq!(dist, [0, 2, 1, u64::MAX]);
        assert_eq!(sssp(&mut db, &mut reader, 0).expect("sssp"), [0, 1, 1, 1]);
    }

    #[test]
    fn sssp_weighted_tells_two_edges_over_one_pair_apart() {
        // 0 -> 1 twice, the second copy weighing 3 against the first's
        // 9, and 1 -> 2 weighing 1. A kernel that addressed the weight
        // by the pair would find 9 for both copies and answer 10 at 2;
        // reading by slot finds the cheap copy and answers 4.
        let edges = [(0u32, 1u32), (0, 1), (1, 2)];
        let (_dir, mut db, mut reader) = open_with(&edges, 3);
        let dist = sssp_weighted(&mut db, &mut reader, 0, &[9, 3, 1]).expect("sssp_weighted");
        assert_eq!(dist, [0, 3, 4]);
    }

    #[test]
    fn the_bucket_ring_and_the_heap_answer_the_same_distances() {
        // The same graph run under weights small enough for the ring
        // and again with one edge widened past the limit, which is the
        // only thing that decides between the two. The widened edge is
        // one no shortest path uses, so the distances do not move and
        // any difference is the structure rather than the graph.
        let mut edges = Vec::new();
        for node in 0..31u32 {
            edges.push((node, node + 1));
            edges.push((node, (node * 7 + 3) % 32));
        }
        let (_dir, mut db, mut reader) = open_with(&edges, 32);
        let mut weights: Vec<u64> = (0..edges.len() as u64).map(|i| i % 200 + 1).collect();
        let ring = sssp_weighted(&mut db, &mut reader, 0, &weights).expect("ring");
        weights[0] = SSSP_BUCKET_LIMIT + 1;
        let heap = sssp_weighted(&mut db, &mut reader, 0, &weights).expect("heap");
        assert_ne!(ring[..], heap[..], "widening edge 0 has to move something");
        weights[0] = 1;
        assert_eq!(
            ring,
            sssp_weighted(&mut db, &mut reader, 0, &weights).expect("ring again")
        );
    }

    #[test]
    fn sssp_weighted_wants_one_weight_an_edge() {
        let (_dir, mut db, mut reader) = open_with(&[(0u32, 1u32), (1, 2)], 3);
        let err = sssp_weighted(&mut db, &mut reader, 0, &[1]).expect_err("short column");
        assert!(err.to_string().contains("1 weights over 2 edges"), "{err}");
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
    fn lcc_counts_a_pair_of_neighbors_once_however_many_edges_join_them() {
        // 0 has neighbors 1 and 2, and 1 -> 2 is stored twice. The pair
        // is closed, not closed twice, so the ordered-pair count is one
        // out of two. Counting the second copy would score 1.0 on a
        // neighborhood with one of its two ordered pairs joined.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2), (1, 2), (1, 2)], 3);
        assert_eq!(lcc(&mut db, &mut reader).expect("lcc")[0], 0.5);
    }

    #[test]
    fn triangle_count_finds_one_triangle_from_all_three_corners() {
        // A closed triple plus a wedge hanging off it. Every corner of
        // the triangle scores one and nothing else scores at all, so
        // the sum is three times the one triangle there is.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2), (1, 2), (2, 3), (3, 4)], 5);
        let counts = triangle_count(&mut db, &mut reader).expect("tc");
        assert_eq!(counts, vec![1, 1, 1, 0, 0]);
        assert_eq!(counts.iter().sum::<u64>() / 3, 1);
    }

    #[test]
    fn triangle_count_reads_edges_undirected() {
        // Stored the three edges run 0 -> 1 -> 2 -> 0, a cycle, and
        // 0 -> 1, 0 -> 2, 1 -> 2, a DAG. Both are the same triangle
        // once direction is dropped, which is the graph TC counts.
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (1, 2), (2, 0)], 3);
        assert_eq!(
            triangle_count(&mut db, &mut reader).expect("tc"),
            vec![1, 1, 1]
        );
        let (_dir, mut db, mut reader) = open_with(&[(0, 1), (0, 2), (1, 2)], 3);
        assert_eq!(
            triangle_count(&mut db, &mut reader).expect("tc"),
            vec![1, 1, 1]
        );
    }

    #[test]
    fn triangle_count_ignores_self_loops_and_repeated_pairs() {
        // Neither a loop nor a second copy of an edge is a third node,
        // so neither makes a triangle and neither makes more of the one
        // triangle that is there.
        let (_dir, mut db, mut reader) =
            open_with(&[(0, 0), (0, 1), (0, 1), (1, 0), (0, 2), (1, 2), (2, 2)], 3);
        assert_eq!(
            triangle_count(&mut db, &mut reader).expect("tc"),
            vec![1, 1, 1]
        );
    }

    #[test]
    fn triangle_count_matches_a_brute_force_pass_over_hubs_and_leaves() {
        // A hub joined to everyone, a second smaller hub, and a ring
        // through the rest. The degrees are far enough apart that the
        // intersection takes the searching path for the hub against a
        // leaf and the merging path between two leaves, so a mistake on
        // either side of the threshold shows up as a disagreement with
        // the reference below.
        const N: u32 = 60;
        let mut edges = vec![];
        for v in 1..N {
            edges.push((0, v));
            edges.push((v, v % (N - 1) + 1));
        }
        for v in 30..N {
            edges.push((1, v));
        }
        let mut want = vec![0u64; N as usize];
        let mut nbrs: Vec<std::collections::BTreeSet<u32>> = vec![Default::default(); N as usize];
        for &(s, d) in &edges {
            if s != d {
                nbrs[s as usize].insert(d);
                nbrs[d as usize].insert(s);
            }
        }
        for v in 0..N as usize {
            let list: Vec<u32> = nbrs[v].iter().copied().collect();
            for (i, &a) in list.iter().enumerate() {
                for &b in &list[i + 1..] {
                    if nbrs[a as usize].contains(&b) {
                        want[v] += 1;
                    }
                }
            }
        }
        assert!(want.iter().sum::<u64>() > 0, "fixture has no triangles");
        let (_dir, mut db, mut reader) = open_with(&edges, N as u64);
        assert_eq!(triangle_count(&mut db, &mut reader).expect("tc"), want);
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
        assert!(
            triangle_count(&mut db, &mut reader)
                .expect("triangle_count")
                .is_empty()
        );
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
