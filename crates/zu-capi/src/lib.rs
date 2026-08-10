//! libzu: the C surface over [`zudb::session::Session`], built for a
//! host that keeps the process alive and queries in a loop (a cgo
//! adapter, an editor, a language binding). One session per thread;
//! nothing here is thread-safe and the header says so.
//!
//! Ownership is the usual C contract: every pointer this library hands
//! out stays valid until the object that produced it is freed, and is
//! freed only by the matching `*_free`/`*_close` call. Errors come
//! back through a `char **err` out-parameter carrying a message the
//! caller releases with `zu_string_free`; a NULL `err` discards it.
//!
//! Results read out column-at-a-time: `zu_result_col_i64` materializes
//! a whole column into a contiguous buffer once, so a host crossing an
//! FFI boundary pays one call per column, not one per cell. That is
//! the difference between an in-process point read spending its budget
//! on the query or on the boundary.

// The pointer contract (what must be valid, who frees what, in which
// order) is one contract for the whole surface; it lives in the module
// docs above and in include/zu.h where a C caller will actually read
// it, not repeated under every function.
#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use zudb::query::{QueryResult, Value};
use zudb::session::Session;

pub const ZU_TYPE_NULL: i32 = 0;
pub const ZU_TYPE_BOOL: i32 = 1;
pub const ZU_TYPE_INT: i32 = 2;
pub const ZU_TYPE_FLOAT: i32 = 3;
pub const ZU_TYPE_STR: i32 = 4;
pub const ZU_TYPE_NODE: i32 = 5;
pub const ZU_TYPE_REL: i32 = 6;
pub const ZU_TYPE_LIST: i32 = 7;
pub const ZU_TYPE_PATH: i32 = 8;

/// An open database session. Opaque to C.
pub struct ZuSession {
    session: Session,
}

/// A prepared statement plus its pending bindings. Holds a raw pointer
/// back to its session; the header requires the statement to die
/// before the session does, the same contract sqlite3_stmt has.
pub struct ZuStmt {
    session: *mut ZuSession,
    id: u64,
    binds: Vec<(String, Value)>,
}

/// One query result, owning the rows and every buffer handed out over
/// the boundary: column-name CStrings up front, columnar i64/f64/
/// validity buffers and cell strings materialized on first request and
/// kept until the result is freed, so returned pointers stay stable.
pub struct ZuResult {
    result: QueryResult,
    col_names: Vec<CString>,
    i64_cols: Vec<Option<Vec<i64>>>,
    f64_cols: Vec<Option<Vec<f64>>>,
    valid_cols: Vec<Option<Vec<u8>>>,
    strs: HashMap<(u64, u32), CString>,
}

impl ZuResult {
    fn new(result: QueryResult) -> ZuResult {
        let col_names = result
            .columns
            .iter()
            .map(|c| CString::new(c.as_str()).unwrap_or_default())
            .collect();
        let cols = result.columns.len();
        ZuResult {
            result,
            col_names,
            i64_cols: vec![None; cols],
            f64_cols: vec![None; cols],
            valid_cols: vec![None; cols],
            strs: HashMap::new(),
        }
    }
}

fn set_err(err: *mut *mut c_char, message: &str) {
    if err.is_null() {
        return;
    }
    let c = CString::new(message.replace('\0', " ")).unwrap_or_default();
    unsafe { *err = c.into_raw() };
}

/// Runs a closure with panics fenced off the FFI boundary: a panic
/// becomes an error message and the null/failure value, never an
/// unwind into C.
fn guard<T>(err: *mut *mut c_char, fail: T, f: impl FnOnce() -> Result<T, String>) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(message)) => {
            set_err(err, &message);
            fail
        }
        Err(_) => {
            set_err(err, "internal panic in libzu");
            fail
        }
    }
}

unsafe fn cstr<'a>(p: *const c_char, what: &str) -> Result<&'a str, String> {
    if p.is_null() {
        return Err(format!("{what} is NULL"));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| format!("{what} is not UTF-8"))
}

/// Returns the library version as a static string; never freed.
#[unsafe(no_mangle)]
pub extern "C" fn zu_version() -> *const c_char {
    const VERSION: &CStr = c"0.0.1";
    VERSION.as_ptr()
}

/// Opens a session on a zu1 file. NULL on failure, with `*err` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_open(path: *const c_char, err: *mut *mut c_char) -> *mut ZuSession {
    guard(err, std::ptr::null_mut(), || {
        let path = unsafe { cstr(path, "path") }?;
        let session = Session::open(Path::new(path)).map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(ZuSession { session })))
    })
}

