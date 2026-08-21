//! The C surface driven the way a host drives it: raw pointers, out
//! parameters, and a status on every call.
//!
//! These go through the extern functions rather than through `zu2`
//! directly, because what is being tested is the boundary. A test that
//! called `Db::session` would pass whether or not the handle casting,
//! the lifetime transmute and the buffer ownership were right, and
//! those three are the whole of what this crate adds.

use std::ffi::{CStr, c_int};
use std::ptr;

use zu2::{Zu2Db, Zu2Options, Zu2Session, Zu2Status};

/// Opens a database in a fresh directory and hands back the handle plus
/// the directory, which has to stay alive as long as the handle does.
fn open(name: &str) -> (tempfile::TempDir, *mut Zu2Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let path = path.to_str().expect("utf8 path").to_owned();
    let mut options = Zu2Options::default();
    assert_eq!(
        unsafe { zu2::zu2_options_init(&mut options) },
        Zu2Status::Ok
    );
    // Async, because these tests are about the boundary and not about
    // the device, and compaction off so nothing moves under them.
    options.durability = 0;
    options.compact_below = u64::MAX;
    options.max_nodes = 1 << 16;
    let mut db: *mut Zu2Db = ptr::null_mut();
    let mut err: *const std::ffi::c_char = ptr::null();
    let mut err_len = 0usize;
    let status = unsafe {
        zu2::zu2_open(
            path.as_ptr() as *const std::ffi::c_char,
            path.len(),
            &options,
            &mut db,
            &mut err,
            &mut err_len,
        )
    };
    assert_eq!(status, Zu2Status::Ok, "open failed: {}", message(err));
    assert!(!db.is_null());
    (dir, db)
}

fn message(err: *const std::ffi::c_char) -> String {
    if err.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned()
}

fn session_on(db: *mut Zu2Db) -> *mut Zu2Session {
    let mut s: *mut Zu2Session = ptr::null_mut();
    assert_eq!(unsafe { zu2::zu2_session_open(db, &mut s) }, Zu2Status::Ok);
    assert!(!s.is_null());
    s
}

fn upsert(s: *mut Zu2Session, key: &[u8], value: &[u8]) {
    let status =
        unsafe { zu2::zu2_upsert(s, key.as_ptr(), key.len(), value.as_ptr(), value.len()) };
    assert_eq!(status, Zu2Status::Ok);
}

/// Reads a key and copies the answer out, which is what every host has
/// to do: the buffer belongs to the session until the next call.
fn read(s: *mut Zu2Session, key: &[u8]) -> Option<Vec<u8>> {
    let mut value: *const u8 = ptr::null();
    let mut len = 0usize;
    let mut found: c_int = 0;
    let status =
        unsafe { zu2::zu2_read(s, key.as_ptr(), key.len(), &mut value, &mut len, &mut found) };
    assert_eq!(status, Zu2Status::Ok);
    if found == 0 {
        assert!(value.is_null());
        assert_eq!(len, 0);
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(value, len) }.to_vec())
}

fn node(s: *mut Zu2Session, key: &[u8]) -> u32 {
    let mut id = u32::MAX;
    let status = unsafe { zu2::zu2_add_node(s, key.as_ptr(), key.len(), &mut id) };
    assert_eq!(status, Zu2Status::Ok);
    id
}

fn edge(s: *mut Zu2Session, src: u32, dst: u32) {
    assert_eq!(unsafe { zu2::zu2_add_edge(s, src, dst) }, Zu2Status::Ok);
}

fn khop(s: *mut Zu2Session, dir: c_int, seed: u32, k: u32) -> Vec<u32> {
    let mut out: *const u32 = ptr::null();
    let mut len = 0usize;
    let status = unsafe { zu2::zu2_khop(s, dir, seed, k, &mut out, &mut len) };
    assert_eq!(status, Zu2Status::Ok);
    let mut answer = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(out, len) }.to_vec()
    };
    answer.sort_unstable();
    answer
}

/// Sorted, because what is being checked is the set that came back and
/// not the order breadth first happened to reach it in. The one place
/// the order matters says so itself.
fn reach(s: *mut Zu2Session, dir: c_int, seed: u32, depth: u32, cap: u64) -> Vec<u32> {
    let mut out: *const u32 = ptr::null();
    let mut len = 0usize;
    let status = unsafe { zu2::zu2_reach(s, dir, seed, depth, cap, &mut out, &mut len) };
    assert_eq!(status, Zu2Status::Ok);
    let mut answer = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(out, len) }.to_vec()
    };
    answer.sort_unstable();
    answer
}

