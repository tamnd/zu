//! What the C loader costs over the Rust path it forwards to
//! (dx/02 §3, zu#167).
//!
//! Two numbers. load builds the same table both ways, once through
//! `bulk_load_keyed` and `store_props` directly and once through the
//! extern "C" loader, and the ratio is the whole of what the boundary
//! adds: a copy per column, a UTF-8 check per string, and a call per
//! column. Both paths do identical work inside the engine, so anything
//! the ratio shows is the boundary and not the load. It is a ceiling.
//! rows is the absolute number, so a host sizing a load knows what it
//! is buying rather than only that it is close to something else.
//!
//! The copy is the thing under test. The loader owns the arrays it is
//! given, because a borrow would be a lifetime rule the header could
//! not state safely, and the question this answers is whether that
//! honesty is affordable. It is: the copy is a memcpy against a load
//! that writes every one of those bytes to disk.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-capi --bench loader

use std::ffi::c_char;
use std::ptr;
use std::time::Instant;

use zu::{
    ZU_TEMPORAL_DATE, ZuLoader, ZuStatus, zu_loader_col_f64, zu_loader_col_i64, zu_loader_col_str,
    zu_loader_col_temporal, zu_loader_create, zu_loader_edges, zu_loader_finish, zu_loader_free,
    zu_loader_table,
};

/// Rows in the table both paths build. Large enough that the per-row
/// work is the cost rather than the noise, small enough that a run
/// finishes while someone is watching it.
const ROWS: u64 = 200_000;
/// Passes, so a scheduler hiccup shows up as a spread and not as the
/// answer.
const REPS: usize = 3;

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

/// The data, built once and outside every measurement, because
/// generating it is not what either path is being asked to do.
struct Data {
    ages: Vec<i64>,
    scores: Vec<f64>,
    names: Vec<String>,
    days: Vec<i64>,
    from: Vec<u32>,
    to: Vec<u32>,
}

fn data() -> Data {
    let rows = ROWS as usize;
    let mut edges: Vec<(u32, u32)> = (0..ROWS as u32)
        .map(|i| (i, (i * 7 + 3) % ROWS as u32))
        .collect();
    edges.sort_unstable();
    edges.dedup();
    Data {
        ages: (0..rows as i64).collect(),
        scores: (0..rows).map(|i| i as f64 * 1.5).collect(),
        // Long enough that a copy is a copy rather than a rounding
        // error, and varying in length so the per-row work is not one
        // branch predicted away.
        names: (0..rows).map(|i| format!("person number {i}")).collect(),
        days: (0..rows as i64).map(|i| i % 30_000).collect(),
        from: edges.iter().map(|e| e.0).collect(),
        to: edges.iter().map(|e| e.1).collect(),
    }
}

/// The load as the Rust side does it: the engine calls, with the
/// borrows the property store wants built the way the corpus loader
/// builds them.
fn native(path: &std::path::Path, d: &Data) {
    let mut db = zudb::zu1::file::Zu1File::create(path).expect("create");
    let mut pairs: Vec<(u32, u32)> = d
        .from
        .iter()
        .copied()
        .zip(d.to.iter().copied())
        .collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs.dedup();
    zudb::zu1::graph::bulk_load_keyed(&mut db, "person", "knows", ROWS, &pairs, None)
        .expect("bulk load");
    let ages: Vec<u64> = d.ages.iter().map(|v| *v as u64).collect();
    let names: Vec<&[u8]> = d.names.iter().map(|s| s.as_bytes()).collect();
    let days: Vec<i32> = d.days.iter().map(|v| *v as i32).collect();
    zudb::zu1::props::store_props(
        &mut db,
        "person",
        &[
            ("age", zudb::zu1::props::PropValues::Int(&ages)),
            ("score", zudb::zu1::props::PropValues::Float(&d.scores)),
            ("name", zudb::zu1::props::PropValues::Str(&names)),
            ("born", zudb::zu1::props::PropValues::Date(&days)),
        ],
    )
    .expect("store");
}

