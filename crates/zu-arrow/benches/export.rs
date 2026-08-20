//! What a borrowed export costs and a taken one does not (dx/13 §3, zu#170).
//!
//! Two numbers, over the same statement and the same buffers. borrowed
//! is [`Table::of`], which is what every client called until now: the
//! result stays readable afterwards, so each buffer the sink filled is
//! memcpied into the array that leaves. taken is [`Table::taken`], which
//! consumes the result and moves those buffers into Arrow, so a column
//! of half a million integers costs a pointer and the export does no
//! work proportional to the answer at all.
//!
//! The ratio is a ceiling, and it is the number the gate holds: taking
//! is meant to be a small fraction of borrowing, and the day it is not
//! is the day something on the taking path started copying again. It is
//! a fixed cost against a cost per row, so it falls as the answer grows
//! and the ceiling is set for this many rows and no fewer. The bytes
//! line is untimed and is the other half of the point, since the copy is
//! not only time but a second whole answer resident while both exist.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-arrow --bench export

use std::time::Instant;

use arrow::array::{Array, Int64Array};
use zu::query::{QueryResult, run};
use zu_arrow::{BATCH, Table};
use zu_zu1::file::Zu1File;

/// People in the graph, and so rows in the column each export moves.
/// Large enough that a memcpy of the column is the measurement rather
/// than the noise, small enough that several results at once do not put
/// the machine under memory pressure.
const NODES: u32 = 500_000;
/// Passes, so a scheduler hiccup shows up as a spread rather than as
/// the answer. The best of them is what prints.
const REPS: usize = 7;

/// A projection of a stored column and nothing else, which is the shape
/// the sink fills down its columns. Deliberately unordered: an ORDER BY
/// sorts rows, so its result is built across them and has no buffers to
/// take, and measuring the taking path on it would measure the fallback
/// instead. The crosscheck is a sum, so it does not care what order the
/// rows arrive in.
const QUERY: &str = "MATCH (a:person) RETURN a.id AS id";

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

/// The answer, run fresh, so an export is never measured against
/// buffers another export already took.
fn fresh(db: &mut Zu1File) -> QueryResult {
    let result = run(QUERY, db, &[]).expect("query");
    assert_eq!(result.rows.len(), NODES as usize, "rows");
    result
}

/// Every id in the column, added up off the Arrow array, which is what
/// makes the export an export rather than a thing the optimizer can
/// delete.
fn sum(table: Table) -> u64 {
    let mut total = 0u64;
    for batch in table.batches(BATCH) {
        let batch = batch.expect("batch");
        let ints = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        for &value in ints.values() {
            total += value as u64;
        }
    }
    total
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("export.zu1");
    let t = Instant::now();
    {
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i * 7 + 3) % NODES)).collect();
        edges.sort_unstable();
        edges.dedup();
        zu::zu1::graph::bulk_load_as(&mut db, "person", "follows", u64::from(NODES), &edges)
            .expect("bulk load");
    }
    println!("load: {NODES} people, {:.1}s", t.elapsed().as_secs_f64());

    let mut db = Zu1File::open(&path).expect("open");
    // The ids are 0 through NODES-1, whichever way the column leaves.
    let reference: u64 = (0..u64::from(NODES)).sum();

    let mut borrowed = f64::MAX;
    let mut taken = f64::MAX;
    for _ in 0..REPS {
        let result = fresh(&mut db);
        let t = Instant::now();
        let table = Table::of(&result, &()).expect("arrays");
        borrowed = borrowed.min(t.elapsed().as_secs_f64());
        assert_eq!(sum(table), reference, "borrowed sum");
        drop(result);

        let result = fresh(&mut db);
        let t = Instant::now();
        let table = Table::taken(result, &()).expect("arrays");
        taken = taken.min(t.elapsed().as_secs_f64());
        assert_eq!(sum(table), reference, "taken sum");
    }
    let ratio = taken / borrowed;
    println!(
        "export: borrowed {:.3} ms, taken {:.3} ms, ratio {ratio:.4}x, {:.1} ns per row saved",
        borrowed * 1e3,
        taken * 1e3,
        (borrowed - taken) * 1e9 / f64::from(NODES)
    );

    // ---- bytes: what each path allocates, untimed ----
    let bytes = NODES as usize * size_of::<i64>();
    println!(
        "bytes: borrowed {} KiB allocated and copied, taken 0 KiB, both resident while the arrays live",
        bytes / 1024
    );

    if gate {
        // A ceiling. Taking moves a pointer and borrowing moves the
        // column, so the ratio is the fixed cost of an export against
        // the cost of a copy of the answer, and it goes down as the
        // answer gets bigger. Lower this ceiling, never raise it.
        if let Some(max) = budget("arrow_taken_export_ratio") {
            if ratio > max {
                println!("GATE FAIL: taken export ratio {ratio:.4}x over ceiling {max}");
                failed = true;
            } else {
                println!("gate: taken export ratio {ratio:.4}x within {max}");
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}
