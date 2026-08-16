//! libzu: the C surface over [`zudb::session::Session`], built for a
//! host that keeps the process alive and queries in a loop (a cgo
//! adapter, an editor, a language binding). One session per thread;
//! nothing here is thread-safe and the header says so.
//!
//! Every fallible call returns a [`ZuStatus`] and writes what it
//! produced through an out-parameter, which is dx/02 R2. The reason is
//! that a single returned pointer cannot say both "this failed" and
//! "this succeeded and there is nothing here", and a binding that
//! cannot tell those apart turns an empty answer into an exception or
//! an error into an empty answer. The status is the control-flow
//! answer and it is complete on its own: an out-parameter is written
//! on every path, `NULL` when there is nothing to point at, so a
//! caller that ignores the status is never left holding a stale
//! pointer from the call before.
//!
//! What a user reads comes back separately, on a [`ZuError`] handle
//! carrying the GQLSTATUS code, the severity, and the message as
//! fields rather than as one string a binding has to parse. Only the
//! calls that can fail for a reason the engine has something to say
//! about take one: opening, running, preparing, executing. The
//! accessors do not, because their failures are structural (a column
//! out of range, a column that does not hold what the accessor reads)
//! and the status names each one exactly.
//!
//! Strings cross the boundary as a pointer and a length, which is
//! dx/02 R7. Most source languages have counted strings, and a
//! NUL-terminated parameter forces every one of them to allocate a
//! copy of a string that already knew how long it was. Each of those
//! calls has a `_z` variant for a caller who genuinely has a C string,
//! so the counted form is the default rather than the only form.
//!
//! Ownership is the usual C contract: every pointer this library hands
//! out stays valid until the object that produced it is freed, and is
//! freed only by the matching `*_free`/`*_close` call, each of which
//! is a no-op on `NULL` (R8).
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
use zudb::{Severity, ZuError as EngineError};

/// What a call answers, which is control flow and nothing else.
///
/// The GQLSTATUS condition a user reads lives on [`ZuError`], not
/// here, because the two are read by different code: a binding
/// branches on the status and shows the condition. Keeping the
/// condition out of the enum is what stops this from growing a variant
/// per condition, which is a table of several hundred rows that no
/// `switch` should ever be written over.
///
/// The values are fixed. New ones are appended, never inserted, so a
/// binding compiled against an older header keeps its numbering. The
/// gaps are held for the rest of the set dx/02 §6 names and nothing
/// produces yet: 1 for `ZU_ROW`, 5 and 6 for the ownership checks, 7
/// for `ZU_INTERRUPTED`, and 12 for `ZU_OOM`. Reserving the numbers is
/// free, and it is what lets those land beside the misuse value they
/// belong with rather than at the end because the end had room.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZuStatus {
    /// The call did what it was asked and wrote its out-parameter.
    Ok = 0,
    /// The call is well formed and there is nothing to read: a column
    /// of a result that has no rows. The out-parameter is `NULL`, and
    /// this is the case a returned `NULL` could not tell from failure.
    Done = 2,
    /// The engine refused the work, and the error handle says why.
    Error = 3,
    /// The caller broke the contract in the header: a `NULL` handle,
    /// an index out of range, an accessor asked for a column that does
    /// not hold what it reads, or a string that is not UTF-8. Nothing
    /// was done and nothing is wrong with the database.
    Misuse = 4,
    /// A write lost to a concurrent one.
    Conflict = 8,
    /// The file says something that cannot be true, which is damage
    /// rather than a request the engine declined.
    Corrupt = 9,
    /// The engine does not implement this yet. Distinct from `Error`
    /// so a binding can present it as "not in this build" rather than
    /// as the user's mistake.
    Unsupported = 10,
    /// The operating system refused a read or a write.
    Io = 11,
}

/// Severity as the standard spells it, on [`zu_error_severity`].
pub const ZU_SEVERITY_SUCCESS: i32 = 0;
pub const ZU_SEVERITY_NO_DATA: i32 = 1;
pub const ZU_SEVERITY_WARNING: i32 = 2;
pub const ZU_SEVERITY_INFORMATIONAL: i32 = 3;
pub const ZU_SEVERITY_EXCEPTION: i32 = 4;

