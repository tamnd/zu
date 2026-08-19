//! What a columnar export costs end to end, before and after the sink
//! kept its vectors.
//!
//! `bench/columnar` in zu-query timed the two walks on a result that
//! was already built. This times the whole thing a client actually
//! does: run a statement over a million rows and read the answer down
//! its columns, three ways. The pipeline fills the buffers and
//! `columnar()` hands them back. ZU_SINK=rows pins the row build the
//! pipeline did before this, so `columnar()` walks the rows twice, and
//! that is the number the columns are measured against. ZU_EXEC2=0 is
//! the old engine, which is where the audit's twenty times DuckDB came
//! from. The three columns are the ones the audit exported: an INT64, a
//! DOUBLE and a short VARCHAR.
//!
//! The row read is timed on both as well, because a client that wants
//! rows must not have paid for columns, and on the pipeline that read
//! is the buffers turned back into rows.
//!
//! Informational, with no gate floor. The floor that matters is the one
//! in the client, against DuckDB, and it is published with the release.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! Run: cargo bench -p zu --bench columns

use std::hint::black_box;
use std::time::Instant;

use zu::query::column::ColumnData;
use zu::query::{self, QueryResult, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

const NODES: u64 = 1_000_000;
const RUNS: usize = 5;

const SOURCE: &str = "MATCH (p:person) RETURN p.score AS n, p.ratio AS f, p.tag AS s";

fn score_of(i: u64) -> u64 {
    (i * 7919) % 100_000
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let score: Vec<u64> = (0..NODES).map(score_of).collect();
    let ratio: Vec<f64> = (0..NODES).map(|i| i as f64 * 1.5).collect();
    // Short enough to live inline in every format that has an inline
    // string, which is what the audit exported.
    let tags: Vec<String> = (0..NODES).map(|i| format!("s{:04}", i % 10_000)).collect();
    let tag: Vec<&[u8]> = tags.iter().map(|t| t.as_bytes()).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("score", PropValues::Int(&score)),
            ("ratio", PropValues::Float(&ratio)),
            ("tag", PropValues::Str(&tag)),
        ],
    )
    .expect("props");
}

/// Fastest of `RUNS`, because what is being compared is the code path
/// and not the scheduler.
fn best(mut run: impl FnMut()) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..RUNS {
        let at = Instant::now();
        run();
        best = best.min(at.elapsed().as_secs_f64());
    }
    best * 1e3
}

/// Reads every column, which is what an export does with the answer it
/// is handed and what keeps the buffers from being optimized away.
fn read(result: &QueryResult) -> (i64, f64, usize) {
    let columns = result.columnar().expect("one type per column");
    let mut ints = 0i64;
    let mut floats = 0.0;
    let mut bytes = 0usize;
    for column in &columns.columns {
        match &column.data {
            ColumnData::Int(v) => ints = v.iter().copied().sum(),
            ColumnData::Float(v) => floats = v.iter().sum(),
            ColumnData::Str(s) => bytes = s.bytes.len(),
            other => panic!("unexpected column {other:?}"),
        }
    }
    (ints, floats, bytes)
}

/// The same answer read across its rows, which is what `fetchall` does.
fn walk(result: &QueryResult) -> (i64, usize) {
    let mut ints = 0i64;
    let mut bytes = 0usize;
    for row in result.rows.iter() {
        match (&row[0], &row[2]) {
            (Value::Int(n), Value::Str(s)) => {
                ints = ints.wrapping_add(*n);
                bytes += s.len();
            }
            other => panic!("unexpected row {other:?}"),
        }
    }
    (ints, bytes)
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("columns.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "columns: {NODES} persons of int, float and a short string, load {:.1}s\n",
        t.elapsed().as_secs_f64()
    );

    let want_ints: i64 = (0..NODES).map(|i| score_of(i) as i64).sum();
    let want_bytes = NODES as usize * 5;

    let mut numbers = Vec::new();
    for engine in ["columns", "rows, as was", "old engine"] {
        // SAFETY: same as the worker count above.
        unsafe {
            match engine {
                "rows, as was" => std::env::set_var("ZU_SINK", "rows"),
                "old engine" => std::env::set_var("ZU_EXEC2", "0"),
                _ => {}
            }
        }
        let query = best(|| {
            let r = query::run(SOURCE, &mut db, &[]).expect("run");
            black_box(&r);
        });
        let held = query::run(SOURCE, &mut db, &[]).expect("run");
        assert_eq!(held.rows.len(), NODES as usize);
        assert_eq!(
            held.rows.columns().is_some(),
            engine == "columns",
            "{engine} kept the wrong sink"
        );
        let export = best(|| {
            let got = read(&held);
            assert_eq!((got.0, got.2), (want_ints, want_bytes));
        });
        // A fresh result each time, because the first row read on the
        // pipeline builds the rows and every one after it is free.
        let rows = best(|| {
            let r = query::run(SOURCE, &mut db, &[]).expect("run");
            assert_eq!(walk(&r), (want_ints, want_bytes));
        });
        // SAFETY: same as the worker count above.
        unsafe {
            std::env::remove_var("ZU_SINK");
            std::env::remove_var("ZU_EXEC2");
        }
        println!(
            "{engine:<12} query {query:>7.1} ms, export {export:>7.1} ms, \
             query+export {:>7.1} ms, rows {rows:>7.1} ms",
            query + export
        );
        numbers.push((query, export, rows));
    }

    let (new, was, old) = (numbers[0], numbers[1], numbers[2]);
    let total = |(query, export, _): (f64, f64, f64)| query + export;
    println!(
        "\nexport path {:.2}x the row sink and {:.2}x the old engine, \
         {:.1} M rows/s",
        total(was) / total(new),
        total(old) / total(new),
        NODES as f64 / 1e3 / total(new),
    );
    println!(
        "row path    {:.2}x the row sink, {:.1} ms against {:.1} ms, \
         which is what a caller who wanted rows pays",
        was.2 / new.2,
        new.2,
        was.2
    );
}
