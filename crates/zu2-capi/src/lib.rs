//! libzu2: the C surface over [`zu2db::Db`] and [`zu2db::Session`], built
//! for a host that keeps the process alive and drives the storage in a
//! loop (a cgo adapter, a benchmark harness, a language binding).
//!
//! The object model is the Rust one and the reasoning is the same as
//! libzu's: a [`Handle`] is the file and the structures over it and is
//! shareable across threads, a [`Zu2Session`] is the state that cannot
//! be shared, and a host that works from four threads opens one db and
//! four sessions. What is different is that zu2 has no query language,
//! so instead of a statement and a result there are records, edges, and
//! traversals.
//!
//! The traversals are the reason this crate exists in the shape it
//! does. zu2's claim is that a hop is an indexed load, and a C API that
//! made a host call back across the boundary once per node would
//! spend more on the boundary than on the hop. So `zu2_khop`,
//! `zu2_reach` and `zu2_triangles` run the whole walk inside one call,
//! inside one announced epoch, and hand back only the answer.
//!
//! Everything a call hands back lives in the session and is valid until
//! the next call on it. That is a copy, and it is a copy on purpose: a
//! neighbour list is only pinned while the epoch is announced, and the
//! epoch ends when the call returns, so a pointer that outlived the
//! call would point into a block a writer may already have replaced.
//!
//! Concurrency is checked rather than left as undefined behaviour. Each
//! handle carries a flag that a call sets on the way in, and a call
//! that finds it already set answers [`Zu2Status::MisuseConcurrent`]
//! instead of writing into a buffer another thread is reading. That
//! flag is also what makes the last-error slot and the scratch buffers
//! sound to reach through an [`UnsafeCell`], which is the only reason
//! the two `Sync` assertions below are allowed to exist.

use std::cell::{RefCell, UnsafeCell};
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zu2db::db::Core;
use zu2db::{Db, Direction, Durability, Error, Options, Result, Session};

/// What a call did.
///
/// The numbers are libzu's, so a host that links both libraries does
/// not carry two tables. The gaps are libzu's cases that have no
/// meaning here: there is no statement to interrupt and no plan to
/// outlive its connection.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zu2Status {
    /// The call did what it was asked and wrote its out-parameter.
    Ok = 0,
    /// The engine refused the work and the handle says why.
    Error = 3,
    /// The caller broke the contract in the header. Nothing was done
    /// and nothing is wrong with the database.
    Misuse = 4,
    /// Two threads used one handle at once. Nothing was done.
    MisuseConcurrent = 5,
    /// The db already has as many sessions open as `sessions` gave it
    /// room for. Nothing was done.
    NoSessions = 6,
}

/// How a database is sized and how durable it is, in the layout the
/// header declares.
///
/// Every field zero means the engine's default, which is why
/// `compact_below` needs a sentinel for off: zero already means "use
/// the default of 128 MiB", so `u64::MAX` is what says "never compact".
/// A load that is going to be measured and thrown away wants that, and
/// it is worth a sentinel rather than a second field.
///
/// `fixed_index` is the same shape of problem and takes the same
/// answer. The engine grows the index by default, so the field that
/// reaches C has to be the one whose zero is the default, which is the
/// negative one.
///
/// New fields go on the end. The header declares the layout and a host
/// zeroes the struct before it fills anything in, so an older caller
/// linked against a newer library gets defaults for what it does not
/// know about rather than a shifted read.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Zu2Options {
    pub durability: u32,
    pub index_buckets: u64,
    pub max_pages: u64,
    pub max_nodes: u64,
    pub space_target_percent: u32,
    pub compact_below: u64,
    pub sessions: u64,
    /// Nonzero pins the index at `index_buckets` however many keys
    /// arrive. That is what a measurement of what crowding costs needs,
    /// and it is the only way a caller who knows its key count exactly
    /// can keep the migration check off its read path.
    pub fixed_index: u32,
}

/// A database and the two things a C caller needs beside it: the flag
/// that catches a second thread, and the message the last failed call
/// left behind.
///
/// The `Db` is inside the `Arc` rather than beside it because a session
/// borrows from it. Holding a clone of the `Arc` is what lets the
/// borrow be handed to C: the `Db` sits at a stable address for as long
/// as any session is alive, whatever order the host closes things in.
pub struct Handle {
    db: Db,
    busy: AtomicBool,
    error: UnsafeCell<CString>,
}

// SAFETY: `Db` is `Send + Sync` on its own. The `UnsafeCell` is only
// ever reached through `enter`, which is a compare-exchange that no two
// threads win, so the message is written and read under an acquire and
// a release the same way a mutex would give.
unsafe impl Sync for Handle {}

/// One worker's view, plus the buffers its answers are handed back in.
///
/// The field order is load bearing and not a style. Fields drop in
/// declaration order, the state holds a `Session` that borrows the `Db`
/// inside the `Arc`, and a `Session` releases its epoch slot when it
/// drops. Put the `Arc` first and closing a session after its database
/// frees the `Db` and then hands the slot back to it, which is a write
/// into freed memory and showed up as an index into an empty slot
/// table.
pub struct Zu2Session {
    busy: AtomicBool,
    state: UnsafeCell<State>,
    /// Keeps the database alive for as long as this session is. Never
    /// read through, which is why it is named for what it does. Last,
    /// so it is released last.
    _owner: Arc<Handle>,
}

