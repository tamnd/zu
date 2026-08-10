//! Graph kernel throughput bench for the in-memory CSR slice.
//!
//! Uses real data when available: edges from soc-LiveJournal1 (set
//! ZU_DATA to the directory holding soc-LiveJournal1.txt, capped at 8 M
//! edges), otherwise a synthetic power-law-ish graph. Reports CSR build
//! edges/s, BFS MTEPS from the max-degree vertex, triangle counting
//! throughput on a 2 M edge prefix, and pagerank iterations/s.
//! No gate floors yet: the numbers are informational.
//!
//! Run: ZU_DATA=~/data/zu cargo bench -p zu-query

use std::time::Instant;

use zu_query::csr::Csr;
use zu_query::{kernels, recursive};

const EDGE_CAP: usize = 8_000_000;
const TRIANGLE_EDGES: usize = 2_000_000;
const SYNTHETIC_NODES: u32 = 1 << 18;
const SYNTHETIC_EDGES: usize = 4_000_000;
const PAGERANK_ITERS: u32 = 10;

fn load_livejournal(dir: &str) -> Option<(u32, Vec<(u32, u32)>)> {
    let path = format!("{dir}/soc-LiveJournal1.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let mut edges: Vec<(u32, u32)> = Vec::with_capacity(EDGE_CAP);
    let mut max_id = 0u32;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(src), Some(dst)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (src, dst): (u32, u32) = (src.parse().ok()?, dst.parse().ok()?);
        max_id = max_id.max(src).max(dst);
        edges.push((src, dst));
        if edges.len() >= EDGE_CAP {
            break;
        }
    }
    let nodes = max_id + 1;
    println!(
        "data: soc-LiveJournal1.txt, {} edges, {nodes} nodes",
        edges.len()
    );
    Some((nodes, edges))
}

fn synthetic() -> (u32, Vec<(u32, u32)>) {
    // Uniform sources, destinations drawn as rng % (1 + rng % n): the
    // low ids soak up mass roughly harmonically, which is power-law-ish
    // enough to exercise skewed adjacency lists.
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let n = u64::from(SYNTHETIC_NODES);
    let edges: Vec<(u32, u32)> = (0..SYNTHETIC_EDGES)
        .map(|_| {
            let src = (next() % n) as u32;
            let dst = (next() % (1 + next() % n)) as u32;
            (src, dst)
        })
        .collect();
    println!(
        "data: synthetic power-law-ish graph, {SYNTHETIC_EDGES} edges, {SYNTHETIC_NODES} nodes, set ZU_DATA for real input"
    );
    (SYNTHETIC_NODES, edges)
}

fn main() {
    let (nodes, edges) = std::env::var("ZU_DATA")
        .ok()
        .and_then(|d| load_livejournal(&d))
        .unwrap_or_else(synthetic);
    let raw_edges = edges.len();

    let start = Instant::now();
    let csr = Csr::from_edges(nodes, edges.clone());
    let secs = start.elapsed().as_secs_f64();
    println!(
        "csr_build: {:.1} M edges/s ({} kept of {raw_edges} raw, {secs:.3} s)",
        raw_edges as f64 / secs / 1e6,
        csr.edge_count()
    );

    let source = (0..nodes).max_by_key(|&v| csr.degree(v)).unwrap_or(0);
    // Warm once, then loop for at least a second of wall time.
    let levels = kernels::bfs(&csr, source);
    let mut iters = 0u32;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 1.0 {
        std::hint::black_box(kernels::bfs(&csr, source));
        iters += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let reached = levels.iter().filter(|&&l| l != u32::MAX).count();
    let traversed: u64 = levels
        .iter()
        .enumerate()
        .filter(|&(_, &l)| l != u32::MAX)
        .map(|(v, _)| u64::from(csr.degree(v as u32)))
        .sum();
    println!(
        "bfs: {:.1} MTEPS from vertex {source} ({reached} of {nodes} reached, {iters} iters)",
        traversed as f64 * f64::from(iters) / secs / 1e6
    );

    let rev = csr.reversed();
    let recursive = recursive::recursive_bfs(&csr, Some(&rev), &[source], u32::MAX, 0);
    assert_eq!(recursive, levels, "recursive_bfs must agree with bfs");
    let mut iters = 0u32;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 1.0 {
        std::hint::black_box(recursive::recursive_bfs(
            &csr,
            Some(&rev),
            &[source],
            u32::MAX,
            0,
        ));
        iters += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "recursive_bfs: {:.1} MTEPS hybrid frontier, all cores ({iters} iters)",
        traversed as f64 * f64::from(iters) / secs / 1e6
    );

    let sources: Vec<u32> = (0..256u32).map(|i| (i * 2654435761) % nodes).collect();
    let start = Instant::now();
    let mut pairs = 0u64;
    recursive::hybrid_bfs(&csr, Some(&rev), &sources, 2, 0, &mut |_, _, _| pairs += 1);
    let secs = start.elapsed().as_secs_f64();
    println!(
        "hybrid_bfs: {:.1} sources/s multi-source morsels (256 sources to 2 hops, {pairs} pairs, {secs:.3} s)",
        256.0 / secs
    );

    let prefix = edges[..edges.len().min(TRIANGLE_EDGES)].to_vec();
    let prefix_len = prefix.len();
    let tri_csr = Csr::from_edges(nodes, prefix);
    let start = Instant::now();
    let triangles = kernels::triangle_count(&tri_csr);
    let secs = start.elapsed().as_secs_f64();
    println!(
        "triangles: {:.2} M edges/s ({triangles} triangles on a {prefix_len} edge prefix, {secs:.3} s)",
        prefix_len as f64 / secs / 1e6
    );

    let start = Instant::now();
    let pr = kernels::pagerank(&csr, 0.85, PAGERANK_ITERS);
    let secs = start.elapsed().as_secs_f64();
    let sum: f64 = pr.iter().sum();
    println!(
        "pagerank: {:.2} iters/s ({PAGERANK_ITERS} iterations, score sum {sum:.6}, {secs:.3} s)",
        f64::from(PAGERANK_ITERS) / secs
    );
}
