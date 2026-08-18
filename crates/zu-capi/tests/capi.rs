//! Exercises libzu the way a C host does: open a database, connect,
//! query, prepare, bind, execute in a loop, read columns out as
//! buffers, and free in the right order. Everything goes through the
//! extern "C" functions and raw pointers; nothing reaches into the Rust
//! types behind them.

use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use zu::{
    ZU_SEVERITY_EXCEPTION, ZU_SEVERITY_WARNING, ZU_TEMPORAL_DATE, ZU_TEMPORAL_DURATION_DAY_TIME,
    ZU_TEMPORAL_DURATION_YEAR_MONTH, ZU_TEMPORAL_LOCAL_DATETIME, ZU_TEMPORAL_LOCAL_TIME,
    ZU_TEMPORAL_ZONED_DATETIME, ZU_TEMPORAL_ZONED_TIME, ZU_TYPE_INT, ZU_TYPE_LIST, ZU_TYPE_NODE,
    ZU_TYPE_NULL, ZU_TYPE_STR, ZU_TYPE_TEMPORAL, ZuAppender, ZuConfig, ZuConn, ZuDatabase, ZuError,
    ZuLoader, ZuResult, ZuStatus, ZuStmt, ZuValue, zu_append_bool, zu_append_bytes,
    zu_append_end_row, zu_append_f64, zu_append_i64, zu_append_str_z, zu_append_temporal,
    zu_appender_buffered, zu_appender_close, zu_appender_col_name, zu_appender_cols,
    zu_appender_committed, zu_appender_discard, zu_appender_flush, zu_appender_free,
    zu_appender_open, zu_appender_open_z, zu_begin, zu_bind_bool, zu_bind_bool_z, zu_bind_i64,
    zu_bind_i64_z, zu_bind_str_z, zu_bind_temporal, zu_bind_temporal_z, zu_commit, zu_config_init,
    zu_config_set_z, zu_conn_close, zu_conn_in_transaction, zu_conn_interrupt, zu_conn_rows_read,
    zu_conn_set_progress, zu_connect, zu_create, zu_create_z, zu_database_close,
    zu_database_create_z, zu_database_open_z, zu_database_path, zu_error_code, zu_error_doc_url,
    zu_error_excerpt, zu_error_free, zu_error_message, zu_error_offset, zu_error_position,
    zu_error_retryable, zu_error_severity, zu_error_standard_text, zu_error_status, zu_execute,
    zu_loader_col_bool, zu_loader_col_f64, zu_loader_col_i64, zu_loader_col_str,
    zu_loader_col_temporal, zu_loader_create, zu_loader_edges, zu_loader_finish, zu_loader_free,
    zu_loader_table, zu_loader_table_z, zu_open, zu_open_z, zu_prepare, zu_prepare_z, zu_query,
    zu_query_z, zu_result_cell, zu_result_cell_str, zu_result_cell_type, zu_result_chunk,
    zu_result_chunk_col_f64, zu_result_chunk_col_i64, zu_result_chunk_col_node_offset,
    zu_result_chunk_col_valid, zu_result_chunk_count, zu_result_col_f64, zu_result_col_i64,
    zu_result_col_name, zu_result_col_node_offset, zu_result_col_valid, zu_result_cols,
    zu_result_free, zu_result_gqlstatus, zu_result_notice, zu_result_notices, zu_result_rows,
    zu_rollback, zu_stmt_close, zu_value_at, zu_value_bool, zu_value_f64, zu_value_i64,
    zu_value_len, zu_value_node, zu_value_str, zu_value_temporal, zu_value_type, zu_version,
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

/// The whole error model dx/03 §5 fixes, as fields: the code, the
/// standard's words, the severity, the place counted both ways, the
/// line that place is on, the page it is written up on, whether to try
/// again, and our own account of it.
#[test]
fn a_refused_statement_carries_the_whole_error_model_as_fields() {
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

        // The same place as an index into the text, for a caller that
        // slices rather than prints, and the line it is on, which is
        // what the column counts characters into.
        let mut offset = u32::MAX;
        assert_eq!(zu_error_offset(err, &mut offset), ZuStatus::Ok);
        assert_eq!(offset, 0);
        let mut excerpt_len = 0usize;
        let excerpt = zu_error_excerpt(err, &mut excerpt_len);
        assert!(!excerpt.is_null(), "a syntax error quotes its line back");
        let excerpt = CStr::from_ptr(excerpt).to_str().expect("utf-8");
        assert_eq!(excerpt, bad);
        assert_eq!(excerpt_len, excerpt.len());

        // The standard's words for the condition, which is what a
        // conformance harness grades, beside our own account of it.
        let mut standard_len = 0usize;
        let standard = CStr::from_ptr(zu_error_standard_text(err, &mut standard_len))
            .to_str()
            .expect("utf-8");
        assert_eq!(
            standard,
            "syntax error or access rule violation, invalid syntax"
        );
        assert_eq!(standard_len, standard.len());
        assert!(
            message.contains("expected"),
            "our detail says more than the standard's name: {message}"
        );

        // Where it is written up, and whether trying again could help,
        // which for text that will not parse it cannot.
        let doc = CStr::from_ptr(zu_error_doc_url(err, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert_eq!(doc, "https://zu.dev/docs/errors/42001");
        assert_eq!(zu_error_retryable(err), 0);
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
        let mut offset = 11u32;
        assert_eq!(zu_error_offset(err, &mut offset), ZuStatus::Done);
        assert_eq!(offset, 11, "an absent offset wrote something");
        // And with no place there is no line to quote, rather than an
        // empty one that reads as a blank.
        assert!(zu_error_excerpt(err, ptr::null_mut()).is_null());
        // The condition is still documented and still not worth a
        // second attempt: dividing by zero divides by zero again.
        assert!(!zu_error_doc_url(err, ptr::null_mut()).is_null());
        assert_eq!(zu_error_retryable(err), 0);
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

/// A boolean goes in as an int and comes back as the value the
/// statement was given, so a host writing a flag onto a node has a
/// binding to put it in rather than a literal it has to build the text
/// around.
#[test]
fn a_boolean_binds_from_an_int_and_reads_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("zbool.zu1");
    seeded(&path);

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let q = c("MATCH (a:person {id: 3}) RETURN $flag AS f");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
            ZuStatus::Ok
        );
        let name = c("flag");
        for (bound, want) in [(1, 1), (0, 0), (-7, 1)] {
            assert_eq!(zu_bind_bool_z(stmt, name.as_ptr(), bound), ZuStatus::Ok);
            let mut result: *mut ZuResult = ptr::null_mut();
            assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
            let mut got = -1i32;
            assert_eq!(zu_value_bool(cell(result, 0, 0), &mut got), ZuStatus::Ok);
            assert_eq!(got, want, "bound {bound}");
            zu_result_free(result);
        }
        // The counted form takes the same name and means the same thing.
        assert_eq!(zu_bind_bool(stmt, name.as_ptr(), 4, 0), ZuStatus::Ok);
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
        let mut got = -1i32;
        assert_eq!(zu_value_bool(cell(result, 0, 0), &mut got), ZuStatus::Ok);
        assert_eq!(got, 0);
        zu_result_free(result);
        zu_stmt_close(stmt);
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
        assert!(zu_error_standard_text(ptr::null(), ptr::null_mut()).is_null());
        assert!(zu_error_doc_url(ptr::null(), ptr::null_mut()).is_null());
        assert!(zu_error_excerpt(ptr::null(), ptr::null_mut()).is_null());
        assert_eq!(zu_error_retryable(ptr::null()), -1);
        assert_eq!(
            zu_error_position(ptr::null(), ptr::null_mut(), ptr::null_mut()),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_error_offset(ptr::null(), ptr::null_mut()),
            ZuStatus::Misuse
        );
        // A length asked of a NULL handle is a zero rather than the
        // number that was in the caller's variable before the call.
        let mut len = 7usize;
        assert!(zu_error_excerpt(ptr::null(), &mut len).is_null());
        assert_eq!(len, 0);
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

/// The two kinds of value a C host could read out of a result and had
/// no way to put back in. A parameter is how a client passes a value it
/// was given, so a boolean or a date that can only travel one way is a
/// statement a client has to build by pasting text together.
#[test]
fn a_bool_and_every_temporal_kind_bind_as_parameters_and_come_back_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bindings.zu1");
    seeded(&path);

    // The same six kinds and counts the reader is checked against
    // above, plus the zoned datetime, so that what goes in through
    // zu_bind_temporal and what comes out through zu_value_temporal are
    // compared over every kind there is.
    let temporals: [(i32, i64, i32); 7] = [
        (ZU_TEMPORAL_DATE, 19782, 0),
        (ZU_TEMPORAL_LOCAL_TIME, 45_296_123_456_789, 0),
        (ZU_TEMPORAL_ZONED_TIME, 45_296_000_000_000, 420),
        (ZU_TEMPORAL_LOCAL_DATETIME, 1_704_067_200_000_000_000, 0),
        (ZU_TEMPORAL_ZONED_DATETIME, 1_704_067_200_000_000_000, -330),
        (ZU_TEMPORAL_DURATION_YEAR_MONTH, 14, 0),
        (ZU_TEMPORAL_DURATION_DAY_TIME, 5_400_000_000_000, 0),
    ];

    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let name = c("v");

        for (want_kind, want_count, want_offset) in temporals {
            let q = c("RETURN $v AS v");
            let mut stmt: *mut ZuStmt = ptr::null_mut();
            assert_eq!(
                zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
                ZuStatus::Ok
            );
            assert_eq!(
                zu_bind_temporal_z(stmt, name.as_ptr(), want_kind, want_count, want_offset),
                ZuStatus::Ok,
                "kind {want_kind}"
            );
            let mut result: *mut ZuResult = ptr::null_mut();
            assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
            let value = cell(result, 0, 0);
            assert_eq!(zu_value_type(value), ZU_TYPE_TEMPORAL, "kind {want_kind}");
            let (mut kind, mut count, mut offset) = (-1, 0i64, i32::MAX);
            assert_eq!(
                zu_value_temporal(value, &mut kind, &mut count, &mut offset),
                ZuStatus::Ok
            );
            assert_eq!(
                (kind, count, offset),
                (want_kind, want_count, want_offset),
                "kind {want_kind}"
            );
            zu_result_free(result);
            zu_stmt_close(stmt);
        }

        // Nonzero is true, which is what a C caller handing over a
        // comparison expects, so 2 is the same binding as 1.
        for (bound, want) in [(1, true), (0, false), (2, true)] {
            let q = c("RETURN $v AS v");
            let mut stmt: *mut ZuStmt = ptr::null_mut();
            assert_eq!(
                zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
                ZuStatus::Ok
            );
            assert_eq!(zu_bind_bool_z(stmt, name.as_ptr(), bound), ZuStatus::Ok);
            let mut result: *mut ZuResult = ptr::null_mut();
            assert_eq!(zu_execute(stmt, &mut result, &mut err), ZuStatus::Ok);
            let value = cell(result, 0, 0);
            let mut got = -1;
            assert_eq!(zu_value_bool(value, &mut got), ZuStatus::Ok);
            assert_eq!(got != 0, want, "bound {bound}");
            zu_result_free(result);
            zu_stmt_close(stmt);
        }

        // A kind that is not one of the seven, an offset wider than the
        // minutes an offset is kept in, and a date further from the
        // epoch than a day count reaches. All three are a caller with a
        // unit confusion, and a binding that wrapped one into a value
        // the engine accepts would answer a different statement without
        // saying so.
        let q = c("RETURN $v AS v");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(conn, q.as_ptr(), &mut stmt, &mut err),
            ZuStatus::Ok
        );
        for (kind, count, offset) in [
            (7, 0, 0),
            (-1, 0, 0),
            (ZU_TEMPORAL_ZONED_TIME, 0, 40_000),
            (ZU_TEMPORAL_DATE, i64::from(i32::MAX) + 1, 0),
        ] {
            assert_eq!(
                zu_bind_temporal_z(stmt, name.as_ptr(), kind, count, offset),
                ZuStatus::Misuse,
                "kind {kind} count {count} offset {offset}"
            );
        }
        zu_stmt_close(stmt);

        assert_eq!(
            zu_bind_bool(ptr::null_mut(), ptr::null(), 0, 1),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_bind_temporal(ptr::null_mut(), ptr::null(), 0, ZU_TEMPORAL_DATE, 0, 0),
            ZuStatus::Misuse
        );

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
        assert_eq!(
            zu_query(
                conn,
                text.as_ptr().cast::<c_char>(),
                text.len(),
                &mut result,
                &mut err
            ),
            ZuStatus::Ok
        );
        assert!(err.is_null());
        assert_eq!(
            zu_result_rows(result),
            0,
            "nothing was written, so there is no person to find"
        );
        zu_result_free(result);
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

/// A pointer this test hands to another thread on purpose.
///
/// dx/02 §5 says a connection may not be used from two threads at
/// once, and stopping one is the one call exempt from that, so a test
/// of it has to do the thing the rest of the surface forbids.
struct Handed(*mut ZuConn);

// SAFETY: the second thread calls only zu_conn_interrupt and
// zu_conn_rows_read on it, which are the two calls documented as safe
// to make while the first thread is inside one.
unsafe impl Send for Handed {}

/// A query with enough work in it that a stop lands while it is
/// running rather than after it: every edge against every edge, which
/// is tens of millions of row visits and answers one number.
const LONG: &str = "MATCH (a:person)-[:follows]->(b:person), \
                    (c:person)-[:follows]->(d:person) RETURN count(*) AS n";

/// The nodes behind that query. Enough that the statement takes
/// seconds if nothing stops it, so a stop that never arrived fails the
/// assertion rather than passing by finishing first.
const CROSS_NODES: u32 = 6000;

#[test]
fn a_statement_stopped_from_another_thread_leaves_the_connection_warm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stop.zu1");
    seeded_wide(&path, CROSS_NODES);
    unsafe {
        let conn = open(&path);
        let handed = Handed(conn);
        let stopped = std::thread::scope(|scope| {
            let asking = scope.spawn(move || {
                // Waits for the statement to be reading rather than
                // sleeping for a guessed interval, so the ask lands
                // inside the run on a slow machine and a fast one.
                let handed = handed;
                let mut rows = 0u64;
                let start = std::time::Instant::now();
                while rows == 0 && start.elapsed() < std::time::Duration::from_secs(30) {
                    assert_eq!(zu_conn_rows_read(handed.0, &mut rows), ZuStatus::Ok);
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                assert!(rows > 0, "the statement never started reading");
                let asked = std::time::Instant::now();
                assert_eq!(zu_conn_interrupt(handed.0), ZuStatus::Ok);
                (rows, asked)
            });
            let mut result: *mut ZuResult = ptr::null_mut();
            let mut err: *mut ZuError = ptr::null_mut();
            let status = zu_query(
                conn,
                LONG.as_ptr().cast::<c_char>(),
                LONG.len(),
                &mut result,
                &mut err,
            );
            let felt = std::time::Instant::now();
            let (seen, asked) = asking.join().expect("the asking thread");
            (status, result, err, seen, felt.duration_since(asked))
        });
        let (status, result, err, seen, took) = stopped;
        assert_eq!(status, ZuStatus::Interrupted, "the statement was stopped");
        // dx/02 asks for fifty milliseconds from the ask to the return,
        // and the executor reads the flag at the boundary of a chunk,
        // which is a fraction of a millisecond of work. The margin is
        // for a machine running the whole suite at once, not for the
        // engine.
        assert!(
            took < std::time::Duration::from_millis(50),
            "the ask took {took:?} to land"
        );
        assert!(result.is_null(), "a stopped statement has no result");
        // Stopping is not failing, but it is still reported as an
        // error handle, and what it says is that it stopped.
        assert!(!err.is_null());
        assert_eq!(zu_error_status(err), ZuStatus::Interrupted);
        zu_error_free(err);

        // How far it got is still readable after the call returned,
        // which is what a host prints beside "cancelled".
        let mut rows = 0u64;
        assert_eq!(zu_conn_rows_read(conn, &mut rows), ZuStatus::Ok);
        assert!(rows >= seen, "the count only goes up within a statement");

        // And the connection is exactly as it was: same plans, same
        // caches, next statement runs.
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(conn, "MATCH (p:person) RETURN count(*) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [i64::from(CROSS_NODES)]);
        zu_result_free(result);

        // The count restarted with that statement rather than carrying
        // the stopped one's total forward.
        let mut rows = 0u64;
        assert_eq!(zu_conn_rows_read(conn, &mut rows), ZuStatus::Ok);
        assert!(
            rows <= u64::from(CROSS_NODES),
            "{rows} rows is the count from the statement before"
        );
        zu_conn_close(conn);
    }
}

/// What a host's callback records: how many times it was called, and
/// the arguments of the first call.
#[derive(Default)]
struct Reports {
    calls: std::sync::atomic::AtomicU64,
    rows: std::sync::atomic::AtomicU64,
    keep_going: AtomicBool,
}

unsafe extern "C" fn report(user: *mut std::ffi::c_void, rows: u64, _ms: u64) -> std::ffi::c_int {
    let reports = unsafe { &*user.cast::<Reports>() };
    reports.calls.fetch_add(1, Ordering::Relaxed);
    reports.rows.store(rows, Ordering::Relaxed);
    i32::from(reports.keep_going.load(Ordering::Relaxed))
}

#[test]
fn a_host_that_asked_to_be_told_can_stop_the_statement_from_the_callback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("progress.zu1");
    seeded_wide(&path, CROSS_NODES);
    let reports = Reports::default();
    unsafe {
        let conn = open(&path);
        let user: *mut std::ffi::c_void = std::ptr::from_ref(&reports)
            .cast_mut()
            .cast::<std::ffi::c_void>();
        assert_eq!(
            zu_conn_set_progress(conn, Some(report), user, 1),
            ZuStatus::Ok
        );

        let mut result: *mut ZuResult = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        let status = zu_query(
            conn,
            LONG.as_ptr().cast::<c_char>(),
            LONG.len(),
            &mut result,
            &mut err,
        );
        assert_eq!(status, ZuStatus::Interrupted, "the callback said stop");
        assert!(result.is_null());
        assert!(!err.is_null());
        zu_error_free(err);
        assert!(
            reports.calls.load(Ordering::Relaxed) >= 1,
            "the host was never told anything"
        );

        // A callback that lets the statement run sees it through, and
        // what it is told is the rows the statement read.
        reports.keep_going.store(true, Ordering::Relaxed);
        reports.calls.store(0, Ordering::Relaxed);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(conn, "MATCH (p:person) RETURN count(*) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1), [i64::from(CROSS_NODES)]);
        zu_result_free(result);

        // Taking the arrangement back stops the reports, and the
        // statement after it runs with nothing watching.
        assert_eq!(
            zu_conn_set_progress(conn, None, ptr::null_mut(), 0),
            ZuStatus::Ok
        );
        reports.calls.store(0, Ordering::Relaxed);
        let result = query(conn, "MATCH (p:person) RETURN count(*) AS n", &mut err);
        zu_result_free(result);
        assert_eq!(
            reports.calls.load(Ordering::Relaxed),
            0,
            "a callback that was taken back was still called"
        );
        zu_conn_close(conn);
    }
}

#[test]
fn the_ways_of_asking_wrongly_are_all_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("misuse.zu1");
    seeded(&path);
    unsafe {
        let mut rows = 12u64;
        assert_eq!(zu_conn_interrupt(ptr::null_mut()), ZuStatus::Misuse);
        assert_eq!(
            zu_conn_rows_read(ptr::null_mut(), &mut rows),
            ZuStatus::Misuse
        );
        assert_eq!(rows, 0, "the out-parameter is written on every path");
        let conn = open(&path);
        assert_eq!(zu_conn_rows_read(conn, ptr::null_mut()), ZuStatus::Misuse);
        assert_eq!(
            zu_conn_set_progress(ptr::null_mut(), Some(report), ptr::null_mut(), 10),
            ZuStatus::Misuse
        );
        // A period of nothing is not a period, and reading it as one
        // would be a thread calling a host as fast as it can.
        assert_eq!(
            zu_conn_set_progress(conn, Some(report), ptr::null_mut(), 0),
            ZuStatus::Misuse
        );
        // Taking the arrangement back needs no period at all.
        assert_eq!(
            zu_conn_set_progress(conn, None, ptr::null_mut(), 0),
            ZuStatus::Ok
        );

        // Asking about a connection that is not running anything is
        // not a mistake: nothing is stopped and nothing has been read.
        assert_eq!(zu_conn_interrupt(conn), ZuStatus::Ok);
        assert_eq!(zu_conn_rows_read(conn, &mut rows), ZuStatus::Ok);
        assert_eq!(rows, 0);
        zu_conn_close(conn);
    }
}

/* ---- transactions ---- */

/// How many people there are, which is what every test below watches
/// across a boundary.
unsafe fn people(conn: *mut ZuConn) -> i64 {
    let mut err: *mut ZuError = ptr::null_mut();
    let result = unsafe { query(conn, "MATCH (p:person) RETURN count(p) AS n", &mut err) };
    let n = unsafe { col_i64(result, 0, 1) }[0];
    unsafe { zu_result_free(result) };
    n
}

/// The condition a refused call carries, freed on the way out because
/// the caller of this only wants the code.
unsafe fn code_of(err: *mut ZuError) -> String {
    assert!(!err.is_null(), "a refusal with no error handle");
    let mut len = 0usize;
    let code = unsafe { zu_error_code(err, &mut len) };
    assert!(!code.is_null(), "a refusal with no condition");
    let code = unsafe { CStr::from_ptr(code) }
        .to_str()
        .expect("utf-8")
        .to_string();
    unsafe { zu_error_free(err) };
    code
}

unsafe fn in_transaction(conn: *mut ZuConn) -> bool {
    let mut out = -1i32;
    assert_eq!(
        unsafe { zu_conn_in_transaction(conn, &mut out) },
        ZuStatus::Ok
    );
    assert!(out == 0 || out == 1, "a flag answered {out}");
    out == 1
}

/// A database with two people in it, made through the calls a C host
/// has: create an empty one and write the table with statements.
unsafe fn two_people(path: &std::path::Path) -> *mut ZuConn {
    let path = path.to_str().expect("utf-8 path");
    let mut conn: *mut ZuConn = ptr::null_mut();
    let mut err: *mut ZuError = ptr::null_mut();
    assert_eq!(
        unsafe {
            zu_create(
                path.as_ptr().cast::<c_char>(),
                path.len(),
                &mut conn,
                &mut err,
            )
        },
        ZuStatus::Ok
    );
    for name in ["ada", "grace"] {
        let text = format!("INSERT (p:person {{name: '{name}'}})");
        let result = unsafe { query(conn, &text, &mut err) };
        unsafe { zu_result_free(result) };
    }
    conn
}

/// What the three calls are for: several statements that are one
/// transaction, kept together or unmade together.
#[test]
fn several_statements_commit_as_one_and_roll_back_as_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("txn.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        assert!(!in_transaction(conn));

        // Rolled back: two statements that both ran, neither of which
        // is there afterwards.
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert!(in_transaction(conn));
        for name in ["zoe", "raj"] {
            let text = format!("INSERT (p:person {{name: '{name}'}})");
            let result = query(conn, &text, &mut err);
            zu_result_free(result);
        }
        assert_eq!(people(conn), 4, "a transaction sees its own writes");
        assert_eq!(zu_rollback(conn, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert!(!in_transaction(conn));
        assert_eq!(people(conn), 2, "the rollback took both statements");

        // Committed: the same two statements, kept.
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        for name in ["zoe", "raj"] {
            let text = format!("INSERT (p:person {{name: '{name}'}})");
            let result = query(conn, &text, &mut err);
            zu_result_free(result);
        }
        assert_eq!(zu_commit(conn, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert!(!in_transaction(conn));
        assert_eq!(people(conn), 4);
        zu_conn_close(conn);

        // And the commit is what a reopen finds, which is the whole of
        // what a commit promises.
        let conn = open(&path);
        assert_eq!(people(conn), 4);
        zu_conn_close(conn);
    }
}

/// The same three words, sent as statements, because these calls are
/// those statements rather than a second mechanism beside them. A host
/// that mixes the two, which is what a driver wrapping user text in a
/// block does, has to be able to.
#[test]
fn the_calls_and_the_statements_they_stand_for_are_one_transaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mixed.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // Begun by the call, ended by the text.
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        let result = query(conn, "INSERT (p:person {name: 'zoe'})", &mut err);
        zu_result_free(result);
        let result = query(conn, "ROLLBACK", &mut err);
        zu_result_free(result);
        assert!(!in_transaction(conn), "the text ended what the call began");
        assert_eq!(people(conn), 2);

        // Begun by the text, ended by the call, and the flag follows
        // the text as readily as it follows the call.
        let result = query(conn, "START TRANSACTION", &mut err);
        zu_result_free(result);
        assert!(in_transaction(conn));
        let result = query(conn, "INSERT (p:person {name: 'zoe'})", &mut err);
        zu_result_free(result);
        assert_eq!(zu_commit(conn, &mut err), ZuStatus::Ok);
        assert_eq!(people(conn), 3);
        zu_conn_close(conn);
    }
}

/// READ ONLY is enforced rather than advisory, and it is enforced at
/// the statement that wrote rather than at the commit, so a host is
/// told which statement was the one it should not have sent.
#[test]
fn a_read_only_transaction_turns_a_write_away_at_the_statement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read-only-txn.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        assert_eq!(zu_begin(conn, 1, &mut err), ZuStatus::Ok);
        assert!(in_transaction(conn));
        assert_eq!(people(conn), 2, "a read only transaction reads");

        let write = "INSERT (p:person {name: 'zoe'})";
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_query(
                conn,
                write.as_ptr().cast::<c_char>(),
                write.len(),
                &mut result,
                &mut err
            ),
            ZuStatus::Error
        );
        assert!(result.is_null());
        assert_eq!(code_of(err), "25G03");
        err = ptr::null_mut();

        // The transaction is still running and still ends normally: a
        // statement it refused did not end it.
        assert!(in_transaction(conn));
        assert_eq!(zu_commit(conn, &mut err), ZuStatus::Ok);
        assert_eq!(people(conn), 2);
        zu_conn_close(conn);
    }
}

