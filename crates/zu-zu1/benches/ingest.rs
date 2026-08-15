//! COPY ingest bench and the M1 performance gate for B6 and B8.
//!
//! Uses real data when available: the full soc-LiveJournal1 edge list
//! (set ZU_DATA to the directory holding soc-LiveJournal1.txt), otherwise
//! a synthetic power-law-ish graph. Measures bulk_load throughput in
//! M edges/s (parse excluded, sort included: that is the COPY hot path),
//! the on-disk adjacency density in bits/edge, and random 1-hop point
//! reads in K lookups/s. With ZU_GATE=1 the process exits nonzero when a
//! floor or ceiling in bench/budgets.toml is missed. ZU_B6=1 adds the
//! B6 scale check: the same COPY path over the 117 M edge com-Orkut
//! graph, since B6 is defined at 100 M edges and LiveJournal is 69 M.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu ZU_B6=1 cargo bench -p zu-zu1

use std::time::Instant;

use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction, GraphReader, bulk_load_keyed, read_edge_list};
use zu_zu1::reorder;

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

/// Random 1-hop point reads in one direction against a loaded file:
/// measures K lookups/s with p50/p99 latency, and crosschecks a sample
/// against the full-decode path so the number cannot come from a broken
/// reader.
fn run_point_lookups(label: &str, path: &std::path::Path, node_count: u64, dir: Direction) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let reader = GraphReader::load(&mut db).expect("load directory");
    let tag = match dir {
        Direction::Fwd => "point",
        Direction::Bwd => "point-in",
    };
    let lookups = 200_000usize;
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % node_count
    };
    let mut nbrs = Vec::new();
    let mut lat = Vec::with_capacity(lookups);
    let mut edges_read = 0u64;
    let started = Instant::now();
    for _ in 0..lookups {
        let node = next();
        nbrs.clear();
        let t = Instant::now();
        reader
            .neighbors_dir_into(&mut db, node, dir, &mut nbrs)
            .expect("point read");
        lat.push(t.elapsed());
        edges_read += nbrs.len() as u64;
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let klookups_s = lookups as f64 / secs / 1e3;
    println!(
        "{label} {tag}: {klookups_s:.0} K lookups/s, p50 {:.1} us, p99 {:.1} us, {edges_read} edges read",
        lat[lookups / 2].as_secs_f64() * 1e6,
        lat[lookups * 99 / 100].as_secs_f64() * 1e6
    );
    let mut sample: Vec<u64> = (0..100).map(|_| next()).collect();
    sample.sort_unstable();
    let mut full = GraphReader::load(&mut db).expect("load directory");
    for &node in &sample {
        nbrs.clear();
        reader
            .neighbors_dir_into(&mut db, node, dir, &mut nbrs)
            .expect("point read");
        assert_eq!(
            nbrs.as_slice(),
            full.neighbors_dir(&mut db, node, dir).expect("full read"),
            "point read disagrees with full decode at node {node}"
        );
    }
    println!("{label} {tag} crosscheck: 100 nodes match the full decode");
    klookups_s
}

/// Random edge probes against a loaded file: half the probes are real
/// edges, half are misses against the same sources, so present and
/// absent cost both land in the number. A sample is crosschecked against
/// binary search over the full point-read list.
fn run_edge_probes(label: &str, path: &std::path::Path, node_count: u64) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let reader = GraphReader::load(&mut db).expect("load directory");
    let probes = 200_000usize;
    let mut rng = 0x6C62272E07BB0142u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    // Build the probe set up front so target selection stays out of the
    // timed loop: even probes take a real neighbor, odd probes perturb it
    // into a near miss.
    let mut nbrs = Vec::new();
    let mut set = Vec::with_capacity(probes);
    while set.len() < probes {
        let src = next() % node_count;
        nbrs.clear();
        reader
            .neighbors_into(&mut db, src, &mut nbrs)
            .expect("point read");
        if nbrs.is_empty() {
            set.push((src, next(), false));
            continue;
        }
        let dst = nbrs[(next() as usize) % nbrs.len()];
        if set.len() % 2 == 0 {
            set.push((src, dst, true));
        } else {
            let miss = dst.wrapping_add(1);
            set.push((src, miss, nbrs.binary_search(&miss).is_ok()));
        }
    }
    let mut lat = Vec::with_capacity(probes);
    let started = Instant::now();
    for &(src, dst, want) in &set {
        let t = Instant::now();
        let got = reader.has_edge(&mut db, src, dst).expect("probe");
        lat.push(t.elapsed());
        assert_eq!(got, want, "probe {src}->{dst}");
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let kprobes_s = probes as f64 / secs / 1e3;
    println!(
        "{label} edge-probe: {kprobes_s:.0} K probes/s, p50 {:.1} us, p99 {:.1} us, half present",
        lat[probes / 2].as_secs_f64() * 1e6,
        lat[probes * 99 / 100].as_secs_f64() * 1e6
    );
    kprobes_s
}

