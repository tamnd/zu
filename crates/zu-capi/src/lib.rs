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

// The pointer contract (what must be valid, who frees what, in which
// order) is one contract for the whole surface; it lives in the module
// docs above and in include/zu.h where a C caller will actually read
// it, not repeated under every function.
#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::mem::{offset_of, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zu_common::{DurationKind, Temporal};
use zudb::query::{QueryResult, Value};
use zudb::{Config, Connection, Database, Severity, ZuError as EngineError};

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
pub struct ZuConn {
    conn: Connection,
    state: Arc<ConnState>,
}

impl ZuConn {
    fn new(conn: Connection) -> ZuConn {
        ZuConn {
            conn,
            state: Arc::new(ConnState {
                alive: AtomicBool::new(true),
                busy: AtomicBool::new(false),
            }),
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
        let result = unsafe { conn_of(conn) }.query(q)?;
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
        let _claim = match unsafe { claim_stmt(stmt) } {
            Ok(claim) => claim,
            Err(status) => return Ok(status),
        };
        let borrowed: Vec<(&str, Value)> = unsafe { stmt_binds(stmt) }
            .iter()
            .map(|(n, v)| (n.as_str(), v.clone()))
            .collect();
        let (conn, id) = unsafe { ((*stmt).conn, (*stmt).id) };
        let result = unsafe { conn_of(conn) }.execute_prepared(id, &borrowed)?;
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
