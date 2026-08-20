//! What the chunked read path is for, measured against the whole-column
//! one it sits beside (dx/02 §3, zu#167).
//!
//! Five numbers. scan drives a whole column both ways and sums it, so
//! the chunked loop pays for the same conversions plus a call and a
//! bounds check per chunk; the ratio is what that overhead costs, and
//! it is a ceiling because the chunked path is meant to be the default
//! for large results rather than a slower alternative to them. early
//! reads the first chunk and stops, which is the shape of a host
//! streaming into its own arrays or a user who pressed a key: the
//! whole-column call converts every row before returning any, so the
//! speedup is the number of chunks in the result, give or take the
//! megabytes the whole-column path also has to fault in, and it is a
//! floor. cell reads the same column one value at a time through the
//! cell reader, which is the path the values with no column take, and
//! the ratio is what a binding would pay for taking it on a column that
//! has one; it is a ceiling, and it is also where a cell that started
//! copying instead of borrowing would show up. buffer is not timed at
//! all, because the point it makes is
//! about memory: the conversion the whole-column path keeps alive until
//! the result is freed grows with the result, and the chunked one does
//! not.
//!
//! Everything goes through the extern "C" surface, because a benchmark
//! of the Rust behind it would not measure the thing a binding pays.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-capi --bench result

use std::ffi::c_char;
use std::ptr;
use std::time::Instant;

use zu::{
    ZuConn, ZuResult, ZuStatus, ZuValue, zu_conn_close, zu_open, zu_query, zu_result_cell,
    zu_result_chunk, zu_result_chunk_col_i64, zu_result_chunk_count, zu_result_col_i64,
    zu_result_free, zu_result_rows, zu_value_i64,
};

/// People in the graph, and so rows in the column every measurement
/// reads. Large enough that the whole-column conversion is the cost
/// rather than the noise, small enough that materializing it several
/// times over does not put the machine under memory pressure.
const NODES: u32 = 500_000;
/// Passes over each measurement, so a scheduler hiccup shows up as a
/// spread rather than as the answer.
const REPS: usize = 5;

/// Ordered, so the row at a given offset is the same row on every run
/// and every machine, and the crosschecks below can be exact rather
/// than merely order-independent. The sort is outside every
/// measurement.
const QUERY: &str = "MATCH (a:person) RETURN a.id AS id ORDER BY id";

/// The same column with nothing above the projection, which is the plan
/// the sink fills columns for. The rows come back in whatever order the
/// scan reached them, which is why the crosschecks on this one are sums
/// rather than positions.
const SCAN: &str = "MATCH (a:person) RETURN a.id AS id";

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