// SAFETY: same argument as `Handle`. A `Session` is not `Sync`, and
// this does not make it so: `enter` hands out `&mut State` to exactly
// one thread at a time and every other one gets `MisuseConcurrent`.
unsafe impl Sync for Zu2Session {}
// SAFETY: a `Session` owns an epoch slot and a scratch buffer and
// borrows a `Core` that is `Sync`. Nothing in it is tied to the thread
// that made it, so it may move, which is what a Go host's goroutine
// scheduler needs.
unsafe impl Send for Zu2Session {}

/// Everything a call on a session may write to.
struct State {
    session: Session<'static>,
    error: CString,
    /// The buffer a value read is answered out of.
    value: Vec<u8>,
    /// The buffer a neighbour list or a frontier is answered out of.
    /// This is the one a returned pointer points into.
    answer: Vec<u32>,
    /// The other two the traversals need. `frontier` is the level being
    /// expanded and `next` is the one being built.
    frontier: Vec<u32>,
    next: Vec<u32>,
    /// One bit per node. Left clean by every call that uses it, by
    /// clearing the bits it set rather than the whole thing, so a probe
    /// on a big graph costs its own frontier and not the node count.
    seen: Vec<u64>,
}

/// Held for the length of a call on a session. Dropping it lets the
/// next call in.
struct Entered<'a> {
    busy: &'a AtomicBool,
    state: &'a mut State,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

impl Zu2Session {
    fn enter(&self) -> Option<Entered<'_>> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // SAFETY: the exchange above succeeded, so this thread is the
        // only one with a reference to the state until the guard drops.
        let state = unsafe { &mut *self.state.get() };
        Some(Entered {
            busy: &self.busy,
            state,
        })
    }
}

impl Handle {
    /// The db equivalent of [`Zu2Session::enter`]. Only the fallible
    /// calls take it; the accessors that cannot fail have no message to
    /// write and are left callable from anywhere.
    fn enter(&self) -> Option<HandleEntered<'_>> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // SAFETY: as above, the exchange makes this exclusive.
        let error = unsafe { &mut *self.error.get() };
        Some(HandleEntered {
            busy: &self.busy,
            db: &self.db,
            error,
        })
    }
}

struct HandleEntered<'a> {
    busy: &'a AtomicBool,
    db: &'a Db,
    error: &'a mut CString,
}

impl Drop for HandleEntered<'_> {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// Turns an engine error into the message a caller reads and the status
/// it branches on, in one place so no call can do half of it.
fn note<T>(slot: &mut CString, outcome: Result<T>) -> std::result::Result<T, Zu2Status> {
    match outcome {
        Ok(value) => {
            slot.clear_message();
            Ok(value)
        }
        Err(error) => {
            slot.set_message(&error);
            Err(Zu2Status::Error)
        }
    }
}

/// Two small operations on the message slot, named so the call sites
/// read as what they mean.
trait Message {
    fn clear_message(&mut self);
    fn set_message(&mut self, error: &Error);
}

impl Message for CString {
    fn clear_message(&mut self) {
        if !self.as_bytes().is_empty() {
            *self = CString::default();
        }
    }

    fn set_message(&mut self, error: &Error) {
        // An engine message with a NUL in it would be a message this
        // library invented, not one a caller wrote, so there is nothing
        // to preserve: an empty string is a truer answer than a
        // truncated one.
        *self = CString::new(error.to_string()).unwrap_or_default();
    }
}

thread_local! {
    /// The message the last failed [`zu2_open`] left, on this thread.
    ///
    /// This is the one message in the library that does not belong to
    /// a handle, for the plain reason that a failed open produced no
    /// handle to hang it on. Per thread rather than global so that two
    /// threads opening two databases cannot overwrite each other's
    /// reason.
    static OPEN_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

/// A counted C string as bytes. NULL with a zero length is empty rather
/// than an error, which is what a host with an empty key would send.
///
/// # Safety
/// `ptr` must point at `len` readable bytes, or be NULL with `len` 0.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller's contract, checked as far as a pointer can be.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// A direction as a walk takes it. The engine keeps an out list and an
/// in list and knows nothing of an undirected edge, so `ZU2_BOTH` is not
/// an engine direction, it is two loads and a merge, and the walks are
/// written over a slice of directions so that stays one code path.
///
/// Undirected is worth having because it is what the reference queries
/// ask for: a host that writes `(a)-[:EDGE]-(b)` with no arrow gets a
/// different answer from one that writes an arrow, and an adapter that
/// quietly answered the directed question would be reporting a number
/// for a query it did not run.
fn ways_of(dir: c_int) -> Option<&'static [Direction]> {
    const OUT: &[Direction] = &[Direction::Out];
    const IN: &[Direction] = &[Direction::In];
    const BOTH: &[Direction] = &[Direction::Out, Direction::In];
    match dir {
        0 => Some(OUT),
        1 => Some(IN),
        2 => Some(BOTH),
        _ => None,
    }
}

fn durability_of(value: u32) -> Option<Durability> {
    match value {
        0 => Some(Durability::Async),
        1 => Some(Durability::Durable),
        _ => None,
    }
}