/// The same load through the boundary, one call per column.
fn capi(path: &std::path::Path, d: &Data) {
    let path = path.to_str().expect("utf-8 path");
    let mut l: *mut ZuLoader = ptr::null_mut();
    unsafe {
        assert_eq!(
            zu_loader_create(
                path.as_ptr().cast::<c_char>(),
                path.len(),
                &mut l,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "create"
        );
        assert_eq!(
            zu_loader_table(
                l,
                "person".as_ptr().cast::<c_char>(),
                6,
                "knows".as_ptr().cast::<c_char>(),
                5,
                ROWS,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "table"
        );
        assert_eq!(
            zu_loader_col_i64(
                l,
                "age".as_ptr().cast::<c_char>(),
                3,
                d.ages.as_ptr(),
                ROWS,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "age"
        );
        assert_eq!(
            zu_loader_col_f64(
                l,
                "score".as_ptr().cast::<c_char>(),
                5,
                d.scores.as_ptr(),
                ROWS,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "score"
        );
        // The pointer and length arrays a C host would already have,
        // built here because a Rust `String` is not one. Outside the
        // measurement for the same reason the data is.
        let ptrs: Vec<*const c_char> = d
            .names
            .iter()
            .map(|s| s.as_ptr().cast::<c_char>())
            .collect();
        let lens: Vec<usize> = d.names.iter().map(String::len).collect();
        assert_eq!(
            zu_loader_col_str(
                l,
                "name".as_ptr().cast::<c_char>(),
                4,
                ptrs.as_ptr(),
                lens.as_ptr(),
                ROWS,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "name"
        );
        assert_eq!(
            zu_loader_col_temporal(
                l,
                "born".as_ptr().cast::<c_char>(),
                4,
                ZU_TEMPORAL_DATE,
                d.days.as_ptr(),
                ROWS,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "born"
        );
        assert_eq!(
            zu_loader_edges(
                l,
                d.from.as_ptr(),
                d.to.as_ptr(),
                d.from.len() as u64,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "edges"
        );
        assert_eq!(zu_loader_finish(l, ptr::null_mut()), ZuStatus::Ok, "finish");
        zu_loader_free(l);
    }
}

/// Both databases answer the same question, so a path that was fast
/// because it wrote less would be caught here rather than praised.
fn readback(path: &std::path::Path) -> (u64, i64) {
    let db = zudb::Database::open(path).expect("open");
    let mut conn = db.connect().expect("connect");
    let result = conn
        .query("MATCH (p:person) RETURN sum(p.age) AS s, count(*) AS n")
        .expect("query");
    let row = &result.rows[0];
    let sum = match &row[0] {
        zudb::query::Value::Int(v) => *v,
        other => panic!("sum is {other:?}"),
    };
    let count = match &row[1] {
        zudb::query::Value::Int(v) => *v as u64,
        other => panic!("count is {other:?}"),
    };
    (count, sum)
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    let dir = tempfile::tempdir().expect("tempdir");
    let t = Instant::now();
    let d = data();
    println!(
        "data: {ROWS} rows, {} edges, {:.1}s to build",
        d.from.len(),
        t.elapsed().as_secs_f64()
    );

    // The reference every run is checked against: the ages are 0
    // through ROWS-1, whichever path put them there.
    let reference = (0..ROWS as i64).sum::<i64>();

    let mut native_load = f64::MAX;
    let mut capi_load = f64::MAX;
    for rep in 0..REPS {
        let path = dir.path().join(format!("native-{rep}.zu1"));
        let t = Instant::now();
        native(&path, &d);
        native_load = native_load.min(t.elapsed().as_secs_f64());
        assert_eq!(readback(&path), (ROWS, reference), "native readback");
        std::fs::remove_file(&path).expect("remove");

        let path = dir.path().join(format!("capi-{rep}.zu1"));
        let t = Instant::now();
        capi(&path, &d);
        capi_load = capi_load.min(t.elapsed().as_secs_f64());
        assert_eq!(readback(&path), (ROWS, reference), "capi readback");
        std::fs::remove_file(&path).expect("remove");
    }

    let ratio = capi_load / native_load;
    println!(
        "load: rust {:.0} ms, c abi {:.0} ms, ratio {ratio:.2}x",
        native_load * 1e3,
        capi_load * 1e3
    );
    println!(
        "rows: {:.1} M rows/s through the boundary, {:.1} M rows/s under it",
        ROWS as f64 / capi_load / 1e6,
        ROWS as f64 / native_load / 1e6
    );

    if gate {
        // A ceiling: the boundary is a copy per column and a call per
        // column, and both are meant to disappear next to the write.
        // Lower this ceiling, never raise it.
        if let Some(max) = budget("capi_load_ratio") {
            if ratio > max {
                println!("GATE FAIL: load ratio {ratio:.2}x over ceiling {max}");
                failed = true;
            } else {
                println!("gate: load ratio {ratio:.2}x within {max}");
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}
