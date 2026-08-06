//! COPY ingest bench and the M1 performance gate for B6 and B8.
//!
//! Uses real data when available: the full soc-LiveJournal1 edge list
//! (set ZU_DATA to the directory holding soc-LiveJournal1.txt), otherwise
//! a synthetic power-law-ish graph. Measures bulk_load throughput in
//! M edges/s (parse excluded, sort included: that is the COPY hot path)
//! and the on-disk adjacency density in bits/edge. With ZU_GATE=1 the
//! process exits nonzero when copy_medges_s drops below its floor or
//! bits_per_edge rises above its ceiling in bench/budgets.toml.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu-zu1

use std::time::Instant;

use zu_zu1::file::Zu1File;
use zu_zu1::graph::{bulk_load, read_edge_list};

fn load_real(dir: &str) -> Option<Vec<(u32, u32)>> {
    let path = format!("{dir}/soc-LiveJournal1.txt");
    let edges = read_edge_list(std::path::Path::new(&path)).ok()?;
    println!("data: soc-LiveJournal1.txt, {} edges", edges.len());
    Some(edges)
}

fn synthetic() -> Vec<(u32, u32)> {
    let n = 1u64 << 20;
    let mut rng = 0x2545F4914F6CDD1Du64;
    let edges = (0..16_000_000)
        .map(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Square the fractions so both endpoints skew low id, which
            // is roughly what a degree-ordered relabeling produces.
            let s = ((rng >> 32) * (rng >> 32)) >> 44;
            let d = ((rng & 0xFFFF_FFFF) * (rng & 0xFFFF_FFFF)) >> 44;
            ((s % n) as u32, (d % n) as u32)
        })
        .collect();
    println!("data: synthetic 16M edges over 1M nodes, set ZU_DATA for real input");
    edges
}

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

fn main() {
    let real = std::env::var("ZU_DATA").ok().and_then(|d| load_real(&d));
    let is_real = real.is_some();
    let edges = real.unwrap_or_else(synthetic);
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");

    let node_count = edges
        .iter()
        .map(|&(s, d)| u64::from(s.max(d)) + 1)
        .max()
        .unwrap_or(0);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ingest.zu1");

    let start = Instant::now();
    let mut sorted = edges;
    sorted.sort_unstable();
    sorted.dedup();
    let mut db = Zu1File::create(&path).expect("create");
    let directory = bulk_load(&mut db, node_count, &sorted).expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    drop(db);

    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    let medges_s = directory.edge_count as f64 / secs / 1e6;
    let bits_per_edge = file_bytes as f64 * 8.0 / directory.edge_count as f64;
    println!(
        "copy: {:.2} M edges/s ({} edges, {} nodes, {} groups, {:.2}s)",
        medges_s,
        directory.edge_count,
        directory.node_count,
        directory.groups.len(),
        secs
    );
    println!("adjacency: {bits_per_edge:.2} bits/edge ({file_bytes} bytes on disk)");

    let verified = zu_zu1::verify(&path).expect("verify");
    println!("verify: ok, {verified} payload bytes checked");

    let mut failed = false;
    if let Some(floor) = budget("copy_medges_s")
        && medges_s < floor
    {
        eprintln!("gate: copy {medges_s:.2} M edges/s below floor {floor}");
        failed = true;
    }
    // The density budget is defined against the real LiveJournal ordering.
    // Random synthetic neighbors sit near 17 bits of entropy per edge, so
    // the ceiling cannot apply there.
    if is_real
        && let Some(ceiling) = budget("bits_per_edge")
        && bits_per_edge > ceiling
    {
        eprintln!("gate: {bits_per_edge:.2} bits/edge above ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
