//! libzu: the C surface over [`zudb::Database`] and
//! [`zudb::Connection`], built for a host that keeps the process alive
//! and queries in a loop (a cgo adapter, an editor, a language
//! binding).
//!
//! The object model is the Rust one, because dx/02 §3 is where both
//! came from. A [`ZuDatabase`] is a path and a configuration that have
//! been checked against a real file; it holds no descriptor and no
//! cache, so it is shareable across threads without a lock. A
//! [`ZuConn`] is the state that cannot be shared: a file handle, the
//! caches, and the plans compiled against a catalog. A host that wants
//! to query from four threads opens one database and connects four
//! times, which is the shape every pooling binding above this needs and
//! the shape a single `zu_open` returning one session could not
//! express.
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
//! about take one: opening, connecting, running, preparing, executing,
//! and setting a configuration key. The accessors do not, because
//! their failures are structural (a column out of range, a column that
//! does not hold what the accessor reads) and the status names each
//! one exactly.
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
//! is a no-op on `NULL` (R8). Where the contract can be checked it is,
//! rather than left as undefined behaviour: see [`ConnState`].
//!
//! Results read out column-at-a-time: `zu_result_col_i64` materializes
//! a whole column into a contiguous buffer once, so a host crossing an
//! FFI boundary pays one call per column, not one per cell. That is
//! the difference between an in-process point read spending its budget
//! on the query or on the boundary.
//!
//! The same columns read out a chunk at a time as well, through
//! `zu_result_chunk_count`, `zu_result_chunk`, and the
//! `zu_result_chunk_col_*` accessors. The whole-column form is the
//! right one for a point read, where the answer is small and one call
//! is cheaper than a loop. The chunked form is the right one for
//! anything large: it converts what was asked for rather than the whole
//! column, and it holds one [`CHUNK_ROWS`]-row buffer per column
//! instead of one buffer per column the length of the result. A host
//! that reads a million-row answer into its own arrays as it goes, or
//! that stops early, pays for what it read.
//!
//! Not every value has a column of host scalars to be read into, which
//! is what [`ZuValue`] is for. A temporal is a count and a unit, a list
//! recurses, a node is a table and an offset, and none of the three
//! fits an `int64_t *`. A cell pointer borrows from the result rather
//! than allocating, so the reader costs a pointer per value and the
//! columnar path stays the one a bulk read uses.
//!
//! Values get in two ways, because a host with data has one of two
//! problems. [`ZuLoader`] builds a database that does not exist yet:
//! `CREATE` and `INSERT` need a table and no statement makes one, so a
//! host with data and an empty file has nowhere else to go, and it is
//! columnar for the same reason a result is, one call per column rather
//! than one per cell. [`ZuAppender`] adds to a database that does
//! exist, a value at a time into buffers that a flush turns into one
//! commit, which is what a host reading rows out of somewhere else has
//! to hand. Both are the entry points the Rust appender and `zu copy`
//! are built on rather than a second way in beside them.

// The pointer contract (what must be valid, who frees what, in which
// order) is one contract for the whole surface; it lives in the module
// docs above and in include/zu.h where a C caller will actually read
// it, not repeated under every function.
#![allow(clippy::missing_safety_doc)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::{offset_of, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zu_common::{DurationKind, FloatBits, IntBits, LogicalType, Temporal};
use zudb::query::{QueryResult, Value};
use zudb::zu1::catalog::Catalog;
use zudb::zu1::file::Zu1File;
use zudb::zu1::graph::bulk_load_keyed;
use zudb::zu1::props::{PropValues, load_props, store_props};
use zudb::{
    Config, Connection, Database, DiagnosticRecord, Field, Interrupt, Position, Severity,
    ZuError as EngineError,
};

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
/// gaps that remain are held for the rest of the set dx/02 §6 names
/// and nothing produces yet: 1 for `ZU_ROW`, 7 for `ZU_INTERRUPTED`,
/// and 12 for `ZU_OOM`.
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
    /// Two threads used one connection at once. Distinct from
    /// [`ZuStatus::Misuse`] because it is the one mistake a host makes
    /// by accident rather than by typo, and because the fix is a
    /// different one: connect again rather than correct the call.
    MisuseConcurrent = 5,
    /// A statement was used after the connection it was prepared on
    /// was closed. Nothing was done; the statement handle is still
    /// safe to close.
    MisuseClosed = 6,
    /// The caller stopped the statement while it was running. The
    /// connection is unharmed and the next call on it runs normally,
    /// which is what tells this apart from every other non-zero status
    /// here: nothing went wrong.
    Interrupted = 7,
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
/// GV60 and GV61, the two reference values. A host reads neither
/// through an accessor: a handle has no contents to hand over, so the
/// tag is the whole of what a binding can say about the cell.
pub const ZU_TYPE_GRAPH: i32 = 11;
pub const ZU_TYPE_BINDING_TABLE: i32 = 12;

/// Which temporal a temporal cell is, on [`zu_value_temporal`].
///
/// A temporal value is a count and a meaning, and the meaning is what
/// picks the unit: days for a date, months for a year-month duration,
/// nanoseconds for the other five. One tag rather than a type per
/// arm, because a host that reads temporals reads all of them and a
/// `switch` over seven is the shape it wants.
pub const ZU_TEMPORAL_DATE: i32 = 0;
pub const ZU_TEMPORAL_LOCAL_TIME: i32 = 1;
pub const ZU_TEMPORAL_ZONED_TIME: i32 = 2;
pub const ZU_TEMPORAL_LOCAL_DATETIME: i32 = 3;
pub const ZU_TEMPORAL_ZONED_DATETIME: i32 = 4;
pub const ZU_TEMPORAL_DURATION_YEAR_MONTH: i32 = 5;
pub const ZU_TEMPORAL_DURATION_DAY_TIME: i32 = 6;

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
    /// The standard's own words for that condition, which is the text a
    /// conformance harness grades and the text a binding shows when it
    /// would rather not repeat our detail.
    standard_text: Option<CString>,
    /// The page that documents the condition, built from the code.
    doc_url: Option<CString>,
    severity: i32,
    /// Whether running the same statement again could succeed, as 0 or
    /// 1, so that a binding's retry loop is a field read and not a set
    /// of codes each binding writes out for itself.
    retryable: i32,
    /// Offset, line and column for a condition raised at a place in the
    /// statement text. A failure that happened at runtime, or in the
    /// engine rather than in a statement, has none.
    position: Option<Position>,
    /// The line that position is on, when the statement text was in
    /// hand where the condition was raised.
    excerpt: Option<CString>,
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
        EngineError::Interrupted => ZuStatus::Interrupted,
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
            standard_text: record.map(|r| c_message(&r.status.standard_text())),
            doc_url: record.map(|r| c_message(&r.doc_url())),
            // An error with no condition is still an exception: it
            // stopped the statement, which is what severity is for.
            severity: record.map_or(ZU_SEVERITY_EXCEPTION, |r| severity_of(r.severity())),
            retryable: i32::from(e.retryable()),
            position: e.position(),
            excerpt: e.excerpt().map(c_message),
        }
    }

    /// A condition a statement raised and survived, on the same handle
    /// a failure comes back on.
    ///
    /// One shape rather than two, because a diagnostic record is a
    /// diagnostic record and the standard says what is on one: the
    /// code, its standard text, the severity, the place, the line that
    /// place is on and the page it is written up on are the same
    /// accessors either way, and a binding that already turns one of
    /// these into an exception gets its warning class for the cost of
    /// reading the severity. That is what tells them apart, and it is
    /// what the field is for. `status` is [`ZuStatus::Ok`] because that
    /// is what the call this came from returned, which is the whole
    /// difference between a notice and a failure.
    fn from_notice(record: &DiagnosticRecord) -> ZuError {
        ZuError {
            status: ZuStatus::Ok,
            message: c_message(&record.to_string()),
            code: Some(c_message(record.status.code())),
            standard_text: Some(c_message(&record.status.standard_text())),
            doc_url: Some(c_message(&record.doc_url())),
            severity: severity_of(record.severity()),
            retryable: i32::from(record.retryable()),
            position: record.position,
            excerpt: record.excerpt.as_deref().map(c_message),
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
            standard_text: None,
            doc_url: None,
            severity: ZU_SEVERITY_EXCEPTION,
            retryable: 0,
            position: None,
            excerpt: None,
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

/* ---- configuration ---- */

/// How a database is opened. The one struct that crosses this boundary
/// by value, which dx/02 R1 allows exactly because it is versioned:
/// `struct_size` is first, the caller sets it, and every field after
/// it is read only when the size says the caller's struct is long
/// enough to hold it.
///
/// That is what lets a field be appended without breaking a binding
/// compiled against the header before it. The alternative, an opaque
/// handle with an allocator and a destructor, costs a heap allocation
/// and two more calls to express three integers a caller already has
/// on its stack.
///
/// Zero means "the default" for every field, so a caller that
/// memsets the struct and sets `struct_size` gets the same database as
/// a caller that passed `NULL`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZuConfig {
    /// `sizeof(zu_config)` as the caller's header defines it. Set by
    /// [`zu_config_init`]; a struct that arrives with anything else is
    /// read as the prefix that size describes.
    pub struct_size: usize,
    /// Bytes the caches may hold. Zero leaves the engine default.
    pub memory_limit: usize,
    /// Worker threads for the parallel stages of a query. Zero lets
    /// the executor pick, and one forces sequential execution, which
    /// is what a host running many connections at once wants.
    pub threads: usize,
    /// Nonzero opens on a descriptor this process cannot write
    /// through.
    pub read_only: i32,
}

impl Default for ZuConfig {
    fn default() -> ZuConfig {
        ZuConfig {
            struct_size: size_of::<ZuConfig>(),
            memory_limit: 0,
            threads: 0,
            read_only: 0,
        }
    }
}

/// Fills a configuration with the defaults and stamps its
/// `struct_size`. A caller that skips this and zeroes the struct by
/// hand must still set `struct_size`, because a zero there describes a
/// struct with no fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_config_init(cfg: *mut ZuConfig) -> ZuStatus {
    if cfg.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { cfg.write(ZuConfig::default()) };
    ZuStatus::Ok
}

/// Sets one option by name, so a binding can forward a user's option
/// map without this ABI growing a setter per option and without the
/// binding hard-coding a struct layout it would have to keep in step.
///
/// Keys are `memory_limit`, `threads`, and `read_only`. The first two
/// take a decimal count of bytes and of threads; suffixes such as `MB`
/// are deliberately not parsed here, because the two readings of that
/// suffix differ by 4.9% and the language the user typed it in is a
/// better place to decide which one they meant. `read_only` takes
/// `true`, `false`, `1`, or `0`.
///
/// An unrecognized key is refused and named, since a binding
/// forwarding a map needs to tell its user which entry was the typo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_config_set(
    cfg: *mut ZuConfig,
    key: *const c_char,
    key_len: usize,
    value: *const c_char,
    value_len: usize,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        if cfg.is_null() {
            return Err(misuse("config is NULL"));
        }
        let key = unsafe { counted(key, key_len, "key") }?;
        let value = unsafe { counted(value, value_len, "value") }?;
        let count = |what: &str| -> Result<usize, EngineError> {
            value
                .parse::<usize>()
                .map_err(|_| misuse(format!("{what} wants a decimal count, not {value:?}")))
        };
        let cfg = unsafe { &mut *cfg };
        match key {
            "memory_limit" => cfg.memory_limit = count("memory_limit")?,
            "threads" => cfg.threads = count("threads")?,
            "read_only" => {
                cfg.read_only = match value {
                    "true" | "1" => 1,
                    "false" | "0" => 0,
                    _ => return Err(misuse(format!("read_only wants a boolean, not {value:?}"))),
                }
            }
            _ => return Err(misuse(format!("unknown configuration key {key:?}"))),
        }
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_config_set_z(
    cfg: *mut ZuConfig,
    key: *const c_char,
    value: *const c_char,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_config_set(cfg, key, zlen(key), value, zlen(value), err) }
}

/// Reads a caller's configuration as far as its `struct_size` says it
/// goes, which is the whole point of the field: a binding built
/// against an older header passes a shorter struct, and the fields it
/// never heard of take their defaults instead of whatever follows its
/// allocation.
unsafe fn config_of(cfg: *const ZuConfig) -> Result<Config, EngineError> {
    // NULL is the defaults, so a host with nothing to configure calls
    // zu_database_open without first building a struct to say so.
    if cfg.is_null() {
        return Ok(Config::new());
    }
    let size = unsafe { (*cfg).struct_size };
    if size < size_of::<usize>() {
        return Err(misuse(format!(
            "config struct_size is {size}, too small to hold the field that holds it"
        )));
    }
    // True when the caller's struct is long enough to contain a field
    // that ends at `end`. A newer caller's longer struct passes every
    // one of these and its extra fields are ignored, which is the
    // other direction of the same compatibility.
    let has = |end: usize| size >= end;
    let mut config = Config::new();
    if has(offset_of!(ZuConfig, memory_limit) + size_of::<usize>()) {
        let bytes = unsafe { (*cfg).memory_limit };
        if bytes != 0 {
            config = config.memory_limit(bytes);
        }
    }
    if has(offset_of!(ZuConfig, threads) + size_of::<usize>()) {
        config = config.threads(unsafe { (*cfg).threads });
    }
    if has(offset_of!(ZuConfig, read_only) + size_of::<i32>()) {
        config = config.read_only(unsafe { (*cfg).read_only } != 0);
    }
    Ok(config)
}

/* ---- handles ---- */

/// Whether a connection is still open, and whether a call is in it.
///
/// dx/02 §5 says a connection may move between threads but must not be
/// used from two at once, and that a statement used after its
/// connection closes is a detected mistake rather than undefined
/// behaviour. Both are checks rather than prose here, because prose in
/// a header is only read by the hosts that were not going to make the
/// mistake.
///
/// The state is behind an `Arc` shared with every statement prepared
/// on the connection, so it outlives the connection itself: a
/// statement closed after its connection finds `alive` false and
/// declines instead of following a dangling pointer.
///
/// `busy` costs one uncontended swap per call, which is a handful of
/// cycles against a query, and it catches the case that otherwise
/// corrupts a plan cache silently on a machine the host could not
/// reproduce.
struct ConnState {
    alive: AtomicBool,
    busy: AtomicBool,
}

