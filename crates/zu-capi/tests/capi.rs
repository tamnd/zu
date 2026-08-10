//! Exercises libzu the way a C host does: open, query, prepare, bind,
//! execute in a loop, read columns out as buffers, and free in the
//! right order. Everything goes through the extern "C" functions and
//! raw pointers; nothing reaches into the Rust types behind them.

use std::ffi::{CStr, CString, c_char};

use zu::{
    ZU_TYPE_INT, ZU_TYPE_STR, zu_bind_i64, zu_close, zu_execute, zu_open, zu_prepare, zu_query,
    zu_result_cell_str, zu_result_cell_type, zu_result_col_i64, zu_result_col_name,
    zu_result_col_valid, zu_result_cols, zu_result_free, zu_result_rows, zu_stmt_close,
    zu_string_free, zu_version,
};

fn seeded(path: &std::path::Path) {
    let mut db = zudb::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    edges.sort_unstable();
    edges.dedup();
    zudb::zu1::graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
}

fn c(text: &str) -> CString {
    CString::new(text).expect("no NUL")
}

#[test]
fn a_c_host_can_open_query_prepare_and_read_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("capi.zu1");
    seeded(&path);

    unsafe {
        assert_eq!(CStr::from_ptr(zu_version()).to_str(), Ok("0.0.1"));

        let mut err: *mut c_char = std::ptr::null_mut();
        let cpath = c(path.to_str().expect("utf-8 path"));
        let session = zu_open(cpath.as_ptr(), &mut err);
        assert!(!session.is_null(), "open failed");

        // One-shot query, whole id column out in one call.
        let q = c("MATCH (a:person) RETURN a.id AS id ORDER BY id LIMIT 5");
        let result = zu_query(session, q.as_ptr(), &mut err);
        assert!(!result.is_null());
        assert_eq!(zu_result_rows(result), 5);
        assert_eq!(zu_result_cols(result), 1);
        assert_eq!(
            CStr::from_ptr(zu_result_col_name(result, 0)).to_str(),
            Ok("id")
        );
        assert_eq!(zu_result_cell_type(result, 0, 0), ZU_TYPE_INT);
        let ids = zu_result_col_i64(result, 0);
        assert!(!ids.is_null());
        assert_eq!(std::slice::from_raw_parts(ids, 5), [0, 1, 2, 3, 4]);
        let valid = zu_result_col_valid(result, 0);
        assert_eq!(std::slice::from_raw_parts(valid, 5), [1, 1, 1, 1, 1]);
        zu_result_free(result);

        // Prepare once, rebind and execute twice: the point-read loop.
        let q = c("MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n");
        let stmt = zu_prepare(session, q.as_ptr(), &mut err);
        assert!(!stmt.is_null());
        let name = c("src");
        for src in [3i64, 42] {
            zu_bind_i64(stmt, name.as_ptr(), src);
            let result = zu_execute(stmt, &mut err);
            assert!(!result.is_null(), "execute src {src}");
            assert_eq!(zu_result_rows(result), 1);
            let n = zu_result_col_i64(result, 0);
            assert!(!n.is_null());
            assert!(*n >= 1, "src {src} has followers in the seed");
            zu_result_free(result);
        }
        zu_stmt_close(stmt);

        // A string cell reads back through the cell accessor and the
        // i64 column accessor refuses the column instead of guessing.
        let q = c("MATCH (a:person {id: 3}) RETURN 'hi' AS s");
        let result = zu_query(session, q.as_ptr(), &mut err);
        assert!(!result.is_null());
        assert_eq!(zu_result_cell_type(result, 0, 0), ZU_TYPE_STR);
        let mut len = 0usize;
        let s = zu_result_cell_str(result, 0, 0, &mut len);
        assert!(!s.is_null());
        assert_eq!(CStr::from_ptr(s).to_str(), Ok("hi"));
        assert_eq!(len, 2);
        assert!(zu_result_col_i64(result, 0).is_null());
        zu_result_free(result);

        // Errors come back as messages, not crashes, and the session
        // keeps working afterwards.
        let q = c("THIS IS NOT A QUERY");
        let result = zu_query(session, q.as_ptr(), &mut err);
        assert!(result.is_null());
        assert!(!err.is_null());
        zu_string_free(err);
        err = std::ptr::null_mut();
        let q = c("MATCH (a:person) RETURN count(a) AS n");
        let result = zu_query(session, q.as_ptr(), &mut err);
        assert!(!result.is_null());
        assert_eq!(*zu_result_col_i64(result, 0), 97);
        zu_result_free(result);

        zu_close(session);
    }
}

#[test]
fn open_failure_reports_and_null_inputs_do_not_crash() {
    unsafe {
        let mut err: *mut c_char = std::ptr::null_mut();
        let missing = c("/nonexistent/nowhere.zu1");
        let session = zu_open(missing.as_ptr(), &mut err);
        assert!(session.is_null());
        assert!(!err.is_null());
        zu_string_free(err);

        let session = zu_open(std::ptr::null(), std::ptr::null_mut());
        assert!(session.is_null());
        zu_close(std::ptr::null_mut());
        zu_result_free(std::ptr::null_mut());
        zu_stmt_close(std::ptr::null_mut());
        assert_eq!(zu_result_rows(std::ptr::null()), 0);
        assert!(zu_result_col_i64(std::ptr::null_mut(), 0).is_null());
    }
}
