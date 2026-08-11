//! P3 ordering gate (Spec/2064g/perf/05 section 5, zu#76): ORDER BY
//! with a LIMIT is the IS and IC shape, and the spec asks for the sort
//! to cost under 5 percent of the query it sits on.
//!
//! The table is a million people with a `bucket` column of a hundred
//! values, so `WHERE p.bucket = 7` leaves ten thousand candidates, the
//! IS-class fan the spec names. Three queries run over that same fan:
//! the plain projection with no order at all, the ordered top ten, and
//! the fully ordered fan. The first two differ only by the ORDER BY and
//! the LIMIT, so the difference between them is what ordering costs,
//! and the third says what the same order costs when nothing bounds it.
//!
//! The top ten rows are crosschecked against a reference sort of the
//! candidates the bench computes itself, so an ordering that drops a
//! tie or reverses one fails here rather than printing a fast number.
//!
//! exec_sort_share_pct is a ceiling on the ordered query's sort share.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench topn

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
const BUCKETS: u64 = 100;
const PICK: u64 = 7;
const TOP: usize = 10;

/// Scores are strided so the candidates of one bucket arrive in no
/// useful order, and they repeat every thousand rows so the top ten has
/// ties to break.
fn score_of(i: u64) -> i64 {
    ((i * 7919) % 1000) as i64
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let bucket: Vec<u64> = (0..NODES).map(|i| i % BUCKETS).collect();
    let score: Vec<u64> = (0..NODES).map(|i| score_of(i) as u64).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("bucket", PropValues::Int(&bucket)),
            ("score", PropValues::Int(&score)),
        ],
    )
    .expect("props");
}

/// The candidates of the picked bucket, ordered the way the query asks:
/// score descending, id ascending inside a tie.
fn reference() -> Vec<Vec<Value>> {
    let mut cands: Vec<(i64, i64)> = (0..NODES)
        .filter(|i| i % BUCKETS == PICK)
        .map(|i| (score_of(i), i as i64))
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    cands
        .into_iter()
        .map(|(s, id)| vec![Value::Int(id), Value::Int(s)])
        .collect()
}

/// Median ms of every query, run round robin rather than one query at
/// a time: the numbers here are subtracted from each other, so a drift
/// in the machine has to land on all of them or it reads as sort cost.
/// The answer is checked on every run.
fn measure(db: &mut Zu1File, qs: &[(&str, &[Vec<Value>])], runs: usize) -> Vec<f64> {
    for (source, want) in qs {
        let warm = query::run(source, db, &[]).expect("warmup");
        assert_eq!(warm.rows, *want, "warmup answer for {source}");
    }
    let mut times = vec![Vec::with_capacity(runs); qs.len()];
    for _ in 0..runs {
        for (at, (source, want)) in qs.iter().enumerate() {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            times[at].push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(r.rows, *want, "answer changed for {source}");
        }
    }
    times
        .iter_mut()
        .map(|t| {
            t.sort_by(f64::total_cmp);
            t[t.len() / 2]
        })
        .collect()
}

const PLAIN: &str = "MATCH (p:person) WHERE p.bucket = 7 \
                     RETURN p.id AS id, p.score AS s";
const TOPN: &str = "MATCH (p:person) WHERE p.bucket = 7 \
                    RETURN p.id AS id, p.score AS s \
                    ORDER BY s DESC, id LIMIT 10";
const SORTED: &str = "MATCH (p:person) WHERE p.bucket = 7 \
                      RETURN p.id AS id, p.score AS s \
                      ORDER BY s DESC, id";

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("topn.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    let ordered = reference();
    let mut unordered = ordered.clone();
    unordered.sort_by_key(|row| match row[0] {
        Value::Int(id) => id,
        ref other => panic!("expected an id, got {other:?}"),
    });
    println!(
        "topn: {NODES} persons, {} candidates in bucket {PICK}, load {:.1}s",
        ordered.len(),
        t.elapsed().as_secs_f64()
    );

    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    let ms = measure(
        &mut db,
        &[
            (PLAIN, &unordered),
            (TOPN, &ordered[..TOP]),
            (SORTED, &ordered),
        ],
        51,
    );
    let (plain, top, sorted) = (ms[0], ms[1], ms[2]);
    let share = (top - plain) / top * 100.0;
    println!(
        "topn: plain {plain:.2} ms, top {TOP} {top:.2} ms, full order {sorted:.2} ms, \
         sort share {share:.1} percent, crosschecked"
    );

    // The same ordered query on the old engine, which materializes a
    // boxed key per row and sorts the whole fan whatever the limit is.
    // SAFETY: as above.
    unsafe { std::env::set_var("ZU_EXEC2", "0") };
    let old = measure(&mut db, &[(TOPN, &ordered[..TOP]), (SORTED, &ordered)], 21);
    let (old_top, old_sorted) = (old[0], old[1]);
    unsafe { std::env::remove_var("ZU_EXEC2") };
    println!(
        "topn: old engine top {TOP} {old_top:.2} ms ({:.1}x), full order {old_sorted:.2} ms ({:.1}x)",
        old_top / top,
        old_sorted / sorted
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(ceiling) = budget("exec_sort_share_pct")
    {
        assert!(
            share <= ceiling,
            "ordering costs {share:.1} percent of the query, over the {ceiling} percent ceiling"
        );
        println!("gate: sort share ceiling met");
    }
}