/// The right to use a connection for the length of one call, released
/// on drop so that an early return, a `?`, or a panic caught at the
/// boundary all put the connection back.
struct Claim(Arc<ConnState>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

fn claim(state: &Arc<ConnState>) -> Result<Claim, ZuStatus> {
    if !state.alive.load(Ordering::Acquire) {
        return Err(ZuStatus::MisuseClosed);
    }
    if state.busy.swap(true, Ordering::AcqRel) {
        return Err(ZuStatus::MisuseConcurrent);
    }
    Ok(Claim(Arc::clone(state)))
}

/* ---- reaching into a handle ---- */

// Every accessor below borrows one field of a handle, never the whole
// handle, and that is what makes the concurrency check itself safe to
// run concurrently.
//
// A second thread arriving at a connection that is already in a call
// has to read `state` to find out that it must go away. Writing that
// as `(&*conn).state` would form a `&ZuConn` covering the same bytes
// as the `&mut ZuConn` the thread inside the call is holding, which is
// undefined behaviour whether or not the second thread goes on to
// touch anything. Projecting the field off the raw pointer instead
// keeps the two disjoint: the thread inside the call borrows the
// connection, the thread being turned away borrows the flag, and
// neither reference covers the other's bytes.

/// The state of a connection, cloned so the caller holds no borrow.
unsafe fn conn_state(conn: *mut ZuConn) -> Arc<ConnState> {
    Arc::clone(unsafe { &(*conn).state })
}

/// The engine connection itself. Only ever called under a [`Claim`],
/// which is what makes the `&mut` unique.
unsafe fn conn_of<'a>(conn: *mut ZuConn) -> &'a mut Connection {
    unsafe { &mut (*conn).conn }
}

/// The word a running statement reads, cloned so the caller holds no
/// borrow. This is the one field a second thread touches on purpose
/// while the first is inside a call.
unsafe fn conn_stop(conn: *mut ZuConn) -> Interrupt {
    unsafe { (*conn).stop.clone() }
}

/// Where a connection keeps what it was asked to report progress to.
unsafe fn conn_progress<'a>(conn: *mut ZuConn) -> &'a Mutex<Option<Hook>> {
    unsafe { &(*conn).progress }
}

/// The state a statement shares with the connection it belongs to.
unsafe fn stmt_state(stmt: *mut ZuStmt) -> Arc<ConnState> {
    Arc::clone(unsafe { &(*stmt).state })
}

/// A statement's pending bindings, under a [`Claim`] as above.
unsafe fn stmt_binds<'a>(stmt: *mut ZuStmt) -> &'a mut Vec<(String, Value)> {
    unsafe { &mut (*stmt).binds }
}

/// An open database: a path and a configuration, both checked against
/// a real file. Opaque to C, thread-safe, and cheap, since it holds no
/// descriptor and no cache.
pub struct ZuDatabase {
    db: Database,
    path: CString,
}

/// One connection, with its own file handle, caches, and plan cache.
/// Opaque to C, and not thread-safe: see [`ConnState`].
///
/// The two fields beside the connection are the ones another thread
/// reads while this one is inside a call, which is what cancellation
/// is: `stop` is the word the running statement checks, and `progress`
/// is what the watcher of that statement was asked to report through.
/// Both are separate fields rather than state reached through
/// `Connection`, because reaching through it would mean forming a
/// reference covering the same bytes as the `&mut Connection` the
/// thread inside the call is holding.
pub struct ZuConn {
    conn: Connection,
    state: Arc<ConnState>,
    stop: Interrupt,
    progress: Mutex<Option<Hook>>,
}

impl ZuConn {
    fn new(conn: Connection) -> ZuConn {
        let stop = conn.interrupt();
        ZuConn {
            conn,
            state: Arc::new(ConnState {
                alive: AtomicBool::new(true),
                busy: AtomicBool::new(false),
            }),
            stop,
            progress: Mutex::new(None),
        }
    }

    fn into_raw(self) -> *mut ZuConn {
        Box::into_raw(Box::new(self))
    }
}

/// A prepared statement plus its pending bindings.
///
/// It holds a raw pointer back to its connection and a clone of that
/// connection's state. The pointer is followed only after the state
/// says the connection is still open, which is what turns the classic
/// use-after-close of a `sqlite3_stmt` into [`ZuStatus::MisuseClosed`].
pub struct ZuStmt {
    conn: *mut ZuConn,
    state: Arc<ConnState>,
    id: u64,
    binds: Vec<(String, Value)>,
}

/// One query result, owning the rows and every buffer handed out over
/// the boundary: column-name CStrings up front, columnar i64/f64/
/// validity buffers and cell strings materialized on first request and
/// kept until the result is freed, so returned pointers stay stable.
///
/// A result owns its rows outright and holds nothing of its
/// connection, so it stays readable after that connection closes. That
/// is deliberate: a host that hands a result to another layer and
/// returns the connection to a pool is doing the ordinary thing, and
/// there is no reason to make it an error when nothing is borrowed.
pub struct ZuResult {
    result: QueryResult,
    col_names: Vec<CString>,
    i64_cols: Vec<Option<Vec<i64>>>,
    f64_cols: Vec<Option<Vec<f64>>>,
    node_cols: Vec<Option<Vec<u64>>>,
    valid_cols: Vec<Option<Vec<u8>>>,
    chunk_i64: Vec<Chunk<i64>>,
    chunk_f64: Vec<Chunk<f64>>,
    chunk_node: Vec<Chunk<u64>>,
    chunk_valid: Vec<Chunk<u8>>,
    strs: HashMap<(u64, u32), CString>,
    /// The statement's completion condition, kept NUL-terminated
    /// because [`zu_result_gqlstatus`] hands it out. Taken from the
    /// result rather than worked out here, so it cannot come to
    /// disagree with what the engine would say.
    gqlstatus: CString,
}

/// Rows in a chunk, which is the width the executor works in.
///
/// The library fixes it rather than letting a caller choose, because a
/// chunk is going to be a vector the executor produced rather than a
/// range this code slices, and a caller who had picked its own size
/// would be asking for a regroup on every read. A caller that wants to
/// know asks each chunk how many rows it has.
const CHUNK_ROWS: usize = 2048;

/// One reusable buffer per column and per accessor, holding whichever
/// chunk was asked for last.
///
/// This is what the chunked path buys. A whole-column accessor converts
/// every row and keeps the conversion until the result is freed, so a
/// column of ten million rows costs eighty megabytes of buffer on top
/// of the rows themselves, and a host that reads the first hundred rows
/// and stops pays for all ten million. A chunk buffer is
/// [`CHUNK_ROWS`] elements however long the result is, and converts
/// only the chunks that were asked for.
struct Chunk<T> {
    /// Which chunk the buffer holds, so asking twice converts once.
    at: Option<u64>,
    buf: Vec<T>,
}

/// Not `derive(Default)`, which would want `T: Default` for a field
/// that is only ever an empty `Vec`.
impl<T> Default for Chunk<T> {
    fn default() -> Chunk<T> {
        Chunk {
            at: None,
            buf: Vec::new(),
        }
    }
}

fn chunk_slots<T>(cols: usize) -> Vec<Chunk<T>> {
    (0..cols).map(|_| Chunk::default()).collect()
}

impl ZuResult {
    fn new(result: QueryResult) -> ZuResult {
        let col_names = result
            .columns
            .iter()
            .map(|c| c_message(c.as_str()))
            .collect();
        let cols = result.columns.len();
        let gqlstatus = c_message(result.status().code());
        ZuResult {
            result,
            col_names,
            i64_cols: vec![None; cols],
            f64_cols: vec![None; cols],
            node_cols: vec![None; cols],
            valid_cols: vec![None; cols],
            chunk_i64: chunk_slots(cols),
            chunk_f64: chunk_slots(cols),
            chunk_node: chunk_slots(cols),
            chunk_valid: chunk_slots(cols),
            strs: HashMap::new(),
            gqlstatus,
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

/// One of the strings an error carries when it has one, as a pointer
/// and a length, with a NULL pointer and a zero length when it does
/// not. A field that is absent is absent rather than empty, because an
/// empty condition code and no condition code are different facts.
fn field(field: &Option<CString>, len: *mut usize) -> *const c_char {
    if !len.is_null() {
        unsafe { *len = field.as_ref().map_or(0, |s| s.as_bytes().len()) };
    }
    field.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
}

/// The GQLSTATUS code, such as `42001`, or NULL when this failure
/// carries no condition. An engine-internal failure has none rather
/// than one that would be a guess, and a binding mapping codes to
/// exception classes needs to be able to tell those apart.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_code(e: *const ZuError, len: *mut usize) -> *const c_char {
    if e.is_null() {
        return field(&None, len);
    }
    field(&unsafe { &*e }.code, len)
}

/// The standard's own name for the condition, such as `syntax error or
/// access rule violation, invalid syntax`, or NULL when this failure
/// carries no condition.
///
/// This is the class name and the subclass name, in the standard's
/// words, which is what a conformance harness compares and what a
/// binding shows when it presents the condition rather than our
/// account of it. [`zu_error_message`] is that account, and it says
/// which table, which token, which value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_standard_text(
    e: *const ZuError,
    len: *mut usize,
) -> *const c_char {
    if e.is_null() {
        return field(&None, len);
    }
    field(&unsafe { &*e }.standard_text, len)
}

/// Where this condition is documented, or NULL when the failure
/// carries no condition. Built from the code, so a binding that puts
/// it in a message hands the reader a page rather than five characters
/// to go and search for.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_doc_url(e: *const ZuError, len: *mut usize) -> *const c_char {
    if e.is_null() {
        return field(&None, len);
    }
    field(&unsafe { &*e }.doc_url, len)
}

/// The line of the statement the condition was raised on, without its
/// newline, or NULL when there is none.
///
/// The column from [`zu_error_position`] counts characters into this
/// line, so a caller has both halves of a caret without holding the
/// statement text: the line to print and the place to point at. There
/// is none when the failure has no position, when that line is empty,
/// and when it is longer than anyone would read under a caret, which
/// is the case a cut line would misplace the column for.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_excerpt(e: *const ZuError, len: *mut usize) -> *const c_char {
    if e.is_null() {
        return field(&None, len);
    }
    field(&unsafe { &*e }.excerpt, len)
}

/// Whether running the same statement again could succeed: 1 for yes,
/// 0 for no, -1 for a NULL error.
///
/// A write that lost to a concurrent one is the yes: nothing of it was
/// applied and the same call on a fresh read may win. A statement that
/// will not parse is the no, and so is one the caller interrupted,
/// which did not fail so much as stop. A binding's retry loop reads
/// this instead of carrying its own list of codes, which is the sort
/// of list that is right in one binding and stale in the other five.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_retryable(e: *const ZuError) -> i32 {
    if e.is_null() {
        return -1;
    }
    unsafe { &*e }.retryable
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

/// Where in the statement text the condition was raised, both 1-based,
/// with the column counted in characters rather than bytes.
///
/// [`ZuStatus::Ok`] and the two out-parameters written when there is a
/// position, [`ZuStatus::Done`] and both left alone when there is not,
/// [`ZuStatus::Misuse`] for a NULL error. Not every failure has one: a
/// division by zero happens while the statement runs and has no token
/// to point at, and an io error has no statement at all. Either
/// out-parameter may be NULL for a caller that wants only the other.
///
/// The message says the same thing in words, and keeps saying it, so
/// that printing the message alone is still a complete report. This is
/// for the caller that would rather underline the token than parse the
/// sentence back into numbers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_position(
    e: *const ZuError,
    line: *mut u32,
    column: *mut u32,
) -> ZuStatus {
    if e.is_null() {
        return ZuStatus::Misuse;
    }
    match unsafe { &*e }.position {
        Some(at) => {
            if !line.is_null() {
                unsafe { *line = at.line };
            }
            if !column.is_null() {
                unsafe { *column = at.column };
            }
            ZuStatus::Ok
        }
        None => ZuStatus::Done,
    }
}

/// The same place counted in bytes from the start of the statement,
/// 0-based, for a caller that indexes the text rather than printing
/// it.
///
/// [`ZuStatus::Ok`] and `offset` written when there is a position,
/// [`ZuStatus::Done`] and `offset` left alone when there is not,
/// [`ZuStatus::Misuse`] for a NULL error, which is
/// [`zu_error_position`] exactly. It is always on a character boundary
/// of the statement, so a binding slicing at it cannot split a
/// character in half, and it is what an editor mapping the failure
/// into its own buffer wants: recovering it from the line and the
/// column means counting the text again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_offset(e: *const ZuError, offset: *mut u32) -> ZuStatus {
    if e.is_null() {
        return ZuStatus::Misuse;
    }
    match unsafe { &*e }.position {
        Some(at) => {
            if !offset.is_null() {
                unsafe { *offset = at.offset };
            }
            ZuStatus::Ok
        }
        None => ZuStatus::Done,
    }
}

/// Frees an error handle. No-op on NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_error_free(e: *mut ZuError) {
    if !e.is_null() {
        drop(unsafe { Box::from_raw(e) });
    }
}

/* ---- databases ---- */

/// Opens a database. `cfg` may be NULL for the defaults.
///
/// The file is opened once here and closed again, so a path that is
/// not a zu1 file, or is one this build cannot read, fails now rather
/// than on the first connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_open(
    path: *const c_char,
    path_len: usize,
    cfg: *const ZuConfig,
    out: *mut *mut ZuDatabase,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let db = Database::open_with(Path::new(path), unsafe { config_of(cfg) }?)?;
        let stored = c_message(path);
        unsafe { *out = Box::into_raw(Box::new(ZuDatabase { db, path: stored })) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_open_z(
    path: *const c_char,
    cfg: *const ZuConfig,
    out: *mut *mut ZuDatabase,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_database_open(path, zlen(path), cfg, out, err) }
}

/// Creates a database and opens it. `cfg` may be NULL for the
/// defaults.
///
/// The path must not exist. A create that opened what it found there
/// would be the call that quietly writes into somebody else's data, and
/// a host that wants either one has [`zu_database_open`] to fall back
/// to and a decision to make about which.
///
/// What it makes is a valid database with nothing in it. That is the
/// entry point a C host had no way to reach before: bulk load makes a
/// database with a table in it, and a host wanting an empty one to run
/// statements against had nowhere to start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_create(
    path: *const c_char,
    path_len: usize,
    cfg: *const ZuConfig,
    out: *mut *mut ZuDatabase,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let db = Database::create_with(Path::new(path), unsafe { config_of(cfg) }?)?;
        let stored = c_message(path);
        unsafe { *out = Box::into_raw(Box::new(ZuDatabase { db, path: stored })) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_create_z(
    path: *const c_char,
    cfg: *const ZuConfig,
    out: *mut *mut ZuDatabase,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_database_create(path, zlen(path), cfg, out, err) }
}

/// The path this database was opened with, valid until it is closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_path(
    db: *const ZuDatabase,
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
    if db.is_null() {
        return ZuStatus::Misuse;
    }
    let path = &unsafe { &*db }.path;
    unsafe { *out = path.as_ptr() };
    if !len.is_null() {
        unsafe { *len = path.as_bytes().len() };
    }
    ZuStatus::Ok
}

/// Closes a database. Connections opened from it keep working, since
/// each holds its own file handle; this releases the path and the
/// configuration and nothing else.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_database_close(db: *mut ZuDatabase) {
    if !db.is_null() {
        drop(unsafe { Box::from_raw(db) });
    }
}

/* ---- connections ---- */

/// A new connection on an open database: its own file handle, its own
/// caches, its own plan cache. This is the call a pool makes, and it
/// is why the database is a separate handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_connect(
    db: *mut ZuDatabase,
    out: *mut *mut ZuConn,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        if db.is_null() {
            return Err(misuse("database is NULL"));
        }
        let conn = unsafe { &*db }.db.connect()?;
        unsafe { *out = ZuConn::new(conn).into_raw() };
        Ok(ZuStatus::Ok)
    })
}

/// Opens a database and one connection on it, for the host that wants
/// exactly one.
///
/// The database handle is not returned because nothing outlives it:
/// the connection carries its own file handle, and a `ZuDatabase` is
/// only a path and a configuration. A host that wants a second
/// connection wants [`zu_database_open`] and [`zu_connect`] instead,
/// which is the point of the split.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_open(
    path: *const c_char,
    path_len: usize,
    out: *mut *mut ZuConn,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let conn = Database::open(Path::new(path))?.connect()?;
        unsafe { *out = ZuConn::new(conn).into_raw() };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_open_z(
    path: *const c_char,
    out: *mut *mut ZuConn,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_open(path, zlen(path), out, err) }
}

/// Creates a database and one connection on it, for the host that
/// wants exactly one. [`zu_open`] for a database that is already there,
/// [`zu_database_create`] for a configuration or a second connection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_create(
    path: *const c_char,
    path_len: usize,
    out: *mut *mut ZuConn,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let conn = Database::create(Path::new(path))?.connect()?;
        unsafe { *out = ZuConn::new(conn).into_raw() };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_create_z(
    path: *const c_char,
    out: *mut *mut ZuConn,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_create(path, zlen(path), out, err) }
}

/// Closes a connection. Statements prepared on it can still be closed
/// afterwards, and anything else done with them answers
/// [`ZuStatus::MisuseClosed`].
///
/// Closing is itself a use of the connection, so it obeys the same
/// rule as every other one. A close racing a call on another thread is
/// the mistake dx/02 §5 forbids; this marks the connection closed and
/// then leaks it rather than freeing memory the other thread is inside
/// of, because a leak on a detected mistake is recoverable and the
/// free is not.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_conn_close(conn: *mut ZuConn) {
    if conn.is_null() {
        return;
    }
    let state = unsafe { conn_state(conn) };
    state.alive.store(false, Ordering::Release);
    if state.busy.load(Ordering::Acquire) {
        return;
    }
    drop(unsafe { Box::from_raw(conn) });
}

/// The name this had before dx/02 §8 split the database from the
/// connection. Kept for one release beside the `zu_session` typedef so
/// that code written against v0 still compiles; both go at the freeze.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_close(conn: *mut ZuConn) {
    unsafe { zu_conn_close(conn) }
}

/* ---- cancellation and progress ---- */