/// A result, materialized fresh, so a measurement of a conversion is
/// never a measurement of one that already happened.
unsafe fn run(conn: *mut ZuConn, text: &str) -> *mut ZuResult {
    let mut result: *mut ZuResult = ptr::null_mut();
    let status = unsafe {
        zu_query(
            conn,
            text.as_ptr().cast::<c_char>(),
            text.len(),
            &mut result,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "query");
    assert_eq!(unsafe { zu_result_rows(result) }, u64::from(NODES));
    result
}

/// The sorted column, which is what every measurement but lent reads.
unsafe fn fresh(conn: *mut ZuConn) -> *mut ZuResult {
    unsafe { run(conn, QUERY) }
}

/// The same column off the plan that fills columns.
unsafe fn scanned(conn: *mut ZuConn) -> *mut ZuResult {
    unsafe { run(conn, SCAN) }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("result.zu1");
    let t = Instant::now();
    {
        let mut db = zudb::zu1::file::Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i * 7 + 3) % NODES)).collect();
        edges.sort_unstable();
        edges.dedup();
        zudb::zu1::graph::bulk_load_as(&mut db, "person", "follows", u64::from(NODES), &edges)
            .expect("bulk load");
    }
    println!("load: {NODES} people, {:.1}s", t.elapsed().as_secs_f64());

    let path_str = path.to_str().expect("utf-8 path");
    let mut conn: *mut ZuConn = ptr::null_mut();
    let status = unsafe {
        zu_open(
            path_str.as_ptr().cast::<c_char>(),
            path_str.len(),
            &mut conn,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "open");

    // The chunk width the library chose, asked for rather than assumed,
    // since it is not a number the header promises.
    let chunk_rows = unsafe {
        let result = fresh(conn);
        let mut rows = 0u64;
        assert_eq!(
            zu_result_chunk(result, 0, ptr::null_mut(), &mut rows),
            ZuStatus::Ok
        );
        let chunks = zu_result_chunk_count(result);
        assert_eq!(chunks, u64::from(NODES).div_ceil(rows), "chunk count");
        zu_result_free(result);
        rows
    };
    println!("shape: {NODES} rows, {chunk_rows} rows per chunk");

    // The reference every measurement is checked against before any
    // number prints: the ids are 0 through NODES-1 in some order.
    let reference: u64 = (0..u64::from(NODES)).sum();

    // ---- scan: the whole column, both ways ----
    let mut whole_scan = f64::MAX;
    let mut chunk_scan = f64::MAX;
    for _ in 0..REPS {
        unsafe {
            let result = fresh(conn);
            let t = Instant::now();
            let mut out: *const i64 = ptr::null();
            assert_eq!(zu_result_col_i64(result, 0, &mut out), ZuStatus::Ok);
            let mut sum = 0u64;
            for &v in std::slice::from_raw_parts(out, NODES as usize) {
                sum += v as u64;
            }
            whole_scan = whole_scan.min(t.elapsed().as_secs_f64());
            assert_eq!(sum, reference, "whole column sum");
            zu_result_free(result);

            let result = fresh(conn);
            let t = Instant::now();
            let mut sum = 0u64;
            for chunk in 0..zu_result_chunk_count(result) {
                let mut rows = 0u64;
                assert_eq!(
                    zu_result_chunk(result, chunk, ptr::null_mut(), &mut rows),
                    ZuStatus::Ok
                );
                let mut out: *const i64 = ptr::null();
                assert_eq!(
                    zu_result_chunk_col_i64(result, chunk, 0, &mut out),
                    ZuStatus::Ok
                );
                for &v in std::slice::from_raw_parts(out, rows as usize) {
                    sum += v as u64;
                }
            }
            chunk_scan = chunk_scan.min(t.elapsed().as_secs_f64());
            assert_eq!(sum, reference, "chunked sum");
            zu_result_free(result);
        }
    }
    let scan_ratio = chunk_scan / whole_scan;
    println!(
        "scan: whole {:.1} ms, chunked {:.1} ms, ratio {scan_ratio:.2}x",
        whole_scan * 1e3,
        chunk_scan * 1e3
    );

    // ---- early: the first chunk and nothing else ----
    let head = chunk_rows as usize;
    let head_reference: u64 = (0..head as u64).sum();
    let mut whole_early = f64::MAX;
    let mut chunk_early = f64::MAX;
    for _ in 0..REPS {
        unsafe {
            let result = fresh(conn);
            let t = Instant::now();
            let mut out: *const i64 = ptr::null();
            assert_eq!(zu_result_col_i64(result, 0, &mut out), ZuStatus::Ok);
            let mut sum = 0u64;
            for &v in std::slice::from_raw_parts(out, head) {
                sum += v as u64;
            }
            whole_early = whole_early.min(t.elapsed().as_secs_f64());
            assert_eq!(sum, head_reference, "whole column head");
            zu_result_free(result);

            let result = fresh(conn);
            let t = Instant::now();
            let mut out: *const i64 = ptr::null();
            assert_eq!(
                zu_result_chunk_col_i64(result, 0, 0, &mut out),
                ZuStatus::Ok
            );
            let mut sum = 0u64;
            for &v in std::slice::from_raw_parts(out, head) {
                sum += v as u64;
            }
            chunk_early = chunk_early.min(t.elapsed().as_secs_f64());
            assert_eq!(sum, head_reference, "chunked head");
            zu_result_free(result);
        }
    }
    let early_speedup = whole_early / chunk_early;
    println!(
        "early: whole {:.3} ms, chunked {:.3} ms, speedup {early_speedup:.0}x for the first {head} rows",
        whole_early * 1e3,
        chunk_early * 1e3
    );

    // ---- cell: the same column one value at a time ----
    //
    // The cell reader exists for the values a column cannot hold, and a
    // binding that reached for it on an int column instead would be
    // taking the wrong path. This measures what that costs, on the one
    // column both paths can read, so the number is a like for like
    // comparison rather than two different questions.
    //
    // It is also the check that a cell allocates nothing. Every value
    // is a borrow of the result's own row, so 500000 of them are 500000
    // pointers and no memory beyond the rows; a copy per cell would
    // show up here as a ratio that keeps climbing with the row count.
    let mut cell_scan = f64::MAX;
    for _ in 0..REPS {
        unsafe {
            let result = fresh(conn);
            let t = Instant::now();
            let mut sum = 0u64;
            for row in 0..u64::from(NODES) {
                let mut value: *const ZuValue = ptr::null();
                assert_eq!(zu_result_cell(result, row, 0, &mut value), ZuStatus::Ok);
                let mut v = 0i64;
                assert_eq!(zu_value_i64(value, &mut v), ZuStatus::Ok);
                sum += v as u64;
            }
            cell_scan = cell_scan.min(t.elapsed().as_secs_f64());
            assert_eq!(sum, reference, "cell by cell sum");
            zu_result_free(result);
        }
    }
    let cell_ratio = cell_scan / whole_scan;
    println!(
        "cell: whole {:.1} ms, cell by cell {:.1} ms, ratio {cell_ratio:.1}x, {:.0} ns per value",
        whole_scan * 1e3,
        cell_scan * 1e3,
        cell_scan * 1e9 / f64::from(NODES)
    );

    // ---- lent: the accessor call itself, on both plans ----
    //
    // The sum is outside the measurement here, because the question is
    // not what reading a column costs but what asking for one does. On
    // a result the executor filled, the answer is a bounds check and a
    // pointer. On a result it built, it is a walk over every row and a
    // copy of every cell, which is what this path was before there was
    // anything to lend.
    let mut lent = f64::MAX;
    let mut copied = f64::MAX;
    for _ in 0..REPS {
        unsafe {
            let result = scanned(conn);
            let mut out: *const i64 = ptr::null();
            let t = Instant::now();
            assert_eq!(zu_result_col_i64(result, 0, &mut out), ZuStatus::Ok);
            lent = lent.min(t.elapsed().as_secs_f64());
            let mut sum = 0u64;
            for &v in std::slice::from_raw_parts(out, NODES as usize) {
                sum += v as u64;
            }
            assert_eq!(sum, reference, "lent column sum");
            zu_result_free(result);

            let result = fresh(conn);
            let mut out: *const i64 = ptr::null();
            let t = Instant::now();
            assert_eq!(zu_result_col_i64(result, 0, &mut out), ZuStatus::Ok);
            copied = copied.min(t.elapsed().as_secs_f64());
            let mut sum = 0u64;
            for &v in std::slice::from_raw_parts(out, NODES as usize) {
                sum += v as u64;
            }
            assert_eq!(sum, reference, "built column sum");
            zu_result_free(result);
        }
    }
    let lent_speedup = copied / lent;
    println!(
        "lent: filled {:.4} ms, built {:.1} ms, speedup {lent_speedup:.0}x for the call itself",
        lent * 1e3,
        copied * 1e3
    );

    // ---- buffer: what each path keeps alive, untimed ----
    let whole_bytes = NODES as usize * size_of::<i64>();
    let chunk_bytes = head * size_of::<i64>();
    println!(
        "buffer: whole {} KiB, chunked {} KiB, {}x smaller and flat in the row count",
        whole_bytes / 1024,
        chunk_bytes / 1024,
        whole_bytes / chunk_bytes
    );

    unsafe { zu_conn_close(conn) };

    if gate {
        // A ceiling: the per-chunk call and bounds check are what the
        // chunked loop adds, and they are meant to disappear next to
        // the conversion. Lower this ceiling, never raise it.
        if let Some(max) = budget("capi_chunk_scan_ratio") {
            if scan_ratio > max {
                println!("GATE FAIL: scan ratio {scan_ratio:.2}x over ceiling {max}");
                failed = true;
            } else {
                println!("gate: scan ratio {scan_ratio:.2}x within {max}");
            }
        }
        // A floor: reading a chunk out of a result has to cost a chunk
        // and not a result. Raise this floor, never lower it.
        if let Some(min) = budget("capi_chunk_early_speedup") {
            if early_speedup < min {
                println!("GATE FAIL: early speedup {early_speedup:.0}x under floor {min}");
                failed = true;
            } else {
                println!("gate: early speedup {early_speedup:.0}x over {min}");
            }
        }
        // A floor: asking a filled result for a column has to cost a
        // pointer rather than a pass over it, and the way that stops
        // being true is a copy creeping back in. Raise this floor,
        // never lower it.
        if let Some(min) = budget("capi_lent_col_speedup") {
            if lent_speedup < min {
                println!("GATE FAIL: lent speedup {lent_speedup:.0}x under floor {min}");
                failed = true;
            } else {
                println!("gate: lent speedup {lent_speedup:.0}x over {min}");
            }
        }
        // A ceiling on the per-value path, which is what catches a cell
        // that starts copying what it is meant to borrow. Lower this
        // ceiling, never raise it.
        if let Some(max) = budget("capi_cell_scan_ratio") {
            if cell_ratio > max {
                println!("GATE FAIL: cell ratio {cell_ratio:.1}x over ceiling {max}");
                failed = true;
            } else {
                println!("gate: cell ratio {cell_ratio:.1}x within {max}");
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}