fn shortest(s: *mut Zu2Session, dir: c_int, src: u32, dst: u32, depth: u32) -> Option<u32> {
    let mut hops = 0u32;
    let mut found = 0 as c_int;
    let status = unsafe { zu2::zu2_shortest(s, dir, src, dst, depth, &mut hops, &mut found) };
    assert_eq!(status, Zu2Status::Ok);
    (found != 0).then_some(hops)
}

fn close(db: *mut Zu2Db, sessions: &[*mut Zu2Session]) {
    for &s in sessions {
        unsafe { zu2::zu2_session_close(s) };
    }
    unsafe { zu2::zu2_close(db) };
}

#[test]
fn a_value_survives_the_boundary() {
    let (_dir, db) = open("records.zu2");
    let s = session_on(db);
    assert_eq!(read(s, b"absent"), None);
    upsert(s, b"user1", b"field0=a");
    assert_eq!(read(s, b"user1").as_deref(), Some(&b"field0=a"[..]));
    upsert(s, b"user1", b"field0=b");
    assert_eq!(read(s, b"user1").as_deref(), Some(&b"field0=b"[..]));
    let mut existed: c_int = 0;
    assert_eq!(
        unsafe { zu2::zu2_delete(s, b"user1".as_ptr(), 5, &mut existed) },
        Zu2Status::Ok
    );
    assert_eq!(existed, 1);
    assert_eq!(read(s, b"user1"), None);
    close(db, &[s]);
}

/// An empty key and an empty value are values, not mistakes. A host
/// with a zero length string sends NULL for the pointer often enough
/// that treating that as misuse would be a bug report a week.
#[test]
fn empty_is_a_value_and_not_an_error() {
    let (_dir, db) = open("empty.zu2");
    let s = session_on(db);
    let status = unsafe { zu2::zu2_upsert(s, ptr::null(), 0, ptr::null(), 0) };
    assert_eq!(status, Zu2Status::Ok);
    assert_eq!(read(s, b""), Some(Vec::new()));
    close(db, &[s]);
}

/// A batch crosses once and comes back with every record in it. The
/// count matters as much as the records: a host that got fewer than it
/// sent has to know where to start again.
#[test]
fn a_batch_writes_every_pair_and_says_how_many() {
    let (_dir, db) = open("batch.zu2");
    let s = session_on(db);
    const N: usize = 64;
    let keys: Vec<String> = (0..N).map(|i| format!("user{i}")).collect();
    let values: Vec<String> = (0..N).map(|i| format!("value{i}")).collect();
    let pairs: Vec<zu2::Zu2Pair> = keys
        .iter()
        .zip(&values)
        .map(|(k, v)| zu2::Zu2Pair {
            key: k.as_ptr(),
            key_len: k.len(),
            value: v.as_ptr(),
            value_len: v.len(),
        })
        .collect();
    let mut written = 0usize;
    let status = unsafe { zu2::zu2_upsert_many(s, pairs.as_ptr(), pairs.len(), &mut written) };
    assert_eq!(status, Zu2Status::Ok);
    assert_eq!(written, N);
    for (key, value) in keys.iter().zip(&values) {
        assert_eq!(read(s, key.as_bytes()), Some(value.as_bytes().to_vec()));
    }

    // An empty batch is a call with nothing to do, not a mistake, which
    // is what a host that loops over a partly filled buffer sends on
    // the last turn.
    written = 7;
    assert_eq!(
        unsafe { zu2::zu2_upsert_many(s, ptr::null(), 0, &mut written) },
        Zu2Status::Ok
    );
    assert_eq!(written, 0);
    close(db, &[s]);
}

