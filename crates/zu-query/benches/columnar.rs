//! What reading a result down its columns costs, against reading it
//! across its rows.
//!
//! `docs/clients/duckdb.md` measured `to_arrow` on a million rows at
//! twenty times DuckDB's, and traced all of it to the transpose every
//! columnar client was writing for itself: one walk of the whole result
//! per column to settle the column's type, another per column per batch
//! to gather the values. This times the shape that was, the shape that
//! is, and the row walk both of them are measured against, on the same
//! rows the audit used: a million of INT64, DOUBLE and a short VARCHAR.
//!
//! Informational, with no gate floor. The floor that matters is the one
//! in the client, against DuckDB, and it is published with the release.
//!
//! Run: cargo bench -p zu-query --bench columnar

use std::hint::black_box;
use std::time::Instant;

use zu_query::column::ColumnType;
use zu_query::exec::{QueryResult, Value};

const ROWS: usize = 1_000_000;
const RUNS: usize = 5;

/// The same three columns the audit exported, and the same widths: a
/// dense integer, a double, and a string short enough to live inline in
/// every format that has an inline string.
fn rows(n: usize) -> QueryResult {
    let mut rows = Vec::with_capacity(n);
    for at in 0..n {
        rows.push(vec![
            Value::Int(at as i64),
            Value::Float(at as f64 * 1.5),
            Value::Str(format!("s{:04}", at % 10_000)),
        ]);
    }
    QueryResult::new(vec!["n".into(), "f".into(), "s".into()], rows)
}

/// Fastest of `RUNS`, because what is being compared is the code path
/// and not the scheduler.
fn best(label: &str, rows: usize, mut run: impl FnMut()) {
    let mut best = f64::INFINITY;
    for _ in 0..RUNS {
        let at = Instant::now();
        run();
        best = best.min(at.elapsed().as_secs_f64());
    }
    println!(
        "{label:<34} {:>8.1} ms  {:>9.0} rows/s",
        best * 1e3,
        rows as f64 / best
    );
}

/// The transpose a client used to write: one pass per column to infer,
/// then one pass per column per batch to gather pointers. Kept here
/// rather than in a client so the two shapes are timed in one process
/// on one result.
fn per_column(result: &QueryResult) {
    const BATCH: usize = 65_536;
    let mut types = Vec::with_capacity(result.columns.len());
    for at in 0..result.columns.len() {
        let mut ty = ColumnType::Null;
        for row in &result.rows {
            let found = ColumnType::of(&row[at]).expect("one type per column");
            ty = ColumnType::unify(ty, found).expect("one type per column");
        }
        types.push(ty);
    }
    let mut start = 0;
    while start < result.rows.len() {
        let batch = &result.rows[start..(start + BATCH).min(result.rows.len())];
        for at in 0..result.columns.len() {
            let values: Vec<&Value> = batch.iter().map(|row| &row[at]).collect();
            black_box(&values);
        }
        start += BATCH;
    }
    black_box(&types);
}

/// The row walk, which is what `fetchall` does and what the audit found
/// us three times faster than DuckDB at.
fn per_row(result: &QueryResult) {
    let mut ints = 0i64;
    let mut bytes = 0usize;
    for row in &result.rows {
        for value in row {
            match value {
                Value::Int(n) => ints = ints.wrapping_add(*n),
                Value::Str(s) => bytes += s.len(),
                _ => {}
            }
        }
    }
    black_box((ints, bytes));
}

fn main() {
    let built = Instant::now();
    let result = rows(ROWS);
    println!(
        "{ROWS} rows of int, float and a short string, built in {:.1} ms\n",
        built.elapsed().as_secs_f64() * 1e3
    );

    best("walk the rows", ROWS, || per_row(&result));
    best("transpose per column, as was", ROWS, || per_column(&result));
    best("columnar()", ROWS, || {
        let columns = result.columnar().expect("one type per column");
        black_box(&columns);
    });

    let columns = result.columnar().expect("one type per column");
    let bytes: usize = columns
        .columns
        .iter()
        .map(|column| match &column.data {
            zu_query::column::ColumnData::Int(v) => v.len() * 8,
            zu_query::column::ColumnData::Float(v) => v.len() * 8,
            zu_query::column::ColumnData::Str(s) => s.bytes.len(),
            _ => 0,
        })
        .sum();
    println!("\nbuffers handed over: {} MiB", bytes / (1 << 20));
}
