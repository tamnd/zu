//! What the typed row layer costs over matching on `Value` by hand
//! (`dx/04` §4).
//!
//! The layer exists so a caller stops writing a `match` per column, and
//! it is only worth having if that convenience is free. Free has a
//! precise meaning here: reading a result through `Row::get` compiles
//! to the same bounds check and the same load as reading `result.rows`
//! directly, because `FromValue` is a match on a reference that the
//! optimizer inlines and every error path out of it is cold and out of
//! line. The tuple measures about one and the single column about a
//! third above it, both of them a fraction of a nanosecond per row, and
//! a ratio that climbs past its ceiling means something started
//! copying, which on this path would be a string.
//!
//! Three numbers. typed_row_x gates tuple destructuring against the
//! hand-written match over the same rows. typed_str_x gates the same
//! for a borrowed string column, which is the one a copy would show up
//! in first, and it sits above the tuple because one column read on its
//! own pays the bounds test that the tuple pays once for the whole row.
//! Both sides of that ratio run under a nanosecond, so its ceiling is
//! set to catch a copy rather than a drift. by_name_x is reported and
//! not gated, because reading by name compares a column name per row
//! and is meant to be slower than reading by index: it
//! measures about five times the read by index, which is the number
//! that tells a caller to hoist `column_index` out of its loop.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench rows

use std::time::Instant;

use zu::query::{QueryResult, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Config, Database, params};

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

const NODES: u32 = 100_000;
const ROUNDS: u32 = 40;

/// A result of one integer column and one string column, which is the
/// shape of nearly every projection a caller reads in a loop. It is
/// produced by a real statement rather than built by hand so the rows
/// hold whatever the executor puts in them.
fn build(path: &std::path::Path) -> (QueryResult, i64) {
    let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    edges.sort_unstable();
    let mut file = Zu1File::create(path).expect("create");
    bulk_load_as(&mut file, "person", "knows", u64::from(NODES), &edges).expect("load");
    drop(file);

    let db = Database::open_with(path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query_with(
            "MATCH (p:person) WHERE p.id >= $floor \
             RETURN p.id AS id, 'a person named for its row' AS name ORDER BY id",
            &params! { "floor" => 0 },
        )
        .expect("query");
    assert_eq!(rows.rows.len(), NODES as usize);
    let sum = (0..i64::from(NODES)).sum();
    (rows, sum)
}

/// The baseline: the match a caller writes when there is no typed row.
fn by_hand(result: &QueryResult) -> f64 {
    let mut total = 0i64;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for row in &result.rows {
            match (&row[0], &row[1]) {
                (Value::Int(id), Value::Str(name)) => total += id + name.len() as i64,
                other => panic!("unexpected row {other:?}"),
            }
        }
    }
    let ns = start.elapsed().as_nanos() as f64;
    std::hint::black_box(total);
    ns
}

/// The same read as a tuple.
fn as_tuple(result: &QueryResult) -> f64 {
    let mut total = 0i64;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for row in result.iter() {
            let (id, name): (i64, &str) = row.get().expect("typed");
            total += id + name.len() as i64;
        }
    }
    let ns = start.elapsed().as_nanos() as f64;
    std::hint::black_box(total);
    ns
}

/// One string column by index, against the same column matched by hand,
/// which is where a copy would announce itself.
fn strings_by_hand(result: &QueryResult) -> f64 {
    let mut total = 0usize;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for row in &result.rows {
            match &row[1] {
                Value::Str(name) => total += name.len(),
                other => panic!("unexpected column {other:?}"),
            }
        }
    }
    let ns = start.elapsed().as_nanos() as f64;
    std::hint::black_box(total);
    ns
}

fn strings_typed(result: &QueryResult) -> f64 {
    let mut total = 0usize;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for row in result.iter() {
            total += row.get_at::<&str>(1).expect("a string").len();
        }
    }
    let ns = start.elapsed().as_nanos() as f64;
    std::hint::black_box(total);
    ns
}

/// The same column by name, the version that compares a string per row.
fn strings_by_name(result: &QueryResult) -> f64 {
    let mut total = 0usize;
    let start = Instant::now();
    for _ in 0..ROUNDS {
        for row in result.iter() {
            total += row.get_by_name::<&str>("name").expect("a string").len();
        }
    }
    let ns = start.elapsed().as_nanos() as f64;
    std::hint::black_box(total);
    ns
}

/// Nanoseconds per row of the fastest of five passes. A pass over four
/// million rows costs a few milliseconds here, which is short enough
/// that one descheduled pass doubles it, so the minimum is the number
/// and five passes is what makes the minimum a real one.
fn best(f: impl Fn(&QueryResult) -> f64, result: &QueryResult) -> f64 {
    let rows = f64::from(NODES) * f64::from(ROUNDS);
    (0..5).map(|_| f(result)).fold(f64::MAX, f64::min) / rows
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rows.zu1");
    let (result, sum) = build(&path);
    let checked: i64 = result
        .iter()
        .map(|row| row.get_at::<i64>(0).expect("an id"))
        .sum();
    assert_eq!(checked, sum, "the typed read must agree with the graph");
    println!("rows bench: {} rows of two columns", result.rows.len());

    let hand_ns = best(by_hand, &result);
    let tuple_ns = best(as_tuple, &result);
    let str_hand_ns = best(strings_by_hand, &result);
    let str_typed_ns = best(strings_typed, &result);
    let by_name_ns = best(strings_by_name, &result);
    let tuple_x = tuple_ns / hand_ns.max(0.0001);
    let str_x = str_typed_ns / str_hand_ns.max(0.0001);
    let name_x = by_name_ns / str_typed_ns.max(0.0001);

    println!("two columns matched by hand: {hand_ns:.2} ns/row");
    println!("two columns as a tuple:      {tuple_ns:.2} ns/row, {tuple_x:.3}x");
    println!("one string matched by hand:  {str_hand_ns:.2} ns/row");
    println!("one string typed by index:   {str_typed_ns:.2} ns/row, {str_x:.3}x");
    println!("one string typed by name:    {by_name_ns:.2} ns/row, {name_x:.3}x the index read");

    let mut failed = false;
    if let Some(ceiling) = budget("typed_row_x")
        && tuple_x > ceiling
    {
        println!("GATE FAIL typed row: {tuple_x:.3}x > ceiling {ceiling}");
        failed = true;
    }
    if let Some(ceiling) = budget("typed_str_x")
        && str_x > ceiling
    {
        println!("GATE FAIL typed string: {str_x:.3}x > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