/// The three ways of asking for a transaction that cannot be had, each
/// of which is a condition rather than a call that quietly did nothing.
#[test]
fn a_transaction_does_not_nest_and_neither_word_ends_one_that_is_not_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nesting.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // Ending nothing. A host that rolls back in an error path
        // wants to hear that the transaction it meant to undo was not
        // the one it thought.
        assert_eq!(zu_commit(conn, &mut err), ZuStatus::Error);
        assert_eq!(code_of(err), "2D000");
        err = ptr::null_mut();
        assert_eq!(zu_rollback(conn, &mut err), ZuStatus::Error);
        assert_eq!(code_of(err), "2D000");
        err = ptr::null_mut();
        assert!(!in_transaction(conn));

        // Beginning twice, which is a nesting this engine does not
        // have and will not pretend to.
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Error);
        assert_eq!(code_of(err), "25G01");
        err = ptr::null_mut();
        assert!(in_transaction(conn), "the refused begin left the first one");
        assert_eq!(zu_rollback(conn, &mut err), ZuStatus::Ok);

        // An error handle is optional here as everywhere: a host that
        // only branches on the status passes NULL and leaks nothing.
        assert_eq!(zu_commit(conn, ptr::null_mut()), ZuStatus::Error);
        zu_conn_close(conn);
    }
}