/// Closes a session. Statements prepared on it must already be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_close(session: *mut ZuSession) {
    if !session.is_null() {
        drop(unsafe { Box::from_raw(session) });
    }
}

/// Runs one parameterless statement. NULL on failure, with `*err` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_query(
    session: *mut ZuSession,
    q: *const c_char,
    err: *mut *mut c_char,
) -> *mut ZuResult {
    guard(err, std::ptr::null_mut(), || {
        if session.is_null() {
            return Err("session is NULL".to_string());
        }
        let q = unsafe { cstr(q, "query") }?;
        let session = unsafe { &mut *session };
        let result = session.session.run(q, &[]).map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(ZuResult::new(result))))
    })
}

/// Compiles a statement against the session's plan cache. The handle
/// carries its own bindings; bind then execute, as many times as
/// wanted. NULL on failure, with `*err` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_prepare(
    session: *mut ZuSession,
    q: *const c_char,
    err: *mut *mut c_char,
) -> *mut ZuStmt {
    guard(err, std::ptr::null_mut(), || {
        if session.is_null() {
            return Err("session is NULL".to_string());
        }
        let q = unsafe { cstr(q, "query") }?;
        let s = unsafe { &mut *session };
        let (id, _) = s.session.prepare(q).map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(ZuStmt {
            session,
            id,
            binds: Vec::new(),
        })))
    })
}

/// Frees a statement and drops its slot in the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_stmt_close(stmt: *mut ZuStmt) {
    if stmt.is_null() {
        return;
    }
    let stmt = unsafe { Box::from_raw(stmt) };
    if !stmt.session.is_null() {
        unsafe { &mut *stmt.session }.session.close_stmt(stmt.id);
    }
}

