//! Exercises libzu the way a C host does: open, query, prepare, bind,
//! execute in a loop, read columns out as buffers, and free in the
//! right order. Everything goes through the extern "C" functions and
//! raw pointers; nothing reaches into the Rust types behind them.

use std::ffi::{CStr, CString, c_char};
use std::ptr;

use zu::{
    ZU_SEVERITY_EXCEPTION, ZU_TYPE_INT, ZU_TYPE_NODE, ZU_TYPE_STR, ZuError, ZuResult, ZuSession,
    ZuStatus, ZuStmt, zu_bind_i64, zu_bind_i64_z, zu_bind_str_z, zu_close, zu_error_code,
    zu_error_free, zu_error_message, zu_error_severity, zu_error_status, zu_execute, zu_open,
    zu_open_z, zu_prepare, zu_prepare_z, zu_query, zu_query_z, zu_result_cell_str,
    zu_result_cell_type, zu_result_col_f64, zu_result_col_i64, zu_result_col_name,
    zu_result_col_node_offset, zu_result_col_valid, zu_result_cols, zu_result_free, zu_result_rows,
    zu_stmt_close, zu_version,
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

/// Opens on the counted form, which is the one every binding uses.
unsafe fn open(path: &std::path::Path) -> *mut ZuSession {
    let path = path.to_str().expect("utf-8 path");
    let mut session: *mut ZuSession = ptr::null_mut();
    let status = unsafe {
        zu_open(
            path.as_ptr().cast::<c_char>(),
            path.len(),
            &mut session,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, ZuStatus::Ok, "open {path}");
    assert!(!session.is_null());
    session
}

/// Runs a statement that is expected to succeed, reusing one error
/// slot so that a success leaving a stale error behind would show up.
unsafe fn query(session: *mut ZuSession, text: &str, err: &mut *mut ZuError) -> *mut ZuResult {
    let mut result: *mut ZuResult = ptr::null_mut();
    let status = unsafe {
        zu_query(
            session,
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

unsafe fn cell_type(result: *mut ZuResult, row: u64, col: u32) -> i32 {
    let mut out = -1i32;
    assert_eq!(
        unsafe { zu_result_cell_type(result, row, col, &mut out) },
        ZuStatus::Ok
    );
    out
}

#[test]
fn a_c_host_can_open_query_prepare_and_read_columns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("capi.zu1");
    seeded(&path);

    unsafe {
        assert_eq!(CStr::from_ptr(zu_version()).to_str(), Ok("0.0.1"));

        let session = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();

        // One-shot query, whole id column out in one call.
        let result = query(
            session,
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
                session,
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
        let result = query(
            session,
            "MATCH (a:person {id: 3}) RETURN 'hi' AS s",
            &mut err,
        );
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

        zu_close(session);
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
        let session = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            session,
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
        let result = query(
            session,
            "MATCH (a:person {id: 3}) RETURN a.id AS id",
            &mut err,
        );
        let mut offsets: *const u64 = ptr::null();
        assert_eq!(
            zu_result_col_node_offset(result, 0, &mut offsets),
            ZuStatus::Misuse
        );
        assert!(offsets.is_null());
        zu_result_free(result);

        zu_close(session);
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
        let session = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let result = query(
            session,
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
        zu_close(session);
    }
}

/// The fields a binding needs off an error, as fields.
#[test]
fn a_refused_statement_carries_a_code_a_severity_and_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("errors.zu1");
    seeded(&path);

    unsafe {
        let session = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let bad = "THIS IS NOT A QUERY";
        let mut result: *mut ZuResult = ptr::null_mut();
        let status = zu_query(
            session,
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
        zu_error_free(err);
        err = ptr::null_mut();

        // The session survives, and the next success clears the slot.
        let result = query(session, "MATCH (a:person) RETURN count(a) AS n", &mut err);
        assert_eq!(col_i64(result, 0, 1)[0], 97);
        zu_result_free(result);

        // A caller who does not want the error still gets the status.
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_query(
                session,
                bad.as_ptr().cast::<c_char>(),
                bad.len(),
                &mut result,
                ptr::null_mut()
            ),
            ZuStatus::Error
        );
        assert!(result.is_null());

        zu_close(session);
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
        let mut session: *mut ZuSession = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        assert_eq!(
            zu_open_z(cpath.as_ptr(), &mut session, &mut err),
            ZuStatus::Ok
        );
        assert!(!session.is_null());

        let q = c("MATCH (a:person {id: $src}) RETURN a.id AS id");
        let mut stmt: *mut ZuStmt = ptr::null_mut();
        assert_eq!(
            zu_prepare_z(session, q.as_ptr(), &mut stmt, &mut err),
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
            zu_prepare_z(session, q.as_ptr(), &mut stmt, &mut err),
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
            zu_query_z(session, q.as_ptr(), &mut result, &mut err),
            ZuStatus::Ok
        );
        assert_eq!(col_i64(result, 0, 1), [97]);
        zu_result_free(result);

        zu_close(session);
    }
}

/// Nothing here crashes on a NULL, and the status says which mistake
/// it was rather than leaving the caller to guess from a NULL return.
#[test]
fn null_inputs_are_misuse_and_not_crashes() {
    unsafe {
        let missing = "/nonexistent/nowhere.zu1";
        let mut session: *mut ZuSession = ptr::null_mut();
        let mut err: *mut ZuError = ptr::null_mut();
        let status = zu_open(
            missing.as_ptr().cast::<c_char>(),
            missing.len(),
            &mut session,
            &mut err,
        );
        assert_ne!(status, ZuStatus::Ok);
        assert!(session.is_null());
        assert!(!err.is_null());
        assert_eq!(zu_error_status(err), status);
        zu_error_free(err);
        err = ptr::null_mut();

        // A NULL path with a length is misuse; the message says so and
        // the out-parameter is still written.
        let mut session: *mut ZuSession = ptr::null_mut();
        assert_eq!(
            zu_open(ptr::null(), 4, &mut session, &mut err),
            ZuStatus::Misuse
        );
        assert!(session.is_null());
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
        zu_close(ptr::null_mut());
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
        let session = open(&path);
        let mut err: *mut ZuError = ptr::null_mut();
        let bad: [u8; 3] = [0xff, 0xfe, 0xfd];
        let mut result: *mut ZuResult = ptr::null_mut();
        assert_eq!(
            zu_query(
                session,
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
        zu_close(session);
    }
}
