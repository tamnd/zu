//! G7 temporal lane gate (Spec/2064g/gql/plan, zu#122): a stored date,
//! time, datetime or duration reaches a kernel as the count it is
//! rather than as a tagged value, so a filter over one is the loop a
//! filter over a number is.
//!
//! Before this, none of the four resolved to a lane at all: a query
//! naming a temporal column went back to the row engine whole, where
//! every value carries a tag and every comparison starts by reading
//! it. The four are counts with a meaning, days for a date and
//! nanoseconds or months for the rest, so the word an integer rides is
//! the word they ride, and the meaning travels on the column instead of
//! in the cell.
//!
//! The table is two million people with a `born` date strided over
//! forty years, a `shift` day-time duration taking one of seven values,
//! and an `age` column that is the same filter written over a number.
//! Six queries run over it: a bound on the date, a selective bound on
//! the same column, an equality on the duration, a grouping keyed by
//! the duration, an equality on a duration a length of time was added
//! to, and the integer bound, which is what the date bound is measured
//! against, since the two do the same work and the difference between
//! them is whatever the lane costs.
//!
//! Each one runs twice, once through the pipeline and once with
//! ZU_EXEC2=0, which is where all of them ran before this change.
//!
//! The answers are crosschecked against the generator on every run, so
//! a lane that reads a date a day early fails here rather than printing
//! a fast number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_temporal_mrows_s_core floors the date bound.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench temporal

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu_common::DurationKind;

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

/// Days since the epoch, strided over roughly forty years so that
/// neighboring rows hold unrelated dates, every chunk holds the whole
/// range and the zone map can rule out none of them. What a bound on
/// this column costs is the comparison and nothing else.
fn born_of(i: u64) -> i64 {
    ((i * 7919) % 14_610) as i64
}

/// The same number of days as an age in years, so that the integer
/// bound below selects the same rows the date bound does and the two
/// rates are a rate for one piece of work rather than two.
fn age_of(i: u64) -> i64 {
    born_of(i)
}

/// Nanoseconds, one of seven whole hours, so a grouping on this column
/// has seven groups whatever the row count.
fn shift_of(i: u64) -> i64 {
    ((i * 104_729) % 7) as i64 * 3_600_000_000_000
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let born: Vec<i32> = (0..NODES).map(|i| born_of(i) as i32).collect();
    let shift: Vec<i64> = (0..NODES).map(shift_of).collect();
    let age: Vec<u64> = (0..NODES).map(|i| age_of(i) as u64).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("born", PropValues::Date(&born)),
            ("shift", PropValues::Duration(DurationKind::DayTime, &shift)),
            ("age", PropValues::Int(&age)),
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

/// The rows a grouping answered, summed, which is the row count when
/// every row falls in a group and is what says the grouping saw them
/// all.
fn grouped(r: &zu::query::QueryResult) -> i64 {
    r.rows
        .iter()
        .map(|row| match row[1] {
            Value::Int(n) => n,
            ref other => panic!("expected a count, got {other:?}"),
        })
        .sum()
}

/// Median ms of `source`, with the answer checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: i64, group: bool, runs: usize) -> f64 {
    let read = |r: &zu::query::QueryResult| match group {
        true => grouped(r),
        false => count(r),
    };
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(read(&warm), want, "warmup answer for {source}");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(read(&r), want, "answer changed for {source}");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("temporal.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "temporal: {NODES} persons, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    // 1990-01-01 and 2009-01-01 as days since the epoch, which is what
    // the column holds and what the literals below spell.
    let common = (0..NODES).filter(|&i| born_of(i) < 7305).count() as i64;
    let rare = (0..NODES).filter(|&i| born_of(i) >= 14_245).count() as i64;
    let one_shift = (0..NODES)
        .filter(|&i| shift_of(i) == 3_600_000_000_000)
        .count() as i64;

    let cases: [(&str, &str, i64, bool); 6] = [
        (
            "date bound",
            "MATCH (p:person) WHERE p.born < DATE '1990-01-01' RETURN count(p) AS n",
            common,
            false,
        ),
        (
            "selective date bound",
            "MATCH (p:person) WHERE p.born >= DATE '2009-01-01' RETURN count(p) AS n",
            rare,
            false,
        ),
        (
            "integer bound",
            "MATCH (p:person) WHERE p.age < 7305 RETURN count(p) AS n",
            common,
            false,
        ),
        (
            "duration equality",
            "MATCH (p:person) WHERE p.shift = DURATION 'PT1H' RETURN count(p) AS n",
            one_shift,
            false,
        ),
        (
            "grouping on a duration",
            "MATCH (p:person) RETURN p.shift AS s, count(p) AS n ORDER BY s",
            NODES as i64,
            true,
        ),
        // A length of time added to a stored one, which selects the
        // rows the equality above selects and does one addition per row
        // more than it does. Two durations of a kind are two counts of
        // the same unit, so the sum is the integer sum and the pair
        // reads as the cost of the arithmetic rather than of the lane.
        (
            "duration arithmetic",
            "MATCH (p:person) WHERE p.shift + DURATION 'PT1H' = DURATION 'PT2H' \
             RETURN count(p) AS n",
            one_shift,
            false,
        ),
    ];

    let mut date_mrows = 0.0;
    for (what, source, want, group) in cases {
        let new = measure(&mut db, source, want, group, 9);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, want, group, 5);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "temporal {what}: {new:.1} ms {mrows:.0} M rows/s, old engine {old:.1} ms, \
             {:.2}x, crosschecked",
            old / new
        );
        if what == "date bound" {
            date_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_temporal_mrows_s_core")
    {
        assert!(
            date_mrows >= floor,
            "date bound at {date_mrows:.0} M rows/s under the {floor} M floor"
        );
        println!("gate: temporal lane floor met");
    }
}