/// What a host is called back on while its statement runs: how many
/// rows have been read out of storage and how long the statement has
/// been going, answering zero to stop it and anything else to let it
/// carry on.
///
/// Nullable, because passing no callback is how a host takes one back.
pub type ZuProgressFn =
    Option<unsafe extern "C" fn(user: *mut c_void, rows: u64, ms: u64) -> c_int>;

/// A registered callback, its opaque argument, and how often to call
/// it.
///
/// `Send` is asserted rather than derived, because the pointer is the
/// host's and Rust has no way to know what is behind it. The header
/// says what that assertion is: the callback runs on a thread of this
/// library's, so whatever the pointer names has to be usable from one.
/// It is the same promise every C library with a worker thread asks
/// for, and it is stated where a host will read it rather than left to
/// be discovered.
#[derive(Clone, Copy)]
struct Hook {
    call: unsafe extern "C" fn(*mut c_void, u64, u64) -> c_int,
    user: *mut c_void,
    every: Duration,
}

unsafe impl Send for Hook {}

/// The thread that reports on a statement while it runs.
///
/// A statement runs on the thread that asked for it and that thread is
/// inside the executor, so somebody else has to do the reporting. It
/// is a thread rather than a hook the executor calls for two reasons:
/// the executor's boundaries belong to the query rather than to the
/// clock, so a host asking for a report every 100 ms would get one per
/// chunk instead, and a callback raised from a worker of a parallel
/// stage would reach a host on a thread it never gave us and possibly
/// several at once. This way the host's function is called on one
/// thread, never re-entering the engine, at the period it asked for.
///
/// It costs a thread per statement, and only for a host that asked for
/// progress at all. That is tens of microseconds against a statement
/// somebody is sitting watching, which is the only statement anybody
/// asks to be told about.
struct Watcher {
    done: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn start(hook: Hook, stop: Interrupt) -> Watcher {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let waiting = Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            // Named here so the closure captures the whole hook, which
            // is the thing that is `Send`, rather than capturing the
            // host's pointer out of it on its own, which is not.
            let hook = hook;
            let start = Instant::now();
            let (lock, wake) = &*waiting;
            loop {
                let finished = lock.lock().unwrap_or_else(|e| e.into_inner());
                let (finished, timing) = wake
                    .wait_timeout_while(finished, hook.every, |done| !*done)
                    .unwrap_or_else(|e| e.into_inner());
                // Woken rather than timed out means the statement is
                // over, and a report after it ended is a report about
                // nothing.
                if !timing.timed_out() {
                    break;
                }
                drop(finished);
                let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                // SAFETY: the host gave us this function and this
                // pointer together and promised both outlive the call
                // that registered them.
                if unsafe { (hook.call)(hook.user, stop.rows(), ms) } == 0 {
                    stop.stop();
                }
            }
        });
        Watcher {
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for Watcher {
    /// Ends the watch and waits for it, so the host's callback is
    /// never running once the call it belongs to has returned.
    ///
    /// Waking it rather than letting the sleep expire is why the
    /// condition variable is here: a statement that took a millisecond
    /// under a callback asking for a report every second should return
    /// in a millisecond.
    fn drop(&mut self) {
        let (lock, wake) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            // A panic in a host's callback has already unwound to the
            // watcher's own boundary; there is nothing here to add to
            // it and nothing to abort the statement for.
            let _ = thread.join();
        }
    }
}

/// Runs one statement with the connection's cancellation word cleared
/// and its progress watch running.
///
/// Cleared on the way in rather than on the way out, so an ask that
/// arrived while nothing was running cannot end the statement about to
/// run, and so the row count a watcher reports starts at zero for each
/// statement rather than accumulating over a session.
unsafe fn watched<T>(conn: *mut ZuConn, run: impl FnOnce() -> T) -> T {
    let stop = unsafe { conn_stop(conn) };
    stop.clear();
    let hook = *unsafe { conn_progress(conn) }
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _watcher = hook.map(|hook| Watcher::start(hook, stop));
    run()
}

/// Asks the statement running on this connection to stop.
///
/// The one call meant to be made from another thread while a
/// connection is in use, which is why it takes no claim: a
/// cancellation that had to wait for the connection to be free could
/// only ever arrive after the statement it was meant to stop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_conn_interrupt(conn: *mut ZuConn) -> ZuStatus {
    if conn.is_null() {
        return ZuStatus::Misuse;
    }
    let state = unsafe { conn_state(conn) };
    if !state.alive.load(Ordering::Acquire) {
        return ZuStatus::MisuseClosed;
    }
    unsafe { conn_stop(conn) }.stop();
    ZuStatus::Ok
}

/// How many rows the statement running on this connection has read out
/// of storage, for a host that would rather poll than be called back.
///
/// Rows read rather than rows answered, because the number is there to
/// show a person that something is happening, and the statement they
/// are waiting on is exactly the one that reads a hundred million rows
/// to answer one. It starts at zero at each statement and holds its
/// last value once one ends, so a host that polls after the call
/// returns reads what the statement cost rather than a zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_conn_rows_read(conn: *mut ZuConn, out: *mut u64) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    if conn.is_null() {
        return ZuStatus::Misuse;
    }
    let state = unsafe { conn_state(conn) };
    if !state.alive.load(Ordering::Acquire) {
        return ZuStatus::MisuseClosed;
    }
    unsafe { *out = conn_stop(conn).rows() };
    ZuStatus::Ok
}

/// Asks to be called back every `interval_ms` while a statement runs
/// on this connection, and to be able to stop it from there.
///
/// A `NULL` callback takes the arrangement back, which is the only way
/// to, and `interval_ms` is then ignored. An interval of zero with a
/// callback is [`ZuStatus::Misuse`]: a period of nothing is not a
/// period, and reading it as one would be a thread calling a host as
/// fast as it can.
///
/// The arrangement belongs to the connection rather than to a
/// statement, so it is made once and covers every statement after it.
/// A statement already running keeps the arrangement it started with.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_conn_set_progress(
    conn: *mut ZuConn,
    cb: ZuProgressFn,
    user: *mut c_void,
    interval_ms: u64,
) -> ZuStatus {
    if conn.is_null() {
        return ZuStatus::Misuse;
    }
    let state = unsafe { conn_state(conn) };
    if !state.alive.load(Ordering::Acquire) {
        return ZuStatus::MisuseClosed;
    }
    let hook = match cb {
        None => None,
        Some(_) if interval_ms == 0 => return ZuStatus::Misuse,
        Some(call) => Some(Hook {
            call,
            user,
            every: Duration::from_millis(interval_ms),
        }),
    };
    *unsafe { conn_progress(conn) }
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = hook;
    ZuStatus::Ok
}

/// Claims a connection for one call, or says why it cannot be had.
unsafe fn claim_conn(conn: *mut ZuConn) -> Result<Claim, ZuStatus> {
    if conn.is_null() {
        return Err(ZuStatus::Misuse);
    }
    claim(&unsafe { conn_state(conn) })
}

/// Claims a statement's connection for one call.
unsafe fn claim_stmt(stmt: *mut ZuStmt) -> Result<Claim, ZuStatus> {
    if stmt.is_null() {
        return Err(ZuStatus::Misuse);
    }
    claim(&unsafe { stmt_state(stmt) })
}

/* ---- statements ---- */

/// Runs one parameterless statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_query(
    conn: *mut ZuConn,
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
        let _claim = match unsafe { claim_conn(conn) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let q = unsafe { counted(q, q_len, "query") }?;
        let result = unsafe { watched(conn, || conn_of(conn).query(q)) }?;
        unsafe { *out = Box::into_raw(Box::new(ZuResult::new(result))) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_query_z(
    conn: *mut ZuConn,
    q: *const c_char,
    out: *mut *mut ZuResult,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_query(conn, q, zlen(q), out, err) }
}

/// Compiles a statement against the connection's plan cache. The
/// handle carries its own bindings; bind then execute, as many times
/// as wanted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_prepare(
    conn: *mut ZuConn,
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
        let _claim = match unsafe { claim_conn(conn) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let q = unsafe { counted(q, q_len, "query") }?;
        let (id, _) = unsafe { conn_of(conn) }.prepare(q)?;
        let state = unsafe { conn_state(conn) };
        unsafe {
            *out = Box::into_raw(Box::new(ZuStmt {
                conn,
                state,
                id,
                binds: Vec::new(),
            }))
        };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_prepare_z(
    conn: *mut ZuConn,
    q: *const c_char,
    out: *mut *mut ZuStmt,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_prepare(conn, q, zlen(q), out, err) }
}

/// Frees a statement and drops its slot in the connection's plan
/// cache. Safe after the connection is closed: the plan went with it,
/// so there is nothing left to release and the handle is only freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_stmt_close(stmt: *mut ZuStmt) {
    if stmt.is_null() {
        return;
    }
    let stmt = unsafe { Box::from_raw(stmt) };
    if let Ok(_claim) = claim(&stmt.state) {
        unsafe { conn_of(stmt.conn) }.close_prepared(stmt.id);
    }
}

/* ---- transactions ---- */

/// Runs one of the three words that bound a transaction.
///
/// The text is what a host would have written, and running it is what
/// keeps these three calls and the statements they stand for one
/// implementation: the same parser, the same session state and the
/// same conditions, so a host that begins with [`zu_begin`] and a host
/// that sends `START TRANSACTION` are told the same thing when they
/// begin twice.
///
/// Not watched, unlike a query. A boundary reads no rows, so a
/// progress hook counting rows read has nothing to say about one, and
/// a thread per commit would cost more than the commit on a small
/// transaction. The interrupt word is still checked by the work a
/// commit does, so a commit of a large transaction stops like anything
/// else.
unsafe fn boundary(conn: *mut ZuConn, word: &str, err: *mut *mut ZuError) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_conn(conn) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        unsafe { conn_of(conn) }.execute(word)?;
        Ok(ZuStatus::Ok)
    })
}

/// Begins a transaction, read-write unless `read_only` is nonzero.
///
/// Every statement outside one is already a transaction of its own, so
/// this does not turn transactions on. What it does is make several
/// statements one: what they wrote is kept by [`zu_commit`] or unmade
/// by [`zu_rollback`], and nothing between the two is visible to
/// another connection.
///
/// Beginning inside a transaction is `25G01` on the error handle
/// rather than a nested transaction, because a nesting this engine
/// does not have would otherwise be a commit that silently committed
/// its parent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_begin(
    conn: *mut ZuConn,
    read_only: i32,
    err: *mut *mut ZuError,
) -> ZuStatus {
    let word = if read_only != 0 {
        "START TRANSACTION READ ONLY"
    } else {
        "START TRANSACTION"
    };
    unsafe { boundary(conn, word, err) }
}

/// Keeps everything the transaction wrote, and ends it.
///
/// A commit that returns [`ZuStatus::Ok`] is durable: the log frame is
/// on the disk before this call returns, so a process that dies
/// afterwards reopens the file with the work in it. A commit with no
/// transaction running is `2D000`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_commit(conn: *mut ZuConn, err: *mut *mut ZuError) -> ZuStatus {
    unsafe { boundary(conn, "COMMIT", err) }
}

/// Unmakes everything the transaction wrote, and ends it.
///
/// A rollback with no transaction running is `2D000`, the same as a
/// commit, rather than a call that quietly did nothing: a host that
/// rolls back in an error path wants to know that the transaction it
/// meant to undo was not the one it thought.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_rollback(conn: *mut ZuConn, err: *mut *mut ZuError) -> ZuStatus {
    unsafe { boundary(conn, "ROLLBACK", err) }
}

/// Whether a transaction is running on this connection.
///
/// This is the one thing about a transaction that no statement
/// answers, and every host that offers a `with` block, a `using`
/// block or a `defer` needs it: the cleanup path has to know whether
/// the body already ended the transaction before it tries to.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_conn_in_transaction(conn: *mut ZuConn, out: *mut i32) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    guard_status(|| {
        let _claim = match unsafe { claim_conn(conn) } {
            Ok(claim) => claim,
            Err(status) => return status,
        };
        let running = unsafe { conn_of(conn) }.session_mut().in_transaction();
        unsafe { *out = i32::from(running) };
        ZuStatus::Ok
    })
}

/* ---- binding ---- */