/// The same divergence the options struct can have, on the struct a
/// batch is made of: a host reads the header's layout and the library
/// reads its own, and neither fails to compile when they disagree.
#[test]
fn the_header_and_the_pair_struct_declare_the_same_fields() {
    const FIELDS: &[&str] = &["key", "key_len", "value", "value_len"];
    let header = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/include/zu2.h"))
        .expect("the header ships with the crate");
    let body = header
        .split_once("typedef struct zu2_pair {")
        .expect("the header declares zu2_pair")
        .1
        .split_once("} zu2_pair;")
        .expect("the declaration is closed")
        .0;
    let found: Vec<&str> = body
        .split(';')
        .filter_map(|line| line.split_whitespace().last())
        .map(|name| name.trim_start_matches('*'))
        .collect();
    assert_eq!(found, FIELDS);
    // Four pointer sized fields, no padding on any target that has a
    // pointer the width of its size_t, which is every one this builds
    // for.
    assert_eq!(
        std::mem::size_of::<zu2::Zu2Pair>(),
        4 * std::mem::size_of::<usize>()
    );
}

/// A bad pointer anywhere in a batch is caught before the first record
/// is written, so the load a host has to unpick is the one it sent and
/// not a prefix of it.
#[test]
fn a_batch_with_a_bad_entry_writes_nothing() {
    let (_dir, db) = open("batch-misuse.zu2");
    let s = session_on(db);
    let good = zu2::Zu2Pair {
        key: b"first".as_ptr(),
        key_len: 5,
        value: b"v".as_ptr(),
        value_len: 1,
    };
    // NULL with a length is the one shape bytes() refuses: NULL with
    // zero is an empty value and a host sends it all the time.
    let bad = zu2::Zu2Pair {
        key: ptr::null(),
        key_len: 4,
        value: b"v".as_ptr(),
        value_len: 1,
    };
    let pairs = [good, bad];
    let mut written = 9usize;
    assert_eq!(
        unsafe { zu2::zu2_upsert_many(s, pairs.as_ptr(), pairs.len(), &mut written) },
        Zu2Status::Misuse
    );
    assert_eq!(written, 0);
    assert_eq!(read(s, b"first"), None, "the good entry went in with it");

    // And a NULL array with a count is misuse rather than a walk off
    // the front of nothing.
    assert_eq!(
        unsafe { zu2::zu2_upsert_many(s, ptr::null(), 3, &mut written) },
        Zu2Status::Misuse
    );
    assert_eq!(
        unsafe { zu2::zu2_upsert_many(ptr::null_mut(), pairs.as_ptr(), 1, ptr::null_mut()) },
        Zu2Status::Misuse
    );
    close(db, &[s]);
}

/// The contract is that the value buffer is good until the next call,
/// and this is the test that would catch it being good for less: the
/// second read has to have replaced the first one's bytes rather than
/// freed them.
#[test]
fn a_value_buffer_lasts_exactly_until_the_next_call() {
    let (_dir, db) = open("buffer.zu2");
    let s = session_on(db);
    upsert(s, b"a", b"aaaaaaaa");
    upsert(s, b"b", b"bbbb");
    let mut value: *const u8 = ptr::null();
    let mut len = 0usize;
    let mut found: c_int = 0;
    unsafe { zu2::zu2_read(s, b"a".as_ptr(), 1, &mut value, &mut len, &mut found) };
    assert_eq!(
        unsafe { std::slice::from_raw_parts(value, len) },
        b"aaaaaaaa"
    );
    unsafe { zu2::zu2_read(s, b"b".as_ptr(), 1, &mut value, &mut len, &mut found) };
    assert_eq!(unsafe { std::slice::from_raw_parts(value, len) }, b"bbbb");
    close(db, &[s]);
}

#[test]
fn out_parameters_are_written_on_the_failing_paths() {
    // A NULL session is misuse and writes nothing but the zeroes.
    let mut value: *const u8 = b"stale".as_ptr();
    let mut len = 7usize;
    let mut found: c_int = 9;
    let status = unsafe {
        zu2::zu2_read(
            ptr::null_mut(),
            b"k".as_ptr(),
            1,
            &mut value,
            &mut len,
            &mut found,
        )
    };
    assert_eq!(status, Zu2Status::Misuse);
    assert!(value.is_null());
    assert_eq!(len, 0);
    assert_eq!(found, 0);

    // A direction that is neither 0 nor 1 is misuse rather than a
    // silent Out, because a host that got the constant wrong wants to
    // hear about it.
    let (_dir, db) = open("misuse.zu2");
    let s = session_on(db);
    let mut degree = 5u32;
    assert_eq!(
        unsafe { zu2::zu2_degree(s, 7, 0, &mut degree) },
        Zu2Status::Misuse
    );
    assert_eq!(degree, 0);
    close(db, &[s]);
}

