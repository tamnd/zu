//! Exercises libzu the way a C host does: open a database, connect,
//! query, prepare, bind, execute in a loop, read columns out as
//! buffers, and free in the right order. Everything goes through the
//! extern "C" functions and raw pointers; nothing reaches into the Rust
//! types behind them.

use std::ffi::{CStr, CString, c_char};
use std::ptr;

use zu::{
    ZU_SEVERITY_EXCEPTION, ZU_TEMPORAL_DATE, ZU_TEMPORAL_DURATION_DAY_TIME,
    ZU_TEMPORAL_DURATION_YEAR_MONTH, ZU_TEMPORAL_LOCAL_DATETIME, ZU_TEMPORAL_LOCAL_TIME,
    ZU_TEMPORAL_ZONED_DATETIME, ZU_TEMPORAL_ZONED_TIME, ZU_TYPE_INT, ZU_TYPE_LIST, ZU_TYPE_NODE,
    ZU_TYPE_NULL, ZU_TYPE_STR, ZU_TYPE_TEMPORAL, ZuConfig, ZuConn, ZuDatabase, ZuError, ZuLoader,
    ZuResult, ZuStatus, ZuStmt, ZuValue, zu_bind_i64, zu_bind_i64_z, zu_bind_str_z, zu_config_init,
    zu_config_set_z, zu_conn_close, zu_connect, zu_create, zu_create_z, zu_database_close,
    zu_database_create_z, zu_database_open_z, zu_database_path, zu_error_code, zu_error_free,
    zu_error_message, zu_error_position, zu_error_severity, zu_error_status, zu_execute,
    zu_loader_col_bool, zu_loader_col_f64, zu_loader_col_i64, zu_loader_col_str,
    zu_loader_col_temporal, zu_loader_create, zu_loader_edges, zu_loader_finish, zu_loader_free,
    zu_loader_table, zu_loader_table_z, zu_open, zu_open_z, zu_prepare, zu_prepare_z, zu_query,
    zu_query_z, zu_result_cell, zu_result_cell_str, zu_result_cell_type, zu_result_chunk,
    zu_result_chunk_col_f64, zu_result_chunk_col_i64, zu_result_chunk_col_node_offset,
    zu_result_chunk_col_valid, zu_result_chunk_count, zu_result_col_f64, zu_result_col_i64,
    zu_result_col_name, zu_result_col_node_offset, zu_result_col_valid, zu_result_cols,
    zu_result_free, zu_result_rows, zu_stmt_close, zu_value_at, zu_value_bool, zu_value_f64,
    zu_value_i64, zu_value_len, zu_value_node, zu_value_str, zu_value_temporal, zu_value_type,
    zu_version,
};

fn seeded(path: &std::path::Path) {
    let mut db = zudb::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    edges.sort_unstable();
    edges.dedup();
    zudb::zu1::graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
}

/// The same graph with enough people to fill several chunks, which the
/// 97 of [`seeded`] deliberately do not.
fn seeded_wide(path: &std::path::Path, nodes: u32) {
    let mut db = zudb::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..nodes).map(|i| (i, (i * 7 + 3) % nodes)).collect();
    edges.sort_unstable();
    edges.dedup();
    zudb::zu1::graph::bulk_load_as(&mut db, "person", "follows", u64::from(nodes), &edges)
        .expect("load");
}

fn c(text: &str) -> CString {
    CString::new(text).expect("no NUL")
}

