//! LDBC Graphalytics reference validation on kgs (docs/12 M4).
//!
//! The ldbc bench proves the kernels against references this repo
//! computes for itself. This bench removes that self-dependency: it
//! runs pagerank, wcc, and sssp over the real KGS go graph, 832 K
//! vertices and 17.9 M undirected edges, and checks every output row
//! against the reference vectors LDBC publishes with the dataset.
//! The vertex ids are sparse, so the load goes through densify_keyed
//! the way any keyed COPY does, and the algorithm parameters come from
//! the dataset's .properties file, not from constants in this file:
//! pagerank runs the published iteration count and sssp starts from
//! the published source vertex. Graphalytics BFS on an undirected
//! graph is unit-weight sssp, so the sssp kernel is checked against
//! the BFS reference, exact per row with the i64::MAX unreachable
//! marker mapped from the kernel's u64::MAX. The wcc reference labels
//! every component by its smallest vertex id, which is exactly the
//! union-by-minimum contract, so labels compare exactly after mapping
//! rows back through the sorted key list. Louvain has no Graphalytics
//! reference (CDLP is a different algorithm), so it is timed and
//! checked for determinism across two runs, the same pin the kernel
//! tests hold. One CALL then runs through zuQL against the loaded
//! file, so the query surface is exercised on a graph three orders
//! larger than the exec tests.
//!
//! With ZU_B5=1 the bench also runs B5 (docs/11): the worst-case
//! optimal triangle count over Graph500 scale 22, oriented by degree
//! and gated against a two-pointer reference computed here.
//!
//! Get the data: curl -sO https://datasets.ldbcouncil.org/graphalytics/kgs.tar.zst
//! && tar --zstd -xf kgs.tar.zst under ZU_DATA; same for
//! graph500-22.tar.zst when running B5.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu --bench graphalytics

use std::collections::HashMap;
use std::time::Instant;

use zu::zu1::algo;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::{
    GraphReader, bulk_load_keyed, densify_keyed, read_key_edge_list, read_key_list,
};
use zu_query::exec::Value;

/// The marker Graphalytics reference files use for an unreachable
/// vertex in BFS output.
const UNREACHABLE: u64 = i64::MAX as u64;

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

/// Reads one algorithm parameter from the dataset's .properties file,
/// e.g. suffix ".pr.num-iterations". The parameters travel with the
/// dataset because the reference outputs were computed with them.
fn property(text: &str, suffix: &str) -> u64 {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=')
            && k.trim().ends_with(suffix)
        {
            return v.trim().parse().unwrap_or_else(|_| panic!("bad {suffix}"));
        }
    }
    panic!("no {suffix} in the properties file");
}

/// Reads a `vertex value` reference file into a map keyed by the
/// original vertex id, values parsed by the caller's closure.
fn read_reference<T>(path: &str, parse: impl Fn(&str) -> T) -> HashMap<u64, T> {
    let text = std::fs::read_to_string(path).expect("reference file");
    let mut map = HashMap::new();
    for line in text.lines() {
        let (v, val) = line
            .split_once(char::is_whitespace)
            .expect("vertex and value");
        map.insert(v.parse::<u64>().expect("vertex id"), parse(val.trim()));
    }
    map
}