pub const ZU_TYPE_NULL: i32 = 0;
pub const ZU_TYPE_BOOL: i32 = 1;
pub const ZU_TYPE_INT: i32 = 2;
pub const ZU_TYPE_FLOAT: i32 = 3;
pub const ZU_TYPE_STR: i32 = 4;
pub const ZU_TYPE_NODE: i32 = 5;
pub const ZU_TYPE_REL: i32 = 6;
pub const ZU_TYPE_LIST: i32 = 7;
pub const ZU_TYPE_PATH: i32 = 8;
pub const ZU_TYPE_TEMPORAL: i32 = 9;
pub const ZU_TYPE_RECORD: i32 = 10;

/// What went wrong, as fields. Opaque to C, read through the
/// accessors, freed with [`zu_error_free`].
///
/// The v0 surface handed back a `char *` and left a binding to find
/// the code, the severity, and the position by looking inside the
/// text. Those are the three things a binding needs as data: the code
/// picks which exception class to raise, the severity decides whether
/// to raise at all, and neither survives being formatted into prose
/// and parsed back out.
pub struct ZuError {
    status: ZuStatus,
    message: CString,
    /// The GQLSTATUS code, when the error carries a condition.
    /// Engine-internal failures carry none rather than a guessed one,
    /// which the accessor reports as `NULL`.
    code: Option<CString>,
    severity: i32,
}

/// The status an engine error becomes.
///
/// Exhaustive on purpose, with no wildcard arm, so that a new
/// [`EngineError`] variant fails to compile here rather than arriving
/// at a binding as whichever status the wildcard happened to name.
fn status_of(e: &EngineError) -> ZuStatus {
    match e {
        // A GQL condition is the engine answering the user: bad
        // syntax, an unknown label, a type that does not convert. The
        // condition itself rides on the handle.
        EngineError::Gql(_) => ZuStatus::Error,
        EngineError::Io(_) => ZuStatus::Io,
        EngineError::Corrupt { .. } => ZuStatus::Corrupt,
        EngineError::Unsupported { .. } => ZuStatus::Unsupported,
        // The engine's word for an argument it will not take, which is
        // the caller's mistake and so the same thing as this ABI's
        // misuse: a table that does not exist, a row of the wrong
        // width, a connection asked to write when it is read-only.
        EngineError::InvalidArgument(_) => ZuStatus::Misuse,
        EngineError::Conflict(_) => ZuStatus::Conflict,
    }
}

fn severity_of(s: Severity) -> i32 {
    match s {
        Severity::Success => ZU_SEVERITY_SUCCESS,
        Severity::NoData => ZU_SEVERITY_NO_DATA,
        Severity::Warning => ZU_SEVERITY_WARNING,
        Severity::Informational => ZU_SEVERITY_INFORMATIONAL,
        Severity::Exception => ZU_SEVERITY_EXCEPTION,
    }
}

/// A NUL inside a message would truncate it at the boundary, so it is
/// replaced rather than allowed to eat the rest of the sentence.
fn c_message(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap_or_default()
}

impl ZuError {
    fn from_engine(e: &EngineError) -> ZuError {
        let record = e.diagnostic();
        ZuError {
            status: status_of(e),
            message: c_message(&e.to_string()),
            code: record.map(|r| c_message(r.status.code())),
            // An error with no condition is still an exception: it
            // stopped the statement, which is what severity is for.
            severity: record.map_or(ZU_SEVERITY_EXCEPTION, |r| severity_of(r.severity())),
        }
    }

    /// A panic that reached the boundary. Unwinding into C is
    /// undefined behaviour, so it is caught and turned into this
    /// instead, which is a bug in this library and says so.
    fn from_panic() -> ZuError {
        ZuError {
            status: ZuStatus::Error,
            message: c"internal panic in libzu".into(),
            code: None,
            severity: ZU_SEVERITY_EXCEPTION,
        }
    }
}

/// Hands an error to the caller, or drops it when the caller passed
/// `NULL` to say they do not want one.
fn set_err(err: *mut *mut ZuError, e: ZuError) -> ZuStatus {
    let status = e.status;
    if !err.is_null() {
        unsafe { *err = Box::into_raw(Box::new(e)) };
    }
    status
}

