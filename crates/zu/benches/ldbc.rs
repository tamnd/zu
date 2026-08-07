//! LDBC SNB SF1 gates for B1, B2, and B4 (docs/11, docs/12 M2).
//!
//! Runs against the real SF1 person-knows-person graph, preprocessed by
//! scripts/prep-ldbc.sh into two plain text files under ZU_DATA:
//! ldbc-sf1-person-keys.txt (one person id per line) and
//! ldbc-sf1-knows.txt (src dst id pairs). The ids exceed u32, so the
//! load goes through densify_keyed and the primary-key index carries
//! the original ids, exactly the path a keyed COPY takes.
//!
//! B1 is warm 1-hop expands at deg <= 100 (the G1 workload shape), p50
//! in us. B2 is warm primary-key lookups, p50 in us. B4 is the 2-hop
//! factorized count over the whole graph through the query engine, p50
//! in ms. Every phase crosschecks against a reference computed from the
//! raw edge list, so a number cannot come from a broken reader or a
//! wrong plan. With ZU_GATE=1 the process exits nonzero when a ceiling
//! in bench/budgets.toml is missed, and missing data fails the gate
//! instead of skipping it.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu --bench ldbc

use std::time::Instant;

use zu::zu1::file::Zu1File;
use zu::zu1::graph::{
    Direction, GraphReader, bulk_load_keyed, densify_keyed, read_key_edge_list, read_key_list,
};
use zu_query::exec::Value;

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

fn xorshift(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

/// Loads the preprocessed SF1 files and bulk loads them into a fresh
/// zu1 file as person/knows with the id key index. Returns the dense
/// sorted edge list for reference computations plus the key of every
/// row.
fn load(data: &str, path: &std::path::Path) -> (Vec<(u32, u32)>, Vec<u64>) {
    let started = Instant::now();
    let keys = read_key_list(std::path::Path::new(&format!(
        "{data}/ldbc-sf1-person-keys.txt"
    )))
    .expect("person keys");
    let edges = read_key_edge_list(std::path::Path::new(&format!("{data}/ldbc-sf1-knows.txt")))
        .expect("knows edges");
    let (mut dense, by_row) = densify_keyed(&keys, &edges).expect("densify");
    dense.sort_unstable();
    dense.dedup();
    let parsed = started.elapsed();
    let load_started = Instant::now();
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_keyed(
        &mut db,
        "person",
        "knows",
        by_row.len() as u64,
        &dense,
        Some(&by_row),
    )
    .expect("bulk load");
    println!(
        "sf1: {} persons, {} knows edges, parse {:.2}s, load {:.2}s",
        by_row.len(),
        dense.len(),
        parsed.as_secs_f64(),
        load_started.elapsed().as_secs_f64()
    );
    (dense, by_row)
}

/// B1: warm 1-hop expands over rows with out-degree at most 100, the G1
/// workload shape docs/11 defines the budget on. The gated number goes
/// through the cached-group path, the read the executor's Expand makes
/// once a query is warm; the storage point path prints alongside as
/// information, its regression floors live with the LiveJournal keys.
/// A sample crosschecks against the reference adjacency built from the
/// raw edge list.
fn run_one_hop(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut outdeg = vec![0u32; node_count as usize];
    for &(s, _) in edges {
        outdeg[s as usize] += 1;
    }
    let eligible: Vec<u64> = (0..node_count)
        .filter(|&n| outdeg[n as usize] <= 100)
        .collect();
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("load knows");
    let lookups = 200_000usize;
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut nbrs = Vec::new();
    for _ in 0..10_000 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        reader
            .neighbors_dir(&mut db, node, Direction::Fwd)
            .expect("warmup read");
    }
    // A single warm expand sits at or below timer resolution, so the
    // clock samples batches of 100 and the quoted latencies are batch
    // means per expand. That is still a latency gate, not a throughput
    // one: a regression that makes expands decode per call moves the
    // p50 three orders of magnitude.
    let batch = 100usize;
    let batches = lookups / batch;
    let mut lat = Vec::with_capacity(batches);
    let mut edges_read = 0u64;
    let started = Instant::now();
    for _ in 0..batches {
        let t = Instant::now();
        for _ in 0..batch {
            let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
            nbrs.clear();
            let hop = reader
                .neighbors_dir(&mut db, node, Direction::Fwd)
                .expect("expand read");
            nbrs.extend_from_slice(hop);
            edges_read += nbrs.len() as u64;
        }
        lat.push(t.elapsed() / batch as u32);
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let p50 = lat[batches / 2].as_secs_f64() * 1e6;
    println!(
        "sf1 1-hop expand (deg <= 100, {} of {} rows): p50 {p50:.3} us, p99 {:.3} us per expand over batches of {batch}, {:.0} K expands/s, {edges_read} edges read",
        eligible.len(),
        node_count,
        lat[batches * 99 / 100].as_secs_f64() * 1e6,
        lookups as f64 / secs / 1e3
    );
    let mut point_lat = Vec::with_capacity(lookups / 10);
    for _ in 0..lookups / 10 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        nbrs.clear();
        let t = Instant::now();
        reader
            .neighbors_dir_into(&mut db, node, Direction::Fwd, &mut nbrs)
            .expect("point read");
        point_lat.push(t.elapsed());
    }
    point_lat.sort_unstable();
    println!(
        "sf1 1-hop point path: p50 {:.2} us, p99 {:.2} us (information, floors on LiveJournal)",
        point_lat[point_lat.len() / 2].as_secs_f64() * 1e6,
        point_lat[point_lat.len() * 99 / 100].as_secs_f64() * 1e6
    );
    for _ in 0..100 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        let want: Vec<u64> = edges
            .iter()
            .filter(|&&(s, _)| u64::from(s) == node)
            .map(|&(_, d)| u64::from(d))
            .collect();
        let hop = reader
            .neighbors_dir(&mut db, node, Direction::Fwd)
            .expect("expand read");
        assert_eq!(hop, want, "expand of {node} disagrees with the edge list");
        nbrs.clear();
        reader
            .neighbors_dir_into(&mut db, node, Direction::Fwd, &mut nbrs)
            .expect("point read");
        assert_eq!(
            nbrs, want,
            "point neighbors of {node} disagree with the edge list"
        );
    }
    println!("sf1 1-hop crosscheck: 100 nodes match the edge list on both paths");
    p50
}