/// Opens on the counted form, which is the one every binding uses.
unsafe fn open(path: &std::path::Path) -> *mut ZuConn {
    let path = path.to_str().expect("utf-8 path");
    let mut conn: *mut ZuConn = ptr::null_mut();
    let status = unsafe {
        zu_open(
            path.as_ptr().cast::<c_char>(),
            path.len(),
            &mut conn,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "open {path}");
    assert!(!conn.is_null());
    conn
}

/// Runs a statement that is expected to succeed, reusing one error
/// slot so that a success leaving a stale error behind would show up.
unsafe fn query(conn: *mut ZuConn, text: &str, err: &mut *mut ZuError) -> *mut ZuResult {
    let mut result: *mut ZuResult = ptr::null_mut();
    let status = unsafe {
        zu_query(
            conn,
            text.as_ptr().cast::<c_char>(),
            text.len(),
            &mut result,
            err,
        )
    };
    assert_eq!(status, ZuStatus::Ok, "{text}");
    assert!(err.is_null(), "a success left an error behind: {text}");
    assert!(!result.is_null());
    result
}

unsafe fn col_i64<'a>(result: *mut ZuResult, col: u32, rows: usize) -> &'a [i64] {
    let mut out: *const i64 = ptr::null();
    assert_eq!(
        unsafe { zu_result_col_i64(result, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

unsafe fn col_name<'a>(result: *mut ZuResult, col: u32) -> &'a str {
    let mut out: *const c_char = ptr::null();
    let mut len = 0usize;
    assert_eq!(
        unsafe { zu_result_col_name(result, col, &mut out, &mut len) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    let s = unsafe { CStr::from_ptr(out) }.to_str().expect("utf-8");
    assert_eq!(len, s.len(), "the length and the NUL have to agree");
    s
}

/// One cell, borrowed from the result. Nothing to free, so the tests
/// below hold as many at once as they like.
unsafe fn cell(result: *mut ZuResult, row: u64, col: u32) -> *const ZuValue {
    let mut out: *const ZuValue = ptr::null();
    assert_eq!(
        unsafe { zu_result_cell(result, row, col, &mut out) },
        ZuStatus::Ok,
        "row {row} column {col}"
    );
    assert!(!out.is_null());
    out
}

unsafe fn at(v: *const ZuValue, i: u64) -> *const ZuValue {
    let mut out: *const ZuValue = ptr::null();
    assert_eq!(
        unsafe { zu_value_at(v, i, &mut out) },
        ZuStatus::Ok,
        "at {i}"
    );
    assert!(!out.is_null());
    out
}

unsafe fn value_i64(v: *const ZuValue) -> i64 {
    let mut out = i64::MIN;
    assert_eq!(unsafe { zu_value_i64(v, &mut out) }, ZuStatus::Ok);
    out
}

/// A cell's string, which is a borrow of the result's own bytes and is
/// not NUL-terminated, so the length is the whole of the answer.
unsafe fn value_str<'a>(v: *const ZuValue) -> &'a str {
    let mut out: *const c_char = ptr::null();
    let mut len = usize::MAX;
    assert_eq!(unsafe { zu_value_str(v, &mut out, &mut len) }, ZuStatus::Ok);
    assert!(!out.is_null());
    let bytes = unsafe { std::slice::from_raw_parts(out.cast::<u8>(), len) };
    std::str::from_utf8(bytes).expect("utf-8")
}

unsafe fn cell_type(result: *mut ZuResult, row: u64, col: u32) -> i32 {
    let mut out = -1i32;
    assert_eq!(
        unsafe { zu_result_cell_type(result, row, col, &mut out) },
        ZuStatus::Ok
    );
    out
}

unsafe fn col_node_offset<'a>(result: *mut ZuResult, col: u32, rows: usize) -> &'a [u64] {
    let mut out: *const u64 = ptr::null();
    assert_eq!(
        unsafe { zu_result_col_node_offset(result, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

unsafe fn col_valid<'a>(result: *mut ZuResult, col: u32, rows: usize) -> &'a [u8] {
    let mut out: *const u8 = ptr::null();
    assert_eq!(
        unsafe { zu_result_col_valid(result, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

/// Where a chunk starts and how many rows it holds.
unsafe fn chunk_span(result: *mut ZuResult, chunk: u64) -> (u64, u64) {
    let mut offset = u64::MAX;
    let mut rows = u64::MAX;
    assert_eq!(
        unsafe { zu_result_chunk(result, chunk, &mut offset, &mut rows) },
        ZuStatus::Ok,
        "chunk {chunk}"
    );
    (offset, rows)
}

unsafe fn chunk_i64<'a>(result: *mut ZuResult, chunk: u64, col: u32, rows: usize) -> &'a [i64] {
    let mut out: *const i64 = ptr::null();
    assert_eq!(
        unsafe { zu_result_chunk_col_i64(result, chunk, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

unsafe fn chunk_node_offset<'a>(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    rows: usize,
) -> &'a [u64] {
    let mut out: *const u64 = ptr::null();
    assert_eq!(
        unsafe { zu_result_chunk_col_node_offset(result, chunk, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

unsafe fn chunk_valid<'a>(result: *mut ZuResult, chunk: u64, col: u32, rows: usize) -> &'a [u8] {
    let mut out: *const u8 = ptr::null();
    assert_eq!(
        unsafe { zu_result_chunk_col_valid(result, chunk, col, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    unsafe { std::slice::from_raw_parts(out, rows) }
}

#[test]
fn a_c_host_can_open_query_prepare_and_read_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("capi.zu1");
    seeded(&path);

    unsafe {
        assert_eq!(CStr::from_ptr(zu_version()).to_str(), Ok("0.0.1"));

        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // One-shot query, whole id column out in one call.
        let result = query(
            conn,
            "MATCH (a:person) RETURN a.id AS id ORDER BY id LIMIT 5",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 5);
        assert_eq!(zu_result_cols(result), 1);
        assert_eq!(col_name(result, 0), "id");
        assert_eq!(cell_type(result, 0, 0), ZU_TYPE_INT);
        assert_eq!(col_i64(result, 0, 5), [0, 1, 2, 3, 4]);
        let mut valid: *const u8 = ptr::null();
        assert_eq!(zu_result_col_valid(result, 0, &mut valid), ZuStatus::Ok);
        assert_eq!(std::slice::from_raw_parts(valid, 5), [1, 1, 1, 1, 1]);
        // An int column widens for a caller that wants doubles.
        let mut floats: *const f64 = ptr::null();
        assert_eq!(zu_result_col_f64(result, 0, &mut floats), ZuStatus::Ok);
        assert_eq!(
            std::slice::from_raw_parts(floats, 5),
            [0.0, 1.0, 2.0, 3.0, 4.0]
        );
        zu_result_free(result);

        // Prepare once, rebind and execute twice: the point-read loop.
        let q = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare(
                conn,
                q.as_ptr().cast::<c_char>(),
                q.len(),
                &mut stmt,
                &mut err
            ),
            ZuStatus::Ok
        );
        assert!(!stmt.is_null());
        let name = "src";
        for src in [3i64, 42] {
            assert_eq!(
                zu_bind_i64(stmt, name.as_ptr().cast::<c_char>(), name.len(), src),
                ZuStatus::Ok
            );
            let mut result: *mut ZuResult = ptr::null_mut();
            assert_eq!(
                zu_execute(stmt, &mut result, &mut err),
                ZuStatus::Ok,
                "src {src}"
            );
            assert_eq!(zu_result_rows(result), 1);
            assert!(
                col_i64(result, 0, 1)[0] >= 1,
                "src {src} has followers in the seed"
            );
            zu_result_free(result);
        }
        zu_stmt_close(stmt);

        // A string cell reads back through the cell accessor, and the
        // i64 column accessor refuses the column instead of guessing.
        let result = query(conn, "MATCH (a:person {id: 3}) RETURN 'hi' AS s", &mut err);
        assert_eq!(cell_type(result, 0, 0), ZU_TYPE_STR);
        let mut len = 0usize;
        let mut s: *const c_char = ptr::null();
        assert_eq!(
            zu_result_cell_str(result, 0, 0, &mut s, &mut len),
            ZuStatus::Ok
        );
        assert!(!s.is_null());
        assert_eq!(CStr::from_ptr(s).to_str(), Ok("hi"));
        assert_eq!(len, 2);
        // Asking twice hands back the same copy rather than making a
        // second one, so a caller walking a string column does not pay
        // per pass and a pointer taken on the first pass stays good.
        let mut again: *const c_char = ptr::null();
        assert_eq!(
            zu_result_cell_str(result, 0, 0, &mut again, ptr::null_mut()),
            ZuStatus::Ok
        );
        assert_eq!(again, s);

        // Out of range writes -1 rather than a tag that reads like an
        // answer.
        let mut ty = 0i32;
        assert_eq!(
            zu_result_cell_type(result, 99, 0, &mut ty),
            ZuStatus::Misuse
        );
        assert_eq!(ty, -1);

        let mut ints: *const i64 = ptr::null();
        assert_eq!(zu_result_col_i64(result, 0, &mut ints), ZuStatus::Misuse);
        assert!(ints.is_null(), "a refused accessor still wrote a pointer");
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// A node is its own type across the boundary, read as the offset that
/// identifies it and refused by the accessors that read numbers.
#[test]
fn a_node_column_reads_as_offsets_and_not_as_integers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nodes.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person) RETURN a AS n ORDER BY a.id LIMIT 4",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 4);
        assert_eq!(cell_type(result, 0, 0), ZU_TYPE_NODE);

        let mut offsets: *const u64 = ptr::null();
        assert_eq!(
            zu_result_col_node_offset(result, 0, &mut offsets),
            ZuStatus::Ok
        );
        assert_eq!(std::slice::from_raw_parts(offsets, 4), [0, 1, 2, 3]);

        // The v0 surface handed these back through the i64 accessor,
        // which is how an internal row number reaches a user who asked
        // for an identity. It is misuse now.
        let mut ints: *const i64 = ptr::null();
        assert_eq!(zu_result_col_i64(result, 0, &mut ints), ZuStatus::Misuse);
        assert!(ints.is_null());

        // And the offset accessor is just as strict the other way.
        zu_result_free(result);
        let result = query(conn, "MATCH (a:person {id: 3}) RETURN a.id AS id", &mut err);
        let mut offsets: *const u64 = ptr::null();
        assert_eq!(
            zu_result_col_node_offset(result, 0, &mut offsets),
            ZuStatus::Misuse
        );
        assert!(offsets.is_null());
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// The case a returned pointer could not express: the call worked and
/// there is nothing to read.
#[test]
fn an_empty_result_is_done_rather_than_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person {id: 999999}) RETURN a.id AS id",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 0);
        assert_eq!(
            zu_result_cols(result),
            1,
            "an empty result still has its shape"
        );

        let mut ints: *const i64 = ptr::null();
        assert_eq!(zu_result_col_i64(result, 0, &mut ints), ZuStatus::Done);
        assert!(ints.is_null(), "there is nothing to point at");
        let mut valid: *const u8 = ptr::null();
        assert_eq!(zu_result_col_valid(result, 0, &mut valid), ZuStatus::Done);

        // Out of range is still misuse, empty or not.
        assert_eq!(zu_result_col_i64(result, 7, &mut ints), ZuStatus::Misuse);
        zu_result_free(result);
        zu_conn_close(conn);
    }
}

/// The fields a binding needs off an error, as fields.
#[test]
fn a_refused_statement_carries_a_code_a_severity_a_place_and_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("errors.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let bad = "THIS IS NOT A QUERY";
        let mut result: *mut ZuResult = ptr::null_mut();
        let status = zu_query(
            conn,
            bad.as_ptr().cast::<c_char>(),
            bad.len(),
            &mut result,
            &mut err,
        );
        assert_eq!(status, ZuStatus::Error);
        assert!(result.is_null(), "a failure still wrote an out-parameter");
        assert!(!err.is_null());

        assert_eq!(
            zu_error_status(err),
            status,
            "the error knows its own status"
        );
        assert_eq!(zu_error_severity(err), ZU_SEVERITY_EXCEPTION);

        let mut len = 0usize;
        let message = zu_error_message(err, &mut len);
        assert!(!message.is_null());
        let message = CStr::from_ptr(message).to_str().expect("utf-8");
        assert_eq!(len, message.len());
        assert!(!message.is_empty());

        let mut code_len = 0usize;
        let code = zu_error_code(err, &mut code_len);
        assert!(!code.is_null(), "a syntax error names a condition");
        let code = CStr::from_ptr(code).to_str().expect("utf-8");
        assert_eq!(code_len, code.len());
        assert_eq!(code.len(), 5, "a GQLSTATUS code is five characters");

        // The place, as two numbers rather than as the words the
        // message also carries. This query is refused at its first
        // token, and the message still says so.
        let mut line = 0u32;
        let mut column = 0u32;
        assert_eq!(zu_error_position(err, &mut line, &mut column), ZuStatus::Ok);
        assert_eq!((line, column), (1, 1));
        assert!(
            message.starts_with("42001: line 1, column 1: "),
            "the message stopped saying where: {message}"
        );
        // Either half on its own, for a caller that wants one.
        let mut only = 0u32;
        assert_eq!(
            zu_error_position(err, &mut only, ptr::null_mut()),
            ZuStatus::Ok
        );
        assert_eq!(only, 1);
        zu_error_free(err);
        err = ptr::null_mut();

        // A condition raised while the statement runs happened at no
        // token, and answers that it has no place rather than pointing
        // at one it guessed. The out-parameters are left alone.
        let divide = "RETURN 1 / 0";
        let mut result: *mut ZuResult = ptr::null_mut();
        let status = zu_query(
            conn,
            divide.as_ptr().cast::<c_char>(),
            divide.len(),
            &mut result,
            &mut err,
        );
        assert_eq!(status, ZuStatus::Error);
        assert!(!err.is_null());
        let code = CStr::from_ptr(zu_error_code(err, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert_eq!(code, "22012", "division by zero");
        let (mut line, mut column) = (7u32, 9u32);
        assert_eq!(
            zu_error_position(err, &mut line, &mut column),
            ZuStatus::Done
        );
        assert_eq!((line, column), (7, 9), "an absent place wrote something");
        zu_error_free(err);
        err = ptr::null_mut();

        // The connection survives, and the next success clears the slot.
        let result = query(conn, "MATCH (a:person) RETURN count(a) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1)[0], 97);
        zu_result_free(result);

        // A caller who does not want the error still gets the status.
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_query(
                conn,
                bad.as_ptr().cast::<c_char>(),
                bad.len(),
                &mut result,
                ptr::null_mut()
            ),
            ZuStatus::Error
        );
        assert!(result.is_null());

        zu_conn_close(conn);
    }
}

/// The `_z` forms are the counted forms with the length filled in, so
/// they have to answer identically.
#[test]
fn the_nul_terminated_variants_agree_with_the_counted_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("zvariants.zu1");
    seeded(&path);

    unsafe {
        let cpath = c(path.to_str().expect("utf-8 path"));
        let mut conn: *mut ZuConn = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(zu_open_z(cpath.as_ptr(), &mut conn, &mut err), ZuStatus::Ok);
        assert!(!conn.is_null());

        let q = c("MATCH (a:person {id: $src}) RETURN a.id AS id");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
            ZuStatus::Ok
        );
        let name = c("src");
        assert_eq!(zu_bind_i64_z(stmt, name.as_ptr(), 11), ZuStatus::Ok);
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
        assert_eq!(col_i64(result, 0, 1), [11]);
        zu_result_free(result);
        zu_stmt_close(stmt);

        // A string binding, both halves NUL-terminated.
        let q = c("MATCH (a:person {id: 3}) RETURN $s AS s");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
            ZuStatus::Ok
        );
        let name = c("s");
        let value = c("hello");
        assert_eq!(
            zu_bind_str_z(stmt, name.as_ptr(), value.as_ptr()),
            ZuStatus::Ok
        );
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
        let mut len = 0usize;
        let mut s: *const c_char = ptr::null();
        assert_eq!(
            zu_result_cell_str(result, 0, 0, &mut s, &mut len),
            ZuStatus::Ok
        );
        assert_eq!(CStr::from_ptr(s).to_str(), Ok("hello"));
        zu_result_free(result);
        zu_stmt_close(stmt);

        let mut result: *mut ZuResult = ptr::null_mut();
        let q = c("MATCH (a:person) RETURN count(a) AS n");
        assert_eq!(
            zu_query_z(conn, q.as_ptr(), &mut result, &mut err),
            ZuStatus::Ok
        );
        assert_eq!(col_i64(result, 0, 1), [97]);
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// Nothing here crashes on a NULL, and the status says which mistake
/// it was rather than leaving the caller to guess from a NULL return.
#[test]
fn null_inputs_are_misuse_and_not_crashes() {
    unsafe {
        let missing = "/nonexistent/nowhere.zu1";
        let mut conn: *mut ZuConn = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        let status = zu_open(
            missing.as_ptr().cast::<c_char>(),
            missing.len(),
            &mut conn,
            &mut err,
        );
        assert_ne!(status, ZuStatus::Ok);
        assert!(conn.is_null());
        assert!(!err.is_null());
        assert_eq!(zu_error_status(err), status);
        zu_error_free(err);
        err = ptr::null_mut();

        // A NULL path with a length is misuse; the message says so and
        // the out-parameter is still written.
        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(
            zu_open(ptr::null(), 4, &mut conn, &mut err),
            ZuStatus::Misuse
        );
        assert!(conn.is_null());
        assert!(!err.is_null());
        zu_error_free(err);

        // No out-parameter at all is misuse rather than a write to
        // address zero.
        assert_eq!(
            zu_open(ptr::null(), 0, ptr::null_mut(), ptr::null_mut()),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_query(
                ptr::null_mut(),
                ptr::null(),
                0,
                ptr::null_mut(),
                ptr::null_mut()
            ),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_execute(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()),
            ZuStatus::Misuse
        );

        // Freeing nothing is nothing.
        zu_conn_close(ptr::null_mut());
        zu_result_free(ptr::null_mut());
        zu_stmt_close(ptr::null_mut());
        zu_error_free(ptr::null_mut());

        // Reading off a NULL handle answers rather than faults.
        assert_eq!(zu_result_rows(ptr::null()), 0);
        assert_eq!(zu_result_cols(ptr::null()), 0);
        assert_eq!(zu_error_severity(ptr::null()), -1);
        assert_eq!(zu_error_status(ptr::null()), ZuStatus::Misuse);
        assert!(zu_error_message(ptr::null(), ptr::null_mut()).is_null());
        assert!(zu_error_code(ptr::null(), ptr::null_mut()).is_null());
        assert_eq!(
            zu_error_position(ptr::null(), ptr::null_mut(), ptr::null_mut()),
            ZuStatus::Misuse
        );
        let mut ints: *const i64 = ptr::null();
        assert_eq!(
            zu_result_col_i64(ptr::null_mut(), 0, &mut ints),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_bind_i64(ptr::null_mut(), ptr::null(), 0, 1),
            ZuStatus::Misuse
        );
    }
}

/// A name that is not UTF-8 is the caller's mistake and is refused at
/// the boundary rather than reaching the binder.
#[test]
fn a_string_that_is_not_utf8_is_refused_at_the_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("utf8.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let bad: [u8; 3] = [0xff, 0xfe, 0xfd];
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_query(
                conn,
                bad.as_ptr().cast::<c_char>(),
                bad.len(),
                &mut result,
                &mut err
            ),
            ZuStatus::Misuse
        );
        assert!(result.is_null());
        assert!(!err.is_null());
        let message = CStr::from_ptr(zu_error_message(err, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert!(message.contains("UTF-8"), "{message}");
        zu_error_free(err);
        zu_conn_close(conn);
    }
}

/// The split dx/02 §8 asks for: one database, many connections. This is
/// the shape a pooling binding needs, and the shape a single handle
/// that conflated the two could not express.
#[test]
fn one_database_serves_many_connections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pool.zu1");
    seeded(&path);

    unsafe {
        let cpath = c(path.to_str().expect("utf-8 path"));
        let mut db: *mut ZuDatabase = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(
            zu_database_open_z(cpath.as_ptr(), ptr::null(), &mut db, &mut err),
            ZuStatus::Ok
        );
        assert!(!db.is_null() && err.is_null());

        let mut out: *const c_char = ptr::null();
        let mut len = 0usize;
        assert_eq!(zu_database_path(db, &mut out, &mut len), ZuStatus::Ok);
        assert_eq!(
            CStr::from_ptr(out).to_str(),
            Ok(path.to_str().expect("utf-8"))
        );
        assert_eq!(len, path.to_str().expect("utf-8").len());

        let mut conns = [ptr::null_mut::<ZuConn>(); 3];
        for conn in &mut conns {
            assert_eq!(zu_connect(db, conn, &mut err), ZuStatus::Ok);
            assert!(!conn.is_null());
        }
        // Each connection has its own catalog and plan cache, so each
        // answers the same question independently.
        for conn in conns {
            let result = query(conn, "MATCH (a:person) RETURN count(a) AS n", &mut err);
            assert_eq!(col_i64(result, 0, 1), [97]);
            zu_result_free(result);
        }

        // Closing the database releases a path and a configuration; the
        // connections hold their own file handles and keep working,
        // which is what lets a host drop the database handle once its
        // pool is filled.
        zu_database_close(db);
        let result = query(conns[0], "MATCH (a:person) RETURN count(a) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [97]);
        zu_result_free(result);

        for conn in conns {
            zu_conn_close(conn);
        }

        // A NULL database is misuse rather than a fault.
        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(
            zu_connect(ptr::null_mut(), &mut conn, &mut err),
            ZuStatus::Misuse
        );
        assert!(conn.is_null() && !err.is_null());
        zu_error_free(err);
    }
}

/// The versioned struct and the setter beside it, which is how a
/// binding forwards a user's option map without knowing the layout.
#[test]
fn a_configuration_arrives_by_field_or_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.zu1");
    seeded(&path);

    unsafe {
        let mut cfg = ZuConfig {
            struct_size: 0,
            memory_limit: 0,
            threads: 0,
            read_only: 0,
        };
        assert_eq!(zu_config_init(&mut cfg), ZuStatus::Ok);
        assert_eq!(cfg.struct_size, std::mem::size_of::<ZuConfig>());
        assert_eq!(zu_config_init(ptr::null_mut()), ZuStatus::Misuse);

        let mut err: *mut ZuError = ptr::null_mut();
        let set = |cfg: &mut ZuConfig, k: &CString, v: &CString, err: &mut *mut ZuError| {
            zu_config_set_z(cfg, k.as_ptr(), v.as_ptr(), err)
        };
        assert_eq!(
            set(&mut cfg, &c("memory_limit"), &c("8388608"), &mut err),
            ZuStatus::Ok
        );
        assert_eq!(cfg.memory_limit, 8 * 1024 * 1024);
        assert_eq!(
            set(&mut cfg, &c("threads"), &c("1"), &mut err),
            ZuStatus::Ok
        );
        assert_eq!(cfg.threads, 1);
        assert_eq!(
            set(&mut cfg, &c("read_only"), &c("true"), &mut err),
            ZuStatus::Ok
        );
        assert_eq!(cfg.read_only, 1);
        assert!(err.is_null(), "a run of successes left an error behind");

        // A typo is named, because a binding forwarding a map has to
        // tell its user which entry was wrong.
        assert_eq!(
            set(&mut cfg, &c("memroy_limit"), &c("1"), &mut err),
            ZuStatus::Misuse
        );
        assert!(!err.is_null());
        let message = CStr::from_ptr(zu_error_message(err, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert!(message.contains("memroy_limit"), "{message}");
        zu_error_free(err);
        err = ptr::null_mut();

        // And so is a value the key cannot take. A suffix is not parsed
        // here on purpose, so it is refused rather than guessed at.
        assert_eq!(
            set(&mut cfg, &c("memory_limit"), &c("512MB"), &mut err),
            ZuStatus::Misuse
        );
        assert!(!err.is_null());
        zu_error_free(err);
        err = ptr::null_mut();
        assert_eq!(
            set(&mut cfg, &c("read_only"), &c("yes"), &mut err),
            ZuStatus::Misuse
        );
        assert!(!err.is_null());
        zu_error_free(err);
        err = ptr::null_mut();
        assert_eq!(
            cfg.memory_limit,
            8 * 1024 * 1024,
            "a refusal changed nothing"
        );
        assert_eq!(cfg.read_only, 1);

        // The configuration reaches the engine: read_only means a write
        // is refused rather than attempted.
        let cpath = c(path.to_str().expect("utf-8 path"));
        let mut db: *mut ZuDatabase = ptr::null_mut();
        assert_eq!(
            zu_database_open_z(cpath.as_ptr(), &cfg, &mut db, &mut err),
            ZuStatus::Ok
        );
        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(zu_connect(db, &mut conn, &mut err), ZuStatus::Ok);
        let result = query(conn, "MATCH (a:person) RETURN count(a) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [97]);
        zu_result_free(result);

        let write = c("CREATE (n:person {id: 999})");
        let mut refused: *mut ZuResult = ptr::null_mut();
        assert_ne!(
            zu_query_z(conn, write.as_ptr(), &mut refused, &mut err),
            ZuStatus::Ok,
            "a read-only connection accepted a write"
        );
        assert!(refused.is_null() && !err.is_null());
        zu_error_free(err);

        zu_conn_close(conn);
        zu_database_close(db);
    }
}

/// A statement outliving its connection is the classic use-after-close,
/// and dx/02 §5 asks for it to be answered rather than undefined.
#[test]
fn a_statement_outliving_its_connection_is_refused_rather_than_undefined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("closed.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let q = c("MATCH (a:person {id: $src}) RETURN a.id AS id");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
            ZuStatus::Ok
        );
        let name = c("src");
        assert_eq!(zu_bind_i64_z(stmt, name.as_ptr(), 7), ZuStatus::Ok);

        zu_conn_close(conn);

        // Every use of the statement now answers, and none of them
        // follows the pointer it still holds.
        assert_eq!(
            zu_bind_i64_z(stmt, name.as_ptr(), 8),
            ZuStatus::MisuseClosed
        );
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_execute(stmt, &mut result, &mut err),
            ZuStatus::MisuseClosed
        );
        assert!(result.is_null());
        assert!(
            err.is_null(),
            "the status names this mistake exactly, so there is nothing to add"
        );

        // And closing it is still the right thing to do, which is what
        // makes this recoverable rather than a leak.
        //
        // The connection handle itself is a different matter: closing
        // freed it, so it is gone in the way any C handle is gone after
        // its free, and nothing here can check a pointer that no longer
        // points at anything. What the check buys is the statement,
        // which is a live handle a host has every reason to still be
        // holding.
        zu_stmt_close(stmt);
    }
}

/// A connection may move between threads but not be used from two at
/// once. The point is the safety property rather than a count: every
/// call answers, one of the two answers ZU_MISUSE_CONCURRENT, and no
/// pair of them is ever inside the engine together.
#[test]
fn two_threads_on_one_connection_are_turned_away_rather_than_let_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("threads.zu1");
    seeded(&path);

    /// A raw handle is not `Send`, because whether it may cross a
    /// thread is exactly what this ABI decides rather than the
    /// compiler. It may, one thread at a time, which is what makes the
    /// wrapper sound and the test worth writing.
    struct Handle(*mut ZuConn);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    let conn = unsafe { open(&path) };
    let handle = Handle(conn);
    let refused = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..2 {
            let handle = &handle;
            let refused = &refused;
            scope.spawn(move || {
                let q = c("MATCH (a:person) RETURN count(a) AS n");
                for _ in 0..200 {
                    let mut result: *mut ZuResult = ptr::null_mut();
                    let status =
                        unsafe { zu_query_z(handle.0, q.as_ptr(), &mut result, ptr::null_mut()) };
                    match status {
                        ZuStatus::Ok => {
                            assert_eq!(unsafe { col_i64(result, 0, 1) }, [97]);
                            unsafe { zu_result_free(result) };
                        }
                        ZuStatus::MisuseConcurrent => {
                            assert!(result.is_null(), "a refusal still handed back a result");
                            refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        other => panic!("unexpected status {other:?}"),
                    }
                }
            });
        }
    });

    // Whether any call actually collided is up to the scheduler, so the
    // count is reported and not asserted on. What is asserted is that
    // the connection survived being used wrongly and still answers.
    let collisions = refused.load(std::sync::atomic::Ordering::Relaxed);
    unsafe {
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(conn, "MATCH (a:person) RETURN count(a) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [97], "after {collisions} collisions");
        zu_result_free(result);
        zu_conn_close(conn);
    }
}

/// The chunked path and the whole-column path are two ways of reading
/// the same column, so the thing worth pinning is that they agree: the
/// chunks laid end to end are the column, boundaries and all.
#[test]
fn a_column_read_chunk_by_chunk_is_the_column_read_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chunks.zu1");
    // Two full chunks and a short one, so the last-chunk arithmetic is
    // exercised rather than assumed.
    let rows = 2048 * 2 + 173;
    seeded_wide(&path, rows);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person) RETURN a AS n, a.id AS id ORDER BY id",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), u64::from(rows));
        assert_eq!(zu_result_chunk_count(result), 3);

        let whole = col_i64(result, 1, rows as usize);
        let mut seen: Vec<i64> = Vec::with_capacity(rows as usize);
        for chunk in 0..zu_result_chunk_count(result) {
            let (offset, count) = chunk_span(result, chunk);
            assert_eq!(
                offset,
                seen.len() as u64,
                "chunk {chunk} starts where the last ended"
            );
            // The offset is what makes a chunk row a row number again.
            assert_eq!(cell_type(result, offset, 1), ZU_TYPE_INT);
            seen.extend_from_slice(chunk_i64(result, chunk, 1, count as usize));
        }
        assert_eq!(seen, whole, "the chunks are the column");

        // Validity and node offsets chunk the same way, and a column is
        // independent of its neighbour: reading the node column for a
        // chunk does not disturb the int column's buffer for it.
        let nodes = col_node_offset(result, 0, rows as usize);
        let valid = col_valid(result, 1, rows as usize);
        for chunk in 0..zu_result_chunk_count(result) {
            let (offset, count) = chunk_span(result, chunk);
            let lo = offset as usize;
            let hi = lo + count as usize;
            let ints = chunk_i64(result, chunk, 1, count as usize);
            assert_eq!(
                chunk_node_offset(result, chunk, 0, count as usize),
                &nodes[lo..hi]
            );
            assert_eq!(
                chunk_valid(result, chunk, 1, count as usize),
                &valid[lo..hi]
            );
            assert_eq!(
                ints,
                &whole[lo..hi],
                "the neighbour's read cost this one nothing"
            );
        }

        // Chunks are readable in any order, so a buffer refilled
        // backwards holds the chunk asked for and not the one before.
        let (_, first_rows) = chunk_span(result, 0);
        assert_eq!(
            chunk_i64(result, 0, 1, first_rows as usize),
            &whole[..first_rows as usize],
            "going back to a chunk rereads it"
        );

        zu_result_free(result);
        zu_conn_close(conn);
    }
}

/// What the chunked accessors refuse, and what they do afterwards. A
/// refusal has to leave the result usable, because a binding that
/// probes a column to find out what it holds would otherwise poison it.
#[test]
fn a_chunk_out_of_range_or_of_the_wrong_kind_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chunk-misuse.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person) RETURN a AS n, a.id AS id ORDER BY id",
            &mut err,
        );
        // 97 people, well under one chunk.
        assert_eq!(zu_result_chunk_count(result), 1);
        let (offset, count) = chunk_span(result, 0);
        assert_eq!((offset, count), (0, 97));

        let mut lo = 7u64;
        let mut n = 7u64;
        assert_eq!(
            zu_result_chunk(result, 1, &mut lo, &mut n),
            ZuStatus::Misuse,
            "there is no second chunk"
        );
        assert_eq!(
            (lo, n),
            (0, 0),
            "a refusal writes zeroes, not the last answer"
        );

        let mut ints: *const i64 = ptr::null();
        assert_eq!(
            zu_result_chunk_col_i64(result, 1, 1, &mut ints),
            ZuStatus::Misuse
        );
        assert!(ints.is_null());
        assert_eq!(
            zu_result_chunk_col_i64(result, 0, 9, &mut ints),
            ZuStatus::Misuse,
            "no ninth column"
        );
        // A node is not an integer here either, chunked or whole.
        assert_eq!(
            zu_result_chunk_col_i64(result, 0, 0, &mut ints),
            ZuStatus::Misuse,
            "a node column does not read as integers"
        );
        assert!(ints.is_null());

        let mut floats: *const f64 = ptr::null();
        assert_eq!(
            zu_result_chunk_col_f64(result, 0, 0, &mut floats),
            ZuStatus::Misuse,
            "nor as doubles"
        );

        // And the result still works, both ways round.
        let ids: Vec<i64> = (0..97).collect();
        assert_eq!(chunk_i64(result, 0, 1, 97), ids, "a refusal left it usable");
        assert_eq!(col_i64(result, 1, 97), ids);
        // An int column widens chunked, the same as it does whole.
        assert_eq!(
            zu_result_chunk_col_f64(result, 0, 1, &mut floats),
            ZuStatus::Ok
        );
        let widened: Vec<f64> = ids.iter().map(|i| *i as f64).collect();
        assert_eq!(std::slice::from_raw_parts(floats, 97), widened);

        zu_result_free(result);
        zu_conn_close(conn);
    }
}

/// A result with no rows has no chunks, which is how the chunked path
/// says what ZU_DONE says on the whole-column path: the loop a caller
/// writes runs zero times and asks nothing.
#[test]
fn an_empty_result_has_no_chunks_and_needs_no_done() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("chunk-empty.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person {id: 999999}) RETURN a.id AS id",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 0);
        assert_eq!(zu_result_chunk_count(result), 0);

        let mut lo = 0u64;
        let mut n = 0u64;
        assert_eq!(
            zu_result_chunk(result, 0, &mut lo, &mut n),
            ZuStatus::Misuse
        );
        let mut ints: *const i64 = ptr::null();
        assert_eq!(
            zu_result_chunk_col_i64(result, 0, 0, &mut ints),
            ZuStatus::Misuse
        );
        assert!(ints.is_null());

        // NULL handles answer the same way the rest of the surface does.
        assert_eq!(zu_result_chunk_count(ptr::null()), 0);
        assert_eq!(
            zu_result_chunk(ptr::null(), 0, &mut lo, &mut n),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_result_chunk_col_i64(ptr::null_mut(), 0, 0, &mut ints),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_result_chunk_col_i64(result, 0, 0, ptr::null_mut()),
            ZuStatus::Misuse,
            "nowhere to write the answer"
        );

        zu_result_free(result);
        zu_conn_close(conn);
    }
}

/// The values a column has nowhere to put. Before the cell reader a C
/// host could see that a cell was a temporal and read nothing out of
/// it, which left every temporal type unreachable from eight of the
/// nine clients.
#[test]
fn a_temporal_cell_reads_as_a_kind_a_count_and_an_offset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("temporal.zu1");
    seeded(&path);

    // Days from the epoch, nanoseconds from midnight, months, and the
    // offset in minutes. Each written out here rather than computed,
    // so that a wrong unit disagrees with the test instead of sharing
    // its arithmetic.
    let cases: [(&str, i32, i64, i32); 6] = [
        ("DATE '2024-02-29'", ZU_TEMPORAL_DATE, 19782, 0),
        (
            "LOCAL TIME '12:34:56.123456789'",
            ZU_TEMPORAL_LOCAL_TIME,
            45_296_123_456_789,
            0,
        ),
        (
            "ZONED TIME '12:34:56+07:00'",
            ZU_TEMPORAL_ZONED_TIME,
            45_296_000_000_000,
            420,
        ),
        (
            "LOCAL DATETIME '2024-01-01T00:00:00'",
            ZU_TEMPORAL_LOCAL_DATETIME,
            1_704_067_200_000_000_000,
            0,
        ),
        ("DURATION 'P1Y2M'", ZU_TEMPORAL_DURATION_YEAR_MONTH, 14, 0),
        (
            "DURATION 'PT1H30M'",
            ZU_TEMPORAL_DURATION_DAY_TIME,
            5_400_000_000_000,
            0,
        ),
    ];

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        for (text, want_kind, want_count, want_offset) in cases {
            let result = query(conn, &format!("RETURN {text} AS n"), &mut err);
            let value = cell(result, 0, 0);
            assert_eq!(zu_value_type(value), ZU_TYPE_TEMPORAL, "{text}");

            let (mut kind, mut count, mut offset) = (-1, 0i64, i32::MAX);
            assert_eq!(
                zu_value_temporal(value, &mut kind, &mut count, &mut offset),
                ZuStatus::Ok,
                "{text}"
            );
            assert_eq!(
                (kind, count, offset),
                (want_kind, want_count, want_offset),
                "{text}"
            );
            zu_result_free(result);
        }

        // A zoned datetime is stored as the instant and remembers the
        // offset it was written with, so the two below are one moment
        // and two offsets. A host that ignored the offset would still
        // have the right instant, which is why the count is the same.
        for (text, want_offset) in [
            ("ZONED DATETIME '2024-01-01T07:00:00+07:00'", 420),
            ("ZONED DATETIME '2024-01-01T00:00:00+00:00'", 0),
        ] {
            let result = query(conn, &format!("RETURN {text} AS n"), &mut err);
            let (mut kind, mut count, mut offset) = (-1, 0i64, i32::MAX);
            assert_eq!(
                zu_value_temporal(cell(result, 0, 0), &mut kind, &mut count, &mut offset),
                ZuStatus::Ok
            );
            assert_eq!(kind, ZU_TEMPORAL_ZONED_DATETIME);
            assert_eq!(count, 1_704_067_200_000_000_000, "{text}");
            assert_eq!(offset, want_offset, "{text}");
            zu_result_free(result);
        }

        zu_conn_close(conn);
    }
}

/// The one composite type, and therefore the one place a decoder has
/// to recurse. An element is read with the accessors its parent was,
/// which is what makes a list of lists a walk rather than a second
/// API.
#[test]
fn a_list_cell_reads_element_by_element_and_nests() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("list.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // Mixed elements, because a list is not typed and a decoder
        // that assumed the first element's type would pass a test made
        // of integers.
        let result = query(conn, "RETURN [1, 'a', true, 2.5] AS n", &mut err);
        let list = cell(result, 0, 0);
        assert_eq!(zu_value_type(list), ZU_TYPE_LIST);
        assert_eq!(zu_value_len(list), 4);
        assert_eq!(value_i64(at(list, 0)), 1);
        assert_eq!(value_str(at(list, 1)), "a");
        let mut b = -1i32;
        assert_eq!(zu_value_bool(at(list, 2), &mut b), ZuStatus::Ok);
        assert_eq!(b, 1);
        let mut f = 0.0;
        assert_eq!(zu_value_f64(at(list, 3), &mut f), ZuStatus::Ok);
        assert_eq!(f, 2.5);
        // One past the end is the caller's mistake and not an empty
        // answer, because a list that short is a different list.
        let mut out: *const ZuValue = ptr::null();
        assert_eq!(zu_value_at(list, 4, &mut out), ZuStatus::Misuse);
        assert!(out.is_null());
        zu_result_free(result);

        // Three deep, read by recursion.
        let result = query(conn, "RETURN [[[1]]] AS n", &mut err);
        let mut here = cell(result, 0, 0);
        for depth in 0..3 {
            assert_eq!(zu_value_type(here), ZU_TYPE_LIST, "depth {depth}");
            assert_eq!(zu_value_len(here), 1, "depth {depth}");
            here = at(here, 0);
        }
        assert_eq!(value_i64(here), 1);
        zu_result_free(result);

        // An empty list has a length of zero and is still a list,
        // which is the pair a returned count could not express on its
        // own and the reason zu_value_len needs no status.
        let result = query(conn, "RETURN [[], 1] AS n", &mut err);
        let list = cell(result, 0, 0);
        assert_eq!(zu_value_len(list), 2);
        assert_eq!(zu_value_type(at(list, 0)), ZU_TYPE_LIST);
        assert_eq!(zu_value_len(at(list, 0)), 0);
        // A scalar is not a composite, so asking it for an element is
        // misuse rather than a zero-length answer.
        assert_eq!(zu_value_len(at(list, 1)), 0);
        assert_eq!(zu_value_at(at(list, 1), 0, &mut out), ZuStatus::Misuse);
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// A node cell carries the table its offset counts in, which
/// `zu_result_col_node_offset` has nowhere to put: a column of two
/// tables has no one answer. A string cell borrows the result's own
/// bytes rather than a copy of them.
#[test]
fn a_cell_carries_what_a_column_of_it_could_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cells.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            conn,
            "MATCH (a:person) RETURN a AS n ORDER BY a.id LIMIT 3",
            &mut err,
        );
        let mut tables = Vec::new();
        for row in 0..3 {
            let value = cell(result, row, 0);
            assert_eq!(zu_value_type(value), ZU_TYPE_NODE);
            let (mut table, mut offset) = (u32::MAX, u64::MAX);
            assert_eq!(zu_value_node(value, &mut table, &mut offset), ZuStatus::Ok);
            assert_eq!(offset, row);
            tables.push(table);
        }
        // One table here, and the point is that the answer exists at
        // all: it is what a binding needs to tell two nodes apart when
        // a query returns rows from two tables.
        assert_eq!(tables[0], tables[1]);
        assert_eq!(tables[1], tables[2]);
        zu_result_free(result);

        // A string cell is a pointer into the result and a length, and
        // the length is the whole of it: there is no NUL to stop at.
        let result = query(
            conn,
            "MATCH (a:person {id: 3}) RETURN 'hi there' AS s",
            &mut err,
        );
        let value = cell(result, 0, 0);
        assert_eq!(zu_value_type(value), ZU_TYPE_STR);
        assert_eq!(value_str(value), "hi there");

        // The NUL-terminated form of the same cell says the same
        // thing, which is what lets a caller pick either.
        let mut out: *const c_char = ptr::null();
        let mut len = 0usize;
        assert_eq!(
            zu_result_cell_str(result, 0, 0, &mut out, &mut len),
            ZuStatus::Ok
        );
        assert_eq!(CStr::from_ptr(out).to_str().expect("utf-8"), "hi there");
        assert_eq!(len, 8);
        zu_result_free(result);

        // A null cell is a value and reads as one, rather than as a
        // cell that is not there.
        let result = query(conn, "MATCH (a:person {id: 3}) RETURN null AS n", &mut err);
        let value = cell(result, 0, 0);
        assert_eq!(zu_value_type(value), ZU_TYPE_NULL);
        let mut i = 7i64;
        assert_eq!(zu_value_i64(value, &mut i), ZuStatus::Misuse);
        assert_eq!(i, 0);

        // Out of range on either axis, and a NULL result, are the same
        // misuse the rest of the surface answers with.
        let mut out: *const ZuValue = ptr::null();
        assert_eq!(zu_result_cell(result, 1, 0, &mut out), ZuStatus::Misuse);
        assert!(out.is_null());
        assert_eq!(zu_result_cell(result, 0, 1, &mut out), ZuStatus::Misuse);
        assert_eq!(
            zu_result_cell(ptr::null(), 0, 0, &mut out),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_result_cell(result, 0, 0, ptr::null_mut()),
            ZuStatus::Misuse
        );
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// A path in a fresh directory, which the loader creates and the tests
/// below then open as an ordinary database.
fn fresh(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

unsafe fn loader(path: &std::path::Path) -> *mut ZuLoader {
    let path = path.to_str().expect("utf-8 path");
    let mut l: *mut ZuLoader = ptr::null_mut();
    let status = unsafe {
        zu_loader_create(
            path.as_ptr().cast::<c_char>(),
            path.len(),
            &mut l,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "create {path}");
    assert!(!l.is_null());
    l
}

/// Names the table on the counted form, which is the one a binding
/// uses.
unsafe fn table(l: *mut ZuLoader, nodes: &str, edges: &str, rows: u64) -> ZuStatus {
    unsafe {
        zu_loader_table(
            l,
            nodes.as_ptr().cast::<c_char>(),
            nodes.len(),
            edges.as_ptr().cast::<c_char>(),
            edges.len(),
            rows,
            ptr::null_mut(),
        )
    }
}

unsafe fn col_i64_in(l: *mut ZuLoader, name: &str, values: &[i64]) -> ZuStatus {
    unsafe {
        zu_loader_col_i64(
            l,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            values.as_ptr(),
            values.len() as u64,
            ptr::null_mut(),
        )
    }
}

unsafe fn col_f64_in(l: *mut ZuLoader, name: &str, values: &[f64]) -> ZuStatus {
    unsafe {
        zu_loader_col_f64(
            l,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            values.as_ptr(),
            values.len() as u64,
            ptr::null_mut(),
        )
    }
}

unsafe fn col_bool_in(l: *mut ZuLoader, name: &str, values: &[i32]) -> ZuStatus {
    unsafe {
        zu_loader_col_bool(
            l,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            values.as_ptr(),
            values.len() as u64,
            ptr::null_mut(),
        )
    }
}

/// The counted form, so a value holding a NUL is a value like any
/// other rather than a string that ends early.
unsafe fn col_str_in(l: *mut ZuLoader, name: &str, values: &[&str]) -> ZuStatus {
    let ptrs: Vec<*const c_char> = values.iter().map(|s| s.as_ptr().cast::<c_char>()).collect();
    let lens: Vec<usize> = values.iter().map(|s| s.len()).collect();
    unsafe {
        zu_loader_col_str(
            l,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            ptrs.as_ptr(),
            lens.as_ptr(),
            values.len() as u64,
            ptr::null_mut(),
        )
    }
}

unsafe fn col_temporal_in(l: *mut ZuLoader, name: &str, kind: i32, values: &[i64]) -> ZuStatus {
    unsafe {
        zu_loader_col_temporal(
            l,
            name.as_ptr().cast::<c_char>(),
            name.len(),
            kind,
            values.as_ptr(),
            values.len() as u64,
            ptr::null_mut(),
        )
    }
}

/// Everything a corpus fixture puts in a table goes in through this,
/// and everything a query can return comes back out, which is the whole
/// point of the loader: the value crosses the boundary twice and by two
/// different mechanisms, so a bug in either one shows up as a mismatch.
#[test]
fn a_loader_builds_a_database_a_query_reads_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = fresh(&dir, "loaded.zu1");
    unsafe {
        let l = loader(&path);
        assert_eq!(table(l, "person", "knows", 3), ZuStatus::Ok);

        assert_eq!(col_i64_in(l, "age", &[30, 40, 50]), ZuStatus::Ok);

        assert_eq!(col_f64_in(l, "score", &[1.5, 2.5, -0.5]), ZuStatus::Ok);

        // Any nonzero is true, so 7 is as much a yes as 1 is.
        assert_eq!(col_bool_in(l, "ok", &[1, 0, 7]), ZuStatus::Ok);

        // The counted form carries a length that is not a strlen: the
        // second name holds an embedded NUL, which is a string a `_z`
        // call could not pass and a counted one has no trouble with.
        assert_eq!(col_str_in(l, "name", &["ann", "b\0b", "cy"]), ZuStatus::Ok);

        // 2024-02-29, 2024-03-01, 1969-12-31, in the days
        // zu_value_temporal reads a date back out as.
        assert_eq!(
            col_temporal_in(l, "born", ZU_TEMPORAL_DATE, &[19782, 19783, -1]),
            ZuStatus::Ok
        );
        assert_eq!(
            col_temporal_in(
                l,
                "at",
                ZU_TEMPORAL_LOCAL_TIME,
                &[45_296_123_456_789, 0, 86_399_000_000_000]
            ),
            ZuStatus::Ok
        );
        assert_eq!(
            col_temporal_in(l, "span", ZU_TEMPORAL_DURATION_YEAR_MONTH, &[14, 0, -3]),
            ZuStatus::Ok
        );

        // Two calls, because a host streaming edges makes as many as it
        // likes, and the duplicate is what the loader deduplicates.
        let from = [0u32, 1];
        let to = [1u32, 2];
        assert_eq!(
            zu_loader_edges(l, from.as_ptr(), to.as_ptr(), 2, ptr::null_mut()),
            ZuStatus::Ok
        );
        let again = [0u32];
        assert_eq!(
            zu_loader_edges(l, again.as_ptr(), again[..].as_ptr(), 1, ptr::null_mut()),
            ZuStatus::Ok
        );
        let dup_to = [1u32];
        assert_eq!(
            zu_loader_edges(l, again.as_ptr(), dup_to.as_ptr(), 1, ptr::null_mut()),
            ZuStatus::Ok
        );

        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(zu_loader_finish(l, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        zu_loader_free(l);

        // From here on it is an ordinary database, opened the ordinary
        // way.
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        let result = query(
            conn,
            "MATCH (p:person) RETURN p.age AS a ORDER BY a",
            &mut err,
        );
        assert_eq!(col_i64(result, 0, 3), [30, 40, 50]);
        zu_result_free(result);

        let result = query(
            conn,
            "MATCH (p:person) RETURN p.score AS s ORDER BY s",
            &mut err,
        );
        let mut scores = Vec::new();
        for row in 0..3 {
            let mut v = 0f64;
            assert_eq!(zu_value_f64(cell(result, row, 0), &mut v), ZuStatus::Ok);
            scores.push(v);
        }
        assert_eq!(scores, vec![-0.5, 1.5, 2.5]);
        zu_result_free(result);

        let result = query(
            conn,
            "MATCH (p:person) RETURN p.ok AS o, p.age AS a ORDER BY a",
            &mut err,
        );
        let mut flags = Vec::new();
        for row in 0..3 {
            let mut v = -1i32;
            assert_eq!(zu_value_bool(cell(result, row, 0), &mut v), ZuStatus::Ok);
            flags.push(v);
        }
        assert_eq!(flags, vec![1, 0, 1], "any nonzero went in as one true");
        zu_result_free(result);

        let result = query(
            conn,
            "MATCH (p:person) RETURN p.name AS n, p.age AS a ORDER BY a",
            &mut err,
        );
        let read: Vec<&str> = (0..3).map(|row| value_str(cell(result, row, 0))).collect();
        assert_eq!(read, vec!["ann", "b\0b", "cy"], "the NUL survived the trip");
        zu_result_free(result);

        // Every temporal comes back as the kind and count it went in
        // as, which is the property a corpus runner leans on.
        for (column, kind, want) in [
            ("born", ZU_TEMPORAL_DATE, [19782i64, 19783, -1]),
            (
                "at",
                ZU_TEMPORAL_LOCAL_TIME,
                [45_296_123_456_789, 0, 86_399_000_000_000],
            ),
            ("span", ZU_TEMPORAL_DURATION_YEAR_MONTH, [14, 0, -3]),
        ] {
            let text = format!("MATCH (p:person) RETURN p.{column} AS t, p.age AS a ORDER BY a");
            let result = query(conn, &text, &mut err);
            for (row, want) in want.iter().enumerate() {
                let mut got_kind = -1i32;
                let mut count = i64::MIN;
                let mut offset = i32::MIN;
                assert_eq!(
                    zu_value_temporal(
                        cell(result, row as u64, 0),
                        &mut got_kind,
                        &mut count,
                        &mut offset
                    ),
                    ZuStatus::Ok,
                    "{column} row {row}"
                );
                assert_eq!(got_kind, kind, "{column} row {row}");
                assert_eq!(count, *want, "{column} row {row}");
                assert_eq!(offset, 0);
            }
            zu_result_free(result);
        }

        // Three distinct edges went in across three calls, one of them
        // a repeat of the first, and three minus the repeat came out.
        let result = query(
            conn,
            "MATCH (:person)-[:knows]->(:person) RETURN 1 AS n",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 3);
        zu_result_free(result);

        zu_conn_close(conn);
    }
}

/// Every way to hand a loader something that cannot be a table, each
/// answered at the call that did it rather than at finish, and none of
/// them leaving the loader in a state where the next call would write
/// half a database.
#[test]
fn a_loader_refuses_what_cannot_be_a_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    unsafe {
        // A column before the table has nothing to be a column of.
        let l = loader(&fresh(&dir, "a.zu1"));
        assert_eq!(col_i64_in(l, "age", &[1, 2]), ZuStatus::Misuse);
        assert_eq!(table(l, "person", "knows", 0), ZuStatus::Misuse);
        assert_eq!(table(l, "", "knows", 2), ZuStatus::Misuse);
        assert_eq!(table(l, "person", "knows", 2), ZuStatus::Ok);
        // One table per loader.
        assert_eq!(table(l, "other", "knows", 2), ZuStatus::Misuse);

        // A column that is not as wide as the table was said to be.
        assert_eq!(col_i64_in(l, "age", &[1, 2, 3]), ZuStatus::Misuse);
        assert_eq!(col_i64_in(l, "age", &[1, 2]), ZuStatus::Ok);
        // And the same name twice, which would be two answers to one
        // question.
        assert_eq!(col_i64_in(l, "age", &[3, 4]), ZuStatus::Misuse);
        assert_eq!(col_i64_in(l, "", &[3, 4]), ZuStatus::Misuse);

        // An edge that reaches past the table it is inside of.
        let from = [0u32];
        let past = [2u32];
        assert_eq!(
            zu_loader_edges(l, from.as_ptr(), past.as_ptr(), 1, ptr::null_mut()),
            ZuStatus::Misuse
        );

        // A zoned column is refused for a reason of its own: the store
        // has nowhere to keep the offset, which is not the caller's
        // mistake and does not read as one.
        let mut err: *mut ZuError = ptr::null_mut();
        let nanos = [0i64, 0];
        assert_eq!(
            zu_loader_col_temporal(
                l,
                "at".as_ptr().cast::<c_char>(),
                2,
                ZU_TEMPORAL_ZONED_DATETIME,
                nanos.as_ptr(),
                2,
                &mut err
            ),
            ZuStatus::Unsupported
        );
        assert!(!err.is_null());
        let mut len = 0usize;
        let message = CStr::from_ptr(zu_error_message(err, &mut len))
            .to_str()
            .expect("utf-8");
        assert!(message.contains("zoned"), "{message}");
        zu_error_free(err);

        // A tag that is not one of the seven.
        assert_eq!(col_temporal_in(l, "at", 99, &nanos), ZuStatus::Misuse);
        // A date that is not a number of days any date has.
        assert_eq!(
            col_temporal_in(l, "d", ZU_TEMPORAL_DATE, &[i64::MAX, 0]),
            ZuStatus::Misuse
        );

        // A string that is not UTF-8 is refused now rather than read
        // back later as something no query could return.
        let bad: [u8; 2] = [0xff, 0xfe];
        let ptrs = [bad.as_ptr().cast::<c_char>(), bad.as_ptr().cast::<c_char>()];
        let lens = [2usize, 2];
        assert_eq!(
            zu_loader_col_str(
                l,
                "n".as_ptr().cast::<c_char>(),
                1,
                ptrs.as_ptr(),
                lens.as_ptr(),
                2,
                ptr::null_mut()
            ),
            ZuStatus::Misuse
        );

        // Everything above was refused, so what is left is the one
        // good column and no edges, and that is what finishes.
        assert_eq!(zu_loader_finish(l, ptr::null_mut()), ZuStatus::Ok);
        zu_loader_free(l);

        // NULL is a misuse and not a crash, and free of NULL is a
        // no-op, on the same terms as every other handle here.
        assert_eq!(
            zu_loader_table_z(
                ptr::null_mut(),
                c("a").as_ptr(),
                c("b").as_ptr(),
                1,
                ptr::null_mut()
            ),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_loader_finish(ptr::null_mut(), ptr::null_mut()),
            ZuStatus::Misuse
        );
        zu_loader_free(ptr::null_mut());

        let mut out: *mut ZuLoader = ptr::null_mut();
        let taken = fresh(&dir, "a.zu1");
        let taken = taken.to_str().expect("utf-8");
        assert_ne!(
            zu_loader_create(
                taken.as_ptr().cast::<c_char>(),
                taken.len(),
                &mut out,
                ptr::null_mut()
            ),
            ZuStatus::Ok,
            "a bulk load builds a database rather than clobbering one"
        );
        assert!(out.is_null());
    }
}

/// A finished loader is spent, including one whose finish failed, so a
/// host that keeps the handle around cannot write half a second table
/// through it. The answer is the one a statement gives after its
/// connection closed, and it means the same thing.
#[test]
fn a_finished_loader_is_spent() {
    let dir = tempfile::tempdir().expect("tempdir");
    unsafe {
        let l = loader(&fresh(&dir, "spent.zu1"));
        assert_eq!(table(l, "person", "knows", 2), ZuStatus::Ok);
        assert_eq!(col_i64_in(l, "age", &[1, 2]), ZuStatus::Ok);
        assert_eq!(zu_loader_finish(l, ptr::null_mut()), ZuStatus::Ok);

        assert_eq!(zu_loader_finish(l, ptr::null_mut()), ZuStatus::MisuseClosed);
        assert_eq!(col_i64_in(l, "height", &[1, 2]), ZuStatus::MisuseClosed);
        assert_eq!(table(l, "thing", "link", 2), ZuStatus::MisuseClosed);
        let ends = [0u32];
        assert_eq!(
            zu_loader_edges(l, ends.as_ptr(), ends.as_ptr(), 1, ptr::null_mut()),
            ZuStatus::MisuseClosed
        );
        zu_loader_free(l);

        // A finish with no table is a failure, and it spends the loader
        // too: there is no half-built load to add to afterwards.
        let l = loader(&fresh(&dir, "empty.zu1"));
        assert_eq!(zu_loader_finish(l, ptr::null_mut()), ZuStatus::Misuse);
        assert_eq!(table(l, "person", "knows", 2), ZuStatus::MisuseClosed);
        zu_loader_free(l);

        // A loader freed before it finished wrote nothing, and the file
        // it created is still there and still empty.
        let unfinished = fresh(&dir, "unfinished.zu1");
        let l = loader(&unfinished);
        assert_eq!(table(l, "person", "knows", 2), ZuStatus::Ok);
        zu_loader_free(l);
        assert!(unfinished.exists(), "the path is the caller's to remove");
        let conn = open(&unfinished);
        let mut err: *mut ZuError = ptr::null_mut();
        let mut result: *mut ZuResult = ptr::null_mut();
        let text = "MATCH (p:person) RETURN p.age AS a";
        assert_ne!(
            zu_query(
                conn,
                text.as_ptr().cast::<c_char>(),
                text.len(),
                &mut result,
                &mut err
            ),
            ZuStatus::Ok,
            "nothing was written, so there is no person table"
        );
        if !err.is_null() {
            zu_error_free(err);
        }
        zu_conn_close(conn);
    }
}

/// The database a C host starts from when there is no file yet.
///
/// Before this call the only way in was a bulk load, which builds a
/// database with a table in it, so a host that wanted an empty one to
/// run statements against had nowhere to begin. It is the gap building
/// a client found, which is what dx/02 §8 says building one is for.
#[test]
fn a_database_is_created_where_there_was_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("made.zu1");
    unsafe {
        let cpath = c(path.to_str().expect("utf-8 path"));
        let mut db: *mut ZuDatabase = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(
            zu_database_create_z(cpath.as_ptr(), ptr::null(), &mut db, &mut err),
            ZuStatus::Ok
        );
        assert!(!db.is_null() && err.is_null());
        assert!(path.exists(), "the file is on disk when the call returns");

        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(zu_connect(db, &mut conn, &mut err), ZuStatus::Ok);
        let result = query(conn, "RETURN 1 AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [1]);
        zu_result_free(result);
        zu_conn_close(conn);
        zu_database_close(db);

        // Creating over it again is refused, and what is there is left
        // alone: a create that opened what it found would be the call
        // that quietly writes into somebody else's data.
        let mut second: *mut ZuDatabase = ptr::null_mut();
        assert_ne!(
            zu_database_create_z(cpath.as_ptr(), ptr::null(), &mut second, &mut err),
            ZuStatus::Ok
        );
        assert!(second.is_null() && !err.is_null());
        zu_error_free(err);
        err = ptr::null_mut();

        // And the convenience beside it, which is zu_open's other half.
        let fresh = dir.path().join("made-too.zu1");
        let cfresh = c(fresh.to_str().expect("utf-8 path"));
        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(
            zu_create_z(cfresh.as_ptr(), &mut conn, &mut err),
            ZuStatus::Ok
        );
        assert!(!conn.is_null() && err.is_null());
        let result = query(conn, "RETURN 2 AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [2]);
        zu_result_free(result);
        zu_conn_close(conn);

        // The counted form is the one a binding calls, and a create in
        // a directory that is not there is the operating system's
        // refusal carried back as one, rather than a handle left
        // behind.
        let missing = dir.path().join("no-such-directory").join("x.zu1");
        let missing = missing.to_str().expect("utf-8 path");
        let mut conn: *mut ZuConn = ptr::null_mut();
        assert_eq!(
            zu_create(
                missing.as_ptr().cast::<c_char>(),
                missing.len(),
                &mut conn,
                &mut err
            ),
            ZuStatus::Io
        );
        assert!(conn.is_null() && !err.is_null());
        zu_error_free(err);
    }
}
