//! What the chunked read path is for, measured against the whole-column
//! one it sits beside (dx/02 §3, zu#167).
//!
//! Three numbers. scan drives a whole column both ways and sums it, so
//! the chunked loop pays for the same conversions plus a call and a
//! bounds check per chunk; the ratio is what that overhead costs, and
//! it is a ceiling because the chunked path is meant to be the default
//! for large results rather than a slower alternative to them. early
//! reads the first chunk and stops, which is the shape of a host
//! streaming into its own arrays or a user who pressed a key: the
//! whole-column call converts every row before returning any, so the
//! speedup is the number of chunks in the result, give or take the
//! megabytes the whole-column path also has to fault in, and it is a
//! floor. buffer is not timed at all, because the point it makes is
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
    ZuConn, ZuResult, ZuStatus, zu_conn_close, zu_open, zu_query, zu_result_chunk,
    zu_result_chunk_col_i64, zu_result_chunk_count, zu_result_col_i64, zu_result_free,
    zu_result_rows,
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

/// The column, materialized fresh, so a measurement of a conversion is
/// never a measurement of one that already happened.
unsafe fn fresh(conn: *mut ZuConn) -> *mut ZuResult {
    let mut result: *mut ZuResult = ptr::null_mut();
    let status = unsafe {
        zu_query(
            conn,
            QUERY.as_ptr().cast::<c_char>(),
            QUERY.len(),
            &mut result,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "query");
    assert_eq!(unsafe { zu_result_rows(result) }, u64::from(NODES));
    result
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
    }

    if failed {
        std::process::exit(1);
    }
}