unsafe fn bind(stmt: *mut ZuStmt, name: *const c_char, name_len: usize, value: Value) -> ZuStatus {
    guard_status(|| {
        // A binding touches only the statement, but taking the claim
        // is what reports a closed connection at the bind rather than
        // three calls later at the execute.
        let _claim = match unsafe { claim_stmt(stmt) } {
            Ok(claim) => claim,
            Err(status) => return status,
        };
        let Ok(name) = (unsafe { counted(name, name_len, "name") }) else {
            return ZuStatus::Misuse;
        };
        let binds = unsafe { stmt_binds(stmt) };
        // Rebinding a name replaces the old value, so a statement in a
        // loop binds the same names over and over without growing.
        binds.retain(|(n, _)| n != name);
        binds.push((name.to_string(), value));
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

/// Binds a boolean. The C side has no bool of its own that a header
/// can promise across compilers, so the value is an int and anything
/// other than nought is true, which is what C means by a condition.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_bool(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
    v: c_int,
) -> ZuStatus {
    unsafe { bind(stmt, name, name_len, Value::Bool(v != 0)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_bool_z(
    stmt: *mut ZuStmt,
    name: *const c_char,
    v: c_int,
) -> ZuStatus {
    unsafe { bind(stmt, name, zlen(name), Value::Bool(v != 0)) }
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

/// Binds a temporal, as one `ZU_TEMPORAL_*` kind and the count in the
/// unit that kind implies.
///
/// This is [`zu_value_temporal`] read backwards, deliberately, and for
/// the reason [`zu_loader_col_temporal`] is: a caller that read a date
/// out as 19782 days writes it back in as 19782 days, and a host needs
/// one mapping rather than two. Unlike the loader this takes the two
/// zoned kinds, because a parameter is a value on its way through a
/// statement and not a column in a store with nowhere to keep an
/// offset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_temporal(
    stmt: *mut ZuStmt,
    name: *const c_char,
    name_len: usize,
    kind: i32,
    count: i64,
    offset: i32,
) -> ZuStatus {
    // The narrowings are checked rather than truncated: a date outside
    // an i32 of days and an offset outside an i16 of minutes are both
    // callers with a unit confusion, and wrapping one into a value the
    // engine accepts would answer a statement about a day in another
    // century without saying so.
    let Ok(minutes) = i16::try_from(offset) else {
        return ZuStatus::Misuse;
    };
    let held = match kind {
        ZU_TEMPORAL_DATE => match i32::try_from(count) {
            Ok(days) => Temporal::Date(days),
            Err(_) => return ZuStatus::Misuse,
        },
        ZU_TEMPORAL_LOCAL_TIME => Temporal::LocalTime(count),
        ZU_TEMPORAL_ZONED_TIME => Temporal::ZonedTime {
            nanos: count,
            offset: minutes,
        },
        ZU_TEMPORAL_LOCAL_DATETIME => Temporal::LocalDatetime(count),
        ZU_TEMPORAL_ZONED_DATETIME => Temporal::ZonedDatetime {
            nanos: count,
            offset: minutes,
        },
        ZU_TEMPORAL_DURATION_YEAR_MONTH => Temporal::Duration(DurationKind::YearMonth, count),
        ZU_TEMPORAL_DURATION_DAY_TIME => Temporal::Duration(DurationKind::DayTime, count),
        _ => return ZuStatus::Misuse,
    };
    unsafe { bind(stmt, name, name_len, Value::Temporal(held)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_bind_temporal_z(
    stmt: *mut ZuStmt,
    name: *const c_char,
    kind: i32,
    count: i64,
    offset: i32,
) -> ZuStatus {
    unsafe { zu_bind_temporal(stmt, name, zlen(name), kind, count, offset) }
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
        let _claim = match unsafe { claim_stmt(stmt) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let borrowed: Vec<(&str, Value)> = unsafe { stmt_binds(stmt) }
            .iter()
            .map(|(n, v)| (n.as_str(), v.clone()))
            .collect();
        let (conn, id) = unsafe { ((*stmt).conn, (*stmt).id) };
        let result = unsafe { watched(conn, || conn_of(conn).execute_prepared(id, &borrowed)) }?;
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
        Some(Value::Graph(_)) => ZU_TYPE_GRAPH,
        Some(Value::BindingTable(_)) => ZU_TYPE_BINDING_TABLE,
        None => return ZuStatus::Misuse,
    };
    unsafe { *out = tag };
    ZuStatus::Ok
}

/// What a cell reads as through one accessor, or `None` when the
/// column holds something that accessor does not read.
///
/// One function per accessor, shared by the whole-column path and the
/// chunked one, so the two can never come to disagree about what a
/// column of bools or of nulls means. Disagreeing would be worse than
/// a plain bug: a host that read a column both ways would get two
/// answers and no reason to suspect either.
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Null => Some(0),
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        Value::Null => Some(0.0),
        _ => None,
    }
}

fn as_node_offset(v: &Value) -> Option<u64> {
    match v {
        Value::Node { offset, .. } => Some(*offset),
        Value::Null => Some(0),
        _ => None,
    }
}

/// Reads every column, because every column has nulls or does not.
fn as_valid(v: &Value) -> Option<u8> {
    Some(u8::from(!matches!(v, Value::Null)))
}

/// Appends one column of `rows` to `buf`, or answers false and leaves
/// whatever it managed on the way to finding out, which the caller
/// discards.
fn fill<T>(
    rows: &[Vec<Value>],
    col: usize,
    buf: &mut Vec<T>,
    read: fn(&Value) -> Option<T>,
) -> bool {
    buf.reserve(rows.len());
    for row in rows {
        match read(&row[col]) {
            Some(v) => buf.push(v),
            None => return false,
        }
    }
    true
}

/// The body every whole-column accessor shares: check the handle and
/// the index, answer `Done` for a result with no rows, and otherwise
/// build the column once and hand back a pointer into it. `read`
/// answering `None` for any cell means the column holds something this
/// accessor does not read, which is the caller asking the wrong
/// question and so misuse rather than an engine failure.
unsafe fn column<T>(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const T,
    slot: impl Fn(&mut ZuResult) -> &mut Option<Vec<T>>,
    read: fn(&Value) -> Option<T>,
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
            let mut buf = Vec::new();
            if !fill(&r.result.rows, c, &mut buf, read) {
                return ZuStatus::Misuse;
            }
            *slot(r) = Some(buf);
        }
        unsafe { *out = slot(r).as_ref().expect("filled").as_ptr() };
        ZuStatus::Ok
    })
}

/// The half-open row range a chunk covers, or `None` when the result
/// has no such chunk. A result with no rows has no chunks at all, so
/// every index is out of range and the loop a caller writes runs zero
/// times without needing a status to say so.
fn chunk_span(total: usize, chunk: u64) -> Option<(usize, usize)> {
    let lo = usize::try_from(chunk).ok()?.checked_mul(CHUNK_ROWS)?;
    if lo >= total {
        return None;
    }
    Some((lo, (lo + CHUNK_ROWS).min(total)))
}

/// The body every chunked accessor shares. Same checks as [`column`],
/// and then one buffer per column reused across chunks: the chunk the
/// buffer already holds costs nothing, and any other chunk replaces it.
unsafe fn chunk_column<T>(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    out: *mut *const T,
    slot: impl Fn(&mut ZuResult) -> &mut Chunk<T>,
    read: fn(&Value) -> Option<T>,
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
        let Some((lo, hi)) = chunk_span(r.result.rows.len(), chunk) else {
            return ZuStatus::Misuse;
        };
        if slot(r).at != Some(chunk) {
            // The buffer comes out so the rows can be borrowed while it
            // is filled, and goes back either way: a column this
            // accessor cannot read still leaves its allocation behind
            // for the next chunk that it can.
            let mut buf = std::mem::take(&mut slot(r).buf);
            buf.clear();
            let ok = fill(&r.result.rows[lo..hi], c, &mut buf, read);
            let held = slot(r);
            held.buf = buf;
            held.at = ok.then_some(chunk);
            if !ok {
                return ZuStatus::Misuse;
            }
        }
        unsafe { *out = slot(r).buf.as_ptr() };
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
    unsafe { column(result, col, out, |r| &mut r.i64_cols[col as usize], as_i64) }
}

/// The whole column as contiguous doubles; ints widen and nulls read
/// 0. `Misuse` when the column holds anything but numbers and nulls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_col_f64(
    result: *mut ZuResult,
    col: u32,
    out: *mut *const f64,
) -> ZuStatus {
    unsafe { column(result, col, out, |r| &mut r.f64_cols[col as usize], as_f64) }
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
            as_node_offset,
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
            as_valid,
        )
    }
}

/* ---- chunked results ---- */

/// How many chunks the result reads out in, 0 when it has no rows.
///
/// This is the loop bound a host writes, and the reason the chunked
/// path needs no equivalent of [`ZuStatus::Done`]: a result with
/// nothing in it has no chunks, so the loop runs zero times on its own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk_count(result: *const ZuResult) -> u64 {
    if result.is_null() {
        return 0;
    }
    unsafe { &*result }.result.rows.len().div_ceil(CHUNK_ROWS) as u64
}

/// Where one chunk starts and how many rows it holds, either through a
/// non-NULL out-parameter. `Misuse` when there is no such chunk.
///
/// A caller has to ask rather than multiply, because chunks are not
/// promised to be the same size as each other. They are today, save the
/// last, and they will stop being once a result is what the executor
/// produced instead of a slice of what it already materialized.
///
/// The offset is what turns a chunk row back into the row number
/// [`zu_result_cell_str`] and [`zu_result_cell_type`] take, which is
/// how a host reads a string column alongside a chunked numeric one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk(
    result: *const ZuResult,
    chunk: u64,
    offset: *mut u64,
    rows: *mut u64,
) -> ZuStatus {
    if !offset.is_null() {
        unsafe { *offset = 0 };
    }
    if !rows.is_null() {
        unsafe { *rows = 0 };
    }
    if result.is_null() {
        return ZuStatus::Misuse;
    }
    let total = unsafe { &*result }.result.rows.len();
    let Some((lo, hi)) = chunk_span(total, chunk) else {
        return ZuStatus::Misuse;
    };
    if !offset.is_null() {
        unsafe { *offset = lo as u64 };
    }
    if !rows.is_null() {
        unsafe { *rows = (hi - lo) as u64 };
    }
    ZuStatus::Ok
}

/// One chunk of a column as contiguous i64s, reading what
/// [`zu_result_col_i64`] reads.
///
/// The pointer is valid until the next call for this column and this
/// accessor, which replaces the contents, or until the result is freed.
/// That is the trade the chunked path makes: the whole-column
/// accessors keep every conversion alive and so can promise a pointer
/// that never changes, and these keep one chunk and so cannot. A host
/// that needs a chunk to outlive the next one copies it, which is the
/// copy it was going to make anyway on its way into a host array.
///
/// Chunks of one column are independent of chunks of another, so
/// reading values and validity for the same chunk together, which is
/// the usual thing to want, costs no reconversion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk_col_i64(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    out: *mut *const i64,
) -> ZuStatus {
    unsafe {
        chunk_column(
            result,
            chunk,
            col,
            out,
            |r| &mut r.chunk_i64[col as usize],
            as_i64,
        )
    }
}

/// One chunk of a column as contiguous doubles, reading what
/// [`zu_result_col_f64`] reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk_col_f64(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    out: *mut *const f64,
) -> ZuStatus {
    unsafe {
        chunk_column(
            result,
            chunk,
            col,
            out,
            |r| &mut r.chunk_f64[col as usize],
            as_f64,
        )
    }
}

/// One chunk of a column as node row offsets, reading what
/// [`zu_result_col_node_offset`] reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk_col_node_offset(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    out: *mut *const u64,
) -> ZuStatus {
    unsafe {
        chunk_column(
            result,
            chunk,
            col,
            out,
            |r| &mut r.chunk_node[col as usize],
            as_node_offset,
        )
    }
}

/// One chunk of a column's validity, one byte per row, reading what
/// [`zu_result_col_valid`] reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_chunk_col_valid(
    result: *mut ZuResult,
    chunk: u64,
    col: u32,
    out: *mut *const u8,
) -> ZuStatus {
    unsafe {
        chunk_column(
            result,
            chunk,
            col,
            out,
            |r| &mut r.chunk_valid[col as usize],
            as_valid,
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

/* ---- cells ---- */

/// One cell of a result, borrowed from the result that holds it.
///
/// Opaque to C and read through the `zu_value_*` accessors. It is not
/// a handle and there is nothing to free: the pointer is into the
/// result's own rows, so handing one out allocates nothing and it
/// stays valid for exactly as long as the result does.
///
/// The columnar accessors above are the fast path and stay the fast
/// path: a host reading a million integers wants one buffer, not a
/// million calls. This is the other question, and before it a C caller
/// could not ask it at all. A temporal, a list, a node's table, and
/// anything nested have no column of host scalars to be read into,
/// which left them readable from Rust and not from C, and a value the
/// ABI cannot express is a value nine bindings cannot return.
///
/// The accessors here read a value as the type it is and nothing else.
/// That is the difference between them and the columns: `col_i64`
/// reads bools and nulls too, because a column is one host array and
/// something has to go in every slot, while `zu_value_i64` on a bool
/// is the caller asking the wrong question and answers `Misuse`.
#[repr(transparent)]
pub struct ZuValue(Value);

/// A borrowed cell, or `None` for a null pointer.
unsafe fn value_of<'a>(v: *const ZuValue) -> Option<&'a Value> {
    match v.is_null() {
        true => None,
        false => Some(unsafe { &(*v).0 }),
    }
}

/// The values inside a composite, in order.
///
/// A record's fields are here as well as on [`zu_value_field`],
/// because a caller that wants the values and not the names should not
/// have to know which composite it is holding.
fn parts(v: &Value) -> Option<&[Value]> {
    match v {
        // A chain never leaves the pipeline (`settle` turns it into
        // the edge list first), so there is no walk of one to write.
        Value::List(items) | Value::Path(items) => Some(items),
        _ => None,
    }
}

fn fields(v: &Value) -> Option<&[(String, Value)]> {
    match v {
        Value::Record(f) => Some(f),
        _ => None,
    }
}

/// A pointer to one cell of a result, valid until [`zu_result_free`].
///
/// `Misuse` when the row or the column is out of range, which is the
/// only way this fails: every cell holds a value, and a null cell is a
/// value too.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_cell(
    result: *const ZuResult,
    row: u64,
    col: u32,
    out: *mut *const ZuValue,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null() };
    if result.is_null() {
        return ZuStatus::Misuse;
    }
    match cell(unsafe { &*result }, row, col) {
        Some(v) => {
            unsafe { *out = std::ptr::from_ref(v).cast::<ZuValue>() };
            ZuStatus::Ok
        }
        None => ZuStatus::Misuse,
    }
}

/// The `ZU_TYPE_*` tag of a cell, or -1 for a `NULL` pointer.
///
/// Returned rather than written through an out-parameter because there
/// is nothing else this can say: a cell that exists has a type. -1 is
/// not one of the tags, so a caller that passed `NULL` cannot read the
/// answer as a type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_type(v: *const ZuValue) -> i32 {
    match unsafe { value_of(v) } {
        None => -1,
        Some(Value::Null) => ZU_TYPE_NULL,
        Some(Value::Bool(_)) => ZU_TYPE_BOOL,
        Some(Value::Int(_)) => ZU_TYPE_INT,
        Some(Value::Float(_)) => ZU_TYPE_FLOAT,
        Some(Value::Str(_)) => ZU_TYPE_STR,
        Some(Value::Node { .. }) => ZU_TYPE_NODE,
        Some(Value::Rel { .. }) => ZU_TYPE_REL,
        Some(Value::List(_)) => ZU_TYPE_LIST,
        Some(Value::Path(_) | Value::Chain(_)) => ZU_TYPE_PATH,
        Some(Value::Temporal(_)) => ZU_TYPE_TEMPORAL,
        Some(Value::Record(_)) => ZU_TYPE_RECORD,
        Some(Value::Graph(_)) => ZU_TYPE_GRAPH,
        Some(Value::BindingTable(_)) => ZU_TYPE_BINDING_TABLE,
    }
}

/// A bool cell as 0 or 1. `Misuse` for anything else, a null included.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_bool(v: *const ZuValue, out: *mut i32) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    match unsafe { value_of(v) } {
        Some(Value::Bool(b)) => {
            unsafe { *out = i32::from(*b) };
            ZuStatus::Ok
        }
        _ => ZuStatus::Misuse,
    }
}

/// An integer cell. `Misuse` for anything else, a bool included: a
/// caller who wanted the widening asks the column for it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_i64(v: *const ZuValue, out: *mut i64) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    match unsafe { value_of(v) } {
        Some(Value::Int(i)) => {
            unsafe { *out = *i };
            ZuStatus::Ok
        }
        _ => ZuStatus::Misuse,
    }
}

/// A float cell, bits intact. An integer is not one here, so a host
/// that reads a float column and finds an integer learns it from the
/// status rather than from a value that has already been converted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_f64(v: *const ZuValue, out: *mut f64) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0.0 };
    match unsafe { value_of(v) } {
        Some(Value::Float(f)) => {
            unsafe { *out = *f };
            ZuStatus::Ok
        }
        _ => ZuStatus::Misuse,
    }
}

/// A string cell as a pointer and a byte length, pointing into the
/// result's own bytes and **not** NUL-terminated.
///
/// That is the one place this differs from [`zu_result_cell_str`], and
/// it is the difference between borrowing and copying. A string inside
/// a list has no row and column to be cached under, and a copy per
/// element is a cost every caller pays so that the ones using `printf`
/// need not think. A caller who genuinely wants a C string of a
/// top-level cell asks [`zu_result_cell_str`], which keeps the copy.
///
/// `len` may not be `NULL`: without it the answer is unusable, unlike
/// the NUL-terminated form where it is a convenience.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_str(
    v: *const ZuValue,
    out: *mut *const c_char,
    len: *mut usize,
) -> ZuStatus {
    if out.is_null() || len.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe {
        *out = std::ptr::null();
        *len = 0;
    }
    match unsafe { value_of(v) } {
        Some(Value::Str(s)) => {
            unsafe {
                *out = s.as_ptr().cast::<c_char>();
                *len = s.len();
            }
            ZuStatus::Ok
        }
        _ => ZuStatus::Misuse,
    }
}

/// A temporal cell as its kind, its count, and its offset from UTC.
///
/// The unit follows the kind: days for `ZU_TEMPORAL_DATE`, months for
/// `ZU_TEMPORAL_DURATION_YEAR_MONTH`, nanoseconds for the rest. A date
/// counts from 1970-01-01 and a datetime from midnight of it; a time
/// counts from midnight. The offset is minutes east of UTC and is 0
/// for the five kinds that carry none, so a caller that ignores it
/// reads the two zoned kinds as their instant, which is what they are
/// stored as.
///
/// `kind` and `count` may not be `NULL`, since a count with no unit
/// says nothing. `offset` may be, for a host that has no zoned type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_temporal(
    v: *const ZuValue,
    kind: *mut i32,
    count: *mut i64,
    offset: *mut i32,
) -> ZuStatus {
    if kind.is_null() || count.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe {
        *kind = -1;
        *count = 0;
    }
    if !offset.is_null() {
        unsafe { *offset = 0 };
    }
    let Some(Value::Temporal(t)) = (unsafe { value_of(v) }) else {
        return ZuStatus::Misuse;
    };
    let (k, n, z) = match *t {
        Temporal::Date(days) => (ZU_TEMPORAL_DATE, i64::from(days), 0),
        Temporal::LocalTime(nanos) => (ZU_TEMPORAL_LOCAL_TIME, nanos, 0),
        Temporal::ZonedTime { nanos, offset } => (ZU_TEMPORAL_ZONED_TIME, nanos, i32::from(offset)),
        Temporal::LocalDatetime(nanos) => (ZU_TEMPORAL_LOCAL_DATETIME, nanos, 0),
        Temporal::ZonedDatetime { nanos, offset } => {
            (ZU_TEMPORAL_ZONED_DATETIME, nanos, i32::from(offset))
        }
        Temporal::Duration(DurationKind::YearMonth, months) => {
            (ZU_TEMPORAL_DURATION_YEAR_MONTH, months, 0)
        }
        Temporal::Duration(DurationKind::DayTime, nanos) => {
            (ZU_TEMPORAL_DURATION_DAY_TIME, nanos, 0)
        }
    };
    unsafe {
        *kind = k;
        *count = n;
    }
    if !offset.is_null() {
        unsafe { *offset = z };
    }
    ZuStatus::Ok
}

