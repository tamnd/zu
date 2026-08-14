//! P3 tail latency gate (Spec/2064g/perf/05, zu#76): a served read's
//! p99 against its own p50, on data with no skew and on data with a
//! lot of it.
//!
//! Throughput benches answer how much work the engine gets through.
//! This one answers something a service cares about more: whether the
//! slow end of a request stream is close to the middle of it. A p50 of
//! half a millisecond with a p99 of fifty is a system nobody can put a
//! timeout on, and no average anywhere would show it.
//!
//! Two graphs, the same node count and the same edge count, so the only
//! thing that differs is how the edges are spread. The uniform one
//! gives every person the same number of friends. The power law one
//! deals degrees out by rank, one over rank to the four fifths, which
//! puts a few hundred thousand friends on the celebrities at the top
//! and one or two on most of the tail. Real social graphs look like the
//! second one, which is why it gets the looser bound rather than no
//! bound at all.
//!
//! The workload is point seeded reads through a session, so the plan is
//! compiled once and every timed run is the lookup, the walk and the
//! answer, which is what a served request is. Seeds are drawn evenly
//! over people, not over edges, because that is how request streams
//! arrive: nobody knows which id in their hand is a celebrity.
//!
//! Two shapes. The friend list is one hop with the rows coming back, so
//! its work is the seed's own degree and the spread in it is the spread
//! in the data. The friend count is the same hop with only the total
//! asked for, which the engine answers off the offsets without reading
//! a neighbor, so its work is the same for every seed and what is left
//! in its tail is the engine alone.
//!
//! Every run is crosschecked against the edge list the graph was built
//! from, so a fast number that answered the wrong thing fails here.
//!
//! tail_p99_p50_uniform_x and tail_p99_p50_power_x are the ceilings, on
//! the friend list shape on each graph.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench tail

use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
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

const NODES: u64 = 500_000;
/// Friends per person on the uniform graph, and the mean the power law
/// graph is scaled to hit, so the two hold the same number of edges to
/// within the rounding of one degree per node.
const DEGREE: u64 = 16;
/// Requests per shape per graph. Enough that the ninety ninth
/// percentile is the twentieth slowest and not the slowest one.
const REQUESTS: usize = 2_000;

fn score_of(i: u64) -> i64 {
    ((i * 7919) % 100_000) as i64
}

/// Where a person's friends are: far apart in row order, so a walk
/// reads a different page each time rather than one warm neighborhood.
fn friend(src: u64, k: u64) -> u32 {
    ((src * 7919 + k * 104_729 + 13) % NODES) as u32
}