/// B5 (docs/11): the unseeded triangle count on Graph500-22 through
/// the query engine's WCOJ close, the gate the Umbra paper's numbers
/// class-bound. The bench stores the degree-oriented DAG, each
/// undirected edge pointing from its lower (degree, id) endpoint to
/// the higher, the standard worst-case optimal formulation where the
/// variable order supplies the orientation, so every triangle appears
/// exactly once and the intersection work is bounded by the oriented
/// degrees, max 1035 on this graph against a raw max degree of
/// 162768. The count asserts against a two-pointer intersection
/// reference computed here from the oriented adjacency, an
/// implementation that shares nothing with the executor's galloping
/// leapfrog. Runs when ZU_B5=1 because the 64 M edge parse and load
/// take minutes; a missing dataset under ZU_B5=1 fails the gate
/// instead of skipping it.
fn run_b5(data: &str, gate: bool) -> Option<(f64, i64)> {
    if !std::env::var("ZU_B5").is_ok_and(|v| v == "1") {
        return None;
    }
    if !std::path::Path::new(&format!("{data}/graph500-22.v")).exists() {
        println!("graph500-22: files not found under ZU_DATA with ZU_B5=1, see the header comment");
        std::process::exit(i32::from(gate));
    }
    if std::env::var_os("ZU_CACHE_MB").is_none() {
        // The intersection's seed side visits groups in effectively
        // random order, so the decoded forward CSR, about 530 MB on
        // this graph, must stay resident per reader or every probe
        // pays a full group decode. Readers pick the budget up at
        // construction, which has not happened yet.
        // SAFETY: the bench is single threaded at this point.
        unsafe { std::env::set_var("ZU_CACHE_MB", "768") };
    }
    let started = Instant::now();
    let keys =
        read_key_list(std::path::Path::new(&format!("{data}/graph500-22.v"))).expect("vertices");
    let edges =
        read_key_edge_list(std::path::Path::new(&format!("{data}/graph500-22.e"))).expect("edges");
    let (dense, by_row) = densify_keyed(&keys, &edges).expect("densify");
    let n = by_row.len();
    let mut deg = vec![0u32; n];
    for &(s, d) in &dense {
        deg[s as usize] += 1;
        deg[d as usize] += 1;
    }
    let mut oriented: Vec<(u32, u32)> = dense
        .iter()
        .map(|&(s, d)| {
            if (deg[s as usize], s) < (deg[d as usize], d) {
                (s, d)
            } else {
                (d, s)
            }
        })
        .collect();
    oriented.sort_unstable();
    oriented.dedup();
    let parsed = started.elapsed();
    let load_started = Instant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph500-22.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_keyed(&mut db, "v", "e", n as u64, &oriented, Some(&by_row)).expect("bulk load");
    // Stats feed the optimizer the same way any COPY-then-query
    // session would; the plan must earn its AspJoin, not be handed it.
    zu::zu1::colors::analyze(&mut db).expect("analyze");
    println!(
        "graph500-22: {} vertices, {} oriented edges, parse {:.2}s, load {:.2}s",
        n,
        oriented.len(),
        parsed.as_secs_f64(),
        load_started.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    let mut adj = vec![Vec::new(); n];
    for &(s, d) in &oriented {
        adj[s as usize].push(d);
    }
    let mut expected = 0i64;
    for &(a, b) in &oriented {
        let (mut x, mut y) = (adj[a as usize].as_slice(), adj[b as usize].as_slice());
        while let (Some(&u), Some(&v)) = (x.first(), y.first()) {
            match u.cmp(&v) {
                std::cmp::Ordering::Less => x = &x[1..],
                std::cmp::Ordering::Greater => y = &y[1..],
                std::cmp::Ordering::Equal => {
                    expected += 1;
                    x = &x[1..];
                    y = &y[1..];
                }
            }
        }
    }
    println!(
        "graph500-22 reference: {expected} triangles in {:.2}s two-pointer",
        t.elapsed().as_secs_f64()
    );

    let source = "MATCH (a:v)-[:e]->(b)-[:e]->(c), (a)-[:e]->(c) RETURN count(*) AS triangles";
    let runs = 3usize;
    zu::query::run(source, &mut db, &[]).expect("warmup run");
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("triangle count");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "triangle count disagrees with the two-pointer reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64();
    println!(
        "graph500-22 triangle count: {expected} triangles, p50 {p50:.3} s, max {:.3} s over {runs} runs",
        lat[runs - 1].as_secs_f64()
    );
    Some((p50, expected))
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let data = match std::env::var("ZU_DATA") {
        Ok(d) if std::path::Path::new(&format!("{d}/kgs.v")).exists() => d,
        _ => {
            println!("graphalytics: kgs files not found under ZU_DATA, see the header comment");
            // A gate that silently skips is not a gate.
            std::process::exit(i32::from(gate));
        }
    };
    let props = std::fs::read_to_string(format!("{data}/kgs.properties")).expect("properties");
    let pr_iterations = property(&props, ".pr.num-iterations") as usize;
    let bfs_source = property(&props, ".bfs.source-vertex");

    let started = Instant::now();
    let keys = read_key_list(std::path::Path::new(&format!("{data}/kgs.v"))).expect("vertices");
    let edges = read_key_edge_list(std::path::Path::new(&format!("{data}/kgs.e"))).expect("edges");
    let (dense, by_row) = densify_keyed(&keys, &edges).expect("densify");
    // Graphalytics lists each undirected edge once; the CSR stores
    // both directions so degree, rank shares, and expansion all see
    // the undirected graph.
    let mut mirrored = Vec::with_capacity(dense.len() * 2);
    for &(s, d) in &dense {
        mirrored.push((s, d));
        mirrored.push((d, s));
    }
    mirrored.sort_unstable();
    mirrored.dedup();
    let parsed = started.elapsed();
    let n = by_row.len();
    let load_started = Instant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("kgs.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_keyed(&mut db, "v", "e", n as u64, &mirrored, Some(&by_row)).expect("bulk load");
    println!(
        "kgs: {} vertices, {} undirected edges, parse {:.2}s, load {:.2}s",
        n,
        dense.len(),
        parsed.as_secs_f64(),
        load_started.elapsed().as_secs_f64()
    );
    let mut reader = GraphReader::load_table(&mut db, "e").expect("load e");

    let t = Instant::now();
    let ranks = algo::pagerank(&mut db, &mut reader, pr_iterations).expect("pagerank");
    let pagerank_s = t.elapsed().as_secs_f64();
    let reference = read_reference(&format!("{data}/kgs-PR"), |v| {
        v.parse::<f64>().expect("rank")
    });
    assert_eq!(reference.len(), n, "PR reference covers every vertex");
    let mut drift = 0.0f64;
    for (row, &rank) in ranks.iter().enumerate() {
        let expected = reference[&by_row[row]];
        drift = drift.max((rank - expected).abs() / expected.abs().max(f64::MIN_POSITIVE));
    }
    // Measured drift is 2.5e-14, pure summation-order noise; the
    // tolerance leaves five orders for platform FP variance while
    // still catching any real formula change.
    assert!(
        drift < 1e-9,
        "pagerank drifts {drift:.2e} from the published reference"
    );
    println!(
        "kgs pagerank: {pr_iterations} iterations in {pagerank_s:.3} s, max relative drift {drift:.2e} against the published reference"
    );

    let t = Instant::now();
    let labels = algo::wcc(&mut db, &mut reader).expect("wcc");
    let wcc_s = t.elapsed().as_secs_f64();
    let reference = read_reference(&format!("{data}/kgs-WCC"), |v| {
        v.parse::<u64>().expect("label")
    });
    assert_eq!(reference.len(), n, "WCC reference covers every vertex");
    for (row, &label) in labels.iter().enumerate() {
        // Rows rank keys in sorted order, so the smallest row of a
        // component holds its smallest vertex id, the exact label the
        // reference uses.
        assert_eq!(
            by_row[label as usize], reference[&by_row[row]],
            "wcc label disagrees at vertex {}",
            by_row[row]
        );
    }
    let components: std::collections::HashSet<u64> = reference.values().copied().collect();
    println!(
        "kgs wcc: {} components in {wcc_s:.3} s, all {n} labels match the published reference",
        components.len()
    );

    let source = by_row.binary_search(&bfs_source).expect("source vertex") as u64;
    let t = Instant::now();
    let dist = algo::sssp(&mut db, &mut reader, source).expect("sssp");
    let bfs_s = t.elapsed().as_secs_f64();
    let reference = read_reference(&format!("{data}/kgs-BFS"), |v| {
        v.parse::<u64>().expect("depth")
    });
    assert_eq!(reference.len(), n, "BFS reference covers every vertex");
    let mut unreachable = 0usize;
    for (row, &d) in dist.iter().enumerate() {
        let ours = if d == u64::MAX { UNREACHABLE } else { d };
        assert_eq!(
            ours, reference[&by_row[row]],
            "bfs depth disagrees at vertex {}",
            by_row[row]
        );
        unreachable += usize::from(ours == UNREACHABLE);
    }
    println!(
        "kgs bfs: source {bfs_source} in {bfs_s:.3} s, all {n} depths match the published reference, {unreachable} unreachable"
    );

    let t = Instant::now();
    let communities = algo::louvain(&mut db, &mut reader).expect("louvain");
    let louvain_s = t.elapsed().as_secs_f64();
    let again = algo::louvain(&mut db, &mut reader).expect("louvain again");
    assert_eq!(communities, again, "louvain is not deterministic");
    let count = communities
        .iter()
        .enumerate()
        .filter(|(v, c)| **c == *v as u64)
        .count();
    println!("kgs louvain: {count} communities in {louvain_s:.3} s, deterministic across two runs");

    let t = Instant::now();
    let r = zu::query::run(
        "CALL wcc('e') YIELD node, component RETURN count(DISTINCT component) AS c",
        &mut db,
        &[],
    )
    .expect("CALL wcc");
    assert_eq!(
        r.rows[0][0],
        Value::Int(components.len() as i64),
        "CALL wcc component count disagrees with the reference"
    );
    println!(
        "kgs CALL wcc through zuQL: {} components over {n} rows in {:.3} s",
        components.len(),
        t.elapsed().as_secs_f64()
    );

    drop(reader);
    drop(db);
    let b5 = run_b5(&data, gate);

    let mut failed = false;
    for (name, secs, key) in [
        ("pagerank", pagerank_s, "graphalytics_pagerank_s"),
        ("wcc", wcc_s, "graphalytics_wcc_s"),
        ("bfs", bfs_s, "graphalytics_bfs_s"),
        ("louvain", louvain_s, "graphalytics_louvain_s"),
    ] {
        if let Some(ceiling) = budget(key)
            && secs > ceiling
        {
            println!("GATE FAIL {name}: {secs:.3} s > ceiling {ceiling}");
            failed = true;
        }
    }
    if let Some((p50, _)) = b5
        && let Some(ceiling) = budget("graph500_triangle_s")
        && p50 > ceiling
    {
        println!("GATE FAIL B5 triangle: p50 {p50:.3} s > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    println!("gate: all ceilings met");
}