/// A node cell as the table it belongs to and its row offset in that
/// table.
///
/// Both, because neither identifies a node on its own: two tables
/// number their rows from zero, and `zu_result_col_node_offset` drops
/// the table because a column of two tables has no one answer to put
/// in it. A binding building a node identity wants the pair.
///
/// Either out-parameter may be `NULL` for a caller that wants the
/// other.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_node(
    v: *const ZuValue,
    table: *mut u32,
    offset: *mut u64,
) -> ZuStatus {
    if !table.is_null() {
        unsafe { *table = 0 };
    }
    if !offset.is_null() {
        unsafe { *offset = 0 };
    }
    let Some(Value::Node {
        table: t,
        offset: o,
    }) = (unsafe { value_of(v) })
    else {
        return ZuStatus::Misuse;
    };
    if !table.is_null() {
        unsafe { *table = *t };
    }
    if !offset.is_null() {
        unsafe { *offset = *o };
    }
    ZuStatus::Ok
}

/// A rel cell as its table and the two row offsets it joins.
///
/// The ends are node offsets in the tables the rel table was declared
/// between, which the catalog names; a rel carries no offset of its
/// own, because a rel is the pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_rel(
    v: *const ZuValue,
    table: *mut u32,
    src: *mut u64,
    dst: *mut u64,
) -> ZuStatus {
    for p in [src, dst] {
        if !p.is_null() {
            unsafe { *p = 0 };
        }
    }
    if !table.is_null() {
        unsafe { *table = 0 };
    }
    let Some(Value::Rel {
        table: t,
        src: s,
        dst: d,
        ..
    }) = (unsafe { value_of(v) })
    else {
        return ZuStatus::Misuse;
    };
    if !table.is_null() {
        unsafe { *table = *t };
    }
    if !src.is_null() {
        unsafe { *src = *s };
    }
    if !dst.is_null() {
        unsafe { *dst = *d };
    }
    ZuStatus::Ok
}

/// How many values a composite holds: the elements of a list, the
/// nodes and edges of a path, the fields of a record.
///
/// 0 for everything else, an empty list and a `NULL` pointer included,
/// which is the same answer [`zu_result_rows`] gives and needs no
/// status for the same reason: there is nothing to read either way,
/// and the type tag is what tells a caller which of the two it has.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_len(v: *const ZuValue) -> u64 {
    let Some(v) = (unsafe { value_of(v) }) else {
        return 0;
    };
    match (parts(v), fields(v)) {
        (Some(items), _) => items.len() as u64,
        (_, Some(f)) => f.len() as u64,
        _ => 0,
    }
}

/// One value inside a composite, borrowed from the same result and
/// read with the same accessors, so a list of lists is a walk and not
/// a special case.
///
/// `Misuse` when the value is not a composite or `i` is past its end,
/// which are the caller's two mistakes and not the engine's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_at(
    v: *const ZuValue,
    i: u64,
    out: *mut *const ZuValue,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null() };
    let Some(v) = (unsafe { value_of(v) }) else {
        return ZuStatus::Misuse;
    };
    let at = match (parts(v), fields(v)) {
        (Some(items), _) => items.get(i as usize),
        (_, Some(f)) => f.get(i as usize).map(|(_, v)| v),
        _ => None,
    };
    match at {
        Some(v) => {
            unsafe { *out = std::ptr::from_ref(v).cast::<ZuValue>() };
            ZuStatus::Ok
        }
        None => ZuStatus::Misuse,
    }
}

/// The name of one field of a record, as a pointer and a byte length,
/// not NUL-terminated, on the same terms as [`zu_value_str`].
///
/// Fields are in name order and a name appears once, which is what
/// makes two records written in different orders one value. A caller
/// looking a field up by name therefore gets a binary search rather
/// than a scan, and a caller comparing two records walks them in step.
///
/// `Misuse` when the value is not a record or `i` is past its last
/// field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_value_field(
    v: *const ZuValue,
    i: u64,
    out: *mut *const c_char,
    len: *mut usize,
) -> ZuStatus {
    if out.is_null() || len.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe {
        *out = std::ptr::null();
        *len = 0;
    }
    let Some(name) = (unsafe { value_of(v) })
        .and_then(fields)
        .and_then(|f| f.get(i as usize))
        .map(|(name, _)| name)
    else {
        return ZuStatus::Misuse;
    };
    unsafe {
        *out = name.as_ptr().cast::<c_char>();
        *len = name.len();
    }
    ZuStatus::Ok
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_free(result: *mut ZuResult) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

/* ---- diagnostics ---- */

/// The completion condition of the statement that produced this
/// result.
///
/// `00000` for a statement that answered with columns, and `00001`,
/// successful completion with the result omitted, for one that had
/// none to give back. The status a call returns says whether it
/// worked; this says which way it worked, in the standard's own terms,
/// which is what a conformance harness grades and what the JSON Lines
/// protocol already puts in every record it writes. It is the one half
/// of the GQLSTATUS envelope a host had no way to read here.
///
/// Never NULL for a non-NULL result, so a host can compare it without
/// testing for one first, and owned by the result rather than by the
/// caller, so it is good until [`zu_result_free`] and is not freed on
/// its own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_gqlstatus(
    result: *const ZuResult,
    len: *mut usize,
) -> *const c_char {
    if result.is_null() {
        if !len.is_null() {
            unsafe { *len = 0 };
        }
        return std::ptr::null();
    }
    let code = &unsafe { &*result }.gqlstatus;
    if !len.is_null() {
        unsafe { *len = code.as_bytes().len() };
    }
    code.as_ptr()
}

/// How many conditions the statement raised and carried on through.
///
/// The other half of the envelope. An exception replaces a result and
/// arrives as an error; a warning rides along with one, because a
/// statement that dropped a null out of an aggregate still has rows to
/// give you and the standard still wants you told. Almost every
/// statement raises none, so a host that reads this and finds nought
/// has paid for one call and no allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_notices(result: *const ZuResult) -> u32 {
    if result.is_null() {
        return 0;
    }
    unsafe { &*result }.result.notices.len() as u32
}

/// One of those conditions, as the handle a failure comes back on and
/// freed the same way with [`zu_error_free`].
///
/// A copy rather than a borrow, so that the rule for every
/// `zu_error *` a host is ever handed stays the one rule: free it. The
/// result keeps its own and can be asked again. What tells a notice
/// from a failure is [`zu_error_severity`], which is a warning here and
/// an exception there, and [`zu_error_status`], which is
/// [`ZuStatus::Ok`] because that is what the call that produced it
/// returned.
///
/// `ZU_DONE` for an index past the end, which is the answer a host
/// walking them gets at the end of the walk rather than a failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_result_notice(
    result: *const ZuResult,
    ix: u32,
    out: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = std::ptr::null_mut() };
    if result.is_null() {
        return ZuStatus::Misuse;
    }
    guard_status(|| {
        let Some(record) = unsafe { &*result }.result.notices.get(ix as usize) else {
            return ZuStatus::Done;
        };
        unsafe { *out = Box::into_raw(Box::new(ZuError::from_notice(record))) };
        ZuStatus::Ok
    })
}

/* ---- bulk load ---- */

/// A database being built. Opaque to C, freed with
/// [`zu_loader_free`], and not thread-safe on the same terms as
/// [`ZuConn`]: it holds the columns it was given, so two threads adding
/// to one loader would be two threads writing one vector.
///
/// Nothing reaches the file until [`zu_loader_finish`], because the
/// property store takes every column of a table in one call and a
/// loader that wrote each one as it arrived would be writing a
/// different table each time. What that costs is the columns held in
/// memory; what it buys is that a load either happened or did not.
pub struct ZuLoader {
    db: Zu1File,
    table: Option<Table>,
    state: Arc<ConnState>,
}

/// What a loader has been told so far.
struct Table {
    nodes: String,
    edges: String,
    rows: u64,
    pairs: Vec<(u32, u32)>,
    columns: Vec<(String, LoadColumn)>,
}

/// A column reduced to the vector the property store keeps it in.
///
/// Owned rather than borrowed from the caller's arrays. A borrow would
/// mean a rule no header can state safely, that every array a host
/// passed stays alive and unmoved until a call it makes later, and a
/// host that broke it would get a corrupt database rather than an
/// error. The copy is a memcpy per column against a load that writes
/// every one of those bytes to disk, and for an int column it is not
/// even extra work: the store keeps ints as `u64` and the conversion
/// has to walk them anyway.
enum LoadColumn {
    Int(Vec<u64>),
    Float(Vec<f64>),
    Bool(Vec<bool>),
    Str(Vec<Vec<u8>>),
    Date(Vec<i32>),
    LocalTime(Vec<i64>),
    LocalDatetime(Vec<i64>),
    Duration(DurationKind, Vec<i64>),
}

/// A counted array from C, on the same terms as [`counted`]: a null
/// pointer with a zero length is the empty array rather than a mistake.
unsafe fn array<'a, T>(p: *const T, len: u64, what: &str) -> Result<&'a [T], EngineError> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(misuse(format!("{what} is NULL with a non-zero length")));
    }
    let len = usize::try_from(len)
        .map_err(|_| misuse(format!("{what} is longer than this machine can address")))?;
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Claims a loader for one call. A loader that has finished answers
/// [`ZuStatus::MisuseClosed`], which is the same answer a statement
/// gives after its connection closed and means the same thing: the
/// handle is still safe to free and nothing else.
unsafe fn claim_loader(l: *mut ZuLoader) -> Result<Claim, ZuStatus> {
    if l.is_null() {
        return Err(ZuStatus::Misuse);
    }
    claim(&Arc::clone(unsafe { &(*l).state }))
}

/// The load being built, under a [`Claim`], and projected off the raw
/// pointer for the reason [`conn_state`] is.
unsafe fn loader_table<'a>(l: *mut ZuLoader) -> &'a mut Option<Table> {
    unsafe { &mut (*l).table }
}

/// The file being built, under a [`Claim`] as above.
unsafe fn loader_db<'a>(l: *mut ZuLoader) -> &'a mut Zu1File {
    unsafe { &mut (*l).db }
}

/// Takes one column, once the call that produced it has turned the
/// caller's array into the vector the store wants.
///
/// The width check is here rather than at `finish` so that a column
/// with a value missing is refused at the call that passed it, where
/// the caller still knows which column it was building.
fn add(
    table: &mut Option<Table>,
    name: &str,
    len: usize,
    column: LoadColumn,
) -> Result<ZuStatus, EngineError> {
    let Some(table) = table.as_mut() else {
        return Err(misuse(
            "the loader has no table yet, so a column has nothing to be in",
        ));
    };
    if name.is_empty() {
        return Err(misuse("a column has a name"));
    }
    if len as u64 != table.rows {
        return Err(misuse(format!(
            "column {name:?} has {len} values against a table of {} rows",
            table.rows
        )));
    }
    if table.columns.iter().any(|(had, _)| had == name) {
        return Err(misuse(format!("column {name:?} was given twice")));
    }
    table.columns.push((name.to_string(), column));
    Ok(ZuStatus::Ok)
}

/// Starts a load into a new database at `path`.
///
/// Fails if the path exists, which is what `zu copy` does and for the
/// same reason: a bulk load builds a database rather than adding to
/// one, so a path that is already a database is a caller who meant a
/// different path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_create(
    path: *const c_char,
    path_len: usize,
    out: *mut *mut ZuLoader,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let path = unsafe { counted(path, path_len, "path") }?;
        let db = Zu1File::create(Path::new(path))?;
        let loader = ZuLoader {
            db,
            table: None,
            state: Arc::new(ConnState {
                alive: AtomicBool::new(true),
                busy: AtomicBool::new(false),
            }),
        };
        unsafe { *out = Box::into_raw(Box::new(loader)) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_create_z(
    path: *const c_char,
    out: *mut *mut ZuLoader,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_loader_create(path, zlen(path), out, err) }
}

/// Names the node table, the rel table, and how many rows the node
/// table has. Everything after this is checked against `rows`.
///
/// The row count is given rather than counted from the first column,
/// so a column with a value missing is an error and not a shorter
/// table, and so a load of nodes with no columns at all is still a
/// load. One table per loader in v0, which is what `bulk_load_keyed`
/// builds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_table(
    l: *mut ZuLoader,
    nodes: *const c_char,
    nodes_len: usize,
    edges: *const c_char,
    edges_len: usize,
    rows: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let nodes = unsafe { counted(nodes, nodes_len, "nodes") }?;
        let edges = unsafe { counted(edges, edges_len, "edges") }?;
        if nodes.is_empty() || edges.is_empty() {
            return Err(misuse("a table has a node name and an edge name"));
        }
        if rows == 0 {
            return Err(misuse(
                "a load of no rows is a load nothing can be read back from",
            ));
        }
        let table = unsafe { loader_table(l) };
        if table.is_some() {
            return Err(misuse("this loader already has a table"));
        }
        *table = Some(Table {
            nodes: nodes.to_string(),
            edges: edges.to_string(),
            rows,
            pairs: Vec::new(),
            columns: Vec::new(),
        });
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_table_z(
    l: *mut ZuLoader,
    nodes: *const c_char,
    edges: *const c_char,
    rows: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_loader_table(l, nodes, zlen(nodes), edges, zlen(edges), rows, err) }
}

/// Adds edges, as the row each one starts at and the row it ends at.
///
/// Two arrays rather than an array of pairs, because a host that has
/// its edges in columns can pass them without building a third array,
/// and a host that has them in pairs is writing a loop either way. The
/// call appends, so a host streaming edges calls it as often as it
/// likes; the loader sorts and deduplicates at [`zu_loader_finish`],
/// which is what the graph builder wants and one fewer thing for a
/// caller to get wrong.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_edges(
    l: *mut ZuLoader,
    from: *const u32,
    to: *const u32,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let from = unsafe { array(from, count, "from") }?;
        let to = unsafe { array(to, count, "to") }?;
        let Some(table) = unsafe { loader_table(l) }.as_mut() else {
            return Err(misuse(
                "the loader has no table yet, so an edge has nothing to join",
            ));
        };
        for (i, (&a, &b)) in from.iter().zip(to).enumerate() {
            if u64::from(a) >= table.rows || u64::from(b) >= table.rows {
                return Err(misuse(format!(
                    "edge {i} joins rows {a} and {b} of a table with {} rows in it",
                    table.rows
                )));
            }
        }
        table
            .pairs
            .extend(from.iter().copied().zip(to.iter().copied()));
        Ok(ZuStatus::Ok)
    })
}

/// Adds a column of integers. `values` holds one per row of the table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_i64(
    l: *mut ZuLoader,
    name: *const c_char,
    name_len: usize,
    values: *const i64,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, name_len, "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let held = values.iter().map(|v| *v as u64).collect();
        add(
            unsafe { loader_table(l) },
            name,
            values.len(),
            LoadColumn::Int(held),
        )
    })
}

/// Adds a column of floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_f64(
    l: *mut ZuLoader,
    name: *const c_char,
    name_len: usize,
    values: *const f64,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, name_len, "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let held = values.to_vec();
        add(
            unsafe { loader_table(l) },
            name,
            values.len(),
            LoadColumn::Float(held),
        )
    })
}

/// Adds a column of booleans, where any nonzero value is true.
///
/// `int32_t` rather than C99 `_Bool`, because the header is C89-safe
/// and because every other truth value on this boundary is already an
/// `int32_t`: [`zu_value_bool`] writes one out and this reads one in.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_bool(
    l: *mut ZuLoader,
    name: *const c_char,
    name_len: usize,
    values: *const i32,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, name_len, "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let held = values.iter().map(|v| *v != 0).collect();
        add(
            unsafe { loader_table(l) },
            name,
            values.len(),
            LoadColumn::Bool(held),
        )
    })
}