/// Reads a caller's options, taking the engine's default wherever a
/// field is zero.
fn options_of(opt: *const Zu2Options) -> Option<Options> {
    let mut options = Options::default();
    if opt.is_null() {
        return Some(options);
    }
    // SAFETY: the caller says this points at a `zu2_options`.
    let given = unsafe { *opt };
    options.durability = durability_of(given.durability)?;
    if given.index_buckets > 0 {
        options.index_buckets = given.index_buckets as usize;
    }
    if given.max_pages > 0 {
        options.max_pages = given.max_pages as usize;
    }
    if given.max_nodes > 0 {
        options.max_nodes = given.max_nodes as usize;
    }
    if given.space_target_percent > 0 {
        options.space_target_percent = given.space_target_percent;
    }
    if given.sessions > 0 {
        options.sessions = given.sessions as usize;
    }
    if given.fixed_index != 0 {
        options.grow_index = false;
    }
    options.compact_below = match given.compact_below {
        0 => options.compact_below,
        u64::MAX => 0,
        given => given,
    };
    Some(options)
}

/// Borrows a database handle.
///
/// # Safety
/// `db` is a pointer [`zu2_open`] wrote and [`zu2_close`] has not
/// consumed.
unsafe fn handle<'a>(db: *const Zu2Db) -> Option<&'a Handle> {
    if db.is_null() {
        return None;
    }
    // SAFETY: the caller's contract. The pointer was an `Arc<Handle>`
    // raw pointer and the `Arc` is still alive.
    Some(unsafe { &*(db as *const Handle) })
}

/// Borrows a session handle.
///
/// # Safety
/// `s` is a pointer [`zu2_session_open`] wrote and
/// [`zu2_session_close`] has not consumed.
unsafe fn session<'a>(s: *const Zu2Session) -> Option<&'a Zu2Session> {
    if s.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    Some(unsafe { &*s })
}

/// The opaque type the header declares. It is never constructed: the
/// pointer a caller holds is an `Arc<Handle>` cast to this, which is
/// what keeps `Handle`'s fields out of the header.
pub enum Zu2Db {}

// ---- lifecycle ----

/// Fills an options struct with the engine's defaults.
///
/// # Safety
/// `opt` points at a writable `zu2_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_options_init(opt: *mut Zu2Options) -> Zu2Status {
    if opt.is_null() {
        return Zu2Status::Misuse;
    }
    // Zero is what every field means by default, and the struct's
    // `Default` is all zeroes, so this is the whole of it. It exists
    // anyway because a caller who has to know that has to read this
    // source to find it out.
    unsafe { opt.write(Zu2Options::default()) };
    Zu2Status::Ok
}

/// Opens the database at `path`, creating it when it is not there.
///
/// # Safety
/// `path` points at `path_len` bytes, `opt` at a `zu2_options` or is
/// NULL, and `out`, `err` and `err_len` at writable slots or are NULL
/// for the last two.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_open(
    path: *const c_char,
    path_len: usize,
    opt: *const Zu2Options,
    out: *mut *mut Zu2Db,
    err: *mut *const c_char,
    err_len: *mut usize,
) -> Zu2Status {
    // Written first and on every path, so a caller who ignores the
    // status is never holding the pointer from the call before.
    if !out.is_null() {
        unsafe { out.write(std::ptr::null_mut()) };
    }
    let publish = |message: &str| {
        OPEN_ERROR.with(|slot| {
            let text = CString::new(message).unwrap_or_default();
            let mut slot = slot.borrow_mut();
            *slot = text;
            if !err.is_null() {
                unsafe { err.write(slot.as_ptr()) };
            }
            if !err_len.is_null() {
                unsafe { err_len.write(slot.as_bytes().len()) };
            }
        });
    };
    publish("");
    if out.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(raw) = (unsafe { bytes(path as *const u8, path_len) }) else {
        publish("path is NULL");
        return Zu2Status::Misuse;
    };
    let Ok(text) = std::str::from_utf8(raw) else {
        publish("path is not utf8");
        return Zu2Status::Misuse;
    };
    let Some(options) = options_of(opt) else {
        publish("durability is neither ZU2_ASYNC nor ZU2_DURABLE");
        return Zu2Status::Misuse;
    };
    let db = match Db::open_or_create(Path::new(text), options) {
        Ok(db) => db,
        Err(error) => {
            publish(&error.to_string());
            return Zu2Status::Error;
        }
    };
    let handle = Arc::new(Handle {
        db,
        busy: AtomicBool::new(false),
        error: UnsafeCell::new(CString::default()),
    });
    unsafe { out.write(Arc::into_raw(handle) as *mut Zu2Db) };
    Zu2Status::Ok
}

/// Closes a database. A no-op on NULL.
///
/// # Safety
/// `db` came from [`zu2_open`] and has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_close(db: *mut Zu2Db) {
    if db.is_null() {
        return;
    }
    // Dropping the `Arc` is not necessarily dropping the `Db`: a
    // session still open holds a clone, and the engine goes away with
    // the last of them. That is the only arrangement that makes a host
    // closing its handles in the wrong order safe rather than fatal.
    drop(unsafe { Arc::from_raw(db as *const Handle) });
}

/// What went wrong in the last fallible call on this db.
///
/// # Safety
/// `db` is live and no other thread is inside a call on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_db_error(db: *const Zu2Db, len: *mut usize) -> *const c_char {
    let Some(handle) = (unsafe { handle(db) }) else {
        if !len.is_null() {
            unsafe { len.write(0) };
        }
        return std::ptr::null();
    };
    // SAFETY: the caller's contract that no call is in flight. Reading
    // a message while another thread writes one is the one race this
    // library asks the host to avoid, and it asks because the
    // alternative is handing back a copy the host then has to free.
    let message = unsafe { &*handle.error.get() };
    if !len.is_null() {
        unsafe { len.write(message.as_bytes().len()) };
    }
    message.as_ptr()
}