#[test]
fn a_bad_path_reports_through_the_open_error() {
    let mut db: *mut Zu2Db = ptr::null_mut();
    let mut err: *const std::ffi::c_char = ptr::null();
    let mut err_len = 0usize;
    let path = "/this/directory/does/not/exist/db.zu2";
    let status = unsafe {
        zu2::zu2_open(
            path.as_ptr() as *const std::ffi::c_char,
            path.len(),
            ptr::null(),
            &mut db,
            &mut err,
            &mut err_len,
        )
    };
    assert_eq!(status, Zu2Status::Error);
    assert!(db.is_null());
    assert!(err_len > 0);
    assert_eq!(message(err).len(), err_len);
}

#[test]
fn the_graph_walks() {
    let (_dir, db) = open("graph.zu2");
    let s = session_on(db);
    // A path 0 -> 1 -> 2 -> 3 with a shortcut 0 -> 2, so the one hop
    // and the two hop frontiers are not the same set and a walk that
    // confused levels with reachability would show it.
    let ids: Vec<u32> = (0..4)
        .map(|i| node(s, format!("v{i}").as_bytes()))
        .collect();
    edge(s, ids[0], ids[1]);
    edge(s, ids[1], ids[2]);
    edge(s, ids[2], ids[3]);
    edge(s, ids[0], ids[2]);

    let mut degree = 0u32;
    assert_eq!(
        unsafe { zu2::zu2_degree(s, 0, ids[0], &mut degree) },
        Zu2Status::Ok
    );
    assert_eq!(degree, 2);
    assert_eq!(
        unsafe { zu2::zu2_degree(s, 1, ids[2], &mut degree) },
        Zu2Status::Ok
    );
    assert_eq!(degree, 2);

    let mut out: *const u32 = ptr::null();
    let mut len = 0usize;
    assert_eq!(
        unsafe { zu2::zu2_neighbours(s, 0, ids[0], &mut out, &mut len) },
        Zu2Status::Ok
    );
    let mut neighbours = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
    neighbours.sort_unstable();
    assert_eq!(neighbours, vec![ids[1], ids[2]]);

    assert_eq!(khop(s, 0, ids[0], 0), vec![ids[0]]);
    assert_eq!(khop(s, 0, ids[0], 1), vec![ids[1], ids[2]]);
    // Two hops from 0 reaches 2 (through 1) and 3 (through 2). That 2
    // is also one hop away does not take it out of the two hop
    // frontier, which is the level-distinct rule the header states.
    assert_eq!(khop(s, 0, ids[0], 2), vec![ids[2], ids[3]]);
    assert_eq!(khop(s, 0, ids[0], 3), vec![ids[3]]);
    assert_eq!(khop(s, 0, ids[0], 4), Vec::<u32>::new());
    assert_eq!(khop(s, 1, ids[3], 1), vec![ids[2]]);

    // 0 -> 1 -> 2 closes because 0 -> 2 is there, and nothing else does.
    let mut triangles = 0u64;
    assert_eq!(
        unsafe { zu2::zu2_triangles(s, ids[0], &mut triangles) },
        Zu2Status::Ok
    );
    assert_eq!(triangles, 1);

    assert_eq!(unsafe { zu2::zu2_nodes(db) }, 4);
    close(db, &[s]);
}

