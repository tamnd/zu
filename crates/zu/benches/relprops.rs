//! Edge property reads (G2, zu#117): what it costs to read a property
//! off an edge a pattern matched, which is the shape every linkbench
//! style workload is made of.
//!
//! A property row is addressed by the edge's ordinal, its place in the
//! load order, and an edge that arrives from anywhere other than a
//! forward walk has to have that ordinal found: locate the source's
//! list in the forward CSR and find the destination in it. This bench
//! is that lookup plus the column read, on the two shapes the workloads
//! ask for. The scan reads a property off every edge in the graph. The
//! point read walks one node's links and reads a property off each,
//! which is the get-links shape, and it runs over a node whose degree
//! is around the average so the number is not a tail.
//!
//! Both go through the query engine rather than the storage API, so the
//! number covers what a query actually pays.
//!
//! rel_prop_over_walk_x holds the scan against the same walk reading a
//! column off the node the edge lands on, which is the same walk and
//! the same number of column reads with the ordinal lookup taken out.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench relprops

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_keyed;
use zu::zu1::props::{PropValues, store_props, store_rel_props};

const NODES: u32 = 200_000;
const DEGREE: u32 = 10;

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

/// The destinations of `src`, strided so a source's list is spread over
/// the whole node range rather than sitting next to it.
fn edges_of(src: u32) -> impl Iterator<Item = (u32, u32)> {
    (0..DEGREE).map(move |k| {
        (
            src,
            (src.wrapping_mul(7919).wrapping_add(k * 104_729)) % NODES,
        )
    })
}

/// The `since` value of the edge at ordinal `i`, which is what the
/// crosscheck sums.
fn since_of(i: usize) -> u64 {
    (i as u64 * 7919) % 100_000
}

/// The `age` of person `i`, the node column the edge column is measured
/// against.
fn age_of(i: u64) -> u64 {
    (i * 104_729) % 100
}

fn build(path: &std::path::Path) -> (usize, u64) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES).flat_map(edges_of).collect();
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", u64::from(NODES), &edges, None).expect("load");
    let since: Vec<u64> = (0..edges.len()).map(since_of).collect();
    let total = since.iter().sum();
    store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).expect("props");
    let age: Vec<u64> = (0..u64::from(NODES)).map(age_of).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).expect("node props");
    (edges.len(), total)
}

/// Median ms of `source`, with the answer checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: Value, runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(warm.rows[0][0], want, "warmup answer for {source}");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(r.rows[0][0], want, "answer changed for {source}");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("relprops.zu1");
    let t = Instant::now();
    let (edges, total) = build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "relprops: {NODES} persons, {edges} edges, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let scan = "MATCH (a:person)-[k:knows]->(b:person) RETURN sum(k.since) AS v";
    let ms = measure(&mut db, scan, Value::Int(total as i64), 5);
    let mreads = edges as f64 / 1e3 / ms;
    // The same walk reading a column off the node at the end of the
    // edge instead of off the edge, which is what the edge property is
    // measured against: same walk, same column read, same number of
    // values, and all that differs is that a node's row is the offset
    // in hand while an edge's has to be found. Both time in one run on
    // one machine, so the ratio is the gate and a shared runner cannot
    // move it.
    let walk = "MATCH (a:person)-[k:knows]->(b:person) RETURN sum(b.age) AS v";
    let walk_want = query::run(walk, &mut db, &[]).expect("walk").rows[0][0].clone();
    let walk_ms = measure(&mut db, walk, walk_want, 5);
    let factor = ms / walk_ms;
    println!(
        "relprops scan: {ms:.1} ms, {mreads:.2} M property reads/s, \
         node column {walk_ms:.1} ms, {factor:.2}x, crosschecked"
    );

    // One node's links, the get-links shape. The source is picked for
    // an ordinary degree, and the answer is summed so a lost edge shows.
    let src = 12_345u32;
    let want: u64 = {
        let mut edges: Vec<(u32, u32)> = (0..NODES).flat_map(edges_of).collect();
        edges.sort_unstable();
        edges.dedup();
        edges
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| *s == src)
            .map(|(i, _)| since_of(i))
            .sum()
    };
    let point =
        format!("MATCH (a:person {{id: {src}}})-[k:knows]->(b:person) RETURN sum(k.since) AS v");
    let point_ms = measure(&mut db, &point, Value::Int(want as i64), 199);
    // The same walk without the property, which is what says how much
    // of the point number is the property and how much is a query over
    // a cold reader: every run opens its own, so the walk pays for the
    // group it decodes and the property read is what is left.
    let bare = format!("MATCH (a:person {{id: {src}}})-[k:knows]->(b:person) RETURN count(b) AS v");
    let degree = query::run(&bare, &mut db, &[]).expect("degree");
    let bare_ms = measure(&mut db, &bare, degree.rows[0][0].clone(), 199);
    println!(
        "relprops one node: {:.1} us for the walk and the properties, {:.1} us for the walk alone",
        point_ms * 1e3,
        bare_ms * 1e3
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(ceiling) = budget("rel_prop_over_walk_x")
    {
        assert!(
            factor <= ceiling,
            "reading a property off every edge costs {factor:.2}x the walk, over the {ceiling}x ceiling"
        );
        println!("gate: edge property ceiling met");
    }
}