/// Runs a closure with panics fenced off the FFI boundary and turns
/// its result into a status.
///
/// The error out-parameter is cleared first, so a caller who reuses
/// one across calls cannot read the previous failure back out of a
/// call that succeeded.
fn guard(err: *mut *mut ZuError, f: impl FnOnce() -> Result<ZuStatus, EngineError>) -> ZuStatus {
    if !err.is_null() {
        unsafe { *err = std::ptr::null_mut() };
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => set_err(err, ZuError::from_engine(&e)),
        Err(_) => set_err(err, ZuError::from_panic()),
    }
}

/// The same fence for the calls that carry no error handle, where a
/// panic can only be reported as the status it already has a name for.
fn guard_status(f: impl FnOnce() -> ZuStatus) -> ZuStatus {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(ZuStatus::Error)
}

fn misuse(what: impl Into<String>) -> EngineError {
    EngineError::InvalidArgument(what.into())
}

/// A counted string from C.
///
/// A null pointer with a zero length is the empty string, not a
/// mistake: that is what an empty `string` looks like coming out of Go
/// and out of several other source languages, and building a slice
/// from a null pointer is undefined behaviour even at length zero, so
/// the case is answered before it can become one.
unsafe fn counted<'a>(p: *const c_char, len: usize, what: &str) -> Result<&'a str, EngineError> {
    if len == 0 {
        return Ok("");
    }
    if p.is_null() {
        return Err(misuse(format!("{what} is NULL with a non-zero length")));
    }
    let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
    std::str::from_utf8(bytes).map_err(|_| misuse(format!("{what} is not UTF-8")))
}

/// The length of a C string, for the `_z` variants. `NULL` is the
/// empty string here for the same reason as above: the counted form it
/// forwards to answers the null case once.
unsafe fn zlen(p: *const c_char) -> usize {
    if p.is_null() {
        return 0;
    }
    unsafe { CStr::from_ptr(p) }.to_bytes().len()
}

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
    node_cols: Vec<Option<Vec<u64>>>,
    valid_cols: Vec<Option<Vec<u8>>>,
    strs: HashMap<(u64, u32), CString>,
}

impl ZuResult {
    fn new(result: QueryResult) -> ZuResult {
        let col_names = result
            .columns
            .iter()
            .map(|c| c_message(c.as_str()))
            .collect();
        let cols = result.columns.len();
        ZuResult {
            result,
            col_names,
            i64_cols: vec![None; cols],
            f64_cols: vec![None; cols],
            node_cols: vec![None; cols],
            valid_cols: vec![None; cols],
            strs: HashMap::new(),
        }
    }
}

/* ---- library ---- */

/// Returns the library version as a static string; never freed.
#[unsafe(no_mangle)]
pub extern "C" fn zu_version() -> *const c_char {
    const VERSION: &CStr = c"0.0.1";
    VERSION.as_ptr()
}

/* ---- errors ---- */

/// The status the failing call returned, so an error carried away from
/// its call site still knows what it was.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_status(e: *const ZuError) -> ZuStatus {
    if e.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { &*e }.status
}

/// The message, NUL-terminated, with its byte length through `len`
/// when that is non-NULL. Valid until [`zu_error_free`]. Never NULL
/// for a non-NULL error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_message(e: *const ZuError, len: *mut usize) -> *const c_char {
    if e.is_null() {
        if !len.is_null() {
            unsafe { *len = 0 };
        }
        return std::ptr::null();
    }
    let e = unsafe { &*e };
    if !len.is_null() {
        unsafe { *len = e.message.as_bytes().len() };
    }
    e.message.as_ptr()
}

/// The GQLSTATUS code, such as `42001`, or NULL when this failure
/// carries no condition. An engine-internal failure has none rather
/// than one that would be a guess, and a binding mapping codes to
/// exception classes needs to be able to tell those apart.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_code(e: *const ZuError, len: *mut usize) -> *const c_char {
    if !len.is_null() {
        unsafe { *len = 0 };
    }
    if e.is_null() {
        return std::ptr::null();
    }
    match &unsafe { &*e }.code {
        Some(code) => {
            if !len.is_null() {
                unsafe { *len = code.as_bytes().len() };
            }
            code.as_ptr()
        }
        None => std::ptr::null(),
    }
}

