//! What counting shortest paths costs against building them (perf/06,
//! zu#77).
//!
//! A pattern under `ALL SHORTEST` matches every path of the least
//! length between its endpoints, and there can be a great many of
//! those: on any graph where several equally short routes run side by
//! side, the count multiplies out along the way. The layered graph
//! below is that shape written plainly, `WIDTH` nodes per layer and
//! every node of a layer joined to every node of the next, so a node in
//! layer k is reached by WIDTH^(k-1) paths of exactly k hops and the
//! whole answer is a sum this bench can check by hand.
//!
//! Two numbers over the same graph and the same pattern. The first
//! counts the paths, which the engine answers off the levelled graph
//! the breadth-first prepass builds: a node's paths are its
//! predecessors' paths added up, so the whole count is one pass. The
//! second asks for the paths themselves, which is one walk per path and
//! is the work the first one is not doing.
//!
//! paths_count_us gates the count, and paths_count_over_enumerate_x
//! gates it against the enumeration on the same run, which is the
//! number that says the count is not quietly walking the paths: a
//! regression that put the enumeration back would show up there whatever
//! the host's speed.
//!
//! Both are crosschecked against the closed form, so a run that got
//! fast by answering the wrong number fails instead of scoring.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench paths

use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

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

/// Nodes per layer, and layers after the single source. Seven layers of
/// six is a third of a million shortest paths over forty three nodes,
/// which is enough that enumerating is clearly the slow way round and
/// small enough that enumerating still finishes.
const WIDTH: u64 = 6;
const LAYERS: u64 = 7;
/// Timed runs per shape; the least is reported, since a bench sharing a
/// machine measures interference and not the engine.
const RUNS: usize = 5;

fn nodes() -> u64 {
    1 + WIDTH * LAYERS
}

/// Node 0, then the layers in row order: 0 joins all of layer 1, and
/// every node of layer i joins every node of layer i + 1.
fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for j in 0..WIDTH {
        out.push((0u32, (1 + j) as u32));
    }
    for layer in 0..LAYERS - 1 {
        for i in 0..WIDTH {
            for j in 0..WIDTH {
                let from = 1 + layer * WIDTH + i;
                let to = 1 + (layer + 1) * WIDTH + j;
                out.push((from as u32, to as u32));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The answer by hand: WIDTH^k paths of k hops, one hop per layer.
fn want() -> i64 {
    (1..=LAYERS).map(|k| WIDTH.pow(k as u32) as i64).sum()
}

/// The least of `RUNS` timings, in microseconds, with the row count the
/// shape has to come back with checked on every one of them.
fn least(session: &mut Session, source: &str, rows: usize) -> f64 {
    session.run(source, &[]).expect("warm run");
    let mut best = f64::MAX;
    for _ in 0..RUNS {
        let t = Instant::now();
        let r = session.run(source, &[]).expect("timed run");
        let us = t.elapsed().as_secs_f64() * 1e6;
        assert_eq!(r.rows.len(), rows, "row count for {source}");
        best = best.min(us);
    }
    best
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("paths.zu1");
    let edges = edges();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", nodes(), &edges).expect("load");
    drop(db);
    let mut session = Session::open(&path).expect("session");

    let total = want();
    let counted = "MATCH ALL SHORTEST (a:person {id: 0})-[r:knows*]->(b) RETURN count(*) AS n";
    let r = session.run(counted, &[]).expect("count");
    assert_eq!(
        r.rows.first().and_then(|row| row.first()),
        Some(&Value::Int(total)),
        "the count is the closed form"
    );
    let count_us = least(&mut session, counted, 1);
    let listed = "MATCH ALL SHORTEST (a:person {id: 0})-[r:knows*]->(b) RETURN size(r) AS hops";
    let enumerate_us = least(&mut session, listed, total as usize);
    let ratio = count_us / enumerate_us.max(0.001);

    println!(
        "paths: {} nodes, {} edges, {total} shortest paths from the source",
        nodes(),
        edges.len(),
    );
    println!("paths count: {count_us:.1} us, crosschecked");
    println!("paths enumerate: {enumerate_us:.1} us over {total} paths, crosschecked");
    println!("paths count over enumerate: {ratio:.5}x");

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        if let Some(ceiling) = budget("paths_count_us") {
            assert!(
                count_us <= ceiling,
                "counting took {count_us:.1} us, over the {ceiling} us ceiling"
            );
        }
        if let Some(ceiling) = budget("paths_count_over_enumerate_x") {
            assert!(
                ratio <= ceiling,
                "counting cost {ratio:.5}x the enumeration, over the {ceiling}x ceiling"
            );
        }
        println!("gate: path count ceilings met");
    }
}