/// Adds a column of strings, as an array of pointers and an array of
/// byte lengths.
///
/// The lengths are separate so that a caller whose strings are not
/// NUL-terminated, which is most of them once a binding is involved,
/// passes what it has. Every string is checked for UTF-8 now rather
/// than read back as something no query could return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_str(
    l: *mut ZuLoader,
    name: *const c_char,
    name_len: usize,
    values: *const *const c_char,
    lens: *const usize,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, name_len, "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let lens = unsafe { array(lens, count, "lens") }?;
        let mut held = Vec::with_capacity(values.len());
        for (row, (&p, &len)) in values.iter().zip(lens).enumerate() {
            let text = unsafe { counted(p, len, &format!("row {row}")) }?;
            held.push(text.as_bytes().to_vec());
        }
        add(
            unsafe { loader_table(l) },
            name,
            values.len(),
            LoadColumn::Str(held),
        )
    })
}

/// [`zu_loader_col_str`] for a caller who has an array of C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_str_z(
    l: *mut ZuLoader,
    name: *const c_char,
    values: *const *const c_char,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, zlen(name), "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let mut held = Vec::with_capacity(values.len());
        for (row, &p) in values.iter().enumerate() {
            let text = unsafe { counted(p, zlen(p), &format!("row {row}")) }?;
            held.push(text.as_bytes().to_vec());
        }
        add(
            unsafe { loader_table(l) },
            name,
            values.len(),
            LoadColumn::Str(held),
        )
    })
}

/// Adds a column of temporals, as one `ZU_TEMPORAL_*` kind and the
/// count each row holds in the unit that kind implies.
///
/// This is [`zu_value_temporal`] read backwards, deliberately: a
/// runner that read a date out as 19782 days writes it back in as
/// 19782 days, and a host needs one mapping rather than two. One call
/// for all of them rather than a call per kind, for the same reason
/// the reader has one tag.
///
/// `ZU_TEMPORAL_ZONED_TIME` and `ZU_TEMPORAL_ZONED_DATETIME` answer
/// [`ZuStatus::Unsupported`], because a stored column has nowhere to
/// keep the offset that makes those two what they are. A zoned value
/// still comes back out of a query, which is where they are read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_col_temporal(
    l: *mut ZuLoader,
    name: *const c_char,
    name_len: usize,
    kind: i32,
    values: *const i64,
    count: u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let name = unsafe { counted(name, name_len, "name") }?;
        let values = unsafe { array(values, count, "values") }?;
        let held =
            match kind {
                ZU_TEMPORAL_DATE => {
                    let mut days = Vec::with_capacity(values.len());
                    for (row, v) in values.iter().enumerate() {
                        days.push(i32::try_from(*v).map_err(|_| {
                            misuse(format!("row {row} is {v} days, which is no date"))
                        })?);
                    }
                    LoadColumn::Date(days)
                }
                ZU_TEMPORAL_LOCAL_TIME => LoadColumn::LocalTime(values.to_vec()),
                ZU_TEMPORAL_LOCAL_DATETIME => LoadColumn::LocalDatetime(values.to_vec()),
                ZU_TEMPORAL_DURATION_YEAR_MONTH => {
                    LoadColumn::Duration(DurationKind::YearMonth, values.to_vec())
                }
                ZU_TEMPORAL_DURATION_DAY_TIME => {
                    LoadColumn::Duration(DurationKind::DayTime, values.to_vec())
                }
                ZU_TEMPORAL_ZONED_TIME | ZU_TEMPORAL_ZONED_DATETIME => {
                    return Err(EngineError::Unsupported {
                        what: "a stored column of the zoned temporal kind",
                        id: kind as u32,
                    });
                }
                other => return Err(misuse(format!("{other} is no ZU_TEMPORAL_ kind"))),
            };
        add(unsafe { loader_table(l) }, name, values.len(), held)
    })
}

/// Writes everything the loader was given and closes it.
///
/// The database is on disk when this returns `ZU_OK`, and
/// [`zu_open`] on the same path reads it. The loader is spent either
/// way: a call after this answers [`ZuStatus::MisuseClosed`], including
/// after a failure, because a load that stopped halfway is not one to
/// add more columns to. Freeing is all that is left.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_finish(l: *mut ZuLoader, err: *mut *mut ZuError) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_loader(l) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        // Spent before the first thing that can fail, so that every way
        // out of here leaves a loader that can only be freed.
        unsafe { &(*l).state }.alive.store(false, Ordering::Release);
        let Some(table) = unsafe { loader_table(l) }.take() else {
            return Err(misuse(
                "the loader has no table, so there is nothing to write",
            ));
        };
        let db = unsafe { loader_db(l) };
        let mut pairs = table.pairs;
        pairs.sort_unstable();
        pairs.dedup();
        bulk_load_keyed(db, &table.nodes, &table.edges, table.rows, &pairs, None)?;
        if table.columns.is_empty() {
            return Ok(ZuStatus::Ok);
        }
        // The store borrows a column at a time, and wants a slice of
        // slices for a string column, which a `Vec<Vec<u8>>` is not. So
        // the row borrows are built first and handed over after, which
        // is the same two-step the corpus loader does.
        let rows: Vec<Vec<&[u8]>> = table
            .columns
            .iter()
            .map(|(_, held)| match held {
                LoadColumn::Str(v) => v.iter().map(Vec::as_slice).collect(),
                _ => Vec::new(),
            })
            .collect();
        let props: Vec<(&str, PropValues<'_>)> = table
            .columns
            .iter()
            .zip(&rows)
            .map(|((name, held), rows)| {
                let values = match held {
                    LoadColumn::Str(_) => PropValues::Str(rows),
                    LoadColumn::Int(v) => PropValues::Int(v),
                    LoadColumn::Float(v) => PropValues::Float(v),
                    LoadColumn::Bool(v) => PropValues::Bool(v),
                    LoadColumn::Date(v) => PropValues::Date(v),
                    LoadColumn::LocalTime(v) => PropValues::LocalTime(v),
                    LoadColumn::LocalDatetime(v) => PropValues::LocalDatetime(v),
                    LoadColumn::Duration(kind, v) => PropValues::Duration(*kind, v),
                };
                (name.as_str(), values)
            })
            .collect();
        store_props(db, &table.nodes, &props)?;
        Ok(ZuStatus::Ok)
    })
}

/// Frees a loader. A loader freed before [`zu_loader_finish`] wrote
/// nothing, and the file it created is left where it is, empty, for the
/// caller to remove: deleting a path this library was handed is not a
/// thing a free should do.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_loader_free(l: *mut ZuLoader) {
    if l.is_null() {
        return;
    }
    let state = Arc::clone(unsafe { &(*l).state });
    state.alive.store(false, Ordering::Release);
    // Freeing under another thread's call would free memory that thread
    // is inside of, so this leaks instead, on the same terms as
    // [`zu_conn_close`].
    if state.busy.load(Ordering::Acquire) {
        return;
    }
    drop(unsafe { Box::from_raw(l) });
}

/* ---- appending ---- */

/// Rows on their way into a table that already exists. Opaque to C,
/// finished with [`zu_appender_close`], freed with [`zu_appender_free`].
///
/// A statement is the wrong shape for loading data. Every row is
/// parsed, bound, planned and committed, and the commit is the
/// expensive part, so a million rows is a million commits and the load
/// is dominated by durability work nobody asked for. [`ZuLoader`] is
/// the right shape for a database that does not exist yet; this is the
/// right shape for one that does. Values go into per-column buffers
/// here, and a flush turns the whole buffer into one commit.
///
/// The buffers are here rather than in the engine's own appender, which
/// this opens for the length of a flush and no longer. That appender
/// borrows the file its connection reads through for as long as it
/// lives, which is a promise a C handle cannot make: a host that held
/// one open across calls and then ran a statement would be two mutable
/// borrows of one file, which is undefined behaviour rather than
/// anything a status could report. What the arrangement costs is a
/// catalog read per flush, against a commit and a fold that both cost
/// time proportional to the table, so it is not where a load spends its
/// time.
///
/// Not thread-safe, on the same terms as [`ZuConn`] and for a stronger
/// reason: it takes that connection's claim for every call, so an
/// appender used from two threads at once, or used while a statement is
/// running on the same connection, answers
/// [`ZuStatus::MisuseConcurrent`] instead of tearing a buffer. That is
/// an atomic swap per value against a push into a vector, which is what
/// buys the check on the one path where a mistake would otherwise
/// produce a database rather than an error.
pub struct ZuAppender {
    /// The connection this writes through. Followed only after
    /// [`ConnState::alive`] says it is still open, on the same terms as
    /// [`ZuStmt`].
    conn: *mut ZuConn,
    state: Arc<ConnState>,
    rows: Rows,
}

/// What an appender holds and every call mutates, in one field so that
/// reaching it is one projection off the raw pointer rather than a
/// borrow of the whole handle: see the note above [`conn_state`].
struct Rows {
    table: String,
    cols: Vec<AppendCol>,
    /// Rows ended and not yet written, which is what a flush turns into
    /// one commit.
    buffered: u64,
    /// Rows this appender has committed, across every flush.
    committed: u64,
    /// How many values of the row being written have been taken. A row
    /// is a row when [`zu_append_end_row`] says so, which is what tells
    /// a short row from a row still being written.
    partial: usize,
    open: bool,
}

/// One column of the table, and what has been buffered for it.
struct AppendCol {
    /// Kept NUL-terminated because [`zu_appender_col_name`] hands it
    /// out, and read back as a `&str` for the messages, which a name
    /// out of the catalog always is.
    name: CString,
    values: Cell,
    /// The node table whose rows this column names, for the two columns
    /// of a rel table and for nothing else: a row of one is an offset
    /// into the table the edge runs from and an offset into the table it
    /// runs to. A negative offset is no row of anything and is refused
    /// where it was appended, which is the one thing the ingest cannot
    /// say for itself: it takes the two ends as counts, so a negative
    /// one reaches it as an enormous positive one and is reported as an
    /// edge to a row that is not there. Whether the row is there at all
    /// is the ingest's own check and is left to it, since it knows about
    /// rows appended and not yet folded and a catalog read here would
    /// not.
    ends: Option<u32>,
}

/// One column's buffered values, in the shape the ingest wants them.
///
/// The arms are the storage arms and not the logical types: a date and
/// a count are both words, and which of the two a word is comes from
/// the column's declared type, read once when the appender opened.
/// Buffering in the storage shape means a flush hands the buffer over
/// with no pass to convert it.
enum Cell {
    Int(Vec<i64>),
    Float(Vec<f64>),
    Bool(Vec<bool>),
    /// Strings rather than bytes, because the store wants the bytes and
    /// the engine's appender wants the `&str`, and a `String` lends out
    /// either without a copy.
    Str(Vec<String>),
    Bytes(Vec<Vec<u8>>),
    Date(Vec<i32>),
    LocalTime(Vec<i64>),
    LocalDatetime(Vec<i64>),
    Duration(DurationKind, Vec<i64>),
}

/// What a value call carried: one value and its type, and nothing about
/// where it goes.
enum Taken<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    Bytes(&'a [u8]),
    Temporal(Temporal),
}

impl Taken<'_> {
    /// What to call this in a message about a column that would not
    /// take it.
    fn names(&self) -> &'static str {
        match self {
            Taken::Bool(_) => "a boolean",
            Taken::Int(_) => "an integer",
            Taken::Float(_) => "a float",
            Taken::Str(_) => "a string",
            Taken::Bytes(_) => "bytes",
            Taken::Temporal(Temporal::Date(_)) => "a date",
            Taken::Temporal(Temporal::LocalTime(_)) => "a local time",
            Taken::Temporal(Temporal::ZonedTime { .. }) => "a zoned time",
            Taken::Temporal(Temporal::LocalDatetime(_)) => "a local datetime",
            Taken::Temporal(Temporal::ZonedDatetime { .. }) => "a zoned datetime",
            Taken::Temporal(Temporal::Duration(DurationKind::YearMonth, _)) => {
                "a year-month duration"
            }
            Taken::Temporal(Temporal::Duration(DurationKind::DayTime, _)) => "a day-time duration",
        }
    }
}

impl Cell {
    /// The buffer a column of this declared type appends into, or
    /// `None` for a type the ingest cannot carry.
    ///
    /// The match is on the exact declared type rather than on its
    /// family, because that is what the ingest checks: it compares the
    /// stored column's type against the type its values claim, so an
    /// `INT32` column or a `VARCHAR(20)` one has no buffer here even
    /// though its bits would fit the same lane.
    fn for_type(ty: &LogicalType) -> Option<Cell> {
        Some(match ty {
            LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            } => Cell::Int(Vec::new()),
            LogicalType::Bool => Cell::Bool(Vec::new()),
            LogicalType::Float {
                bits: FloatBits::B64,
                precision: None,
            } => Cell::Float(Vec::new()),
            LogicalType::Date => Cell::Date(Vec::new()),
            LogicalType::LocalTime => Cell::LocalTime(Vec::new()),
            LogicalType::LocalDatetime => Cell::LocalDatetime(Vec::new()),
            LogicalType::Duration(kind) => Cell::Duration(*kind, Vec::new()),
            LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            } => Cell::Str(Vec::new()),
            LogicalType::Bytes {
                min: None,
                max: None,
                fixed: false,
            } => Cell::Bytes(Vec::new()),
            _ => return None,
        })
    }

    /// One value into this column, or what the column holds instead.
    fn push(&mut self, taken: Taken<'_>) -> Result<(), &'static str> {
        match (&mut *self, taken) {
            (Cell::Int(v), Taken::Int(n)) => v.push(n),
            (Cell::Float(v), Taken::Float(f)) => v.push(f),
            (Cell::Bool(v), Taken::Bool(b)) => v.push(b),
            (Cell::Str(v), Taken::Str(s)) => v.push(s.to_string()),
            (Cell::Bytes(v), Taken::Bytes(b)) => v.push(b.to_vec()),
            (Cell::Date(v), Taken::Temporal(Temporal::Date(days))) => v.push(days),
            (Cell::LocalTime(v), Taken::Temporal(Temporal::LocalTime(nanos))) => v.push(nanos),
            (Cell::LocalDatetime(v), Taken::Temporal(Temporal::LocalDatetime(nanos))) => {
                v.push(nanos);
            }
            (Cell::Duration(kind, v), Taken::Temporal(Temporal::Duration(was, count)))
                if *kind == was =>
            {
                v.push(count);
            }
            _ => return Err(self.holds()),
        }
        Ok(())
    }

    /// What this column takes, worded for the end of a sentence about a
    /// value that was something else.
    fn holds(&self) -> &'static str {
        match self {
            Cell::Int(_) => "integers",
            Cell::Float(_) => "floats",
            Cell::Bool(_) => "booleans",
            Cell::Str(_) => "strings",
            Cell::Bytes(_) => "bytes",
            Cell::Date(_) => "dates",
            Cell::LocalTime(_) => "local times",
            Cell::LocalDatetime(_) => "local datetimes",
            Cell::Duration(DurationKind::YearMonth, _) => "year-month durations",
            Cell::Duration(DurationKind::DayTime, _) => "day-time durations",
        }
    }

    /// The last value back off, for a row that was refused partway.
    fn pop(&mut self) {
        match self {
            Cell::Int(v) => drop(v.pop()),
            Cell::Float(v) => drop(v.pop()),
            Cell::Bool(v) => drop(v.pop()),
            Cell::Str(v) => drop(v.pop()),
            Cell::Bytes(v) => drop(v.pop()),
            Cell::Date(v) => drop(v.pop()),
            Cell::LocalTime(v) => drop(v.pop()),
            Cell::LocalDatetime(v) => drop(v.pop()),
            Cell::Duration(_, v) => drop(v.pop()),
        }
    }

    fn clear(&mut self) {
        match self {
            Cell::Int(v) => v.clear(),
            Cell::Float(v) => v.clear(),
            Cell::Bool(v) => v.clear(),
            Cell::Str(v) => v.clear(),
            Cell::Bytes(v) => v.clear(),
            Cell::Date(v) => v.clear(),
            Cell::LocalTime(v) => v.clear(),
            Cell::LocalDatetime(v) => v.clear(),
            Cell::Duration(_, v) => v.clear(),
        }
    }

    /// One buffered value as the engine's appender takes it, borrowed
    /// rather than copied: on a string column that is the difference
    /// between one copy on the way in and two.
    fn field(&self, row: usize) -> Field<'_> {
        match self {
            Cell::Int(v) => Field::Int(v[row]),
            Cell::Float(v) => Field::Float(v[row]),
            Cell::Bool(v) => Field::Bool(v[row]),
            Cell::Str(v) => Field::Str(&v[row]),
            Cell::Bytes(v) => Field::Bytes(&v[row]),
            Cell::Date(v) => Field::Temporal(Temporal::Date(v[row])),
            Cell::LocalTime(v) => Field::Temporal(Temporal::LocalTime(v[row])),
            Cell::LocalDatetime(v) => Field::Temporal(Temporal::LocalDatetime(v[row])),
            Cell::Duration(kind, v) => Field::Temporal(Temporal::Duration(*kind, v[row])),
        }
    }
}