/// Closing inside a transaction rolls it back, which is the answer
/// that does not depend on a destructor running: a host that failed
/// halfway and dropped everything gets the database it had before.
#[test]
fn a_connection_closed_inside_a_transaction_keeps_nothing_it_wrote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("closed-open.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        let result = query(conn, "INSERT (p:person {name: 'zoe'})", &mut err);
        zu_result_free(result);
        zu_conn_close(conn);

        let conn = open(&path);
        assert_eq!(people(conn), 2, "the open transaction went with the close");
        assert!(
            !in_transaction(conn),
            "and a fresh connection is not in one"
        );
        zu_conn_close(conn);
    }
}

/// The same misuse the rest of the surface answers, on the four calls
/// this section adds: nothing crashes, and the status says which
/// mistake it was.
#[test]
fn the_transaction_calls_answer_a_null_handle_and_a_closed_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("txn-misuse.zu1");
    unsafe {
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(zu_begin(ptr::null_mut(), 0, &mut err), ZuStatus::Misuse);
        assert_eq!(zu_commit(ptr::null_mut(), &mut err), ZuStatus::Misuse);
        assert_eq!(zu_rollback(ptr::null_mut(), &mut err), ZuStatus::Misuse);
        let mut flag = -1i32;
        assert_eq!(
            zu_conn_in_transaction(ptr::null_mut(), &mut flag),
            ZuStatus::Misuse
        );
        assert_eq!(flag, 0, "the out-parameter is written on every path");

        let conn = two_people(&path);
        assert_eq!(
            zu_conn_in_transaction(conn, ptr::null_mut()),
            ZuStatus::Misuse
        );

        // A statement prepared here and used after the close is the
        // one that answers ZU_MISUSE_CLOSED, because a statement keeps
        // the connection's state alive to be able to; the connection
        // handle itself is gone once it is closed, like every other
        // handle this header frees.
        let text = "MATCH (p:person) RETURN count(p) AS n";
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare(
                conn,
                text.as_ptr().cast::<c_char>(),
                text.len(),
                &mut stmt,
                &mut err
            ),
            ZuStatus::Ok
        );
        assert_eq!(zu_begin(conn, 0, &mut err), ZuStatus::Ok);
        zu_conn_close(conn);
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_execute(stmt, &mut result, &mut err),
            ZuStatus::MisuseClosed
        );
        assert!(result.is_null());
        zu_stmt_close(stmt);
    }
}