#[test]
fn reach_walks_the_component_and_stops_where_it_is_told() {
    let (_dir, db) = open("reach.zu2");
    let s = session_on(db);
    let ids: Vec<u32> = (0..6)
        .map(|i| node(s, format!("v{i}").as_bytes()))
        .collect();
    for pair in ids.windows(2) {
        edge(s, pair[0], pair[1]);
    }
    // Five, not six: the seed is where the walk started and no edge
    // leads back to it, so it is not something the walk reached.
    assert_eq!(reach(s, 0, ids[0], 0, 0).len(), 5);
    // The depth bound is the `*1..k` in a pattern.
    assert_eq!(reach(s, 0, ids[0], 1, 0), vec![ids[1]]);
    assert_eq!(reach(s, 0, ids[0], 3, 0), vec![ids[1], ids[2], ids[3]]);
    // The size bound is a bound on what is collected, not a suggestion.
    let capped = reach(s, 0, ids[0], 0, 2);
    assert_eq!(capped, vec![ids[1], ids[2]]);
    // A second walk has to see a clean bitmap, which is the part that
    // breaks when the clear is skipped after an early stop.
    assert_eq!(reach(s, 0, ids[0], 0, 0).len(), 5);
    // Backwards from the far end is the same chain read the other way.
    assert_eq!(reach(s, 1, ids[5], 0, 0).len(), 5);
    // Undirected from the middle is both halves and the seed as well,
    // because an edge with no arrow on it can be walked back along and
    // two hops of that arrive where they started. That is a walk over
    // an undirected graph rather than Cypher's rule that one path may
    // not use one relationship twice, and the header says so.
    assert_eq!(reach(s, 2, ids[3], 1, 0), vec![ids[2], ids[4]]);
    assert_eq!(reach(s, 2, ids[3], 2, 0).len(), 5);
    assert_eq!(reach(s, 2, ids[3], 0, 0).len(), 6);
    close(db, &[s]);
}

#[test]
fn a_cycle_puts_the_seed_back_in_its_own_answer() {
    let (_dir, db) = open("cycle.zu2");
    let s = session_on(db);
    let ids: Vec<u32> = (0..3)
        .map(|i| node(s, format!("v{i}").as_bytes()))
        .collect();
    edge(s, ids[0], ids[1]);
    edge(s, ids[1], ids[2]);
    edge(s, ids[2], ids[0]);
    // Three hops round a triangle arrive back where they started, and
    // that is a node reachable in one to three hops.
    assert_eq!(reach(s, 0, ids[0], 3, 0).len(), 3);
    assert_eq!(reach(s, 0, ids[0], 2, 0).len(), 2);
    close(db, &[s]);
}

#[test]
fn shortest_counts_hops_and_says_when_there_are_none() {
    let (_dir, db) = open("shortest.zu2");
    let s = session_on(db);
    let ids: Vec<u32> = (0..6)
        .map(|i| node(s, format!("v{i}").as_bytes()))
        .collect();
    // A chain 0..4 with a shortcut 0 -> 3, and 5 off on its own.
    for pair in ids[..5].windows(2) {
        edge(s, pair[0], pair[1]);
    }
    edge(s, ids[0], ids[3]);

    assert_eq!(shortest(s, 0, ids[0], ids[0], 0), Some(0));
    assert_eq!(shortest(s, 0, ids[0], ids[1], 0), Some(1));
    // Two, by the shortcut, not three the long way round.
    assert_eq!(shortest(s, 0, ids[0], ids[4], 0), Some(2));
    assert_eq!(shortest(s, 0, ids[4], ids[0], 0), None);
    // The other way round the same edges, which is the undirected
    // question a `-[:EDGE*]-` pattern asks.
    assert_eq!(shortest(s, 1, ids[4], ids[0], 0), Some(2));
    assert_eq!(shortest(s, 2, ids[4], ids[0], 0), Some(2));
    // No path at all, and no path within the bound, report the same
    // thing, which is why a caller who cares passes no bound.
    assert_eq!(shortest(s, 2, ids[0], ids[5], 0), None);
    assert_eq!(shortest(s, 0, ids[0], ids[4], 1), None);
    assert_eq!(shortest(s, 0, ids[0], ids[4], 2), Some(2));
    // The bitmap is clean after a walk that stopped early on arrival.
    assert_eq!(shortest(s, 0, ids[0], ids[4], 0), Some(2));
    close(db, &[s]);
}

#[test]
fn removing_an_edge_takes_it_out_of_the_walk() {
    let (_dir, db) = open("remove.zu2");
    let s = session_on(db);
    let a = node(s, b"a");
    let b = node(s, b"b");
    let c = node(s, b"c");
    edge(s, a, b);
    edge(s, a, c);
    assert_eq!(khop(s, 0, a, 1), vec![b, c]);
    assert_eq!(unsafe { zu2::zu2_remove_edge(s, a, b) }, Zu2Status::Ok);
    assert_eq!(khop(s, 0, a, 1), vec![c]);
    // Removing what is not there is not an error, the same way a delete
    // of an absent key is not.
    assert_eq!(unsafe { zu2::zu2_remove_edge(s, a, b) }, Zu2Status::Ok);
    close(db, &[s]);
}