/// Opens a session.
///
/// # Safety
/// `db` is live and `out` points at a writable slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_session_open(db: *mut Zu2Db, out: *mut *mut Zu2Session) -> Zu2Status {
    if !out.is_null() {
        unsafe { out.write(std::ptr::null_mut()) };
    }
    if out.is_null() {
        return Zu2Status::Misuse;
    }
    if db.is_null() {
        return Zu2Status::Misuse;
    }
    // SAFETY: `db` is an `Arc<Handle>` raw pointer the caller still
    // holds, so incrementing before rebuilding one keeps the count
    // right and leaves the caller's pointer usable.
    let owner = unsafe {
        Arc::increment_strong_count(db as *const Handle);
        Arc::from_raw(db as *const Handle)
    };
    let session = match owner.db.try_session() {
        Ok(session) => session,
        Err(_) => {
            // The `Arc` was incremented on the way in and the caller
            // still holds its own, so this hands back the clone this
            // call took rather than the caller's.
            return Zu2Status::NoSessions;
        }
    };
    // SAFETY: the session borrows the `Db`, which lives inside the
    // `Arc` this struct holds a clone of, at an address that does not
    // move for as long as the clone does. The clone is the last field
    // of the struct, so it is released after the session that borrows
    // it, which is the part of this that is easy to get wrong.
    let session: Session<'static> = unsafe { std::mem::transmute(session) };
    let boxed = Box::new(Zu2Session {
        busy: AtomicBool::new(false),
        state: UnsafeCell::new(State {
            session,
            error: CString::default(),
            value: Vec::new(),
            answer: Vec::new(),
            frontier: Vec::new(),
            next: Vec::new(),
            seen: Vec::new(),
        }),
        _owner: owner,
    });
    unsafe { out.write(Box::into_raw(boxed)) };
    Zu2Status::Ok
}

/// Closes a session. A no-op on NULL.
///
/// # Safety
/// `s` came from [`zu2_session_open`] and has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_session_close(s: *mut Zu2Session) {
    if s.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(s) });
}

/// What went wrong in the last fallible call on this session.
///
/// # Safety
/// `s` is live and no other thread is inside a call on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_session_error(s: *const Zu2Session, len: *mut usize) -> *const c_char {
    let Some(s) = (unsafe { session(s) }) else {
        if !len.is_null() {
            unsafe { len.write(0) };
        }
        return std::ptr::null();
    };
    // SAFETY: as `zu2_db_error`, on the caller's contract.
    let message = unsafe { &(*s.state.get()).error };
    if !len.is_null() {
        unsafe { len.write(message.as_bytes().len()) };
    }
    message.as_ptr()
}

/// Changes how far this session waits before acknowledging a write.
///
/// # Safety
/// `s` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_set_durability(s: *mut Zu2Session, durability: u32) -> Zu2Status {
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(durability) = durability_of(durability) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    call.state.session.set_durability(durability);
    call.state.error.clear_message();
    Zu2Status::Ok
}

// ---- records ----

/// Writes `value` under `key`.
///
/// # Safety
/// `s` is live and both pointers cover their stated lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_upsert(
    s: *mut Zu2Session,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> Zu2Status {
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let (Some(key), Some(value)) = (unsafe { bytes(key, key_len) }, unsafe {
        bytes(value, value_len)
    }) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    match note(&mut state.error, state.session.upsert(key, value)) {
        Ok(()) => Zu2Status::Ok,
        Err(status) => status,
    }
}