/* ---- appending ---- */

/// A database whose person table has a column of every type an appender
/// takes, declared by the one means a C host has for declaring a table
/// at all: writing a row of it. The five columns come out in the order
/// the statement named them, which is the order every row below is
/// written in.
unsafe fn five_columns(path: &std::path::Path) -> *mut ZuConn {
    let path = path.to_str().expect("utf-8 path");
    let mut conn: *mut ZuConn = ptr::null_mut();
    let mut err: *mut ZuError = ptr::null_mut();
    assert_eq!(
        unsafe {
            zu_create(
                path.as_ptr().cast::<c_char>(),
                path.len(),
                &mut conn,
                &mut err,
            )
        },
        ZuStatus::Ok
    );
    let text = "INSERT (p:person {uid: 1, name: 'ada', born: DATE '2024-03-01', \
                ok: true, score: 1.5})";
    let result = unsafe { query(conn, text, &mut err) };
    unsafe { zu_result_free(result) };
    conn
}

unsafe fn appender(conn: *mut ZuConn, table: &str) -> *mut ZuAppender {
    let table = c(table);
    let mut app: *mut ZuAppender = ptr::null_mut();
    let mut err: *mut ZuError = ptr::null_mut();
    assert_eq!(
        unsafe { zu_appender_open_z(conn, table.as_ptr(), &mut app, &mut err) },
        ZuStatus::Ok
    );
    assert!(err.is_null());
    assert!(!app.is_null());
    app
}

