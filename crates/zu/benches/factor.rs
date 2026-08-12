//! Factorized aggregation over a hop (perf/13 section 1, zu#76): an
//! aggregate that reads nothing off the expanded level does not need
//! the expanded rows, only how many of them there were.
//!
//! What the walk would have built is one row per neighbor carrying the
//! same group key and the same argument as the source row it came off.
//! So the hop becomes a weight: a degree read, offsets alone, with the
//! neighbor array never touched and one group probe per source row
//! instead of one per neighbor. A source row with no edge weighs
//! nothing and drops out, which is what keeps a key whose rows have no
//! edge at all from opening a group.
//!
//! The graph is a million people over a thousand cities with out
//! degrees from zero to seven, so an eighth of them have no edge and
//! have to stay out of the answer, and the group keys cut across the
//! degrees rather than following them.
//!
//! Four shapes. The counted key is the one that goes through the
//! counting slots, where the whole per row path is a hash and an
//! increment. The summed key goes through the general probe, where the
//! weight scales the sum and the argument is read off the source level.
//! The bare sum has no group at all. The two hop keeps its first expand
//! and weighs the second, so the key is read off a pinned level two
//! below the weights.
//!
//! Every case is crosschecked against the edge list, per group and in
//! total, so a weight that lands on the wrong key fails here.
//!
//! Everything runs at one worker, so the rate is per core.
//!
//! exec_factor_mrows_s_core floors the counted key in millions of hop
//! rows a second, the rows the walk would have produced.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench factor

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

const NODES: u64 = 1_000_000;
/// Cities, the group keys. They run across the degrees rather than with
/// them, so no group is all zero degree rows.
const CITIES: u64 = 1_000;

fn city(i: u64) -> u64 {
    (i * 31) % CITIES
}

fn score(i: u64) -> u64 {
    i * 3
}

/// Out degree zero to seven by row, so an eighth of the table has no
/// edge at all and the mean is 3.5.
fn degree(i: u64) -> u64 {
    i % 8
}

fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 4);
    for i in 0..NODES {
        for k in 0..degree(i) {
            out.push((i as u32, ((i * 7919 + k * 104_729) % NODES) as u32));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// What a query has to answer: rows out and the sum of the last column.
struct Want {
    rows: u64,
    total: i64,
}

fn check(r: &query::QueryResult, want: &Want, source: &str) {
    assert_eq!(r.rows.len() as u64, want.rows, "rows for {source}");
    let mut total = 0i64;
    for row in &r.rows {
        let Value::Int(n) = row[row.len() - 1] else {
            panic!("expected an int in the last column of {source}");
        };
        total += n;
    }
    assert_eq!(total, want.total, "column total for {source}");
}

/// Median ms of `source`, with the answer checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: &Want, runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    check(&warm, want, source);
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            check(&r, want, source);
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("factor.zu1");
    let t = Instant::now();
    let edges = edges();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    let cities: Vec<u64> = (0..NODES).map(city).collect();
    let scores: Vec<u64> = (0..NODES).map(score).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("city", PropValues::Int(&cities)),
            ("score", PropValues::Int(&scores)),
        ],
    )
    .expect("props");
    drop(db);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };

    // The answers, straight off the edge list: the hop rows, what they
    // weigh per city, the score they sum to, and the second hop each of
    // them opens.
    let mut out: Vec<u64> = vec![0; NODES as usize];
    for &(s, _) in &edges {
        out[s as usize] += 1;
    }
    let hops = edges.len() as u64;
    let mut per_city: Vec<u64> = vec![0; CITIES as usize];
    let mut score_sum: i64 = 0;
    let mut two_hops: u64 = 0;
    for &(s, d) in &edges {
        per_city[city(u64::from(s)) as usize] += 1;
        score_sum += score(u64::from(s)) as i64;
        two_hops += out[d as usize];
    }
    let groups = per_city.iter().filter(|&&n| n > 0).count() as u64;
    println!(
        "factor: {NODES} persons, {hops} edges over {groups} cities, {two_hops} two hop rows, \
         load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let cases: [(&str, &str, Want, u64); 4] = [
        (
            "counted key",
            "MATCH (a:person)-[:knows]->(b) RETURN a.city AS town, count(*) AS n",
            Want {
                rows: groups,
                total: hops as i64,
            },
            hops,
        ),
        (
            "summed key",
            "MATCH (a:person)-[:knows]->(b) RETURN a.city AS town, sum(a.score) AS s",
            Want {
                rows: groups,
                total: score_sum,
            },
            hops,
        ),
        (
            "bare sum",
            "MATCH (a:person)-[:knows]->(b) RETURN sum(a.score) AS s",
            Want {
                rows: 1,
                total: score_sum,
            },
            hops,
        ),
        (
            "two hop key",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN a.city AS town, count(*) AS n",
            Want {
                rows: groups,
                total: two_hops as i64,
            },
            two_hops,
        ),
    ];

    let mut counted = 0.0;
    for (what, source, want, rows) in cases {
        let new = measure(&mut db, source, &want, 5);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, &want, 3);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = rows as f64 / 1e3 / new;
        println!(
            "factor {what}: {new:.1} ms {mrows:.1} M hop rows/s, old engine {old:.1} ms, {:.1}x, \
             crosschecked",
            old / new
        );
        if what == "counted key" {
            counted = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_factor_mrows_s_core")
    {
        assert!(
            counted >= floor,
            "the counted key reads {counted:.1} M hop rows/s, under the {floor} M floor"
        );
        println!("gate: factor floor met");
    }
}