/// Reads the newest value for `key`.
///
/// # Safety
/// `s` is live, `key` covers `key_len`, and the three out-parameters
/// point at writable slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_read(
    s: *mut Zu2Session,
    key: *const u8,
    key_len: usize,
    value: *mut *const u8,
    value_len: *mut usize,
    found: *mut c_int,
) -> Zu2Status {
    if !value.is_null() {
        unsafe { value.write(std::ptr::null()) };
    }
    if !value_len.is_null() {
        unsafe { value_len.write(0) };
    }
    if !found.is_null() {
        unsafe { found.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(key) = (unsafe { bytes(key, key_len) }) else {
        return Zu2Status::Misuse;
    };
    if value.is_null() || value_len.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let mut buffer = std::mem::take(&mut state.value);
    let outcome = state.session.read(key, &mut buffer);
    state.value = buffer;
    let hit = match note(&mut state.error, outcome) {
        Ok(hit) => hit,
        Err(status) => return status,
    };
    if hit {
        unsafe { value.write(state.value.as_ptr()) };
        unsafe { value_len.write(state.value.len()) };
    }
    if !found.is_null() {
        unsafe { found.write(c_int::from(hit)) };
    }
    Zu2Status::Ok
}

/// Removes `key`.
///
/// # Safety
/// `s` is live, `key` covers `key_len`, `existed` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_delete(
    s: *mut Zu2Session,
    key: *const u8,
    key_len: usize,
    existed: *mut c_int,
) -> Zu2Status {
    if !existed.is_null() {
        unsafe { existed.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(key) = (unsafe { bytes(key, key_len) }) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let outcome = state.session.delete(key);
    match note(&mut state.error, outcome) {
        Ok(hit) => {
            if !existed.is_null() {
                unsafe { existed.write(c_int::from(hit)) };
            }
            Zu2Status::Ok
        }
        Err(status) => status,
    }
}

// ---- graph ----

/// Creates a node under an external key and returns its dense id.
///
/// # Safety
/// `s` is live, `key` covers `key_len`, `node` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_add_node(
    s: *mut Zu2Session,
    key: *const u8,
    key_len: usize,
    node: *mut u32,
) -> Zu2Status {
    if !node.is_null() {
        unsafe { node.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(key) = (unsafe { bytes(key, key_len) }) else {
        return Zu2Status::Misuse;
    };
    if node.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let outcome = state.session.add_node(key);
    match note(&mut state.error, outcome) {
        Ok(id) => {
            unsafe { node.write(id) };
            Zu2Status::Ok
        }
        Err(status) => status,
    }
}

/// The dense id of the node with this key.
///
/// # Safety
/// `s` is live, `key` covers `key_len`, `node` is writable, `found`
/// is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_node_of(
    s: *mut Zu2Session,
    key: *const u8,
    key_len: usize,
    node: *mut u32,
    found: *mut c_int,
) -> Zu2Status {
    if !node.is_null() {
        unsafe { node.write(0) };
    }
    if !found.is_null() {
        unsafe { found.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(key) = (unsafe { bytes(key, key_len) }) else {
        return Zu2Status::Misuse;
    };
    if node.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let mut buffer = std::mem::take(&mut state.value);
    let outcome = state.session.node_of(key, &mut buffer);
    state.value = buffer;
    match note(&mut state.error, outcome) {
        Ok(Some(id)) => {
            unsafe { node.write(id) };
            if !found.is_null() {
                unsafe { found.write(1) };
            }
            Zu2Status::Ok
        }
        Ok(None) => Zu2Status::Ok,
        Err(status) => status,
    }
}

/// Links `src` to `dst`.
///
/// # Safety
/// `s` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_add_edge(s: *mut Zu2Session, src: u32, dst: u32) -> Zu2Status {
    unsafe { edge(s, src, dst, true) }
}

/// Unlinks `src` from `dst`.
///
/// # Safety
/// `s` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_remove_edge(s: *mut Zu2Session, src: u32, dst: u32) -> Zu2Status {
    unsafe { edge(s, src, dst, false) }
}

/// # Safety
/// `s` is live.
unsafe fn edge(s: *mut Zu2Session, src: u32, dst: u32, add: bool) -> Zu2Status {
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let outcome = if add {
        state.session.add_edge(src, dst)
    } else {
        state.session.remove_edge(src, dst)
    };
    match note(&mut state.error, outcome) {
        Ok(()) => Zu2Status::Ok,
        Err(status) => status,
    }
}

/// The out, in or undirected degree.
///
/// One direction is a counter the engine already keeps, so it is a load.
/// `ZU2_BOTH` is the number of distinct neighbours either way round,
/// which is a merge of the two lists rather than a sum of the two
/// counts, because an edge that runs both ways is one neighbour.
///
/// # Safety
/// `s` is live and `degree` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_degree(
    s: *mut Zu2Session,
    dir: c_int,
    node: u32,
    degree: *mut u32,
) -> Zu2Status {
    if !degree.is_null() {
        unsafe { degree.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(ways) = ways_of(dir) else {
        return Zu2Status::Misuse;
    };
    if degree.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let answer = if let [one] = ways {
        state.session.degree(*one, node)
    } else {
        let mut buffer = std::mem::take(&mut state.answer);
        gather(&mut state.session, ways, node, &mut buffer);
        let count = buffer.len() as u32;
        state.answer = buffer;
        count
    };
    state.error.clear_message();
    unsafe { degree.write(answer) };
    Zu2Status::Ok
}

/// A node's neighbours, copied into the session's buffer.
///
/// # Safety
/// `s` is live and both out-parameters are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_neighbours(
    s: *mut Zu2Session,
    dir: c_int,
    node: u32,
    out: *mut *const u32,
    len: *mut usize,
) -> Zu2Status {
    if !out.is_null() {
        unsafe { out.write(std::ptr::null()) };
    }
    if !len.is_null() {
        unsafe { len.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(ways) = ways_of(dir) else {
        return Zu2Status::Misuse;
    };
    if out.is_null() || len.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let mut answer = std::mem::take(&mut state.answer);
    gather(&mut state.session, ways, node, &mut answer);
    state.answer = answer;
    state.error.clear_message();
    unsafe { out.write(state.answer.as_ptr()) };
    unsafe { len.write(state.answer.len()) };
    Zu2Status::Ok
}

/// One node's neighbours over every direction asked for, in `into`.
///
/// A single direction is a copy of a list the engine already holds in
/// order. Both directions is that twice and then a sort, because the two
/// lists overlap wherever a pair of nodes point at each other and a
/// neighbour named twice would be counted twice. The same sort is what
/// makes the two-list path safe against a load the engine retries, since
/// a retry can run the closure again and the duplicate falls out.
fn gather(session: &mut Session<'_>, ways: &[Direction], node: u32, into: &mut Vec<u32>) {
    if let [one] = ways {
        session.neighbours_into(*one, node, into);
        return;
    }
    into.clear();
    for &direction in ways {
        session.neighbours(direction, node, |slice| into.extend_from_slice(slice));
    }
    into.sort_unstable();
    into.dedup();
}

/// The distinct nodes exactly `k` hops from `seed`.
///
/// # Safety
/// `s` is live and both out-parameters are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_khop(
    s: *mut Zu2Session,
    dir: c_int,
    seed: u32,
    k: u32,
    out: *mut *const u32,
    len: *mut usize,
) -> Zu2Status {
    if !out.is_null() {
        unsafe { out.write(std::ptr::null()) };
    }
    if !len.is_null() {
        unsafe { len.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(ways) = ways_of(dir) else {
        return Zu2Status::Misuse;
    };
    if out.is_null() || len.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    khop(state, ways, seed, k);
    state.error.clear_message();
    unsafe { out.write(state.answer.as_ptr()) };
    unsafe { len.write(state.answer.len()) };
    Zu2Status::Ok
}

/// The walk behind [`zu2_khop`], with the buffers taken out of the
/// state so the borrow checker can see that the closure and the session
/// are not the same borrow.
fn khop(state: &mut State, ways: &[Direction], seed: u32, k: u32) {
    let mut current = std::mem::take(&mut state.frontier);
    let mut next = std::mem::take(&mut state.next);
    let mut seen = std::mem::take(&mut state.seen);
    resize_seen(&mut seen, &state.session);
    current.clear();
    current.push(seed);
    state.session.walk(|walk| {
        for _ in 0..k {
            next.clear();
            for &node in &current {
                for &direction in ways {
                    walk.neighbours(direction, node, |slice| {
                        for &far in slice {
                            // The bitmap makes the level distinct, and it
                            // also makes the closure safe to run twice: a
                            // retry meets its own bits and adds nothing. It
                            // is also what keeps the undirected walk from
                            // naming a node once per direction.
                            if mark(&mut seen, far) {
                                next.push(far);
                            }
                        }
                    });
                }
            }
            // Cleared per level rather than cumulatively, because distinct
            // here means distinct within the level: a node the walk
            // already passed is still a node k hops out.
            clear(&mut seen, &next);
            std::mem::swap(&mut current, &mut next);
            if current.is_empty() {
                break;
            }
        }
    });
    state.answer.clear();
    state.answer.extend_from_slice(&current);
    state.frontier = current;
    state.next = next;
    state.seen = seen;
}

/// The distinct nodes reachable from `seed` in one hop or more, up to
/// `max_depth` hops, breadth first.
///
/// `max_depth` 0 is no bound and walks the whole reachable set, and
/// `max_visited` 0 is no bound on the size of the answer. A walk that
/// hits the size bound stops there and reports what it has, which is how
/// a probe on a graph with one enormous component stays a probe.
///
/// The seed is not in the answer unless a path leads back to it. That is
/// deliberate and it is the difference between this and a component
/// walk: the question a host asks is the one `-[:EDGE*1..k]->` asks, and
/// a seed counted as its own neighbour would put every answer one out.
///
/// # Safety
/// `s` is live and both out-parameters are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_reach(
    s: *mut Zu2Session,
    dir: c_int,
    seed: u32,
    max_depth: u32,
    max_visited: u64,
    out: *mut *const u32,
    len: *mut usize,
) -> Zu2Status {
    if !out.is_null() {
        unsafe { out.write(std::ptr::null()) };
    }
    if !len.is_null() {
        unsafe { len.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(ways) = ways_of(dir) else {
        return Zu2Status::Misuse;
    };
    if out.is_null() || len.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    reach(state, ways, seed, max_depth, max_visited);
    state.error.clear_message();
    unsafe { out.write(state.answer.as_ptr()) };
    unsafe { len.write(state.answer.len()) };
    Zu2Status::Ok
}

fn reach(state: &mut State, ways: &[Direction], seed: u32, max_depth: u32, max_visited: u64) {
    let mut current = std::mem::take(&mut state.frontier);
    let mut next = std::mem::take(&mut state.next);
    let mut seen = std::mem::take(&mut state.seen);
    let mut visited = std::mem::take(&mut state.answer);
    resize_seen(&mut seen, &state.session);
    let cap = if max_visited == 0 {
        usize::MAX
    } else {
        max_visited as usize
    };
    let depth_cap = if max_depth == 0 { u32::MAX } else { max_depth };
    visited.clear();
    current.clear();
    current.push(seed);
    let mut depth = 0;
    // The seed goes into the frontier without going into the bitmap, so
    // it is somewhere the walk can arrive at rather than somewhere it
    // has been.
    state.session.walk(|walk| {
        'walk: while !current.is_empty() && visited.len() < cap && depth < depth_cap {
            depth += 1;
            next.clear();
            for &node in &current {
                for &direction in ways {
                    let full = walk.neighbours(direction, node, |slice| {
                        for &far in slice {
                            if mark(&mut seen, far) {
                                visited.push(far);
                                next.push(far);
                                if visited.len() >= cap {
                                    return true;
                                }
                            }
                        }
                        false
                    });
                    if full {
                        break 'walk;
                    }
                }
            }
            std::mem::swap(&mut current, &mut next);
        }
    });
    // Cumulative here, unlike the k-hop walk, so the whole visited set
    // is what has to be given back.
    clear(&mut seen, &visited);
    state.answer = visited;
    state.frontier = current;
    state.next = next;
    state.seen = seen;
}

/// The number of hops on a shortest path from `src` to `dst`, or not
/// found if no path of at most `max_depth` hops exists.
///
/// `max_depth` 0 is no bound, and that is what a host that wants a true
/// answer passes: a bounded walk that ends without arriving cannot tell
/// "no path" from "no short path", and it reports the same not-found for
/// both. A source that is the destination is nought hops and found.
///
/// The walk is one breadth first search from `src` that stops the moment
/// it arrives. It is not a meet in the middle, and it does not need to
/// be for `ZU2_BOTH`: undirected here means the edge is followed either
/// way round, which is the question `(a)-[:EDGE*]-(b)` asks.
///
/// # Safety
/// `s` is live and both out-parameters are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_shortest(
    s: *mut Zu2Session,
    dir: c_int,
    src: u32,
    dst: u32,
    max_depth: u32,
    hops: *mut u32,
    found: *mut c_int,
) -> Zu2Status {
    if !hops.is_null() {
        unsafe { hops.write(0) };
    }
    if !found.is_null() {
        unsafe { found.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    let Some(ways) = ways_of(dir) else {
        return Zu2Status::Misuse;
    };
    if hops.is_null() || found.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let answer = shortest(state, ways, src, dst, max_depth);
    state.error.clear_message();
    if let Some(distance) = answer {
        unsafe { hops.write(distance) };
        unsafe { found.write(1) };
    }
    Zu2Status::Ok
}

fn shortest(
    state: &mut State,
    ways: &[Direction],
    src: u32,
    dst: u32,
    max_depth: u32,
) -> Option<u32> {
    if src == dst {
        return Some(0);
    }
    let mut current = std::mem::take(&mut state.frontier);
    let mut next = std::mem::take(&mut state.next);
    let mut seen = std::mem::take(&mut state.seen);
    let mut touched = std::mem::take(&mut state.answer);
    resize_seen(&mut seen, &state.session);
    let depth_cap = if max_depth == 0 { u32::MAX } else { max_depth };
    touched.clear();
    current.clear();
    current.push(src);
    mark(&mut seen, src);
    touched.push(src);
    let mut distance = 0;
    let mut arrived = None;
    state.session.walk(|walk| {
        'walk: while !current.is_empty() && distance < depth_cap {
            distance += 1;
            next.clear();
            for &node in &current {
                for &direction in ways {
                    let hit = walk.neighbours(direction, node, |slice| {
                        for &far in slice {
                            // Outside the mark rather than under it. A mark
                            // is a fact about what this walk has done and
                            // an arrival is a fact about the graph, and the
                            // seqlock can run this closure a second time,
                            // where the mark is already set and the
                            // arrival is still true. Under the guard the
                            // second run skipped the test, the walk carried
                            // on with the destination marked and unreachable
                            // for the rest of the search, and a one hop pair
                            // came back as no path. #468
                            if far == dst {
                                return true;
                            }
                            if mark(&mut seen, far) {
                                // Every marked node is remembered so the
                                // bitmap can be handed back clean at a cost
                                // of what was walked rather than what
                                // exists.
                                touched.push(far);
                                next.push(far);
                            }
                        }
                        false
                    });
                    if hit {
                        arrived = Some(distance);
                        break 'walk;
                    }
                }
            }
            std::mem::swap(&mut current, &mut next);
        }
    });
    clear(&mut seen, &touched);
    state.answer = touched;
    state.frontier = current;
    state.next = next;
    state.seen = seen;
    arrived
}

/// Closed directed triangles through `seed`.
///
/// # Safety
/// `s` is live and `count` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_triangles(
    s: *mut Zu2Session,
    seed: u32,
    count: *mut u64,
) -> Zu2Status {
    if !count.is_null() {
        unsafe { count.write(0) };
    }
    let Some(s) = (unsafe { session(s) }) else {
        return Zu2Status::Misuse;
    };
    if count.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = s.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let state = &mut *call.state;
    let answer = triangles(state, seed);
    state.error.clear_message();
    unsafe { count.write(answer) };
    Zu2Status::Ok
}

fn triangles(state: &mut State, seed: u32) -> u64 {
    let mut first = std::mem::take(&mut state.frontier);
    let mut seen = std::mem::take(&mut state.seen);
    resize_seen(&mut seen, &state.session);
    let mut total = 0u64;
    state.session.walk(|walk| {
        walk.neighbours(Direction::Out, seed, |slice| {
            first.clear();
            first.extend_from_slice(slice);
        });
        // The seed's own neighbourhood goes in the bitmap once, so the
        // closing test on every candidate is a bit rather than a search.
        for &near in first.iter() {
            mark(&mut seen, near);
        }
        for &near in &first {
            total += walk.neighbours(Direction::Out, near, |slice| {
                let mut hits = 0u64;
                for &far in slice {
                    if is_marked(&seen, far) {
                        hits += 1;
                    }
                }
                hits
            });
        }
    });
    clear(&mut seen, &first);
    state.frontier = first;
    state.seen = seen;
    total
}

/// Grows the bitmap to cover every node the graph has allocated.
fn resize_seen(seen: &mut Vec<u64>, session: &Session<'_>) {
    let words = (session.core_ref().graph().nodes() as usize)
        .div_ceil(64)
        .max(1);
    if seen.len() < words {
        seen.resize(words, 0);
    }
}

/// Sets a node's bit and says whether it was this call that set it.
/// A node past the bitmap is treated as already seen, which drops it:
/// the alternative is growing the map inside the walk, and a node
/// allocated after the walk started is not part of the answer anyway.
fn mark(seen: &mut [u64], node: u32) -> bool {
    let word = node as usize / 64;
    let bit = 1u64 << (node % 64);
    if word >= seen.len() || seen[word] & bit != 0 {
        return false;
    }
    seen[word] |= bit;
    true
}

fn is_marked(seen: &[u64], node: u32) -> bool {
    let word = node as usize / 64;
    word < seen.len() && seen[word] & (1u64 << (node % 64)) != 0
}

/// Clears exactly the bits a walk set, so the next probe costs its own
/// frontier rather than the node count.
fn clear(seen: &mut [u64], set: &[u32]) {
    for &node in set {
        let word = node as usize / 64;
        if word < seen.len() {
            seen[word] &= !(1u64 << (node % 64));
        }
    }
}

/// How many nodes the graph holds.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_nodes(db: *const Zu2Db) -> u32 {
    match unsafe { handle(db) } {
        Some(handle) => core_of(handle).graph().nodes(),
        None => 0,
    }
}

fn core_of(handle: &Handle) -> &Core {
    handle.db.core()
}

// ---- administration ----

/// Makes everything appended so far durable.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_sync(db: *mut Zu2Db) -> Zu2Status {
    let Some(handle) = (unsafe { handle(db) }) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = handle.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let outcome = call.db.sync();
    match note(call.error, outcome) {
        Ok(()) => Zu2Status::Ok,
        Err(status) => status,
    }
}

/// Compacts until another pass would not pay for itself.
///
/// # Safety
/// `db` is live and `reclaimed` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_compact(db: *mut Zu2Db, reclaimed: *mut u64) -> Zu2Status {
    if !reclaimed.is_null() {
        unsafe { reclaimed.write(0) };
    }
    let Some(handle) = (unsafe { handle(db) }) else {
        return Zu2Status::Misuse;
    };
    let Some(call) = handle.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let outcome = call.db.compact();
    match note(call.error, outcome) {
        Ok(bytes) => {
            if !reclaimed.is_null() {
                unsafe { reclaimed.write(bytes) };
            }
            Zu2Status::Ok
        }
        Err(status) => status,
    }
}

/// Bytes the file occupies on the device, holes excluded.
///
/// # Safety
/// `db` is live and `bytes` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_disk_bytes(db: *mut Zu2Db, bytes: *mut u64) -> Zu2Status {
    if !bytes.is_null() {
        unsafe { bytes.write(0) };
    }
    let Some(handle) = (unsafe { handle(db) }) else {
        return Zu2Status::Misuse;
    };
    if bytes.is_null() {
        return Zu2Status::Misuse;
    }
    let Some(call) = handle.enter() else {
        return Zu2Status::MisuseConcurrent;
    };
    let outcome = call.db.disk_bytes();
    match note(call.error, outcome) {
        Ok(size) => {
            unsafe { bytes.write(size) };
            Zu2Status::Ok
        }
        Err(status) => status,
    }
}

/// Addresses the log has spent.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_log_bytes(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.log_bytes(),
        None => 0,
    }
}

