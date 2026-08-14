//! G1 static comparison gate (Spec/2064g/gql/plan, zu#116): a
//! comparison whose operand types the compiler knows runs the kernel
//! for those types, and reads no value tag per row.
//!
//! An integer column against an integer constant already did that. An
//! integer column against a float constant did not: the two sides had
//! different physical types, so the whole query went back to the row
//! engine, where every value carries a tag and every comparison starts
//! by reading it. The bound now moves into the integer domain at
//! compile time, `p.age > 30.5` becoming `p.age > 30`, and the query
//! runs the kernel the integer bound runs and skips the chunks the
//! integer bound skips.
//!
//! The table is two million people with an `age` column of a hundred
//! strided values, a `score` column, and a `seq` column that climbs
//! with the row. Seven predicates run over it: the integer bound,
//! which is the number the float bound has to reach; the float bound;
//! a selective float bound; a conjunction, which is the shape a filter
//! usually has; the same bound written both ways over `seq`, where the
//! zone map rules out most of the table before anything is decoded and
//! the two spellings have to rule out the same chunks; and a bound the
//! rewrite refuses, `p.age < 1e300`, which still falls back and is
//! here so the refusal stays visible and stays correct.
//!
//! Each one runs twice, once through the pipeline and once with
//! ZU_EXEC2=0, which is where the float bounds ran before this change.
//! The integer bound's ratio is the same measurement on a shape that
//! was already compiled, so it says how much of the gap is the port
//! rather than the rewrite.
//!
//! The counts are crosschecked against the generator on every run, so
//! a rewrite that moves the answer fails here rather than printing a
//! fast number. That matters more than usual: the whole claim is that
//! `> 30.5` and `> 30` select the same people.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_compare_mrows_s_core floors the float bound, the shape the
//! rewrite is for.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench compare

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
/// Where the bound on the ordered column sits, three quarters of the
/// way in, so most of the chunks are behind it.
const ORDERED: u64 = NODES / 4 * 3;

/// Ages run zero to ninety nine and are strided, so neighboring rows
/// hold unrelated values, every chunk holds the whole range and the
/// zone map can rule out none of them. What a bound on this column
/// costs is the comparison and nothing else.
fn age_of(i: u64) -> i64 {
    ((i * 7919) % 100) as i64
}

fn score_of(i: u64) -> i64 {
    ((i * 104_729) % 1000) as i64
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let age: Vec<u64> = (0..NODES).map(|i| age_of(i) as u64).collect();
    let score: Vec<u64> = (0..NODES).map(|i| score_of(i) as u64).collect();
    let seq: Vec<u64> = (0..NODES).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&age)),
            ("score", PropValues::Int(&score)),
            ("seq", PropValues::Int(&seq)),
        ],
    )
    .expect("props");
}

/// The one row and the count in it.
fn count(r: &zu::query::QueryResult) -> i64 {
    assert_eq!(r.rows.len(), 1, "a counting query returns one row");
    match r.rows[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected a count, got {other:?}"),
    }
}

/// Median ms of `source`, with the count checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: i64, runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(count(&warm), want, "warmup answer for {source}");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(count(&r), want, "answer changed for {source}");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("compare.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "compare: {NODES} persons, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    // The generator answers each predicate, and the integer bound and
    // the float bound above it have to agree, which is the rewrite
    // written out as a count.
    let over_30 = (0..NODES).filter(|&i| age_of(i) > 30).count() as i64;
    let over_30_5 = (0..NODES).filter(|&i| age_of(i) as f64 > 30.5).count() as i64;
    let over_97 = (0..NODES).filter(|&i| age_of(i) as f64 > 97.5).count() as i64;
    let both = (0..NODES)
        .filter(|&i| age_of(i) as f64 > 30.5 && score_of(i) > 600)
        .count() as i64;
    assert_eq!(
        over_30, over_30_5,
        "an age is a whole number, so the two bounds select the same people"
    );
    let ordered = (NODES - ORDERED - 1) as i64;

    let cases: [(&str, String, i64); 7] = [
        (
            "integer bound",
            "MATCH (p:person) WHERE p.age > 30 RETURN count(p) AS n".into(),
            over_30,
        ),
        (
            "float bound",
            "MATCH (p:person) WHERE p.age > 30.5 RETURN count(p) AS n".into(),
            over_30_5,
        ),
        (
            "selective float bound",
            "MATCH (p:person) WHERE p.age > 97.5 RETURN count(p) AS n".into(),
            over_97,
        ),
        (
            "float bound and a term",
            "MATCH (p:person) WHERE p.age > 30.5 AND p.score > 600 RETURN count(p) AS n".into(),
            both,
        ),
        (
            "ordered integer bound",
            format!("MATCH (p:person) WHERE p.seq > {ORDERED} RETURN count(p) AS n"),
            ordered,
        ),
        (
            "ordered float bound",
            format!("MATCH (p:person) WHERE p.seq > {ORDERED}.5 RETURN count(p) AS n"),
            ordered,
        ),
        (
            "refused bound",
            "MATCH (p:person) WHERE p.age < 1e300 RETURN count(p) AS n".into(),
            NODES as i64,
        ),
    ];

    let mut float_mrows = 0.0;
    for (what, source, want) in cases {
        let new = measure(&mut db, &source, want, 9);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, &source, want, 5);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "compare {what}: {new:.1} ms {mrows:.0} M rows/s, old engine {old:.1} ms, \
             {:.2}x, crosschecked",
            old / new
        );
        if what == "float bound" {
            float_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_compare_mrows_s_core")
    {
        assert!(
            float_mrows >= floor,
            "float bound at {float_mrows:.0} M rows/s under the {floor} M floor"
        );
        println!("gate: comparison floor met");
    }
}