impl Rows {
    /// One value into the row being written, or nothing at all.
    ///
    /// A value the column will not take ends the row it was in: the
    /// values that row had already written come back off and the next
    /// value starts a new row. A row half written and left there would
    /// make the buffers ragged, which the ingest refuses at the flush, a
    /// long way from the value that caused it.
    fn take(&mut self, taken: Taken<'_>) -> Result<(), EngineError> {
        let at = self.partial;
        let width = self.cols.len();
        if at == width {
            let why = format!(
                "this row already carries the {width} values '{}' takes: {}",
                self.table,
                self.names()
            );
            return Err(self.refuse(why));
        }
        if let (Taken::Int(offset), Some(_)) = (&taken, self.cols[at].ends)
            && *offset < 0
        {
            let why = format!(
                "value {at} of this row is {offset}, and column '{}' of '{}' holds row offsets, \
                 which count from zero",
                self.named(at),
                self.table
            );
            return Err(self.refuse(why));
        }
        let names = taken.names();
        if let Err(holds) = self.cols[at].values.push(taken) {
            let why = format!(
                "value {at} of this row is {names} and column '{}' of '{}' holds {holds}",
                self.named(at),
                self.table
            );
            return Err(self.refuse(why));
        }
        self.partial += 1;
        Ok(())
    }

    /// Ends the row being written, which is what makes it a row.
    fn end_row(&mut self) -> Result<(), EngineError> {
        let width = self.cols.len();
        if self.partial != width {
            let why = format!(
                "this row carries {} value{} and '{}' takes {width}: {}",
                self.partial,
                if self.partial == 1 { "" } else { "s" },
                self.table,
                self.names()
            );
            return Err(self.refuse(why));
        }
        self.partial = 0;
        self.buffered += 1;
        Ok(())
    }

    /// Takes back the values the row being written had managed to
    /// write, and hands over why it was refused.
    fn refuse(&mut self, why: String) -> EngineError {
        self.undo();
        misuse(why)
    }

    /// The row being written, taken back off. A row is not a row until
    /// it is ended, so this loses nothing anybody appended.
    fn undo(&mut self) {
        for col in self.cols.iter_mut().take(self.partial) {
            col.values.pop();
        }
        self.partial = 0;
    }

    /// Everything buffered, gone, which is what a flush and a discard
    /// both leave behind.
    fn empty(&mut self) {
        self.undo();
        for col in &mut self.cols {
            col.values.clear();
        }
        self.buffered = 0;
    }