/// Addresses the log still spans.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_log_span(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.log_span(),
        None => 0,
    }
}

/// Entries in use in the hash index.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_index_occupancy(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.index_occupancy() as u64,
        None => 0,
    }
}

/// Buckets in the live hash table.
///
/// Against `zu2_index_occupancy` this is the load factor, which is what
/// says whether a read is walking a chain because the table is crowded
/// or because the keys collided.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_index_buckets(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.index_buckets() as u64,
        None => 0,
    }
}

/// Times the index has doubled since the database was opened.
///
/// A run that reports zero here either sized its table right or never
/// grew, and those are different things: the load factor tells them
/// apart. A run that reports several has paid for every one of them,
/// and this is what lets a benchmark say so rather than leave it to be
/// inferred from a curve.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_index_grows(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.index_grows(),
        None => 0,
    }
}

/// Nonzero while a doubling is still draining the old table.
///
/// A measurement taken here is a measurement of a migration in
/// progress, which is a real state to be in and not the steady one, so
/// a phase that ends with this set is worth reporting differently.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_index_resizing(db: *const Zu2Db) -> u32 {
    match unsafe { handle(db) } {
        Some(handle) => u32::from(handle.db.index_resizing()),
        None => 0,
    }
}

/// Log pages holding memory right now, each 4 MiB.
///
/// The memory side of the space column. `zu2_disk_bytes` says what the
/// filesystem is holding and this says what the process is, and a
/// comparison that reports one without the other is picking whichever
/// number reads better.
///
/// # Safety
/// `db` is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_resident_pages(db: *const Zu2Db) -> u64 {
    match unsafe { handle(db) } {
        Some(handle) => handle.db.resident_pages() as u64,
        None => 0,
    }
}

/// The library version.
///
/// # Safety
/// `len` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zu2_version(len: *mut usize) -> *const c_char {
    const VERSION: &CStr =
        match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
            Ok(text) => text,
            Err(_) => panic!("version string holds a NUL"),
        };
    if !len.is_null() {
        unsafe { len.write(VERSION.to_bytes().len()) };
    }
    VERSION.as_ptr()
}
