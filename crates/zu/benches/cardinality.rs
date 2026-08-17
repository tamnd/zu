//! P3 cardinality gate (Spec/2064g/perf/12 sections 4 and 6, zu#76):
//! the estimator held against measured row counts on graphs this file
//! builds itself, so it runs anywhere without the LDBC text files.
//!
//! The LDBC bench already reports q-error on SF1, but SF1 is one graph
//! with one degree distribution and it is not in CI. The estimator's
//! failure mode is skew it has never seen, so this bench varies the
//! data instead of the queries: a uniform graph where every node looks
//! like the mean, a hub graph where a handful of nodes hold most of
//! the edges, and a many-to-one graph where the two directions of the
//! same table could not look less alike. Every shape runs the same
//! corpus.
//!
//! Two things come out of each run. The q-errors are pooled per shape
//! and reported as percentiles, which is telemetry and moves with the
//! estimator. The ceilings are held against the measured rows, and
//! that is not telemetry: a ceiling is a promise the DP orders joins
//! on, so a single violation fails the run whether ZU_GATE is set or
//! not. There is no budget line for it because no number of wrong
//! ceilings is worth writing down as acceptable.
//!
//! card_gen_qerror_p90 and card_gen_qerror_max are ceilings on the
//! pooled q-errors across all three shapes.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench cardinality

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

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

const NODES: u64 = 20_000;
const BUCKETS: u64 = 50;

/// A cheap deterministic spread. Nothing here needs a real generator,
/// only edges that do not arrive in a helpful order.
fn mix(i: u64) -> u64 {
    let mut h = i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^ (h >> 32)
}

/// Every node walks out to the same number of neighbors, so both degree
/// sequences are flat and the mean describes every row. This is the
/// case an estimator with no statistics at all still gets right, and it
/// is here to catch a ceiling that is wrong even without skew.
fn uniform() -> Vec<(u32, u32)> {
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|s| (0..8u64).map(move |k| (s as u32, (mix(s * 8 + k) % NODES) as u32)))
        .collect();
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// Sixteen hubs hold roughly half the edges and everyone else holds a
/// few, so the mean out-degree describes nobody. A row that already
/// walked an edge is far likelier to be sitting on a hub than a node
/// picked at random, which is exactly what the degree norms are for.
fn hub() -> Vec<(u32, u32)> {
    let hubs = 16u64;
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for h in 0..hubs {
        for k in 0..(NODES / 8) {
            edges.push((h as u32, (mix(h * NODES + k) % NODES) as u32));
        }
    }
    for s in hubs..NODES {
        for k in 0..2u64 {
            edges.push((s as u32, (mix(s * 3 + k) % NODES) as u32));
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// Every node points at one of two hundred targets. Walking forward
/// multiplies by one and walking backward multiplies by a hundred, so
/// an estimator that reads one degree sequence for both directions is
/// off by that factor in one of them.
fn funnel() -> Vec<(u32, u32)> {
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .map(|s| (s as u32, (mix(s) % 200) as u32))
        .collect();
    edges.sort_unstable();
    edges.dedup();
    edges
}

fn build(path: &std::path::Path, edges: &[(u32, u32)]) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, edges).expect("load");
    let bucket: Vec<u64> = (0..NODES).map(|i| mix(i) % BUCKETS).collect();
    let name: Vec<String> = (0..NODES).map(|i| format!("n{}", mix(i) % 4000)).collect();
    let name: Vec<&[u8]> = name.iter().map(|n| n.as_bytes()).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("bucket", PropValues::Int(&bucket)),
            ("name", PropValues::Str(&name)),
        ],
    )
    .expect("props");
    zu::zu1::colors::analyze(&mut db).expect("analyze");
}