/// A node looked up by key comes back as the id it was created with,
/// which is what a loader does on its second pass over an edge list.
#[test]
fn a_node_is_found_by_its_key() {
    let (_dir, db) = open("keys.zu2");
    let s = session_on(db);
    let a = node(s, b"alice");
    let mut id = u32::MAX;
    let mut found: c_int = 0;
    assert_eq!(
        unsafe { zu2::zu2_node_of(s, b"alice".as_ptr(), 5, &mut id, &mut found) },
        Zu2Status::Ok
    );
    assert_eq!((found, id), (1, a));
    let mut missing = 7u32;
    let mut found: c_int = 1;
    assert_eq!(
        unsafe { zu2::zu2_node_of(s, b"bob".as_ptr(), 3, &mut missing, &mut found) },
        Zu2Status::Ok
    );
    assert_eq!((found, missing), (0, 0));
    close(db, &[s]);
}

/// A host that asks for more sessions than it said it would gets told
/// so. It used to abort the process: `Db::session` panicked when the
/// epoch table was full, and a panic crossing `extern "C"` is an abort,
/// so go-ycsb at a threadcount above the default 128 died with no
/// message at all.
#[test]
fn a_session_past_the_count_is_refused_and_not_fatal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sessions.zu2");
    let path = path.to_str().expect("utf8 path").to_owned();
    let mut options = Zu2Options::default();
    assert_eq!(
        unsafe { zu2::zu2_options_init(&mut options) },
        Zu2Status::Ok
    );
    options.durability = 0;
    options.compact_below = u64::MAX;
    options.sessions = 2;
    let mut db: *mut Zu2Db = ptr::null_mut();
    let status = unsafe {
        zu2::zu2_open(
            path.as_ptr() as *const std::ffi::c_char,
            path.len(),
            &options,
            &mut db,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(status, Zu2Status::Ok);
    let first = session_on(db);
    let second = session_on(db);
    let mut third: *mut Zu2Session = ptr::null_mut();
    assert_eq!(
        unsafe { zu2::zu2_session_open(db, &mut third) },
        Zu2Status::NoSessions
    );
    assert!(third.is_null(), "a refused open still wrote a pointer");
    // The two that were opened still work, and the refusal did not cost
    // the db a reference either.
    upsert(first, b"k", b"v");
    assert_eq!(read(second, b"k").as_deref(), Some(&b"v"[..]));
    // And a slot comes back when its session closes.
    unsafe { zu2::zu2_session_close(first) };
    let again = session_on(db);
    close(db, &[second, again]);
}

/// Two sessions on one database see each other's writes, which is the
/// arrangement the whole API is shaped around: one db, a session per
/// thread.
#[test]
fn sessions_share_the_database() {
    let (_dir, db) = open("shared.zu2");
    let one = session_on(db);
    let two = session_on(db);
    upsert(one, b"k", b"v");
    assert_eq!(read(two, b"k").as_deref(), Some(&b"v"[..]));
    let a = node(one, b"a");
    let b = node(two, b"b");
    edge(one, a, b);
    assert_eq!(khop(two, 0, a, 1), vec![b]);
    close(db, &[one, two]);
}

/// Closing the db before its sessions is not the usual order and is not
/// fatal: the engine goes away with the last handle that holds it.
#[test]
fn a_session_outlives_a_closed_database() {
    let (_dir, db) = open("outlive.zu2");
    let s = session_on(db);
    upsert(s, b"k", b"v");
    unsafe { zu2::zu2_close(db) };
    assert_eq!(read(s, b"k").as_deref(), Some(&b"v"[..]));
    unsafe { zu2::zu2_session_close(s) };
}

/// A session moves between threads. It must not be in two calls at
/// once, which is a different promise, and one this test does not make.
#[test]
fn a_session_moves_between_threads() {
    let (_dir, db) = open("threads.zu2");
    let s = session_on(db);
    upsert(s, b"k", b"v");
    let address = s as usize;
    let handle = std::thread::spawn(move || {
        let s = address as *mut Zu2Session;
        assert_eq!(read(s, b"k").as_deref(), Some(&b"v"[..]));
        upsert(s, b"k2", b"v2");
    });
    handle.join().expect("worker");
    assert_eq!(read(s, b"k2").as_deref(), Some(&b"v2"[..]));
    close(db, &[s]);
}

/// What a database says about itself, which is what the storage table
/// in 07-benchmarks.md is read out of.
#[test]
fn the_database_reports_its_size() {
    let (_dir, db) = open("size.zu2");
    let s = session_on(db);
    for i in 0..1000u32 {
        upsert(s, format!("key{i}").as_bytes(), &[b'x'; 100]);
    }
    assert_eq!(unsafe { zu2::zu2_sync(db) }, Zu2Status::Ok);
    let mut bytes = 0u64;
    assert_eq!(
        unsafe { zu2::zu2_disk_bytes(db, &mut bytes) },
        Zu2Status::Ok
    );
    assert!(
        bytes > 0,
        "a database with a thousand records occupies something"
    );
    // Not compared against each other on purpose. The log writes whole
    // 4 MiB pages, so a young database occupies a page and has spent a
    // fraction of it, and the device number is the larger of the two
    // until the file is bigger than its rounding.
    assert!(unsafe { zu2::zu2_log_bytes(db) } > 0);
    assert!(unsafe { zu2::zu2_log_span(db) } > 0);
    assert_eq!(unsafe { zu2::zu2_index_occupancy(db) }, 1000);
    let mut reclaimed = u64::MAX;
    assert_eq!(
        unsafe { zu2::zu2_compact(db, &mut reclaimed) },
        Zu2Status::Ok
    );
    assert_ne!(reclaimed, u64::MAX);
    // Everything is still readable after a compaction pass, which is
    // the only property of compaction this crate is responsible for.
    assert_eq!(read(s, b"key42").as_deref(), Some(&[b'x'; 100][..]));
    close(db, &[s]);
}

#[test]
fn durability_is_settable_per_session() {
    let (_dir, db) = open("durable.zu2");
    let s = session_on(db);
    assert_eq!(unsafe { zu2::zu2_set_durability(s, 1) }, Zu2Status::Ok);
    upsert(s, b"k", b"v");
    assert_eq!(unsafe { zu2::zu2_set_durability(s, 0) }, Zu2Status::Ok);
    upsert(s, b"k2", b"v2");
    assert_eq!(unsafe { zu2::zu2_set_durability(s, 4) }, Zu2Status::Misuse);
    close(db, &[s]);
}

#[test]
fn the_version_is_the_crate_version() {
    let mut len = 0usize;
    let text = unsafe { zu2::zu2_version(&mut len) };
    assert!(!text.is_null());
    assert_eq!(
        unsafe { CStr::from_ptr(text) }.to_str().expect("utf8"),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(len, env!("CARGO_PKG_VERSION").len());
}

/// A concurrent edge write must not turn a path into no path.
///
/// The walk marks every node it discovers so a frontier stays distinct,
/// and the arrival test sat inside that guard. A neighbour read is a
/// seqlock, so a writer that bumps a version while the closure is
/// running makes the engine run the closure again, and the second run
/// meets its own bit on the destination, skips the arrival test and
/// reports no path for a pair one hop apart.
///
/// The writer never touches the edge being looked for. It adds and
/// removes an unrelated one on the same node, which is all it takes to
/// move the version under the reader.
#[test]
fn a_concurrent_edge_write_does_not_hide_a_path() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_dir, db) = open("shortest-race.zu2");
    let reader = session_on(db);
    let writer = session_on(db);

    let src = node(reader, b"src");
    // A wide neighbourhood with the destination at the end of it, so
    // the closure spends its whole walk inside the window the writer
    // has to land in. A two element list would make this test pass by
    // being too fast to interrupt rather than by being correct.
    for i in 0..512u32 {
        let id = node(reader, format!("filler{i}").as_bytes());
        edge(reader, src, id);
    }
    let dst = node(reader, b"dst");
    edge(reader, src, dst);
    let churn = node(reader, b"churn");

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let address = writer as usize;
    let hand = std::thread::spawn(move || {
        let w = address as *mut Zu2Session;
        while !flag.load(Ordering::Relaxed) {
            assert_eq!(unsafe { zu2::zu2_add_edge(w, src, churn) }, Zu2Status::Ok);
            assert_eq!(
                unsafe { zu2::zu2_remove_edge(w, src, churn) },
                Zu2Status::Ok
            );
        }
    });

    let mut missed = 0;
    for _ in 0..20_000 {
        if shortest(reader, 0, src, dst, 0) != Some(1) {
            missed += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    hand.join().expect("writer");
    assert_eq!(missed, 0, "the walk lost a one hop path {missed} times");
    close(db, &[reader, writer]);
}

/// The header and the struct are two declarations of one layout and
/// nothing was checking that they agree. A field added to one and not
/// the other does not fail to compile on either side: the host reads
/// its own header, the library reads its own struct, and every field
/// past the divergence is silently the wrong one. Adding `fixed_index`
/// is what made that concrete, since it is the first field appended
/// since the struct was written.
///
/// The names are listed here rather than derived because there is
/// nothing to derive them from on the Rust side without a macro, and a
/// list that has to be edited in two places is exactly the thing under
/// test. Editing it in three is the point: the third one fails loudly.
#[test]
fn the_header_and_the_options_struct_declare_the_same_fields() {
    const FIELDS: &[(&str, &str)] = &[
        ("zu2_durability", "durability"),
        ("uint64_t", "index_buckets"),
        ("uint64_t", "max_pages"),
        ("uint64_t", "max_nodes"),
        ("uint32_t", "space_target_percent"),
        ("uint64_t", "compact_below"),
        ("uint64_t", "sessions"),
        ("uint32_t", "fixed_index"),
        ("uint32_t", "salvage"),
    ];
    let header = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/include/zu2.h"))
        .expect("the header ships with the crate");
    let body = header
        .split_once("typedef struct zu2_options {")
        .expect("the header declares zu2_options")
        .1
        .split_once("} zu2_options;")
        .expect("the declaration is closed")
        .0;
    // Comments carry prose with semicolons and braces in it, so they go
    // before anything is split on either.
    let mut code = String::new();
    let mut rest = body;
    while let Some((before, after)) = rest.split_once("/*") {
        code.push_str(before);
        rest = after.split_once("*/").expect("an unclosed comment").1;
    }
    code.push_str(rest);
    let found: Vec<(String, String)> = code
        .split(';')
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let ty = words.next()?;
            let name = words.next()?;
            Some((ty.to_owned(), name.to_owned()))
        })
        .collect();
    let want: Vec<(String, String)> = FIELDS
        .iter()
        .map(|(t, n)| ((*t).to_owned(), (*n).to_owned()))
        .collect();
    assert_eq!(
        found, want,
        "the header's zu2_options is not the struct zu2-capi compiled against"
    );

    // And the offsets, which is what catches a type changed on one
    // side rather than a field added to one side. Both languages lay a
    // repr(C) struct out the same way, so walking the header's types
    // gives the offsets the struct has to have, padding and all.
    let mut offset = 0usize;
    let mut align = 1usize;
    let mut offsets = Vec::new();
    for (ty, name) in FIELDS {
        let size = match *ty {
            "uint64_t" => 8,
            "uint32_t" | "zu2_durability" => 4,
            other => panic!("the header uses {other}, which this test has no size for"),
        };
        offset = offset.next_multiple_of(size);
        offsets.push((*name, offset));
        offset += size;
        align = align.max(size);
    }
    let expect = offset.next_multiple_of(align);
    assert_eq!(
        std::mem::size_of::<Zu2Options>(),
        expect,
        "the struct is {} bytes and the header lays out {expect}",
        std::mem::size_of::<Zu2Options>()
    );
    assert_eq!(std::mem::align_of::<Zu2Options>(), align);
    for (name, want) in offsets {
        let got = match name {
            "durability" => std::mem::offset_of!(Zu2Options, durability),
            "index_buckets" => std::mem::offset_of!(Zu2Options, index_buckets),
            "max_pages" => std::mem::offset_of!(Zu2Options, max_pages),
            "max_nodes" => std::mem::offset_of!(Zu2Options, max_nodes),
            "space_target_percent" => std::mem::offset_of!(Zu2Options, space_target_percent),
            "compact_below" => std::mem::offset_of!(Zu2Options, compact_below),
            "sessions" => std::mem::offset_of!(Zu2Options, sessions),
            "fixed_index" => std::mem::offset_of!(Zu2Options, fixed_index),
            "salvage" => std::mem::offset_of!(Zu2Options, salvage),
            other => panic!("{other} is in the header and this test does not know it"),
        };
        assert_eq!(
            got, want,
            "{name} is at {got} in the struct and {want} in the header"
        );
    }
}