    /// A column's name, for a message about a value that did not fit
    /// it. A name out of the catalog is UTF-8, so nothing is ever lost
    /// here.
    fn named(&self, at: usize) -> Cow<'_, str> {
        self.cols[at].name.to_string_lossy()
    }

    /// The columns of the table, named, for a message about a row that
    /// is the wrong width: a host that miscounted wants to see what the
    /// count was supposed to be made of.
    fn names(&self) -> String {
        self.cols
            .iter()
            .map(|col| col.name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The columns of the table an appender was opened on, in the order it
/// declares them.
///
/// A node table's columns are the ones the property store holds, with
/// the types it holds them as, which is what the engine's appender
/// checks a row against. A rel table has no property columns: a row of
/// one is the two ends of an edge, as offsets into the tables it runs
/// between, so those are the two columns and they are named for what
/// they are.
///
/// Read here rather than left to the flush so that a value that does
/// not belong in a column is refused by the call that appended it, a
/// million rows before the flush that would have carried it, and named
/// rather than guessed at from the values that came before.
fn shape(conn: &mut Connection, table: &str) -> Result<Vec<AppendCol>, EngineError> {
    let catalog = Catalog::load(conn.session_mut().file_mut()?)?;
    if let Some(rel) = catalog.rel_by_name(table) {
        let named = |id: u32, fallback: &str| {
            catalog
                .node_by_id(id)
                .map_or_else(|| fallback.to_string(), |node| node.name.clone())
        };
        // Named for the tables the edge runs between, since that is what
        // a row of a rel table is and there is nothing else to call the
        // two columns.
        return Ok(vec![
            AppendCol {
                name: c_message(&format!("from {}", named(rel.from, "the source table"))),
                values: Cell::Int(Vec::new()),
                ends: Some(rel.from),
            },
            AppendCol {
                name: c_message(&format!("to {}", named(rel.to, "the destination table"))),
                values: Cell::Int(Vec::new()),
                ends: Some(rel.to),
            },
        ]);
    }
    let id = catalog
        .node_by_name(table)
        .map(|node| node.id)
        .ok_or_else(|| misuse(format!("no node table or rel table '{table}'")))?;
    let file = conn.session_mut().file_mut()?;
    let directory = load_props(file, id)?.ok_or_else(|| {
        misuse(format!(
            "'{table}' stores no properties, so it has no columns to append to"
        ))
    })?;
    directory
        .columns
        .iter()
        .map(|column| {
            Ok(AppendCol {
                name: c_message(&column.name),
                values: Cell::for_type(&column.ty).ok_or_else(|| {
                    misuse(format!(
                        "column '{}' of '{table}' holds {}, which this engine cannot yet \
                         append to",
                        column.name, column.ty
                    ))
                })?,
                ends: None,
            })
        })
        .collect()
}

/// Writes what is buffered and answers what this appender has committed
/// in all.
///
/// One commit, whatever the buffer holds: the values are sealed into
/// the data file as segments, one frame naming them is synced to the
/// log, and the fold that follows puts them where every query looks. A
/// flush with nothing buffered touches no file, so a host can flush on
/// a timer without writing empty commits.
///
/// A row that was never ended is not a row, and comes back off here
/// rather than going in as a short one.
unsafe fn write_out(conn: *mut ZuConn, rows: &mut Rows) -> Result<u64, EngineError> {
    rows.undo();
    if rows.buffered == 0 {
        return Ok(rows.committed);
    }
    let count = rows.buffered;
    let conn = unsafe { conn_of(conn) };
    {
        let mut appender = conn.appender(&rows.table)?;
        // One vector, refilled per row rather than allocated per row,
        // which over a million rows is one allocation rather than a
        // million.
        let mut row: Vec<Field<'_>> = Vec::with_capacity(rows.cols.len());
        for at in 0..count as usize {
            row.clear();
            row.extend(rows.cols.iter().map(|col| col.values.field(at)));
            appender.append_row(&row[..]).map_err(|e| {
                // The engine reports the value and the column; which row
                // of the batch it was is the part only this side knows,
                // and it is the part that says where to look.
                misuse(format!("row {at} of this batch: {e}"))
            })?;
        }
        appender.close()?;
    }
    rows.empty();
    rows.committed += count;
    Ok(rows.committed)
}

/// The state an appender shares with the connection it writes through.
unsafe fn appender_state(app: *mut ZuAppender) -> Arc<ConnState> {
    Arc::clone(unsafe { &(*app).state })
}

/// The connection an appender writes through, as a pointer, so that
/// nothing here borrows the handle as a whole.
unsafe fn appender_conn(app: *mut ZuAppender) -> *mut ZuConn {
    unsafe { (*app).conn }
}

/// What an appender holds, under a [`Claim`] and projected off the raw
/// pointer for the reason [`conn_state`] is.
unsafe fn appender_rows<'a>(app: *mut ZuAppender) -> &'a mut Rows {
    unsafe { &mut (*app).rows }
}

/// Claims an appender's connection for one call.
///
/// An appender that has been closed answers [`ZuStatus::MisuseClosed`],
/// which is the same answer a statement gives after its connection
/// closed and means the same thing: the handle is still safe to free
/// and nothing else. The flag is read under the claim, which is what
/// makes reading it a read of a field nobody else is writing.
unsafe fn claim_appender(app: *mut ZuAppender) -> Result<Claim, ZuStatus> {
    if app.is_null() {
        return Err(ZuStatus::Misuse);
    }
    let claim = claim(&unsafe { appender_state(app) })?;
    if !unsafe { appender_rows(app) }.open {
        return Err(ZuStatus::MisuseClosed);
    }
    Ok(claim)
}

/// Opens an appender on `table`, which is a node table or a rel table
/// of the graph this connection reads.
///
/// An engine appender is opened here and dropped, purely to find out
/// whether it can be opened at all: a table nothing declares, a column
/// that holds a null, a table a keyed rel table is built over, and a
/// read-only connection are all refused here rather than at the first
/// flush. A host about to buffer a million rows wants to hear about
/// them now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_open(
    conn: *mut ZuConn,
    table: *const c_char,
    table_len: usize,
    out: *mut *mut ZuAppender,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if out.is_null() {
        return guard(err, || Err(misuse("out is NULL")));
    }
    unsafe { *out = std::ptr::null_mut() };
    guard(err, || {
        let _claim = match unsafe { claim_conn(conn) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let table = unsafe { counted(table, table_len, "table") }?;
        let engine = unsafe { conn_of(conn) };
        engine.appender(table).map(drop)?;
        let cols = shape(engine, table)?;
        let app = ZuAppender {
            conn,
            state: unsafe { conn_state(conn) },
            rows: Rows {
                table: table.to_string(),
                cols,
                buffered: 0,
                committed: 0,
                partial: 0,
                open: true,
            },
        };
        unsafe { *out = Box::into_raw(Box::new(app)) };
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_open_z(
    conn: *mut ZuConn,
    table: *const c_char,
    out: *mut *mut ZuAppender,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_appender_open(conn, table, zlen(table), out, err) }
}

/// One value into the row being written, which every `zu_append_*` call
/// below is.
///
/// The value arrives as a closure rather than as a value because two of
/// the calls have a conversion that can fail (a string that is not
/// UTF-8, a kind that is no kind), and running it inside the fence is
/// what lets the error carry the same handle as everything else and end
/// the row it was in on the same terms.
unsafe fn append<'a>(
    app: *mut ZuAppender,
    err: *mut *mut ZuError,
    taken: impl FnOnce() -> Result<Taken<'a>, EngineError>,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let rows = unsafe { appender_rows(app) };
        // A value that is not a value at all ends its row on the same
        // terms as one the column would not take, because the next call
        // is the next value of a row and a row left half written would
        // put it in the wrong column.
        let taken = match taken() {
            Ok(taken) => taken,
            Err(e) => {
                rows.undo();
                return Err(e);
            }
        };
        rows.take(taken)?;
        Ok(ZuStatus::Ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_bool(
    app: *mut ZuAppender,
    v: i32,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { append(app, err, || Ok(Taken::Bool(v != 0))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_i64(
    app: *mut ZuAppender,
    v: i64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { append(app, err, || Ok(Taken::Int(v))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_f64(
    app: *mut ZuAppender,
    v: f64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { append(app, err, || Ok(Taken::Float(v))) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_str(
    app: *mut ZuAppender,
    v: *const c_char,
    v_len: usize,
    err: *mut *mut ZuError,
) -> ZuStatus {
    let taken = || unsafe { counted(v, v_len, "value") }.map(Taken::Str);
    unsafe { append(app, err, taken) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_str_z(
    app: *mut ZuAppender,
    v: *const c_char,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { zu_append_str(app, v, zlen(v), err) }
}

/// Bytes into a `BYTES` column, which is the one value that is not a
/// string and not a number.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_bytes(
    app: *mut ZuAppender,
    v: *const u8,
    v_len: usize,
    err: *mut *mut ZuError,
) -> ZuStatus {
    let taken = || unsafe { array(v, v_len as u64, "value") }.map(Taken::Bytes);
    unsafe { append(app, err, taken) }
}

/// A temporal, as one `ZU_TEMPORAL_*` kind and the count in the unit
/// that kind implies.
///
/// This is [`zu_value_temporal`] read backwards, deliberately, and for
/// the reason [`zu_loader_col_temporal`] is: a host that read a date out
/// as 19782 days writes it back in as 19782 days, and needs one mapping
/// rather than two. `ZU_TEMPORAL_ZONED_TIME` and
/// `ZU_TEMPORAL_ZONED_DATETIME` answer [`ZuStatus::Unsupported`] for the
/// reason the loader gives: a stored column has nowhere to keep the
/// offset that makes those two what they are.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_temporal(
    app: *mut ZuAppender,
    kind: i32,
    count: i64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    unsafe { append(app, err, || temporal_of(kind, count).map(Taken::Temporal)) }
}

/// One `ZU_TEMPORAL_*` kind and a count as the value they stand for.
fn temporal_of(kind: i32, count: i64) -> Result<Temporal, EngineError> {
    Ok(match kind {
        ZU_TEMPORAL_DATE => Temporal::Date(
            i32::try_from(count)
                .map_err(|_| misuse(format!("{count} days is no date any column could hold")))?,
        ),
        ZU_TEMPORAL_LOCAL_TIME => Temporal::LocalTime(count),
        ZU_TEMPORAL_LOCAL_DATETIME => Temporal::LocalDatetime(count),
        ZU_TEMPORAL_DURATION_YEAR_MONTH => Temporal::Duration(DurationKind::YearMonth, count),
        ZU_TEMPORAL_DURATION_DAY_TIME => Temporal::Duration(DurationKind::DayTime, count),
        ZU_TEMPORAL_ZONED_TIME | ZU_TEMPORAL_ZONED_DATETIME => {
            return Err(EngineError::Unsupported {
                what: "a stored column of the zoned temporal kind",
                id: kind as u32,
            });
        }
        other => return Err(misuse(format!("{other} is no ZU_TEMPORAL_ kind"))),
    })
}

/// Ends the row being written, which is what makes it a row.
///
/// A row of the wrong width is refused here with nothing of it kept, so
/// an appender is still usable once the host has fixed its loop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_append_end_row(
    app: *mut ZuAppender,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        unsafe { appender_rows(app) }.end_row()?;
        Ok(ZuStatus::Ok)
    })
}

/// Writes every buffered row and makes it readable.
///
/// On return the buffer is empty and the rows are there: every later
/// statement on any connection sees them, and before it returns nothing
/// does. A flush that fails keeps its rows, so what did not go in is
/// still there to be looked at and tried again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_flush(
    app: *mut ZuAppender,
    err: *mut *mut ZuError,
) -> ZuStatus {
    guard(err, || {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let conn = unsafe { appender_conn(app) };
        unsafe { write_out(conn, appender_rows(app)) }?;
        Ok(ZuStatus::Ok)
    })
}

/// Rows buffered and not yet written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_buffered(app: *mut ZuAppender, out: *mut u64) -> ZuStatus {
    unsafe { count_of(app, out, |rows| rows.buffered) }
}

/// Rows this appender has committed, across every flush.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_committed(app: *mut ZuAppender, out: *mut u64) -> ZuStatus {
    unsafe { count_of(app, out, |rows| rows.committed) }
}

/// One count off an appender, written before anything can fail so that
/// a host that ignores the status is never left reading the call
/// before.
unsafe fn count_of(app: *mut ZuAppender, out: *mut u64, of: impl Fn(&Rows) -> u64) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    guard_status(|| {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return status,
        };
        unsafe { *out = of(appender_rows(app)) };
        ZuStatus::Ok
    })
}

/// How many values a row of this table carries, which is how many
/// `zu_append_*` calls stand between two `zu_append_end_row` calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_cols(app: *mut ZuAppender, out: *mut u32) -> ZuStatus {
    if out.is_null() {
        return ZuStatus::Misuse;
    }
    unsafe { *out = 0 };
    guard_status(|| {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return status,
        };
        unsafe { *out = appender_rows(app).cols.len() as u32 };
        ZuStatus::Ok
    })
}

/// The name of one column, borrowed from the appender and valid until
/// it is freed.
///
/// A row is written by position and the columns are read by name, so a
/// host that wants to check the order it is writing in, or to say which
/// column its own data does not fit, needs them both ways.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_col_name(
    app: *mut ZuAppender,
    col: u32,
    len: *mut usize,
) -> *const c_char {
    if !len.is_null() {
        unsafe { *len = 0 };
    }
    if app.is_null() {
        return std::ptr::null();
    }
    let Ok(_claim) = (unsafe { claim_appender(app) }) else {
        return std::ptr::null();
    };
    let Some(col) = unsafe { appender_rows(app) }.cols.get(col as usize) else {
        return std::ptr::null();
    };
    if !len.is_null() {
        unsafe { *len = col.name.as_bytes().len() };
    }
    col.name.as_ptr()
}

/// Throws away what is buffered and answers how many rows that was.
///
/// The way out of a load that went wrong halfway. A host that has
/// noticed the rows are wrong wants them gone, and closing would write
/// them. Rows an earlier flush committed are committed, and this does
/// not reach them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_discard(app: *mut ZuAppender, out: *mut u64) -> ZuStatus {
    if !out.is_null() {
        unsafe { *out = 0 };
    }
    guard_status(|| {
        let _claim = match unsafe { claim_appender(app) } {
            Ok(claim) => claim,
            Err(status) => return status,
        };
        let rows = unsafe { appender_rows(app) };
        let dropped = rows.buffered;
        rows.empty();
        if !out.is_null() {
            unsafe { *out = dropped };
        }
        ZuStatus::Ok
    })
}

/// Flushes what is left and spends the appender, answering how many
/// rows it committed in all.
///
/// Closing twice is not an error and writes nothing the second time,
/// because a host that closes in a cleanup path and again where the
/// load ended would otherwise fail on the way out. A close whose flush
/// failed leaves the appender open with its rows still buffered, so the
/// host can fix what was wrong and close again.
///
/// Freeing is what is left afterwards: see [`zu_appender_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_close(
    app: *mut ZuAppender,
    out: *mut u64,
    err: *mut *mut ZuError,
) -> ZuStatus {
    if !out.is_null() {
        unsafe { *out = 0 };
    }
    guard(err, || {
        if app.is_null() {
            return Ok(ZuStatus::Misuse);
        }
        let _claim = match claim(&unsafe { appender_state(app) }) {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let conn = unsafe { appender_conn(app) };
        let rows = unsafe { appender_rows(app) };
        if !rows.open {
            if !out.is_null() {
                unsafe { *out = rows.committed };
            }
            return Ok(ZuStatus::Ok);
        }
        let committed = unsafe { write_out(conn, rows) }?;
        rows.open = false;
        if !out.is_null() {
            unsafe { *out = committed };
        }
        Ok(ZuStatus::Ok)
    })
}

/// Frees an appender, writing what it still holds.
///
/// The flush is here because rows that were appended and never flushed
/// are rows the host meant to write, and a host that meant the other
/// thing calls [`zu_appender_discard`] and gets exactly it. What it
/// cannot do is say that the write failed, which is what
/// [`zu_appender_close`] is for: close first if the answer matters, and
/// this frees an appender that has nothing left to write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu_appender_free(app: *mut ZuAppender) {
    if app.is_null() {
        return;
    }
    let state = unsafe { appender_state(app) };
    if let Ok(_claim) = unsafe { claim_appender(app) } {
        let conn = unsafe { appender_conn(app) };
        let rows = unsafe { appender_rows(app) };
        rows.open = false;
        // The result goes nowhere because there is nowhere for it to go,
        // which is the whole reason close() exists. A connection that is
        // closed or in a call is not written through at all: the claim
        // above is what says which.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { write_out(conn, rows) }));
    }
    // Freeing under another thread's call would free memory that thread
    // is inside of, so this leaks instead, on the same terms as
    // [`zu_conn_close`].
    if state.busy.load(Ordering::Acquire) {
        return;
    }
    drop(unsafe { Box::from_raw(app) });
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

    /// The rest of the model dx/03 §5 fixes, as fields on the handle:
    /// the standard's words, the page, the place counted both ways,
    /// the line that place is on, and whether to try again.
    #[test]
    fn an_error_handle_carries_the_whole_error_model() {
        let source = "MATCH (n)\nRETURN n.x +";
        let offset = source.find("n.x").expect("n.x");
        let e = ZuError::from_engine(&EngineError::gql_in(
            codes::C42001,
            source,
            offset,
            "expected an expression",
        ));
        let text = |s: &Option<CString>| s.as_ref().map(|s| s.to_str().expect("utf-8").to_string());
        assert_eq!(
            text(&e.standard_text).as_deref(),
            Some("syntax error or access rule violation, invalid syntax")
        );
        assert_eq!(
            text(&e.doc_url).as_deref(),
            Some("https://zu.dev/docs/errors/42001")
        );
        assert_eq!(text(&e.excerpt).as_deref(), Some("RETURN n.x +"));
        let at = e.position.expect("a place");
        assert_eq!((at.offset, at.line, at.column), (17, 2, 8));
        assert_eq!(e.retryable, 0);

        // A lost write is the failure worth repeating, and it says so
        // without carrying a condition to say it with.
        let lost = ZuError::from_engine(&EngineError::Conflict("write lost".to_string()));
        assert_eq!(lost.retryable, 1);
        assert!(lost.standard_text.is_none() && lost.doc_url.is_none());
        assert!(lost.excerpt.is_none() && lost.position.is_none());

        // And a panic caught at the boundary is a bug here rather than
        // anything a caller can retry or look up.
        let panicked = ZuError::from_panic();
        assert_eq!(panicked.retryable, 0);
        assert!(panicked.doc_url.is_none());
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

    /// The versioned struct is what R1 allows across the boundary, and
    /// the size field is the whole of the version. A short struct is
    /// read as the prefix it is, which is the case that happens when a
    /// binding built against an older header calls a newer library.
    #[test]
    fn a_short_config_is_read_as_the_prefix_it_describes() {
        let mut cfg = ZuConfig {
            memory_limit: 4096,
            threads: 3,
            read_only: 1,
            ..ZuConfig::default()
        };

        let full = unsafe { config_of(&cfg) }.expect("whole struct");
        assert_eq!(
            full,
            Config::new().memory_limit(4096).threads(3).read_only(true)
        );

        // A caller whose header stopped before read_only.
        cfg.struct_size = offset_of!(ZuConfig, read_only);
        let older = unsafe { config_of(&cfg) }.expect("prefix");
        assert_eq!(older, Config::new().memory_limit(4096).threads(3));

        // And one whose header had only the size field, which is the
        // smallest struct this can read.
        cfg.struct_size = size_of::<usize>();
        assert_eq!(
            unsafe { config_of(&cfg) }.expect("size only"),
            Config::new()
        );

        // NULL is the defaults, and a size that cannot hold itself is
        // not a struct at all.
        assert_eq!(
            unsafe { config_of(std::ptr::null()) }.expect("defaults"),
            Config::new()
        );
        cfg.struct_size = 1;
        assert!(unsafe { config_of(&cfg) }.is_err());
    }

    /// A borrowed cell, which is what `zu_result_cell` hands out and
    /// what a nested read walks over. Taking the pointer from a
    /// reference is the whole of the conversion, and it is only sound
    /// because [`ZuValue`] is `repr(transparent)`.
    fn cell_of(v: &Value) -> *const ZuValue {
        std::ptr::from_ref(v).cast::<ZuValue>()
    }

    /// The seven temporal kinds, their units, and the offset only two
    /// of them carry. This is the mapping a binding turns into a host
    /// date, so getting a unit wrong here is a value that is wrong by
    /// a factor of 86,400,000,000,000 and still looks like a date.
    #[test]
    fn every_temporal_kind_reads_as_its_own_count_and_unit() {
        let cases = [
            (Temporal::Date(19782), ZU_TEMPORAL_DATE, 19782, 0),
            (Temporal::LocalTime(3_600), ZU_TEMPORAL_LOCAL_TIME, 3_600, 0),
            (
                Temporal::ZonedTime {
                    nanos: 7,
                    offset: 420,
                },
                ZU_TEMPORAL_ZONED_TIME,
                7,
                420,
            ),
            (
                Temporal::LocalDatetime(-9),
                ZU_TEMPORAL_LOCAL_DATETIME,
                -9,
                0,
            ),
            (
                Temporal::ZonedDatetime {
                    nanos: -9,
                    offset: -330,
                },
                ZU_TEMPORAL_ZONED_DATETIME,
                -9,
                -330,
            ),
            (
                Temporal::Duration(DurationKind::YearMonth, 14),
                ZU_TEMPORAL_DURATION_YEAR_MONTH,
                14,
                0,
            ),
            (
                Temporal::Duration(DurationKind::DayTime, i64::MIN),
                ZU_TEMPORAL_DURATION_DAY_TIME,
                i64::MIN,
                0,
            ),
        ];
        for (t, want_kind, want_count, want_offset) in cases {
            let value = Value::Temporal(t);
            let (mut kind, mut count, mut offset) = (-1, 0i64, i32::MAX);
            assert_eq!(
                unsafe { zu_value_temporal(cell_of(&value), &mut kind, &mut count, &mut offset) },
                ZuStatus::Ok,
                "{t:?}"
            );
            assert_eq!((kind, count, offset), (want_kind, want_count, want_offset));
            // The offset is the one part a host with no zoned type can
            // decline to hear about.
            assert_eq!(
                unsafe {
                    zu_value_temporal(cell_of(&value), &mut kind, &mut count, std::ptr::null_mut())
                },
                ZuStatus::Ok
            );
        }
    }

    /// A record reads as fields and values through the same two
    /// accessors a list uses, and its fields are in name order however
    /// they were written. No statement produces one yet, which is why
    /// this is here rather than beside the queries.
    #[test]
    fn a_record_reads_as_named_fields_in_name_order() {
        let value = Value::record(vec![
            ("z".to_string(), Value::Int(1)),
            ("a".to_string(), Value::Str("hi".to_string())),
        ]);
        let cell = cell_of(&value);
        assert_eq!(unsafe { zu_value_type(cell) }, ZU_TYPE_RECORD);
        assert_eq!(unsafe { zu_value_len(cell) }, 2);

        let names: Vec<String> = (0..2)
            .map(|i| {
                let (mut out, mut len) = (std::ptr::null(), 0);
                assert_eq!(
                    unsafe { zu_value_field(cell, i, &mut out, &mut len) },
                    ZuStatus::Ok
                );
                let bytes = unsafe { std::slice::from_raw_parts(out.cast::<u8>(), len) };
                String::from_utf8(bytes.to_vec()).expect("utf-8")
            })
            .collect();
        assert_eq!(names, ["a", "z"]);

        let mut first = std::ptr::null();
        assert_eq!(unsafe { zu_value_at(cell, 0, &mut first) }, ZuStatus::Ok);
        assert_eq!(unsafe { zu_value_type(first) }, ZU_TYPE_STR);

        // One past the end on either accessor, and a field name asked
        // of something that has none.
        let mut out = std::ptr::null();
        let mut len = 0;
        assert_eq!(
            unsafe { zu_value_field(cell, 2, &mut out, &mut len) },
            ZuStatus::Misuse
        );
        assert_eq!(
            unsafe { zu_value_at(cell, 2, &mut first) },
            ZuStatus::Misuse
        );
        let list = Value::List(vec![Value::Int(1)]);
        assert_eq!(
            unsafe { zu_value_field(cell_of(&list), 0, &mut out, &mut len) },
            ZuStatus::Misuse
        );
    }

    /// A rel carries the two ends it joins and the table it is in, none
    /// of which the columnar accessors have anywhere to put. A path is
    /// its elements, alternating, and reads as a composite.
    #[test]
    fn a_rel_and_a_path_read_as_the_parts_they_are_made_of() {
        let rel = Value::Rel {
            table: 4,
            src: 11,
            dst: 12,
            ord: 3,
        };
        let (mut table, mut src, mut dst) = (0u32, 0u64, 0u64);
        assert_eq!(
            unsafe { zu_value_rel(cell_of(&rel), &mut table, &mut src, &mut dst) },
            ZuStatus::Ok
        );
        assert_eq!((table, src, dst), (4, 11, 12));
        // Asked for one end and nothing else, which is the read a
        // binding building an adjacency does.
        assert_eq!(
            unsafe {
                zu_value_rel(
                    cell_of(&rel),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dst,
                )
            },
            ZuStatus::Ok
        );
        assert_eq!(dst, 12);

        let path = Value::path(vec![
            Value::Node {
                table: 1,
                offset: 11,
            },
            rel,
            Value::Node {
                table: 1,
                offset: 12,
            },
        ])
        .expect("a walk of one edge");
        let cell = cell_of(&path);
        assert_eq!(unsafe { zu_value_type(cell) }, ZU_TYPE_PATH);
        assert_eq!(unsafe { zu_value_len(cell) }, 3);
        let mut middle = std::ptr::null();
        assert_eq!(unsafe { zu_value_at(cell, 1, &mut middle) }, ZuStatus::Ok);
        assert_eq!(unsafe { zu_value_type(middle) }, ZU_TYPE_REL);
    }

    /// Every accessor asked of the wrong type answers `Misuse` and
    /// writes its out-parameter anyway, so a caller that ignored the
    /// status reads a zero rather than whatever the call before left.
    #[test]
    fn an_accessor_asked_of_the_wrong_type_refuses_and_still_writes() {
        let value = Value::Str("not a number".to_string());
        let cell = cell_of(&value);

        let mut i = 7i64;
        assert_eq!(unsafe { zu_value_i64(cell, &mut i) }, ZuStatus::Misuse);
        assert_eq!(i, 0);
        let mut f = 7.0;
        assert_eq!(unsafe { zu_value_f64(cell, &mut f) }, ZuStatus::Misuse);
        assert_eq!(f, 0.0);
        let mut b = 7i32;
        assert_eq!(unsafe { zu_value_bool(cell, &mut b) }, ZuStatus::Misuse);
        assert_eq!(b, 0);
        let (mut table, mut offset) = (7u32, 7u64);
        assert_eq!(
            unsafe { zu_value_node(cell, &mut table, &mut offset) },
            ZuStatus::Misuse
        );
        assert_eq!((table, offset), (0, 0));
        let (mut kind, mut count) = (7i32, 7i64);
        assert_eq!(
            unsafe { zu_value_temporal(cell, &mut kind, &mut count, std::ptr::null_mut()) },
            ZuStatus::Misuse
        );
        assert_eq!((kind, count), (-1, 0));

        // A bool is not an integer here even though a bool column
        // reads as one, which is the whole difference between the two
        // paths and is worth pinning rather than describing.
        let b = Value::Bool(true);
        assert_eq!(
            unsafe { zu_value_i64(cell_of(&b), &mut i) },
            ZuStatus::Misuse
        );
        assert_eq!(as_i64(&Value::Bool(true)), Some(1));

        // And a NULL pointer is misuse on every one of them rather
        // than a crash, since a binding that lost a result will pass
        // one eventually.
        let null: *const ZuValue = std::ptr::null();
        assert_eq!(unsafe { zu_value_type(null) }, -1);
        assert_eq!(unsafe { zu_value_len(null) }, 0);
        assert_eq!(unsafe { zu_value_i64(null, &mut i) }, ZuStatus::Misuse);
        let mut out = std::ptr::null();
        let mut len = 9;
        assert_eq!(
            unsafe { zu_value_str(null, &mut out, &mut len) },
            ZuStatus::Misuse
        );
        assert_eq!(len, 0);
        // A length that cannot be written is an answer nobody could
        // read, so the accessor refuses rather than half answering.
        assert_eq!(
            unsafe { zu_value_str(cell, &mut out, std::ptr::null_mut()) },
            ZuStatus::Misuse
        );
    }

    /// A connection is claimed for the length of a call and released
    /// when the claim drops, so the second thread is turned away
    /// rather than let in beside the first.
    #[test]
    fn a_claim_excludes_a_second_one_and_a_closed_connection_refuses_both() {
        let state = Arc::new(ConnState {
            alive: AtomicBool::new(true),
            busy: AtomicBool::new(false),
        });
        let held = claim(&state).expect("free");
        assert_eq!(claim(&state).err(), Some(ZuStatus::MisuseConcurrent));
        drop(held);
        // Released, so the next call gets in.
        drop(claim(&state).expect("released"));

        state.alive.store(false, Ordering::Release);
        assert_eq!(claim(&state).err(), Some(ZuStatus::MisuseClosed));
    }
}