/// B2: warm primary-key lookups through the sealed sorted index, all
/// hits, each asserted against the row the key was loaded at.
fn run_key_lookups(path: &std::path::Path, by_row: &[u64]) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("load knows");
    let lookups = 200_000usize;
    let mut rng = 0x853C_49E6_748F_EA9Bu64;
    let n = by_row.len() as u64;
    for _ in 0..10_000 {
        let row = xorshift(&mut rng) % n;
        reader
            .lookup_key(&mut db, by_row[row as usize])
            .expect("warmup lookup");
    }
    let set: Vec<(u64, u64)> = (0..lookups)
        .map(|_| {
            let row = xorshift(&mut rng) % n;
            (by_row[row as usize], row)
        })
        .collect();
    let mut lat = Vec::with_capacity(lookups);
    let started = Instant::now();
    for &(key, want) in &set {
        let t = Instant::now();
        let got = reader.lookup_key(&mut db, key).expect("lookup");
        lat.push(t.elapsed());
        assert_eq!(got, Some(want), "key {key}");
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let p50 = lat[lookups / 2].as_secs_f64() * 1e6;
    println!(
        "sf1 key-lookup: p50 {p50:.2} us, p99 {:.2} us, {:.0} K lookups/s, all hits",
        lat[lookups * 99 / 100].as_secs_f64() * 1e6,
        lookups as f64 / secs / 1e3
    );
    assert_eq!(
        reader
            .lookup_key(&mut db, by_row[n as usize - 1] + 1)
            .expect("miss"),
        None,
        "a key above the domain must miss"
    );
    p50
}

/// B4: the 2-hop factorized count over the whole graph through the
/// query engine, parse to result. The count is asserted against the
/// reference sum over the edge list, so the number cannot come from a
/// plan that drops or duplicates paths.
fn run_two_hop(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut outdeg = vec![0u64; node_count as usize];
    for &(s, _) in edges {
        outdeg[s as usize] += 1;
    }
    let expected: i64 = edges.iter().map(|&(_, d)| outdeg[d as usize] as i64).sum();
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS paths";
    let runs = 50usize;
    for _ in 0..5 {
        zu::query::run(source, &mut db, &[]).expect("warmup run");
    }
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("two-hop count");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "2-hop count disagrees with the edge list reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 2-hop count: {expected} paths, p50 {p50:.3} ms, p99 {:.3} ms over {runs} runs",
        lat[runs * 99 / 100].as_secs_f64() * 1e3
    );
    p50
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let data = match std::env::var("ZU_DATA") {
        Ok(d)
            if std::path::Path::new(&format!("{d}/ldbc-sf1-person-keys.txt")).exists()
                && std::path::Path::new(&format!("{d}/ldbc-sf1-knows.txt")).exists() =>
        {
            d
        }
        _ => {
            println!(
                "ldbc: SF1 files not found under ZU_DATA, run scripts/prep-ldbc.sh on this host"
            );
            // A gate that silently skips is not a gate.
            std::process::exit(i32::from(gate));
        }
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sf1.zu1");
    let (edges, by_row) = load(&data, &path);
    let node_count = by_row.len() as u64;

    let hop_p50 = run_one_hop(&path, &edges, node_count);
    let key_p50 = run_key_lookups(&path, &by_row);
    let two_hop_p50 = run_two_hop(&path, &edges, node_count);

    let mut failed = false;
    if let Some(ceiling) = budget("ldbc_hop_p50_us")
        && hop_p50 > ceiling
    {
        println!("GATE FAIL B1 1-hop: p50 {hop_p50:.2} us > ceiling {ceiling}");
        failed = true;
    }
    if let Some(ceiling) = budget("ldbc_key_p50_us")
        && key_p50 > ceiling
    {
        println!("GATE FAIL B2 key-lookup: p50 {key_p50:.2} us > ceiling {ceiling}");
        failed = true;
    }
    if let Some(ceiling) = budget("ldbc_two_hop_p50_ms")
        && two_hop_p50 > ceiling
    {
        println!("GATE FAIL B4 2-hop: p50 {two_hop_p50:.3} ms > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