/// Every person knows the same number of people.
fn uniform_edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity((NODES * DEGREE) as usize);
    for i in 0..NODES {
        for k in 0..DEGREE {
            out.push((i as u32, friend(i, k)));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Degrees by rank, one over rank to the four fifths, scaled so the
/// total lands on the same edge count the uniform graph has. Rank is
/// the row itself, so person 0 is the biggest celebrity and the ids
/// stay easy to reason about from the outside.
fn power_law_degrees() -> Vec<u64> {
    let raw: Vec<f64> = (0..NODES)
        .map(|r| 1.0 / ((r + 1) as f64).powf(0.8))
        .collect();
    let scale = (NODES * DEGREE) as f64 / raw.iter().sum::<f64>();
    raw.iter()
        .map(|w| ((w * scale).round() as u64).max(1))
        .collect()
}

fn power_law_edges(degrees: &[u64]) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity((NODES * DEGREE) as usize);
    for (i, &deg) in degrees.iter().enumerate() {
        for k in 0..deg {
            out.push((i as u32, friend(i as u64, k)));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn build(path: &std::path::Path, edges: &[(u32, u32)]) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, edges).expect("load");
    let score: Vec<u64> = (0..NODES).map(|i| score_of(i) as u64).collect();
    store_props(&mut db, "person", &[("score", PropValues::Int(&score))]).expect("props");
}

/// The seeds the request stream asks for, spread evenly over people and
/// the same on every host and every run.
fn seeds() -> Vec<i64> {
    (0..REQUESTS)
        .map(|i| ((i as u64 * 2_654_435_761) % NODES) as i64)
        .collect()
}

/// Out degree per person, read off the edge list, which is what the
/// crosscheck compares against and what the report describes the skew
/// with.
fn degrees(edges: &[(u32, u32)]) -> Vec<u32> {
    let mut out = vec![0u32; NODES as usize];
    for &(src, _) in edges {
        out[src as usize] += 1;
    }
    out
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    let last = sorted.len() - 1;
    sorted[((sorted.len() as f64 * q).ceil() as usize).min(last).max(1) - 1]
}

/// What one shape did over the whole request stream, in microseconds.
struct Tail {
    p50: f64,
    p99: f64,
    max: f64,
}

impl Tail {
    fn ratio(&self) -> f64 {
        self.p99 / self.p50
    }
}

/// Runs the stream once and reports its distribution. `want` is the row
/// count the seed's own degree says has to come back, so a shape that
/// walked the wrong list fails before it is timed.
fn stream(
    session: &mut Session,
    source: &str,
    seeds: &[i64],
    want: impl Fn(i64) -> u64,
    rows_of: impl Fn(&zu::query::QueryResult) -> u64,
) -> Tail {
    // A warm pass over the same seeds: the plan compiles once, the
    // catalog and the readers land, and what is timed below is the
    // request rather than the first touch of a page.
    for &s in seeds {
        let r = session
            .run(source, &[("x", Value::Int(s))])
            .expect("warm run");
        assert_eq!(rows_of(&r), want(s), "warm answer for seed {s}");
    }
    let mut us: Vec<f64> = seeds
        .iter()
        .map(|&s| {
            let t = Instant::now();
            let r = session
                .run(source, &[("x", Value::Int(s))])
                .expect("timed run");
            let us = t.elapsed().as_secs_f64() * 1e6;
            assert_eq!(rows_of(&r), want(s), "answer for seed {s}");
            us
        })
        .collect();
    us.sort_by(f64::total_cmp);
    Tail {
        p50: quantile(&us, 0.50),
        p99: quantile(&us, 0.99),
        max: us[us.len() - 1],
    }
}

fn count_rows(r: &zu::query::QueryResult) -> u64 {
    match r.rows.first().map(|row| &row[0]) {
        Some(&Value::Int(n)) => n as u64,
        other => panic!("expected a count, got {other:?}"),
    }
}

fn returned_rows(r: &zu::query::QueryResult) -> u64 {
    r.rows.len() as u64
}

/// Both shapes over one graph, printed, with the friend list's ratio
/// handed back because that is the one the ceiling sits on.
fn measure(what: &str, path: &std::path::Path, deg: &[u32], seeds: &[i64]) -> f64 {
    let mut session = Session::open(path).expect("session");
    let list = stream(
        &mut session,
        "MATCH (p:person {id: $x})-[:knows]->(f) RETURN f.id AS f",
        seeds,
        |s| u64::from(deg[s as usize]),
        returned_rows,
    );
    let count = stream(
        &mut session,
        "MATCH (p:person {id: $x})-[:knows]->(f) RETURN count(f) AS n",
        seeds,
        |s| u64::from(deg[s as usize]),
        count_rows,
    );
    let mut asked: Vec<u32> = seeds.iter().map(|&s| deg[s as usize]).collect();
    asked.sort_unstable();
    println!(
        "tail {what} friend list: p50 {:.1} us, p99 {:.1} us, {:.2}x, max {:.1} us over {REQUESTS} requests, \
         seed degree p50 {} p99 {} max {}, crosschecked",
        list.p50,
        list.p99,
        list.ratio(),
        list.max,
        asked[asked.len() / 2],
        asked[asked.len() * 99 / 100],
        asked[asked.len() - 1],
    );
    println!(
        "tail {what} friend count: p50 {:.1} us, p99 {:.1} us, {:.2}x, max {:.1} us over {REQUESTS} requests, crosschecked",
        count.p50,
        count.p99,
        count.ratio(),
        count.max,
    );
    list.ratio()
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let t = Instant::now();
    let uniform = uniform_edges();
    let power = power_law_edges(&power_law_degrees());
    let uniform_path = dir.path().join("tail-uniform.zu1");
    let power_path = dir.path().join("tail-power.zu1");
    build(&uniform_path, &uniform);
    build(&power_path, &power);
    let uniform_deg = degrees(&uniform);
    let power_deg = degrees(&power);
    let mut sorted = power_deg.clone();
    sorted.sort_unstable();
    println!(
        "tail: {NODES} persons, uniform {} edges, power law {} edges with degree p50 {} p99 {} max {}, load {:.1}s",
        uniform.len(),
        power.len(),
        sorted[sorted.len() / 2],
        sorted[sorted.len() * 99 / 100],
        sorted[sorted.len() - 1],
        t.elapsed().as_secs_f64()
    );

    let seeds = seeds();
    let uniform_x = measure("uniform", &uniform_path, &uniform_deg, &seeds);
    let power_x = measure("power law", &power_path, &power_deg, &seeds);

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        for (what, got, key) in [
            ("uniform", uniform_x, "tail_p99_p50_uniform_x"),
            ("power law", power_x, "tail_p99_p50_power_x"),
        ] {
            let Some(ceiling) = budget(key) else {
                continue;
            };
            assert!(
                got <= ceiling,
                "{what} p99 is {got:.2}x its p50, over the {ceiling}x ceiling"
            );
        }
        println!("gate: tail ceilings met");
    }
}