/// The severity, one of the `ZU_SEVERITY_*` values, or -1 for a NULL
/// error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_severity(e: *const ZuError) -> i32 {
    if e.is_null() {
        return -1;
    }
    unsafe { &*e }.severity
}

/// Frees an error handle. No-op on NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_free(e: *mut ZuError) {
    if !e.is_null() {
        drop(unsafe { Box::from_raw(e) });
    }
}

/* ---- sessions ---- */

/// Opens a session on a zu1 file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_open(
    path: *const c_char,
    path_len: usize,
    out: *mut *mut ZuSession,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let session = Session::open(Path::new(path))?;
        unsafe { *out = Box::into_raw(Box::new(ZuSession { session })) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_open_z(
    path: *const c_char,
    out: *mut *mut ZuSession,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_open(path, zlen(path), out, err) }
}

/// Closes a session. Statements prepared on it must already be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_close(session: *mut ZuSession) {
    if !session.is_null() {
        drop(unsafe { Box::from_raw(session) });
    }
}

/// Runs one parameterless statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_query(
    session: *mut ZuSession,
    q: *const c_char,
    q_len: usize,
    out: *mut *mut ZuResult,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        if session.is_null() {
            return Err(misuse("session is NULL"));
        }
        let q = unsafe { counted(q, q_len, "query") }?;
        let session = unsafe { &mut *session };
        let result = session.session.run(q, &[])?;
        unsafe { *out = Box::into_raw(Box::new(ZuResult::new(result))) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_query_z(
    session: *mut ZuSession,
    q: *const c_char,
    out: *mut *mut ZuResult,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_query(session, q, zlen(q), out, err) }
}

/// Compiles a statement against the session's plan cache. The handle
/// carries its own bindings; bind then execute, as many times as
/// wanted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_prepare(
    session: *mut ZuSession,
    q: *const c_char,
    q_len: usize,
    out: *mut *mut ZuStmt,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        if session.is_null() {
            return Err(misuse("session is NULL"));
        }
        let q = unsafe { counted(q, q_len, "query") }?;
        let s = unsafe { &mut *session };
        let (id, _) = s.session.prepare(q)?;
        unsafe {
            *out = Box::into_raw(Box::new(ZuStmt {
                session,
                id,
                binds: Vec::new(),
            }))
        };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_prepare_z(
    session: *mut ZuSession,
    q: *const c_char,
    out: *mut *mut ZuStmt,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_prepare(session, q, zlen(q), out, err) }
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

/* ---- binding ---- */