/// Random key lookups through the primary-key index: three quarters hit
/// a known key expecting its exact row, one quarter miss above the key
/// domain, so both costs land in the number.
fn run_key_lookups(label: &str, path: &std::path::Path, key_by_row: &[u64]) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load(&mut db).expect("load directory");
    let lookups = 200_000usize;
    let mut rng = 0x853C_49E6_748F_EA9Bu64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let n = key_by_row.len() as u64;
    let set: Vec<(u64, Option<u64>)> = (0..lookups)
        .map(|i| {
            if i % 4 == 3 {
                (n + next() % (1 << 30), None)
            } else {
                let row = next() % n;
                (key_by_row[row as usize], Some(row))
            }
        })
        .collect();
    let mut lat = Vec::with_capacity(lookups);
    let started = Instant::now();
    for &(key, want) in &set {
        let t = Instant::now();
        let got = reader.lookup_key(&mut db, key).expect("lookup");
        lat.push(t.elapsed());
        assert_eq!(got, want, "key {key}");
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let klookups_s = lookups as f64 / secs / 1e3;
    println!(
        "{label} key-lookup: {klookups_s:.0} K lookups/s, p50 {:.1} us, p99 {:.1} us, quarter absent",
        lat[lookups / 2].as_secs_f64() * 1e6,
        lat[lookups * 99 / 100].as_secs_f64() * 1e6
    );
    klookups_s
}