/// One row of the five-column table, a value at a time and ended, which
/// is the loop this whole section exists for.
unsafe fn five(app: *mut ZuAppender, uid: i64, name: &str, born: i64, ok: i32, score: f64) {
    let mut err: *mut ZuError = ptr::null_mut();
    let name = c(name);
    unsafe {
        assert_eq!(zu_append_i64(app, uid, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_str_z(app, name.as_ptr(), &mut err), ZuStatus::Ok);
        assert_eq!(
            zu_append_temporal(app, ZU_TEMPORAL_DATE, born, &mut err),
            ZuStatus::Ok
        );
        assert_eq!(zu_append_bool(app, ok, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_f64(app, score, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_end_row(app, &mut err), ZuStatus::Ok);
    }
    assert!(err.is_null(), "a row that went in left an error behind");
}

/// What a refused call said, freed on the way out for the reason
/// [`code_of`] frees it.
unsafe fn message_of(err: *mut ZuError) -> String {
    assert!(!err.is_null(), "a refusal with no error handle");
    let mut len = 0usize;
    let text = unsafe { zu_error_message(err, &mut len) };
    assert!(!text.is_null(), "a refusal with no message");
    let text = unsafe { CStr::from_ptr(text) }
        .to_str()
        .expect("utf-8")
        .to_string();
    assert_eq!(len, text.len());
    unsafe { zu_error_free(err) };
    text
}

unsafe fn buffered(app: *mut ZuAppender) -> u64 {
    let mut out = u64::MAX;
    assert_eq!(unsafe { zu_appender_buffered(app, &mut out) }, ZuStatus::Ok);
    out
}

unsafe fn committed(app: *mut ZuAppender) -> u64 {
    let mut out = u64::MAX;
    assert_eq!(
        unsafe { zu_appender_committed(app, &mut out) },
        ZuStatus::Ok
    );
    out
}

/// The columns of an appender, counted and named, which is what a host
/// checks its own column order against.
unsafe fn shape_of(app: *mut ZuAppender) -> Vec<String> {
    let mut cols = u32::MAX;
    assert_eq!(unsafe { zu_appender_cols(app, &mut cols) }, ZuStatus::Ok);
    (0..cols)
        .map(|col| {
            let mut len = 0usize;
            let name = unsafe { zu_appender_col_name(app, col, &mut len) };
            assert!(!name.is_null(), "column {col} of {cols} has no name");
            let name = unsafe { CStr::from_ptr(name) }
                .to_str()
                .expect("utf-8")
                .to_string();
            assert_eq!(len, name.len(), "the length and the NUL have to agree");
            name
        })
        .collect()
}

/// How many edges there are, which is what the rel table test watches.
unsafe fn edges(conn: *mut ZuConn) -> i64 {
    let mut err: *mut ZuError = ptr::null_mut();
    let result = unsafe {
        query(
            conn,
            "MATCH ()-[e:follows]->() RETURN count(e) AS n",
            &mut err,
        )
    };
    let n = unsafe { col_i64(result, 0, 1) }[0];
    unsafe { zu_result_free(result) };
    n
}

/// What the whole thing is for: rows written a value at a time, buffered
/// until they are asked for, and put in by one commit that every later
/// statement sees.
#[test]
fn rows_appended_a_value_at_a_time_go_in_at_one_flush() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append.zu1");
    unsafe {
        let conn = five_columns(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "person");

        five(app, 10, "grace", 19800, 1, 2.5);
        five(app, 11, "zoe", 19801, 0, -0.5);
        five(app, 12, "raj", 19802, 1, 0.0);
        assert_eq!(buffered(app), 3);
        assert_eq!(committed(app), 0);
        assert_eq!(people(conn), 1, "nothing is there before the flush");

        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert_eq!(buffered(app), 0);
        assert_eq!(committed(app), 3);
        assert_eq!(people(conn), 4);

        // Every value of every column, read back the way it went in.
        let result = query(conn, "MATCH (p:person) RETURN p.uid AS uid", &mut err);
        assert_eq!(col_i64(result, 0, 4), [1, 10, 11, 12]);
        zu_result_free(result);
        let result = query(
            conn,
            "MATCH (p:person) WHERE p.uid = 11 \
             RETURN p.name AS name, p.born AS born, p.ok AS ok, p.score AS score",
            &mut err,
        );
        let mut len = 0usize;
        let mut name: *const c_char = ptr::null();
        assert_eq!(
            zu_result_cell_str(result, 0, 0, &mut name, &mut len),
            ZuStatus::Ok
        );
        assert_eq!(CStr::from_ptr(name).to_str(), Ok("zoe"));
        let mut kind = -1i32;
        let mut count = 0i64;
        let mut offset = 1i32;
        assert_eq!(
            zu_value_temporal(cell(result, 0, 1), &mut kind, &mut count, &mut offset),
            ZuStatus::Ok
        );
        assert_eq!((kind, count), (ZU_TEMPORAL_DATE, 19801));
        let mut ok = -1i32;
        assert_eq!(zu_value_bool(cell(result, 0, 2), &mut ok), ZuStatus::Ok);
        assert_eq!(ok, 0);
        let mut score = f64::NAN;
        assert_eq!(zu_value_f64(cell(result, 0, 3), &mut score), ZuStatus::Ok);
        assert_eq!(score, -0.5);
        zu_result_free(result);

        // A flush with nothing buffered is not a commit, and says so by
        // leaving the count where it was.
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Ok);
        assert_eq!(committed(app), 3);

        zu_appender_free(app);
        zu_conn_close(conn);

        // And the flush is what a reopen finds, which is what a commit
        // promises.
        let conn = open(&path);
        assert_eq!(people(conn), 4);
        zu_conn_close(conn);
    }
}

/// A value that does not belong in the column it landed in, which is
/// the mistake a host writing by position actually makes.
#[test]
fn a_value_a_column_will_not_take_ends_its_row_and_names_the_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wrong-value.zu1");
    unsafe {
        let conn = five_columns(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "person");

        // A string where the first column takes integers, named both
        // ways round: what the value was and what the column holds.
        let text = c("nope");
        assert_eq!(
            zu_append_str_z(app, text.as_ptr(), &mut err),
            ZuStatus::Misuse
        );
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(message.contains("column 'uid' of 'person'"), "{message}");
        assert!(message.contains("a string"), "{message}");
        assert!(message.contains("integers"), "{message}");

        // Two values in and the third refused, which is the case that
        // has something to take back off.
        assert_eq!(zu_append_i64(app, 10, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_str_z(app, text.as_ptr(), &mut err), ZuStatus::Ok);
        assert_eq!(
            zu_append_bytes(app, b"raw".as_ptr(), 3, &mut err),
            ZuStatus::Misuse
        );
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(message.contains("column 'born' of 'person'"), "{message}");
        assert!(message.contains("bytes"), "{message}");
        assert!(message.contains("dates"), "{message}");
        assert_eq!(buffered(app), 0, "a row that was refused was never a row");

        // A temporal of a kind no stored column can hold, and a kind
        // that is no kind: values that are not values at all, which end
        // their row the way a value the column refused does. The next
        // call is the next value of a row, and a row left half written
        // would put it in the wrong column.
        assert_eq!(zu_append_i64(app, 10, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_str_z(app, text.as_ptr(), &mut err), ZuStatus::Ok);
        assert_eq!(
            zu_append_temporal(app, ZU_TEMPORAL_ZONED_DATETIME, 0, &mut err),
            ZuStatus::Unsupported
        );
        assert!(message_of(err).contains("zoned"));
        err = ptr::null_mut();
        assert_eq!(zu_append_i64(app, 10, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_str_z(app, text.as_ptr(), &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_temporal(app, 99, 0, &mut err), ZuStatus::Misuse);
        assert!(message_of(err).contains("99"));
        err = ptr::null_mut();

        // And the appender is still the appender: a good row after all
        // of that goes in on its own, which it could not if any of the
        // refused ones had left a value behind.
        five(app, 10, "grace", 19800, 1, 2.5);
        assert_eq!(buffered(app), 1);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Ok);
        assert_eq!(people(conn), 2);

        zu_appender_free(app);
        zu_conn_close(conn);
    }
}

/// A row that is not the width of the table, both ways round: too few
/// values when it was ended, and one value too many before it was.
#[test]
fn a_row_of_the_wrong_width_is_refused_with_nothing_of_it_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("width.zu1");
    unsafe {
        let conn = five_columns(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "person");

        assert_eq!(zu_append_i64(app, 10, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_end_row(app, &mut err), ZuStatus::Misuse);
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(
            message.contains("carries 1 value and 'person' takes 5"),
            "{message}"
        );
        assert!(
            message.contains("uid, name, born, ok, score"),
            "a host that miscounted is shown what the count is made of: {message}"
        );
        assert_eq!(buffered(app), 0);

        // A sixth value on a five-column row, refused at the value
        // rather than left to make the buffers ragged.
        let name = c("grace");
        assert_eq!(zu_append_i64(app, 10, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_str_z(app, name.as_ptr(), &mut err), ZuStatus::Ok);
        assert_eq!(
            zu_append_temporal(app, ZU_TEMPORAL_DATE, 19800, &mut err),
            ZuStatus::Ok
        );
        assert_eq!(zu_append_bool(app, 1, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_f64(app, 2.5, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_i64(app, 99, &mut err), ZuStatus::Misuse);
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(
            message.contains("already carries the 5 values"),
            "{message}"
        );
        assert_eq!(buffered(app), 0, "the row it was too many for is gone too");

        // A row that was started and never ended is not a row, and the
        // flush takes it back off rather than writing a short one.
        five(app, 10, "grace", 19800, 1, 2.5);
        assert_eq!(zu_append_i64(app, 11, &mut err), ZuStatus::Ok);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert_eq!(committed(app), 1);
        assert_eq!(people(conn), 2);

        zu_appender_free(app);
        zu_conn_close(conn);
    }
}

/// A row is written by position and the columns are read by name, so a
/// host that wants to check the order it is writing in can.
#[test]
fn the_columns_a_row_carries_are_there_to_be_read_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("columns.zu1");
    unsafe {
        let conn = five_columns(&path);
        let app = appender(conn, "person");
        assert_eq!(shape_of(app), ["uid", "name", "born", "ok", "score"]);

        // Out of range is NULL rather than a name that is not there,
        // and the length is written on that path too.
        let mut len = usize::MAX;
        assert!(zu_appender_col_name(app, 5, &mut len).is_null());
        assert_eq!(len, 0);
        assert!(zu_appender_col_name(app, u32::MAX, ptr::null_mut()).is_null());

        zu_appender_free(app);
        zu_conn_close(conn);
    }
}

/// The two ways of ending a load that is not going to be flushed by
/// hand: throw the rows away, or let the free write them.
#[test]
fn a_discard_throws_the_rows_away_and_a_free_writes_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("discard.zu1");
    unsafe {
        let conn = five_columns(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "person");

        five(app, 10, "grace", 19800, 1, 2.5);
        five(app, 11, "zoe", 19801, 0, -0.5);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Ok);

        five(app, 12, "raj", 19802, 1, 0.0);
        five(app, 13, "ida", 19803, 1, 1.0);
        let mut dropped = u64::MAX;
        assert_eq!(zu_appender_discard(app, &mut dropped), ZuStatus::Ok);
        assert_eq!(dropped, 2);
        assert_eq!(buffered(app), 0);
        assert_eq!(
            committed(app),
            2,
            "rows an earlier flush committed are committed"
        );
        assert_eq!(people(conn), 3);

        // A discard with nothing buffered is nothing, and out may be
        // NULL for a host that does not care how many it was.
        assert_eq!(zu_appender_discard(app, ptr::null_mut()), ZuStatus::Ok);
        assert_eq!(buffered(app), 0);

        // And the free is the other answer: rows that were appended are
        // rows the host meant to write.
        five(app, 14, "kay", 19804, 0, 3.5);
        zu_appender_free(app);
        assert_eq!(people(conn), 4);
        zu_conn_close(conn);
    }
}

/// A rel table has no property columns: a row of one is the two ends of
/// an edge, and the two things that can be wrong with one are answered
/// in the two different places they are known.
#[test]
fn a_rel_table_takes_the_two_ends_of_an_edge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("edges.zu1");
    seeded(&path);
    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "follows");
        assert_eq!(shape_of(app), ["from person", "to person"]);
        let before = edges(conn);

        // An offset that is no row of anything, refused by the call that
        // appended it: the ingest takes the two ends as counts, so a
        // negative one would reach it as an enormous positive one and be
        // reported as an edge to a row that is not there.
        assert_eq!(zu_append_i64(app, -1, &mut err), ZuStatus::Misuse);
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(message.contains("'from person' of 'follows'"), "{message}");
        assert!(message.contains("count from zero"), "{message}");

        // An edge to a row that is not there, refused at the flush,
        // before anything is written and with the rows still buffered.
        assert_eq!(zu_append_i64(app, 3, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_i64(app, 9999, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_end_row(app, &mut err), ZuStatus::Ok);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::Misuse);
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(message.contains("(3, 9999)"), "{message}");
        assert_eq!(buffered(app), 1, "a flush that failed keeps its rows");
        assert_eq!(committed(app), 0);
        assert_eq!(edges(conn), before, "and the file is left as it was");

        let mut dropped = 0u64;
        assert_eq!(zu_appender_discard(app, &mut dropped), ZuStatus::Ok);
        assert_eq!(dropped, 1);

        // An edge between two rows that are there, which goes in.
        assert_eq!(zu_append_i64(app, 3, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_i64(app, 96, &mut err), ZuStatus::Ok);
        assert_eq!(zu_append_end_row(app, &mut err), ZuStatus::Ok);
        let mut total = 0u64;
        assert_eq!(zu_appender_close(app, &mut total, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert_eq!(total, 1);
        assert_eq!(edges(conn), before + 1);

        zu_appender_free(app);
        zu_conn_close(conn);
    }
}

/// What an open refuses, which is where a host about to buffer a
/// million rows wants to hear about it.
#[test]
fn an_appender_opens_on_a_table_or_says_at_once_why_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("open.zu1");
    seeded(&path);
    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let mut app: *mut ZuAppender = ptr::null_mut();

        // A table nothing declares.
        let table = c("nobody");
        assert_eq!(
            zu_appender_open_z(conn, table.as_ptr(), &mut app, &mut err),
            ZuStatus::Misuse
        );
        assert!(app.is_null(), "a refused open writes NULL through out");
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(
            message.contains("no node table or rel table 'nobody'"),
            "{message}"
        );

        // A node table that stores no properties, which has no columns
        // for a row to be made of.
        let table = c("person");
        assert_eq!(
            zu_appender_open_z(conn, table.as_ptr(), &mut app, &mut err),
            ZuStatus::Unsupported
        );
        assert!(app.is_null());
        let message = message_of(err);
        err = ptr::null_mut();
        assert!(message.contains("stores no properties"), "{message}");

        // A name that is not UTF-8, and an out-parameter that is not
        // there to be written.
        assert_eq!(
            zu_appender_open(
                conn,
                b"\xff".as_ptr().cast::<c_char>(),
                1,
                &mut app,
                &mut err
            ),
            ZuStatus::Misuse
        );
        assert!(message_of(err).contains("table"));
        err = ptr::null_mut();
        assert_eq!(
            zu_appender_open(conn, table.as_ptr(), 6, ptr::null_mut(), &mut err),
            ZuStatus::Misuse
        );
        assert!(message_of(err).contains("out is NULL"));

        zu_conn_close(conn);
    }
}

/// Closing spends the appender and says what it wrote, and every call
/// after that says the handle is spent rather than pretending.
#[test]
fn a_closed_appender_is_spent_and_closing_twice_is_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("close.zu1");
    unsafe {
        let conn = five_columns(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let app = appender(conn, "person");

        five(app, 10, "grace", 19800, 1, 2.5);
        five(app, 11, "zoe", 19801, 0, -0.5);
        let mut total = 0u64;
        assert_eq!(zu_appender_close(app, &mut total, &mut err), ZuStatus::Ok);
        assert!(err.is_null());
        assert_eq!(total, 2);
        assert_eq!(people(conn), 3);

        // Closing again writes nothing and answers the same count, so a
        // cleanup path may close what the load already did.
        let mut again = 0u64;
        assert_eq!(zu_appender_close(app, &mut again, &mut err), ZuStatus::Ok);
        assert_eq!(again, 2);
        assert_eq!(people(conn), 3);

        // Everything else is spent.
        let mut out = u64::MAX;
        assert_eq!(zu_append_i64(app, 12, &mut err), ZuStatus::MisuseClosed);
        assert!(err.is_null(), "a spent handle is misuse, not a failure");
        assert_eq!(zu_append_end_row(app, &mut err), ZuStatus::MisuseClosed);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::MisuseClosed);
        assert_eq!(zu_appender_buffered(app, &mut out), ZuStatus::MisuseClosed);
        assert_eq!(out, 0, "the out-parameter is written on every path");
        assert_eq!(zu_appender_committed(app, &mut out), ZuStatus::MisuseClosed);
        let mut cols = u32::MAX;
        assert_eq!(zu_appender_cols(app, &mut cols), ZuStatus::MisuseClosed);
        assert_eq!(cols, 0);
        assert!(zu_appender_col_name(app, 0, ptr::null_mut()).is_null());
        assert_eq!(zu_appender_discard(app, &mut out), ZuStatus::MisuseClosed);

        zu_appender_free(app);
        zu_conn_close(conn);
    }
}

/// The ways of calling these wrongly, all of them answered rather than
/// left to the debugger.
#[test]
fn the_appending_calls_answer_a_null_handle_and_a_closed_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-misuse.zu1");
    unsafe {
        let mut err: *mut ZuError = ptr::null_mut();
        let mut app: *mut ZuAppender = ptr::null_mut();
        let table = c("person");
        assert_eq!(
            zu_appender_open_z(ptr::null_mut(), table.as_ptr(), &mut app, &mut err),
            ZuStatus::Misuse
        );
        assert!(app.is_null());
        assert_eq!(
            zu_append_bool(ptr::null_mut(), 1, &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_i64(ptr::null_mut(), 1, &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_f64(ptr::null_mut(), 1.0, &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_str_z(ptr::null_mut(), table.as_ptr(), &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_bytes(ptr::null_mut(), b"x".as_ptr(), 1, &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_temporal(ptr::null_mut(), ZU_TEMPORAL_DATE, 0, &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_append_end_row(ptr::null_mut(), &mut err),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_appender_flush(ptr::null_mut(), &mut err),
            ZuStatus::Misuse
        );
        let mut out = u64::MAX;
        assert_eq!(
            zu_appender_buffered(ptr::null_mut(), &mut out),
            ZuStatus::Misuse
        );
        assert_eq!(out, 0);
        assert_eq!(
            zu_appender_committed(ptr::null_mut(), &mut out),
            ZuStatus::Misuse
        );
        let mut cols = u32::MAX;
        assert_eq!(
            zu_appender_cols(ptr::null_mut(), &mut cols),
            ZuStatus::Misuse
        );
        assert_eq!(cols, 0);
        assert!(zu_appender_col_name(ptr::null_mut(), 0, ptr::null_mut()).is_null());
        assert_eq!(
            zu_appender_discard(ptr::null_mut(), &mut out),
            ZuStatus::Misuse
        );
        assert_eq!(
            zu_appender_close(ptr::null_mut(), &mut out, &mut err),
            ZuStatus::Misuse
        );
        // Freeing nothing is nothing, like every other free here.
        zu_appender_free(ptr::null_mut());

        let conn = five_columns(&path);
        let app = appender(conn, "person");
        // The out-parameters that are not optional.
        assert_eq!(zu_appender_buffered(app, ptr::null_mut()), ZuStatus::Misuse);
        assert_eq!(
            zu_appender_committed(app, ptr::null_mut()),
            ZuStatus::Misuse
        );
        assert_eq!(zu_appender_cols(app, ptr::null_mut()), ZuStatus::Misuse);

        // An appender outliving its connection answers the way a
        // statement does, because it keeps that connection's state
        // alive for exactly this: the rows it still held are gone with
        // the connection they were going to be written through.
        five(app, 10, "grace", 19800, 1, 2.5);
        zu_conn_close(conn);
        assert_eq!(zu_append_i64(app, 11, &mut err), ZuStatus::MisuseClosed);
        assert_eq!(zu_appender_flush(app, &mut err), ZuStatus::MisuseClosed);
        assert_eq!(
            zu_appender_close(app, &mut out, &mut err),
            ZuStatus::MisuseClosed
        );
        zu_appender_free(app);

        let conn = open(&path);
        assert_eq!(people(conn), 1);
        zu_conn_close(conn);
    }
}

/* ---- diagnostics ---- */

/// The condition a statement completed with, which every result
/// carries whether or not anything went wrong.
unsafe fn gqlstatus_of(result: *mut ZuResult) -> String {
    let mut len = 0usize;
    let code = unsafe { zu_result_gqlstatus(result, &mut len) };
    assert!(!code.is_null(), "a result with no completion condition");
    let code = unsafe { CStr::from_ptr(code) }.to_str().expect("utf-8");
    assert_eq!(len, code.len(), "the length and the NUL have to agree");
    code.to_string()
}

/// One notice off a result, as the handle every other condition comes
/// back on.
unsafe fn notice(result: *mut ZuResult, ix: u32) -> *mut ZuError {
    let mut out: *mut ZuError = ptr::null_mut();
    assert_eq!(
        unsafe { zu_result_notice(result, ix, &mut out) },
        ZuStatus::Ok
    );
    assert!(!out.is_null());
    out
}

/// A statement that answered and a statement that had nothing to
/// answer with are two different completions, and the standard has a
/// condition for each.
#[test]
fn a_result_carries_the_condition_its_statement_completed_with() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("status.zu1");
    unsafe {
        let conn = two_people(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        let result = query(conn, "MATCH (p:person) RETURN count(p) AS n", &mut err);
        assert_eq!(gqlstatus_of(result), "00000");
        assert_eq!(zu_result_notices(result), 0, "nothing was raised");
        zu_result_free(result);

        // No rows is still a projection, so it completes the same way a
        // full one does: what 00000 answers is whether there were
        // columns, not whether any of them were filled.
        let result = query(
            conn,
            "MATCH (p:person) WHERE p.name = 'nobody' RETURN p.name AS name",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 0);
        assert_eq!(gqlstatus_of(result), "00000");
        zu_result_free(result);

        // A statement with no projection to give back, which is not a
        // failure and not an empty answer either: 00001 is the
        // standard's word for exactly this.
        let result = query(conn, "INSERT (p:person {name: 'zoe'})", &mut err);
        assert_eq!(zu_result_cols(result), 0);
        assert_eq!(gqlstatus_of(result), "00001");
        zu_result_free(result);

        // Every path writes the length, and a handle that is not there
        // answers rather than being followed.
        let mut len = usize::MAX;
        assert!(zu_result_gqlstatus(ptr::null_mut(), &mut len).is_null());
        assert_eq!(len, 0);
        assert_eq!(zu_result_notices(ptr::null_mut()), 0);

        zu_conn_close(conn);
    }
}

/// A warning rides with the answer rather than replacing it, which is
/// the whole reason it is not an error: the rows are still rows.
#[test]
fn a_condition_a_statement_survived_comes_back_beside_its_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notice.zu1");
    seeded(&path);
    unsafe {
        let conn = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // The optional group misses for most people, so the aggregate
        // has a null argument on those rows and ignores it. That is
        // 01G11, and the answer is still an answer.
        let result = query(
            conn,
            "MATCH (a:person) OPTIONAL MATCH (a)-[:follows]->(b) WHERE b.id > 90 \
             RETURN avg(b.id) AS avg_friend",
            &mut err,
        );
        assert_eq!(zu_result_rows(result), 1, "a warning is not an exception");
        assert_eq!(gqlstatus_of(result), "00000");
        assert_eq!(zu_result_notices(result), 1);

        let n = notice(result, 0);
        let mut len = 0usize;
        assert_eq!(
            CStr::from_ptr(zu_error_code(n, &mut len)).to_str(),
            Ok("01G11")
        );
        assert_eq!(len, 5);
        assert_eq!(
            zu_error_severity(n),
            ZU_SEVERITY_WARNING,
            "the severity is what tells a notice from a failure"
        );
        assert_eq!(
            zu_error_status(n),
            ZuStatus::Ok,
            "which is what the call that produced it returned"
        );
        let standard = CStr::from_ptr(zu_error_standard_text(n, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert!(standard.contains("null value eliminated"), "{standard}");
        let url = CStr::from_ptr(zu_error_doc_url(n, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert_eq!(url, "https://zu.dev/docs/errors/01G11");
        let message = CStr::from_ptr(zu_error_message(n, ptr::null_mut()))
            .to_str()
            .expect("utf-8");
        assert!(message.contains("01G11"), "{message}");
        assert_eq!(zu_error_retryable(n), 0, "a warning is not a retry");
        // Raised while the statement ran rather than at a token, so
        // there is no place and no line, and both say so together.
        let mut line = 0u32;
        let mut column = 0u32;
        assert_eq!(zu_error_position(n, &mut line, &mut column), ZuStatus::Done);
        let mut offset = 0u32;
        assert_eq!(zu_error_offset(n, &mut offset), ZuStatus::Done);
        zu_error_free(n);

        // A copy rather than a borrow: the result still has its own and
        // answers again, which is what lets a host free the first one
        // before it asks for the second.
        let again = notice(result, 0);
        assert_eq!(
            CStr::from_ptr(zu_error_code(again, ptr::null_mut())).to_str(),
            Ok("01G11")
        );
        zu_error_free(again);

        // Past the end is the end of the walk rather than a failure.
        let mut out: *mut ZuError = ptr::null_mut();
        assert_eq!(zu_result_notice(result, 1, &mut out), ZuStatus::Done);
        assert!(out.is_null());
        assert_eq!(zu_result_notice(result, u32::MAX, &mut out), ZuStatus::Done);
        zu_result_free(result);

        // One warning per statement however many groups dropped a null,
        // because a host wants to know it happened rather than how
        // often.
        let result = query(
            conn,
            "MATCH (a:person) OPTIONAL MATCH (a)-[:follows]->(b) WHERE b.id > 90 \
             RETURN a.id AS id, avg(b.id) AS avg_friend",
            &mut err,
        );
        assert!(
            zu_result_rows(result) > 1,
            "several groups, several chances"
        );
        assert_eq!(zu_result_notices(result), 1);
        zu_result_free(result);

        // And the ways of asking wrongly.
        assert_eq!(
            zu_result_notice(ptr::null_mut(), 0, &mut out),
            ZuStatus::Misuse
        );
        assert!(out.is_null());
        let result = query(conn, "MATCH (p:person) RETURN count(p) AS n", &mut err);
        assert_eq!(
            zu_result_notice(result, 0, ptr::null_mut()),
            ZuStatus::Misuse
        );
        zu_result_free(result);

        zu_conn_close(conn);
    }
}