unsafe fn bind(stmt: *mut ZuStmt, name: *const c_char, name_len: usize, value: Value) -> ZuStatus {
    guard_status(|| {
        if stmt.is_null() {
            return ZuStatus::Misuse;
        }
        let Ok(name) = (unsafe { counted(name, name_len, "name") }) else {
            return ZuStatus::Misuse;
        };
        let stmt = unsafe { &mut *stmt };
        // Rebinding a name replaces the old value, so a statement in a
        // loop binds the same names over and over without growing.
        stmt.binds.retain(|(n, _)| n != name);
        stmt.binds.push((name.to_string(), value));
        ZuStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_i64(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
    v: i64,
) -> ZuStatus {
    unsafe { bind(stmt, name, name_len, Value::Int(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_i64_z(stmt: *mut ZuStmt, name: *const c_char, v: i64) -> ZuStatus {
    unsafe { bind(stmt, name, zlen(name), Value::Int(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_f64(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
    v: f64,
) -> ZuStatus {
    unsafe { bind(stmt, name, name_len, Value::Float(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_f64_z(stmt: *mut ZuStmt, name: *const c_char, v: f64) -> ZuStatus {
    unsafe { bind(stmt, name, zlen(name), Value::Float(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_str(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
    v: *const c_char,
    v_len: usize,
) -> ZuStatus {
    let Ok(v) = (unsafe { counted(v, v_len, "value") }) else {
        return ZuStatus::Misuse;
    };
    unsafe { bind(stmt, name, name_len, Value::Str(v.to_string())) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_str_z(
    stmt: *mut ZuStmt,
    name: *const c_char,
    v: *const c_char,
) -> ZuStatus {
    unsafe { zu_bind_str(stmt, name, zlen(name), v, zlen(v)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_null(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
) -> ZuStatus {
    unsafe { bind(stmt, name, name_len, Value::Null) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_null_z(stmt: *mut ZuStmt, name: *const c_char) -> ZuStatus {
    unsafe { bind(stmt, name, zlen(name), Value::Null) }
}

/// Executes a prepared statement with its current bindings. Bindings
/// survive the call, so a loop only rebinds what changed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_execute(
    stmt: *mut ZuStmt,
    out: *mut *mut ZuResult,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        if stmt.is_null() {
            return Err(misuse("stmt is NULL"));
        }
        let stmt = unsafe { &mut *stmt };
        if stmt.session.is_null() {
            return Err(misuse("stmt has no session"));
        }
        let session = unsafe { &mut *stmt.session };
        let borrowed: Vec<(&str, Value)> = stmt
            .binds
            .iter()
            .map(|(n, v)| (n.as_str(), v.clone()))
            .collect();
        let result = session.session.execute(stmt.id, &borrowed)?;
        unsafe { *out = Box::into_raw(Box::new(ZuResult::new(result))) };
        Ok(ZuStatus::Ok)
    })
}

/* ---- results ---- */

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

/// Column name, valid until the result is freed, with its byte length
/// through `len` when that is non-NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_name(
    result: *const ZuResult,
    col: u32,
    out: *mut *const c_char,
    len: *mut usize,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null() };
    if !len.is_null() {
        unsafe { *len = 0 };
    }
    if result.is_null() {
        return ZuStatus::Misuse;
    }
    match unsafe { &*result }.col_names.get(col as usize) {
        Some(name) => {
            unsafe { *out = name.as_ptr() };
            if !len.is_null() {
                unsafe { *len = name.as_bytes().len() };
            }
            ZuStatus::Ok
        }
        None => ZuStatus::Misuse,
    }
}

fn cell(result: &ZuResult, row: u64, col: u32) -> Option<&Value> {
    result.result.rows.get(row as usize)?.get(col as usize)
}

/// Type tag of one cell, one of the `ZU_TYPE_*` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cell_type(
    result: *const ZuResult,
    row: u64,
    col: u32,
    out: *mut i32,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    // -1 rather than ZU_TYPE_NULL, because every tag is a type a cell
    // can hold and a caller who ignored the status would read this one
    // as an answer.
    unsafe { *out = -1 };
    if result.is_null() {
        return ZuStatus::Misuse;
    }
    let tag = match cell(unsafe { &*result }, row, col) {
        Some(Value::Null) => ZU_TYPE_NULL,
        Some(Value::Bool(_)) => ZU_TYPE_BOOL,
        Some(Value::Int(_)) => ZU_TYPE_INT,
        Some(Value::Float(_)) => ZU_TYPE_FLOAT,
        Some(Value::Str(_)) => ZU_TYPE_STR,
        Some(Value::Node { .. }) => ZU_TYPE_NODE,
        Some(Value::Rel { .. }) => ZU_TYPE_REL,
        Some(Value::List(_)) => ZU_TYPE_LIST,
        Some(Value::Path(_)) | Some(Value::Chain(_)) => ZU_TYPE_PATH,
        Some(Value::Record(_)) => ZU_TYPE_RECORD,
        Some(Value::Temporal(_)) => ZU_TYPE_TEMPORAL,
        None => return ZuStatus::Misuse,
    };
    unsafe { *out = tag };
    ZuStatus::Ok
}

/// The body every whole-column accessor shares: check the handle and
/// the index, answer `Done` for a result with no rows, and otherwise
/// build the column once and hand back a pointer into it. `build`
/// returns `None` when the column holds something this accessor does
/// not read, which is the caller asking the wrong question and so
/// misuse rather than an engine failure.
unsafe fn column<T>(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const T,
    slot: impl Fn(&mut ZuResult) -> &mut Option<Vec<T>>,
    build: impl FnOnce(&QueryResult, usize) -> Option<Vec<T>>,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null() };
    guard_status(|| {
        if result.is_null() {
            return ZuStatus::Misuse;
        }
        let r = unsafe { &mut *result };
        let c = col as usize;
        if c >= r.result.columns.len() {
            return ZuStatus::Misuse;
        }
        if r.result.rows.is_empty() {
            return ZuStatus::Done;
        }
        if slot(r).is_none() {
            match build(&r.result, c) {
                Some(built) => *slot(r) = Some(built),
                None => return ZuStatus::Misuse,
            }
        }
        unsafe { *out = slot(r).as_ref().expect("filled").as_ptr() };
        ZuStatus::Ok
    })
}

/// The whole column as contiguous i64s, one call for all rows. Ints
/// and bools carry their value and nulls read 0, which
/// [`zu_result_col_valid`] tells apart. `Misuse` when the column holds
/// anything else, node cells included: reading a node as its offset is
/// what [`zu_result_col_node_offset`] is for, and doing it quietly
/// here is how a binding ends up returning an internal row number to a
/// user who asked for an identity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_i64(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const i64,
) -> ZuStatus {
    unsafe {
        column(
            result,
            col,
            out,
            |r| &mut r.i64_cols[col as usize],
            |q, c| {
                let mut v = Vec::with_capacity(q.rows.len());
                for row in &q.rows {
                    match &row[c] {
                        Value::Int(i) => v.push(*i),
                        Value::Bool(b) => v.push(i64::from(*b)),
                        Value::Null => v.push(0),
                        _ => return None,
                    }
                }
                Some(v)
            },
        )
    }
}

/// The whole column as contiguous doubles; ints widen and nulls read
/// 0. `Misuse` when the column holds anything but numbers and nulls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_f64(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const f64,
) -> ZuStatus {
    unsafe {
        column(
            result,
            col,
            out,
            |r| &mut r.f64_cols[col as usize],
            |q, c| {
                let mut v = Vec::with_capacity(q.rows.len());
                for row in &q.rows {
                    match &row[c] {
                        Value::Float(f) => v.push(*f),
                        Value::Int(i) => v.push(*i as f64),
                        Value::Null => v.push(0.0),
                        _ => return None,
                    }
                }
                Some(v)
            },
        )
    }
}

/// The whole column as the row offsets of its nodes, which is the
/// engine's identity for a node and the thing a binding stores when a
/// user holds onto one. Unsigned, because an offset is a count.
/// `Misuse` when the column holds anything but nodes and nulls; a null
/// reads 0, which [`zu_result_col_valid`] tells apart.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_node_offset(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const u64,
) -> ZuStatus {
    unsafe {
        column(
            result,
            col,
            out,
            |r| &mut r.node_cols[col as usize],
            |q, c| {
                let mut v = Vec::with_capacity(q.rows.len());
                for row in &q.rows {
                    match &row[c] {
                        Value::Node { offset, .. } => v.push(*offset),
                        Value::Null => v.push(0),
                        _ => return None,
                    }
                }
                Some(v)
            },
        )
    }
}

/// One byte per row: 1 where the cell holds a value, 0 where it is
/// null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_valid(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const u8,
) -> ZuStatus {
    unsafe {
        column(
            result,
            col,
            out,
            |r| &mut r.valid_cols[col as usize],
            |q, c| {
                Some(
                    q.rows
                        .iter()
                        .map(|row| u8::from(!matches!(row[c], Value::Null)))
                        .collect(),
                )
            },
        )
    }
}

/// One string cell, NUL-terminated, valid until the result is freed,
/// with its byte length through `len` when that is non-NULL. `Misuse`
/// when the cell is out of range or does not hold a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cell_str(
    result: *mut ZuResult,
    row: u64,
    col: u32,
    out: *mut *const c_char,
    len: *mut usize,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null() };
    if !len.is_null() {
        unsafe { *len = 0 };
    }
    guard_status(|| {
        if result.is_null() {
            return ZuStatus::Misuse;
        }
        let r = unsafe { &mut *result };
        // The cell is copied once, on the first ask, because the copy
        // is what the returned pointer points at and it has to outlive
        // the call. A second ask for the same cell reads the copy, so a
        // caller walking a string column twice does not pay twice.
        if !r.strs.contains_key(&(row, col)) {
            let Some(Value::Str(s)) = cell(r, row, col) else {
                return ZuStatus::Misuse;
            };
            let owned = c_message(s);
            r.strs.insert((row, col), owned);
        }
        let c = &r.strs[&(row, col)];
        unsafe { *out = c.as_ptr() };
        if !len.is_null() {
            unsafe { *len = c.as_bytes().len() };
        }
        ZuStatus::Ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_free(result: *mut ZuResult) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zudb::gqlstatus::codes;

    /// Every engine error has a status, and the mapping is the one
    /// dx/02 fixes rather than whichever one a wildcard would have
    /// produced. [`status_of`] has no wildcard arm, so a new variant
    /// fails to compile there; this pins the six that exist.
    #[test]
    fn every_engine_error_maps_to_the_status_the_abi_promises() {
        let cases = [
            (
                EngineError::gql(codes::C42001, "unexpected token"),
                ZuStatus::Error,
            ),
            (
                EngineError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
                ZuStatus::Io,
            ),
            (
                EngineError::Corrupt {
                    what: "block",
                    detail: "bad checksum".to_string(),
                },
                ZuStatus::Corrupt,
            ),
            (
                EngineError::Unsupported {
                    what: "opcode",
                    id: 7,
                },
                ZuStatus::Unsupported,
            ),
            (
                EngineError::InvalidArgument("no such table".to_string()),
                ZuStatus::Misuse,
            ),
            (
                EngineError::Conflict("write lost".to_string()),
                ZuStatus::Conflict,
            ),
        ];
        for (error, want) in cases {
            assert_eq!(status_of(&error), want, "{error}");
        }
    }

    /// A GQL error carries its condition out as data; an internal one
    /// carries no code rather than a guessed one, and is still an
    /// exception because it still stopped the statement.
    #[test]
    fn an_error_handle_carries_the_code_and_severity_it_has() {
        let gql = ZuError::from_engine(&EngineError::gql(codes::C42001, "bad"));
        assert_eq!(gql.status, ZuStatus::Error);
        assert_eq!(
            gql.code.as_deref().map(|c| c.to_str().expect("utf-8")),
            Some(codes::C42001.code())
        );
        assert_eq!(gql.severity, ZU_SEVERITY_EXCEPTION);

        let internal = ZuError::from_engine(&EngineError::Conflict("lost".to_string()));
        assert_eq!(internal.status, ZuStatus::Conflict);
        assert!(internal.code.is_none());
        assert_eq!(internal.severity, ZU_SEVERITY_EXCEPTION);
        assert!(internal.message.to_str().expect("utf-8").contains("lost"));
    }

    /// A NUL inside a message would truncate it at the boundary, so
    /// the whole sentence survives with the NUL replaced rather than
    /// the message ending where the caller's data happened to.
    #[test]
    fn a_nul_inside_a_message_does_not_truncate_it() {
        let e = ZuError::from_engine(&EngineError::InvalidArgument(
            "table \0 does not exist".to_string(),
        ));
        let message = e.message.to_str().expect("utf-8");
        assert!(message.ends_with("does not exist"), "{message}");
        assert!(!message.contains('\0'));
    }

    /// A null pointer with a zero length is the empty string, because
    /// that is what an empty string looks like coming out of several
    /// source languages and building a slice from it would otherwise
    /// be undefined behaviour.
    #[test]
    fn an_empty_counted_string_may_have_a_null_pointer() {
        assert_eq!(
            unsafe { counted(std::ptr::null(), 0, "q") }.expect("empty"),
            ""
        );
        assert!(unsafe { counted(std::ptr::null(), 3, "q") }.is_err());
        assert_eq!(unsafe { zlen(std::ptr::null()) }, 0);
        assert_eq!(unsafe { zlen(c"abc".as_ptr()) }, 3);
    }
}
