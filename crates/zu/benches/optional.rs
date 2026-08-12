//! P3 OPTIONAL MATCH gate (Spec/2064g/docs/07, zu#76): the left outer
//! bracket runs its group once per outer row and hands a row that
//! matched nothing a null level to carry, instead of sending the whole
//! query back to the old engine.
//!
//! The table is two million people and a knows graph built so that a
//! known share of them has no outgoing edge at all: every fourth person
//! is isolated, so a quarter of the outer rows take the miss path and
//! the number is not just the matching case with a branch in it. The
//! rest have two friends each, so a hit fans out as well.
//!
//! Four queries run over it. The count is the bracket with nothing
//! built above it, so it is the walk and the miss handling and nothing
//! else, and that is the shape the floor sits on. The rows shape
//! returns every outer row with its friend, nulls included, which is
//! the projection reading a level that may not be there. The third
//! puts the group's own WHERE inside the bracket, which turns most
//! hits into misses and so runs the null path for nearly every row;
//! that one is dominated by the filter running over a vector of two,
//! which is what a row at a time expand hands it, not by the bracket.
//! The fourth is the same walk and the same projection with the
//! bracket taken off, so the gap between it and the rows shape is what
//! the bracket costs.
//!
//! Each one runs twice, once through the pipeline and once with
//! ZU_EXEC2=0, which is where the first three ran before: an OPTIONAL
//! MATCH had no compiled shape at all.
//!
//! Every run is crosschecked against the edge list the graph was built
//! from, misses counted separately, so a bracket that drops a miss or
//! double counts a hit fails here rather than printing a fast number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_optional_mrows_s_core floors the counted bracket in millions of
//! outer rows a second.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench optional

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

const NODES: u64 = 2_000_000;
/// The score cut the third query puts inside the bracket. Scores are
/// strided over a hundred thousand, so this keeps about a fifth of the
/// friends and turns most of the hits into misses.
const CUT: i64 = 80_000;

fn score_of(i: u64) -> i64 {
    ((i * 7919) % 100_000) as i64
}

/// Every fourth person is isolated, the rest know two people picked far
/// apart in row order so the walk does not read one neighborhood over
/// and over.
fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 3 / 2);
    for i in 0..NODES {
        if i % 4 == 0 {
            continue;
        }
        out.push((i as u32, ((i * 7919 + 13) % NODES) as u32));
        out.push((i as u32, ((i * 104_729 + 7) % NODES) as u32));
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

/// What the generator says each query has to answer: rows out, of which
/// misses, and the total of the friend ids the hits contributed. The
/// last one is what catches a bracket that pairs the wrong two rows.
struct Want {
    rows: u64,
    misses: u64,
    friend_total: i64,
}

/// Walks the edge list the way the bracket has to: one row per hit, one
/// row per outer row with no hit at all.
fn expected(edges: &[(u32, u32)], keep: impl Fn(u32) -> bool) -> Want {
    let mut hits = vec![0u32; NODES as usize];
    let mut friend_total = 0i64;
    let mut rows = 0u64;
    for &(src, dst) in edges {
        if !keep(dst) {
            continue;
        }
        hits[src as usize] += 1;
        friend_total += i64::from(dst);
        rows += 1;
    }
    let misses = hits.iter().filter(|&&n| n == 0).count() as u64;
    Want {
        rows: rows + misses,
        misses,
        friend_total,
    }
}

/// Reads a result the way the crosscheck needs it, counting the rows
/// whose friend came back null.
fn shape(r: &query::QueryResult) -> (u64, u64, i64) {
    // The counted shape answers with one row holding the total, and
    // there is nothing in it to say how many of those were misses.
    if r.columns.len() == 1 && r.rows.len() == 1 && r.columns[0] == "n" {
        let Value::Int(n) = r.rows[0][0] else {
            panic!("the count is an int");
        };
        return (n as u64, u64::MAX, 0);
    }
    let mut misses = 0;
    let mut friend_total = 0i64;
    for row in &r.rows {
        match row[1] {
            Value::Null => misses += 1,
            Value::Int(n) => friend_total += n,
            ref other => panic!("expected a friend id or a null, got {other:?}"),
        }
    }
    (r.rows.len() as u64, misses, friend_total)
}

fn check(r: &query::QueryResult, want: &Want, source: &str) {
    let (rows, misses, friend_total) = shape(r);
    assert_eq!(rows, want.rows, "rows for {source}");
    if misses != u64::MAX {
        assert_eq!(misses, want.misses, "misses for {source}");
        assert_eq!(friend_total, want.friend_total, "friend total for {source}");
    }
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
    let path = dir.path().join("optional.zu1");
    let t = Instant::now();
    let edges = edges();
    build(&path, &edges);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "optional: {NODES} persons, {} edges, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let all = expected(&edges, |_| true);
    let cut = expected(&edges, |dst| score_of(u64::from(dst)) > CUT);
    println!(
        "optional: {} rows of which {} misses, under the cut {} rows of which {} misses",
        all.rows, all.misses, cut.rows, cut.misses
    );

    // The same rows with the bracket taken off, which is the pipeline
    // doing the same walk and the same projection with no miss to
    // account for. The gap between this and the rows shape is what the
    // bracket costs.
    let hits = Want {
        rows: all.rows - all.misses,
        misses: 0,
        friend_total: all.friend_total,
    };

    let filtered = format!(
        "MATCH (p:person) OPTIONAL MATCH (p)-[:knows]->(f) WHERE f.score > {CUT} RETURN p.id AS p, f.id AS f"
    );
    let cases: [(&str, &str, &Want, usize); 4] = [
        (
            "count",
            "MATCH (p:person) OPTIONAL MATCH (p)-[:knows]->(f) RETURN count(*) AS n",
            &all,
            9,
        ),
        (
            "rows",
            "MATCH (p:person) OPTIONAL MATCH (p)-[:knows]->(f) RETURN p.id AS p, f.id AS f",
            &all,
            5,
        ),
        ("filtered rows", &filtered, &cut, 5),
        (
            "required rows",
            "MATCH (p:person)-[:knows]->(f) RETURN p.id AS p, f.id AS f",
            &hits,
            5,
        ),
    ];

    let mut count_mrows = 0.0;
    for (what, source, want, runs) in cases {
        let new = measure(&mut db, source, want, runs);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, want, runs.min(3));
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "optional {what}: {new:.1} ms {mrows:.0} M outer rows/s, old engine {old:.1} ms, \
             {:.2}x, crosschecked",
            old / new
        );
        if what == "count" {
            count_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_optional_mrows_s_core")
    {
        assert!(
            count_mrows >= floor,
            "counted bracket at {count_mrows:.0} M outer rows/s under the {floor} M floor"
        );
        println!("gate: optional floor met");
    }
}