unsafe fn bind(stmt: *mut ZuStmt, name: *const c_char, value: Value) {
    let Ok(name) = (unsafe { cstr(name, "name") }) else {
        return;
    };
    if stmt.is_null() {
        return;
    }
    let stmt = unsafe { &mut *stmt };
    // Rebinding a name replaces the old value, so a statement in a
    // loop binds the same names over and over without growing.
    stmt.binds.retain(|(n, _)| n != name);
    stmt.binds.push((name.to_string(), value));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_i64(stmt: *mut ZuStmt, name: *const c_char, v: i64) {
    unsafe { bind(stmt, name, Value::Int(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_f64(stmt: *mut ZuStmt, name: *const c_char, v: f64) {
    unsafe { bind(stmt, name, Value::Float(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_str(stmt: *mut ZuStmt, name: *const c_char, v: *const c_char) {
    let Ok(v) = (unsafe { cstr(v, "value") }) else {
        return;
    };
    unsafe { bind(stmt, name, Value::Str(v.to_string())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_null(stmt: *mut ZuStmt, name: *const c_char) {
    unsafe { bind(stmt, name, Value::Null) }
}

/// Executes a prepared statement with its current bindings. Bindings
/// survive the call, so a loop only rebinds what changed. NULL on
/// failure, with `*err` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_execute(stmt: *mut ZuStmt, err: *mut *mut c_char) -> *mut ZuResult {
    guard(err, std::ptr::null_mut(), || {
        if stmt.is_null() {
            return Err("stmt is NULL".to_string());
        }
        let stmt = unsafe { &mut *stmt };
        if stmt.session.is_null() {
            return Err("stmt has no session".to_string());
        }
        let session = unsafe { &mut *stmt.session };
        let borrowed: Vec<(&str, Value)> = stmt
            .binds
            .iter()
            .map(|(n, v)| (n.as_str(), v.clone()))
            .collect();
        let result = session
            .session
            .execute(stmt.id, &borrowed)
            .map_err(|e| e.to_string())?;
        Ok(Box::into_raw(Box::new(ZuResult::new(result))))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_rows(result: *const ZuResult) -> u64 {
    if result.is_null() {
        return 0;
    }
    unsafe { &*result }.result.rows.len() as u64
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cols(result: *const ZuResult) -> u32 {
    if result.is_null() {
        return 0;
    }
    unsafe { &*result }.result.columns.len() as u32
}

/// Column name; valid until the result is freed. NULL out of range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_name(result: *const ZuResult, col: u32) -> *const c_char {
    if result.is_null() {
        return std::ptr::null();
    }
    match unsafe { &*result }.col_names.get(col as usize) {
        Some(name) => name.as_ptr(),
        None => std::ptr::null(),
    }
}

fn cell(result: &ZuResult, row: u64, col: u32) -> Option<&Value> {
    result.result.rows.get(row as usize)?.get(col as usize)
}

/// Type tag of one cell (ZU_TYPE_*), or -1 out of range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cell_type(result: *const ZuResult, row: u64, col: u32) -> i32 {
    if result.is_null() {
        return -1;
    }
    match cell(unsafe { &*result }, row, col) {
        Some(Value::Null) => ZU_TYPE_NULL,
        Some(Value::Bool(_)) => ZU_TYPE_BOOL,
        Some(Value::Int(_)) => ZU_TYPE_INT,
        Some(Value::Float(_)) => ZU_TYPE_FLOAT,
        Some(Value::Str(_)) => ZU_TYPE_STR,
        Some(Value::Node { .. }) => ZU_TYPE_NODE,
        Some(Value::Rel { .. }) => ZU_TYPE_REL,
        Some(Value::List(_)) => ZU_TYPE_LIST,
        Some(Value::Path(_)) => ZU_TYPE_PATH,
        None => -1,
    }
}

/// The whole column as contiguous i64s, one FFI call for all rows.
/// Ints and bools carry their value, nulls read 0 (see
/// `zu_result_col_valid`), node cells read their offset so an id
/// column of nodes still lands in one buffer. NULL if the column holds
/// anything else. The buffer lives until the result is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_i64(result: *mut ZuResult, col: u32) -> *const i64 {
    if result.is_null() {
        return std::ptr::null();
    }
    let r = unsafe { &mut *result };
    let c = col as usize;
    if c >= r.result.columns.len() {
        return std::ptr::null();
    }
    if r.i64_cols[c].is_none() {
        let mut out = Vec::with_capacity(r.result.rows.len());
        for row in &r.result.rows {
            match &row[c] {
                Value::Int(i) => out.push(*i),
                Value::Bool(b) => out.push(i64::from(*b)),
                Value::Null => out.push(0),
                Value::Node { offset, .. } => out.push(*offset as i64),
                _ => return std::ptr::null(),
            }
        }
        r.i64_cols[c] = Some(out);
    }
    r.i64_cols[c].as_ref().expect("filled").as_ptr()
}

/// The whole column as contiguous doubles; ints widen, nulls read 0.
/// NULL if the column holds anything but numbers and nulls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_f64(result: *mut ZuResult, col: u32) -> *const f64 {
    if result.is_null() {
        return std::ptr::null();
    }
    let r = unsafe { &mut *result };
    let c = col as usize;
    if c >= r.result.columns.len() {
        return std::ptr::null();
    }
    if r.f64_cols[c].is_none() {
        let mut out = Vec::with_capacity(r.result.rows.len());
        for row in &r.result.rows {
            match &row[c] {
                Value::Float(f) => out.push(*f),
                Value::Int(i) => out.push(*i as f64),
                Value::Null => out.push(0.0),
                _ => return std::ptr::null(),
            }
        }
        r.f64_cols[c] = Some(out);
    }
    r.f64_cols[c].as_ref().expect("filled").as_ptr()
}

/// One byte per row: 1 where the cell holds a value, 0 where it is
/// null. Never NULL for an in-range column.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_valid(result: *mut ZuResult, col: u32) -> *const u8 {
    if result.is_null() {
        return std::ptr::null();
    }
    let r = unsafe { &mut *result };
    let c = col as usize;
    if c >= r.result.columns.len() {
        return std::ptr::null();
    }
    if r.valid_cols[c].is_none() {
        let out = r
            .result
            .rows
            .iter()
            .map(|row| u8::from(!matches!(row[c], Value::Null)))
            .collect();
        r.valid_cols[c] = Some(out);
    }
    r.valid_cols[c].as_ref().expect("filled").as_ptr()
}

/// One string cell, NUL-terminated, valid until the result is freed.
/// `len` (when non-NULL) gets the byte length without the terminator.
/// NULL when the cell is not a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cell_str(
    result: *mut ZuResult,
    row: u64,
    col: u32,
    len: *mut usize,
) -> *const c_char {
    if result.is_null() {
        return std::ptr::null();
    }
    let r = unsafe { &mut *result };
    let Some(Value::Str(s)) = cell(r, row, col) else {
        return std::ptr::null();
    };
    let s = s.clone();
    let c = r
        .strs
        .entry((row, col))
        .or_insert_with(|| CString::new(s.replace('\0', " ")).unwrap_or_default());
    if !len.is_null() {
        unsafe { *len = c.as_bytes().len() };
    }
    c.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_free(result: *mut ZuResult) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

/// Frees an error message produced by any call here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}
