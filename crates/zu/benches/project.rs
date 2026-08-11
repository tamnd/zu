//! P3 projection gate (Spec/2064g/perf/05 section 5, zu#76): a computed
//! projection is a column of the level its properties come from, not a
//! per-row evaluation over the sink, and this bench says what that is
//! worth on the two shapes a projection turns up in.
//!
//! The table is two million people with a `score` column and a `bucket`
//! column of a hundred values. Three queries run over it: the sum of an
//! expression over every row, which builds no rows at all and so is the
//! arithmetic on its own; the same expression returned for a hundredth
//! of the table, which is the selective read a filter feeds; and the
//! expression returned for every row, which is the projection with the
//! row build behind it.
//!
//! Each one runs twice, once through the pipeline and once with
//! ZU_EXEC2=0, which is where all three of them ran before the port:
//! an arithmetic projection had no compiled shape, so the whole query
//! went back to the old engine. The ratio is what the port moved.
//!
//! The answers are crosschecked against the generator on every run, so
//! an expression the kernels get wrong fails here rather than printing
//! a fast number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_project_mrows_s_core floors the summed expression, the shape
//! with no row build in the way of the arithmetic.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench project

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
const BUCKETS: u64 = 100;
const PICK: u64 = 7;

/// Scores are strided so neighboring rows hold unrelated values and the
/// expression cannot be constant-folded away by the memory system.
fn score_of(i: u64) -> i64 {
    ((i * 7919) % 100_000) as i64
}

fn bucket_of(i: u64) -> u64 {
    i % BUCKETS
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let score: Vec<u64> = (0..NODES).map(|i| score_of(i) as u64).collect();
    let bucket: Vec<u64> = (0..NODES).map(bucket_of).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("score", PropValues::Int(&score)),
            ("bucket", PropValues::Int(&bucket)),
        ],
    )
    .expect("props");
}

/// The expression every query computes, `score * 2 + bucket`, as the
/// generator sees it.
fn value_of(i: u64) -> i64 {
    score_of(i) * 2 + bucket_of(i) as i64
}

/// Rows returned and the total of the last column, which is the only
/// crosscheck all three shapes share.
fn shape(r: &zu::query::QueryResult) -> (usize, i64) {
    let last = r.columns.len() - 1;
    let total = r
        .rows
        .iter()
        .map(|row| match row[last] {
            Value::Int(n) => n,
            ref other => panic!("expected an int, got {other:?}"),
        })
        .sum();
    (r.rows.len(), total)
}

/// Median ms of `source`, with the answer checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: (usize, i64), runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(shape(&warm), want, "warmup answer for {source}");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(shape(&r), want, "answer changed for {source}");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("project.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "project: {NODES} persons, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let sum_all: i64 = (0..NODES).map(value_of).sum();
    let picked: Vec<i64> = (0..NODES)
        .filter(|&i| bucket_of(i) == PICK)
        .map(value_of)
        .collect();
    let pick_rows = picked.len();
    let pick_total: i64 = picked.iter().sum();

    // Three shapes, each on the pipeline and each on the old engine,
    // which is where every one of them ran before the port.
    let cases: [(&str, &str, (usize, i64), usize); 3] = [
        (
            "sum",
            "MATCH (p:person) RETURN sum(p.score * 2 + p.bucket) AS v",
            (1, sum_all),
            9,
        ),
        (
            "selective rows",
            "MATCH (p:person) WHERE p.bucket = 7 RETURN p.score * 2 + p.bucket AS v",
            (pick_rows, pick_total),
            9,
        ),
        (
            "every row",
            "MATCH (p:person) RETURN p.score * 2 + p.bucket AS v",
            (NODES as usize, sum_all),
            5,
        ),
    ];

    let mut sum_mrows = 0.0;
    for (what, source, want, runs) in cases {
        let new = measure(&mut db, source, want, runs);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, want, runs.min(5));
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "project {what}: {new:.1} ms {mrows:.0} M rows/s, old engine {old:.1} ms, \
             {:.2}x, crosschecked",
            old / new
        );
        if what == "sum" {
            sum_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_project_mrows_s_core")
    {
        assert!(
            sum_mrows >= floor,
            "summed projection at {sum_mrows:.0} M rows/s under the {floor} M floor"
        );
        println!("gate: projection floor met");
    }
}