/// One entry of the corpus: a name for the printout, the query text,
/// and the parameters it wants bound.
type CardQuery = (&'static str, &'static str, Vec<(&'static str, Value)>);

/// One graph shape: a name and the edge list it builds.
type Shape = (&'static str, fn() -> Vec<(u32, u32)>);

/// The corpus, in the shapes perf/12 section 4 names: plain scans, the
/// hop chain that the degree norms drive, a cycle close, and the
/// property predicates that the column statistics drive. Every query
/// counts rather than returning them, so nothing here is bounded by a
/// LIMIT and every operator reports the cardinality it really produced.
fn corpus() -> Vec<CardQuery> {
    vec![
        ("scan", "MATCH (p:person) RETURN count(p) AS n", vec![]),
        (
            "hop",
            "MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
            vec![],
        ),
        (
            "hop-back",
            "MATCH (a:person)<-[:knows]-(b) RETURN count(b) AS n",
            vec![],
        ),
        (
            "hop-undirected",
            "MATCH (a:person)-[:knows]-(b) RETURN count(b) AS n",
            vec![],
        ),
        (
            "two-hop",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS n",
            vec![],
        ),
        (
            "two-hop-in",
            "MATCH (a:person)<-[:knows]-(b)<-[:knows]-(c) RETURN count(c) AS n",
            vec![],
        ),
        (
            "triangle",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
             RETURN count(*) AS n",
            vec![],
        ),
        (
            "seeded-hop",
            "MATCH (p:person {id: $id})-[:knows]->(f) RETURN count(f) AS n",
            vec![("id", Value::Int(3))],
        ),
        (
            "eq-bucket",
            "MATCH (p:person) WHERE p.bucket = $v RETURN count(p) AS n",
            vec![("v", Value::Int(7))],
        ),
        (
            "eq-name",
            "MATCH (p:person) WHERE p.name = $v RETURN count(p) AS n",
            vec![("v", Value::Str("n17".into()))],
        ),
        (
            "eq-then-hop",
            "MATCH (p:person)-[:knows]->(f) WHERE p.bucket = $v RETURN count(f) AS n",
            vec![("v", Value::Int(7))],
        ),
    ]
}

/// Runs the corpus over one shape and returns its q-errors plus one
/// line per violated ceiling. A violation names the operator, what it
/// was promised, and what it actually produced, because the useful
/// question afterwards is always which bound was wrong.
fn measure(path: &std::path::Path, shape: &str) -> (Vec<f64>, Vec<String>) {
    let mut db = Zu1File::open(path).expect("open");
    let mut qerrors = Vec::new();
    let mut violations = Vec::new();
    for (name, source, params) in corpus() {
        let borrowed: Vec<(&str, Value)> = params.iter().map(|(k, v)| (*k, v.clone())).collect();
        let profile = query::profile(source, &mut db, &borrowed).expect("profile");
        let mut worst: Option<(f64, String, f64, u64)> = None;
        for stage in &profile.stages {
            for op in &stage.ops {
                if op.bound_violation() {
                    violations.push(format!(
                        "{shape} {name}: {} produced {} rows past a ceiling of {:.0}",
                        op.name(),
                        op.flat,
                        op.bnd.unwrap_or_default()
                    ));
                }
                let (Some(q), Some(est)) = (op.qerror(), op.est) else {
                    continue;
                };
                qerrors.push(q);
                if worst.as_ref().is_none_or(|(w, ..)| q > *w) {
                    worst = Some((q, op.name(), est, op.flat));
                }
            }
        }
        let (q, op, est, act) = worst.expect("every query has at least a scan");
        println!("{shape} {name}: worst q {q:.1} at {op}, est {est:.0} vs {act} actual");
    }
    (qerrors, violations)
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shapes: [Shape; 3] = [("uniform", uniform), ("hub", hub), ("funnel", funnel)];

    let mut pooled = Vec::new();
    let mut violations = Vec::new();
    for (shape, edges_of) in shapes {
        let edges = edges_of();
        let path = dir.path().join(format!("{shape}.zu1"));
        let t = Instant::now();
        build(&path, &edges);
        println!(
            "{shape}: {NODES} persons, {} knows edges, built in {:.2}s",
            edges.len(),
            t.elapsed().as_secs_f64()
        );
        let (qerrors, bad) = measure(&path, shape);
        let mut sorted = qerrors.clone();
        sorted.sort_by(f64::total_cmp);
        let pick = |pct: usize| sorted[(sorted.len() - 1) * pct / 100];
        println!(
            "{shape}: {} operators, q-error p50 {:.2}, p90 {:.2}, max {:.2}, {} bound violations",
            sorted.len(),
            pick(50),
            pick(90),
            sorted[sorted.len() - 1],
            bad.len()
        );
        pooled.extend(qerrors);
        violations.extend(bad);
    }

    pooled.sort_by(f64::total_cmp);
    let pick = |pct: usize| pooled[(pooled.len() - 1) * pct / 100];
    let (p50, p90, max) = (pick(50), pick(90), pooled[pooled.len() - 1]);
    println!(
        "cardinality: {} operators over {} queries and 3 shapes, \
         q-error p50 {p50:.2}, p90 {p90:.2}, max {max:.2}, {} bound violations",
        pooled.len(),
        corpus().len() * 3,
        violations.len()
    );

    // A violated ceiling is a bug and not a measurement, so it fails
    // the run with or without ZU_GATE. The percentiles are the other
    // way around: they move with the estimator and only a gated run
    // holds them to the budget.
    for line in &violations {
        println!("BOUND VIOLATION {line}");
    }
    assert!(
        violations.is_empty(),
        "{} ceilings did not hold",
        violations.len()
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        for (name, got, key) in [
            ("p90", p90, "card_gen_qerror_p90"),
            ("max", max, "card_gen_qerror_max"),
        ] {
            if let Some(ceiling) = budget(key) {
                assert!(
                    got <= ceiling,
                    "q-error {name} {got:.2} over the {ceiling} ceiling"
                );
            }
        }
        println!("gate: all ceilings met");
    }
}