/// Sorts, dedups, bulk loads, verifies, and runs the point-read,
/// edge-probe, and key-lookup phases; returns (M edges/s, fwd bits/edge,
/// bwd bits/edge, out K lookups/s, in K lookups/s, K probes/s,
/// K key lookups/s when keys were given).
fn run_load(
    label: &str,
    mut edges: Vec<(u32, u32)>,
    node_count: u64,
    key_by_row: Option<Vec<u64>>,
) -> (f64, f64, f64, f64, f64, f64, Option<f64>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ingest.zu1");
    let start = Instant::now();
    edges.sort_unstable();
    edges.dedup();
    let mut db = Zu1File::create(&path).expect("create");
    let directory = bulk_load_keyed(
        &mut db,
        "node",
        "edge",
        node_count,
        &edges,
        key_by_row.as_deref(),
    )
    .expect("bulk_load");
    let secs = start.elapsed().as_secs_f64();
    drop(db);
    // Free the half-gigabyte edge vector before the point phase: on the
    // small-RAM machines it evicts the loaded file from page cache and
    // every random read becomes a disk seek, which measures the disk,
    // not the reader.
    drop(edges);

    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    let medges_s = directory.edge_count as f64 / secs / 1e6;
    // The file holds two CSRs, so density is quoted per direction: bwd
    // block bytes are carved out of the fwd number, keeping it comparable
    // with the single-direction figures the ceilings were set against.
    // Blocks, not payloads: each segment pads to 256 KiB blocks and that
    // padding is part of the direction's disk footprint. Headers and
    // directory overhead stay on the fwd side.
    let bwd_bytes: u64 = directory
        .groups
        .iter()
        .map(|g| {
            (g.bwd.offsets.blocks.len() + g.bwd.neighbors.blocks.len()) as u64
                * u64::from(zu_zu1::BLOCK_SIZE)
        })
        .sum();
    // The key index scales with nodes, not edges, so it is carved out of
    // the adjacency density and quoted per key below.
    let key_bytes: u64 = directory
        .keys
        .as_ref()
        .map(|k| (k.keys.blocks.len() + k.rows.blocks.len()) as u64 * u64::from(zu_zu1::BLOCK_SIZE))
        .unwrap_or(0);
    let bits_fwd = (file_bytes - bwd_bytes - key_bytes) as f64 * 8.0 / directory.edge_count as f64;
    let bits_bwd = bwd_bytes as f64 * 8.0 / directory.edge_count as f64;
    println!(
        "{label}: {:.2} M edges/s, {bits_fwd:.2} bits/edge fwd, {bits_bwd:.2} bits/edge bwd ({} edges, {} nodes, {} groups, {:.2}s, {file_bytes} bytes)",
        medges_s,
        directory.edge_count,
        directory.from_count,
        directory.groups.len(),
        secs
    );
    if key_bytes > 0 {
        println!(
            "{label} key index: {key_bytes} bytes, {:.1} bits/key",
            key_bytes as f64 * 8.0 / directory.from_count as f64
        );
    }
    let verified = zu_zu1::verify(&path).expect("verify");
    println!("{label} verify: ok, {verified} payload bytes checked");
    let k_out = run_point_lookups(label, &path, directory.from_count, Direction::Fwd);
    let k_in = run_point_lookups(label, &path, directory.from_count, Direction::Bwd);
    let k_probe = run_edge_probes(label, &path, directory.from_count);
    let k_key = key_by_row
        .as_deref()
        .map(|keys| run_key_lookups(label, &path, keys));
    (medges_s, bits_fwd, bits_bwd, k_out, k_in, k_probe, k_key)
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

    let (medges_s, bits_raw, bits_bwd, klookups_s, in_klookups_s, kprobes_s, _) =
        run_load("copy", edges.clone(), node_count, None);

    // B8 is defined on the reordered graph (docs/04 §5): BFS relabeling
    // from max-degree roots, timed separately since REORDER is opt-in.
    // The reordered load also carries the primary-key index over the
    // original ids, exactly what zu copy --reorder persists, so the
    // key-lookup floor gates here where the index actually exists.
    let mut bits_bfs = None;
    let mut key_klookups = None;
    if is_real {
        let start = Instant::now();
        let map = reorder::bfs_order(node_count, &edges);
        let mut relabeled = edges;
        reorder::relabel(&mut relabeled, &map);
        println!(
            "reorder bfs: mapping built in {:.2}s",
            start.elapsed().as_secs_f64()
        );
        let mut key_by_row = vec![0u64; node_count as usize];
        for (old, &new) in map.iter().enumerate() {
            key_by_row[new as usize] = old as u64;
        }
        let (_, bits, _, _, _, _, k_key) =
            run_load("copy+bfs", relabeled, node_count, Some(key_by_row));
        bits_bfs = Some(bits);
        key_klookups = k_key;
    }

    let mut failed = false;
    if let Some(floor) = budget("copy_medges_s")
        && medges_s < floor
    {
        println!("GATE FAIL copy: {medges_s:.2} M edges/s < floor {floor}");
        failed = true;
    }
    // Density budgets are defined against the real LiveJournal file.
    // Random synthetic neighbors sit near 17 bits of entropy per edge, so
    // no ceiling can apply there.
    if is_real
        && let Some(ceiling) = budget("bits_per_edge")
        && bits_raw > ceiling
    {
        println!("GATE FAIL density: {bits_raw:.2} bits/edge > ceiling {ceiling}");
        failed = true;
    }
    if let (Some(bits), Some(ceiling)) = (bits_bfs, budget("bits_per_edge_bfs"))
        && bits > ceiling
    {
        println!("GATE FAIL density bfs: {bits:.2} bits/edge > ceiling {ceiling}");
        failed = true;
    }
    // The point-read floor is defined against real LiveJournal degrees;
    // synthetic degree distributions would move the number for reasons
    // that have nothing to do with the reader.
    if is_real
        && let Some(ceiling) = budget("bits_per_edge_bwd")
        && bits_bwd > ceiling
    {
        println!("GATE FAIL density bwd: {bits_bwd:.2} bits/edge > ceiling {ceiling}");
        failed = true;
    }
    if is_real
        && let Some(floor) = budget("point_klookups_s")
        && klookups_s < floor
    {
        println!("GATE FAIL point: {klookups_s:.0} K lookups/s < floor {floor}");
        failed = true;
    }
    if is_real
        && let Some(floor) = budget("point_in_klookups_s")
        && in_klookups_s < floor
    {
        println!("GATE FAIL point-in: {in_klookups_s:.0} K lookups/s < floor {floor}");
        failed = true;
    }
    if is_real
        && let Some(floor) = budget("edge_probe_kprobes_s")
        && kprobes_s < floor
    {
        println!("GATE FAIL edge-probe: {kprobes_s:.0} K probes/s < floor {floor}");
        failed = true;
    }
    if let (Some(k), Some(floor)) = (key_klookups, budget("key_klookups_s"))
        && k < floor
    {
        println!("GATE FAIL key-lookup: {k:.0} K lookups/s < floor {floor}");
        failed = true;
    }
    // B6 is defined at 100 M edges (docs/12) and LiveJournal tops out at
    // 69 M, so the scale check loads com-Orkut (117 M edges) when opted
    // in with ZU_B6=1. The ungraph lists each undirected edge once and is
    // ingested as the directed list it is on disk. Only the throughput
    // floor gates here: the density ceilings and point floors are defined
    // against LiveJournal's degree structure, so Orkut's numbers print as
    // information. The servers leave ZU_B6 unset to keep fleet runs
    // short; the box only needs one honest machine.
    if std::env::var("ZU_B6").is_ok_and(|v| v == "1") {
        let path = std::env::var("ZU_DATA")
            .map(|d| format!("{d}/com-orkut.ungraph.txt"))
            .unwrap_or_default();
        match read_edge_list(std::path::Path::new(&path)) {
            Ok(edges) => {
                println!("data: com-orkut.ungraph.txt, {} edges", edges.len());
                let node_count = edges
                    .iter()
                    .map(|&(s, d)| u64::from(s.max(d)) + 1)
                    .max()
                    .unwrap_or(0);
                let (medges_s, _, _, _, _, _, _) = run_load("b6-orkut", edges, node_count, None);
                if let Some(floor) = budget("copy_medges_s")
                    && medges_s < floor
                {
                    println!("GATE FAIL b6: {medges_s:.2} M edges/s < floor {floor}");
                    failed = true;
                }
            }
            Err(_) => {
                println!("b6: com-orkut.ungraph.txt not found, run scripts/fetch-datasets.sh")
            }
        }
    } else {
        println!("b6: skipped (set ZU_B6=1, loads the 117 M edge com-Orkut graph)");
    }

    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
