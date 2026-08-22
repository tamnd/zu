//! A write that was acknowledged as durable survives the process being
//! killed outright.
//!
//! tests/damage.rs takes a file apart by hand and asks what a reopen
//! makes of it, which is the right way to test a hole in the middle of a
//! log and no way at all to test the tail: what a crash leaves at the
//! tail is whatever the flusher had got through, and only a real crash
//! writes that. So this one kills a real process mid write.
//!
//! The child is this same test binary run again with `ZU2_CRASH_CHILD`
//! set, which is the usual trick for a test that needs a second process
//! and does not want a second crate to build. It writes durably in a
//! loop and prints each key the moment `upsert` returns, so what the
//! parent has read off the pipe is a list of writes the engine said were
//! on the device. Then it is killed with no warning, and every one of
//! those keys has to come back.
//!
//! `ZU2_CRASH_MS` moves where the kill lands. It is worth walking over a
//! range rather than trusting one number: #570 only showed up between
//! 550 and 700 ms, which at these page counts is where the first
//! compaction passes fall.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use zu2::{Db, Durability, Options};

/// Small enough that the child laps the log several times a second and
/// the index doubles from one bucket, so the kill has a fair chance of
/// landing in the middle of a compaction pass rather than always in the
/// middle of a plain append. Eight pages and not two: at two a durable
/// write costs a device flush apiece and the child gets a few hundred
/// writes out rather than tens of thousands, which is #585 and which
/// leaves the kill landing in the same place every time.
///
/// `ordered` maintains the key order alongside the hash index, which is
/// a second structure the crash has to leave recoverable, so both are
/// run.
fn options(ordered: bool) -> Options {
    Options {
        durability: Durability::Durable,
        index_buckets: 1,
        max_pages: 8,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        compact_below: 1 << 20,
        ordered,
        ..Options::default()
    }
}

/// How many distinct keys the child cycles over. A bounded live set is
/// what lets the log lap: a run that only ever writes new keys fills it
/// instead, and then there is nothing for a pass to reclaim.
const KEYS: u64 = 8000;

fn key(i: u64) -> Vec<u8> {
    format!("key{:016}", i % KEYS).into_bytes()
}

/// Wide enough that the log laps every forty thousand writes or so at
/// these page counts.
fn value(i: u64) -> Vec<u8> {
    let mut v = format!("value{i:016}").into_bytes();
    v.resize(200, b'v');
    v
}

/// Which write a value came from, read back out of it.
fn round(value: &[u8]) -> u64 {
    std::str::from_utf8(&value[5..21])
        .expect("value is ascii")
        .parse()
        .expect("value carries its round")
}

/// How many keys a transaction writes at once, and how many groups of
/// that size the transaction child cycles over. Small enough that a
/// commit is quick and there are many of them before the kill, wide
/// enough that a torn one would be obvious.
const GROUP: u64 = 16;
const GROUPS: u64 = 400;

/// How many nodes the graph child cycles over. Small, so it laps and
/// the hub's neighbourhood block doubles several times before the kill.
const NODES: u64 = 512;

/// The key `j` of group `g`. Laid out so a group is contiguous in the
/// key order and so the groups together cover the same sort of key space
/// the other children use.
fn member(g: u64, j: u64) -> Vec<u8> {
    format!("key{:016}", g * GROUP + j).into_bytes()
}

/// The child. Returns without doing anything when it is the parent's
/// own run of this test, which is how one binary is both.
#[test]
fn writes_until_it_is_killed() {
    let Some(path) = std::env::var_os("ZU2_CRASH_CHILD") else {
        return;
    };
    let ordered = std::env::var_os("ZU2_CRASH_ORDERED").is_some();
    let db = Db::create(std::path::Path::new(&path), options(ordered)).expect("create");
    let mut session = db.session();
    if std::env::var_os("ZU2_CRASH_TXN").is_some() {
        // A group at a time, all of it or none of it. `t` is printed
        // after `commit` returns, so a `t` the parent read is a
        // transaction the engine said was on the device.
        for t in 0..=u64::MAX {
            let g = t % GROUPS;
            let mut txn = session.transaction();
            for j in 0..GROUP {
                txn.upsert(&member(g, j), &value(t)).expect("stage");
            }
            txn.commit().expect("commit");
            println!("{t}");
        }
    }
    if std::env::var_os("ZU2_CRASH_GRAPH").is_some() {
        // A node and its two edges at a time, printed after the second
        // edge returns. The ring edge keeps every neighbourhood small
        // and the hub edge makes one of them double over and over,
        // which are the two shapes the block has.
        for i in 0..NODES {
            let id = session.add_node(&key(i)).expect("node");
            assert_eq!(id, i as u32, "ids are handed out in creation order");
        }
        for i in 0..=u64::MAX {
            let n = i % NODES;
            session
                .add_edge(n as u32, ((n + 1) % NODES) as u32)
                .expect("ring edge");
            session.add_edge(0, n as u32).expect("hub edge");
            println!("{i}");
        }
    }
    // Until it is killed, which at these rates is somewhere in the first
    // few hundred thousand.
    for i in 0..=u64::MAX {
        session.upsert(&key(i), &value(i)).expect("upsert");
        // After the write returned, so the parent only ever counts on
        // what the engine has already acknowledged.
        println!("{i}");
    }
}

/// Runs a child, kills it, and gives back every write the parent saw
/// acknowledged in the order it saw them, along with the path it was
/// killed on. `modes` are the environment variables that pick which
/// child this is, and none of them is the plain one.
fn kill_a_writer(dir: &std::path::Path, modes: &[&str]) -> (std::path::PathBuf, Vec<u64>) {
    let path = dir.join("c.zu2");
    let mut command = Command::new(std::env::current_exe().expect("current exe"));
    command
        .args(["--exact", "writes_until_it_is_killed", "--nocapture"])
        .env("ZU2_CRASH_CHILD", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for mode in modes {
        command.env(mode, "1");
    }
    let mut child = command.spawn().expect("spawn");

    // On a thread, because the pipe has to be drained while the child
    // runs: a child that fills it would block in `println` and stop
    // writing, and then the kill would land on an idle process.
    let (sender, receiver) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout");
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(i) = line.trim().parse::<u64>() {
                let _ = sender.send(i);
            }
        }
    });

    // Where in the child's life the kill lands decides what it is caught
    // in the middle of: a page still being filled, a flush, a pass.
    let delay: u64 = std::env::var("ZU2_CRASH_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    std::thread::sleep(Duration::from_millis(delay));
    child.kill().expect("kill");
    child.wait().expect("wait");
    reader.join().expect("reader");

    let mut acknowledged = Vec::new();
    while let Ok(i) = receiver.try_recv() {
        acknowledged.push(i);
    }
    assert!(
        acknowledged.len() > 16,
        "the child only got {} writes out before it was killed",
        acknowledged.len()
    );

    // The last line can be half a line, since the kill lands wherever it
    // lands, and a partial line is not an acknowledgement.
    acknowledged.pop();
    println!("{} acknowledged writes before the kill", acknowledged.len());
    (path, acknowledged)
}

/// The last write to each key the parent saw acknowledged, in key order,
/// which is the order a scan gives them back in. It is not the order
/// they were written in: the last cycle over the key space is a partial
/// one, so the low keys carry a later write than the high ones do.
///
/// How far into the key space the child got depends on the machine and
/// on where the kill landed, so what comes back is however much of it
/// the child covered rather than all of it. The child writes the keys in
/// order, so the keys covered are the first `n` of them and the live set
/// is exactly those, which is what lets the scan test still know what it
/// is looking at.
fn last_per_key(mut acknowledged: Vec<u64>, keys: u64) -> Vec<u64> {
    // Walking backwards keeps the first sighting of a key, which is the
    // last write to it.
    let mut seen = HashSet::new();
    acknowledged.reverse();
    acknowledged.retain(|i| seen.insert(i % keys));
    // Enough of them that the run is worth something. A child that got
    // this far has lapped the log and doubled the index whatever the
    // machine was doing at the time.
    let enough = (keys / 2).min(512);
    assert!(
        acknowledged.len() as u64 >= enough,
        "the child only covered {} keys of {keys} before the kill, and {enough} is the least this is worth running on",
        acknowledged.len()
    );
    acknowledged.sort_by_key(|i| i % keys);
    // Contiguous from zero, since the child writes them in order, and
    // the tests below read that into the live set.
    for (n, i) in acknowledged.iter().enumerate() {
        assert_eq!(i % keys, n as u64, "the child skipped a key");
    }
    acknowledged
}

/// Whether a value found under a key is one the crash was allowed to
/// leave there, given the last write to that key the parent saw
/// acknowledged.
///
/// Not `value(i)` exactly: the parent drops the last line as possibly
/// half a line, and the child can get another write in between its last
/// flush to the pipe and the kill. A later write to the same key is a
/// write nobody acknowledged and is allowed to be there or not, so the
/// contract is that what is under the key is write `i` or something
/// after it, never anything before it.
fn allowed(i: u64, found: &[u8]) -> bool {
    let got = round(found);
    got >= i && got % KEYS == i % KEYS
}

#[test]
fn a_durable_write_survives_a_kill() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, acknowledged) = kill_a_writer(dir.path(), &[]);
    let acknowledged = last_per_key(acknowledged, KEYS);

    let db = Db::open(&path, options(false)).expect("reopen");
    let mut session = db.session();
    let mut out = Vec::new();
    for i in &acknowledged {
        out.clear();
        assert!(
            session.read(&key(*i), &mut out).expect("read"),
            "write {i} was acknowledged as durable and its key is not there after the kill"
        );
        assert!(
            allowed(*i, &out),
            "write {i} was acknowledged as durable and key {} came back holding write {}",
            i % KEYS,
            round(&out)
        );
    }
}

/// The same kill, with the key order maintained as well, and the reopen
/// asked for the whole of it in one scan rather than key by key. A
/// recovery that rebuilt the hash index and left the order behind reads
/// perfectly well one key at a time and cannot answer this.
#[test]
fn an_ordered_database_survives_a_kill() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, acknowledged) = kill_a_writer(dir.path(), &["ZU2_CRASH_ORDERED"]);
    let acknowledged = last_per_key(acknowledged, KEYS);

    let db = Db::open(&path, options(true)).expect("reopen");
    let mut session = db.session();
    let mut seen = Vec::new();
    session
        .scan(&key(0), KEYS as usize * 2, |k, v| {
            seen.push((k.to_vec(), v.to_vec()))
        })
        .expect("scan");
    // More than was acknowledged is allowed and fewer is not. The pipe
    // is a pipe rather than a terminal, so the child's lines go out a
    // block at a time and the writes that were in the buffer when the
    // kill landed are writes that happened and were never acknowledged.
    // They are allowed to be there, and since the child writes the keys
    // in order they are the keys just past the ones that were.
    assert!(
        seen.len() >= acknowledged.len(),
        "the scan gave back {} keys and {} of them were acknowledged",
        seen.len(),
        acknowledged.len()
    );
    for (i, (got_key, got_value)) in acknowledged.iter().zip(&seen) {
        assert_eq!(
            got_key.as_slice(),
            key(*i).as_slice(),
            "the scan is out of order or skipped a key after the kill"
        );
        assert!(
            allowed(*i, got_value),
            "write {i} was acknowledged as durable and the scan gave back write {}",
            round(got_value)
        );
    }
}

/// A transaction is all of it or none of it, and a kill is the only way
/// to find out whether that holds when the answer is decided by what is
/// on the device rather than by what is in memory.
///
/// tests/transactions.rs makes the same case against a reopen after a
/// clean close, which proves the markers are written and read. It cannot
/// prove anything about a group the process died in the middle of, and
/// that is the group that matters: a record goes into the index before
/// the transaction it belongs to has committed, and what makes that safe
/// is that the index does not outlive the crash.
#[test]
fn a_transaction_is_all_of_it_or_none_of_it_after_a_kill() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, acknowledged) = kill_a_writer(dir.path(), &["ZU2_CRASH_TXN"]);
    let acknowledged = last_per_key(acknowledged, GROUPS);

    let db = Db::open(&path, options(false)).expect("reopen");
    let mut session = db.session();
    let mut out = Vec::new();
    for t in &acknowledged {
        let g = t % GROUPS;
        let mut group = None;
        for j in 0..GROUP {
            out.clear();
            assert!(
                session.read(&member(g, j), &mut out).expect("read"),
                "transaction {t} committed and key {j} of group {g} is not there after the kill"
            );
            let got = round(&out);
            // Every member carries the transaction that wrote it, so a
            // group holding two different numbers is a torn commit, and
            // it is torn whether or not either number is one the parent
            // saw acknowledged.
            let group = *group.get_or_insert(got);
            assert_eq!(
                got, group,
                "group {g} came back torn after the kill: key 0 holds {group} and key {j} holds {got}"
            );
            // And the group as a whole is the transaction the parent saw
            // acknowledged or a later one, never an earlier one, which
            // is the durability half.
            assert!(
                got >= *t && got % GROUPS == g,
                "transaction {t} committed and group {g} came back holding {got}"
            );
        }
    }
}

/// The same transaction child with the key order kept as well, read back
/// through a scan.
///
/// A scan answers from the skip list beside the hash index and a read
/// answers from the index, so a recovery that dropped a provisional
/// record from one and not the other passes the test above and gives a
/// torn group back here. The two structures are filled by different code
/// on the way in and rebuilt by different code on the way back, and the
/// only thing that says they agree about a group the process died inside
/// is asking both.
#[test]
fn a_transaction_is_all_of_it_or_none_of_it_in_the_key_order_too() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, acknowledged) = kill_a_writer(dir.path(), &["ZU2_CRASH_TXN", "ZU2_CRASH_ORDERED"]);
    let acknowledged = last_per_key(acknowledged, GROUPS);

    let db = Db::open(&path, options(true)).expect("reopen");
    let mut session = db.session();
    let mut seen = Vec::new();
    session
        .scan(&member(0, 0), (GROUPS * GROUP) as usize * 2, |k, v| {
            seen.push((k.to_vec(), round(v)))
        })
        .expect("scan");
    // Every key of every acknowledged group, in key order, since the
    // groups are laid out contiguously and the child writes them in
    // order. Groups past the ones acknowledged are allowed to be there.
    let wanted = acknowledged.len() * GROUP as usize;
    assert!(
        seen.len() >= wanted,
        "the scan gave back {} keys and {wanted} of them were in acknowledged groups",
        seen.len()
    );
    for (t, group) in acknowledged.iter().zip(seen.chunks(GROUP as usize)) {
        let g = t % GROUPS;
        let round = group[0].1;
        for (j, (got_key, got)) in group.iter().enumerate() {
            assert_eq!(
                got_key.as_slice(),
                member(g, j as u64).as_slice(),
                "the scan is out of order or skipped a key after the kill"
            );
            assert_eq!(
                *got, round,
                "group {g} came back torn in the key order: key 0 holds {round} and key {j} holds {got}"
            );
        }
        assert!(
            round >= *t && round % GROUPS == g,
            "transaction {t} committed and the key order gave back {round} for group {g}"
        );
    }
}

/// The graph plane after a kill.
///
/// An edge is not a record with a key. It is a bit in a neighbourhood
/// block that another edge to the same node rewrites, and the block
/// doubles in place as a node collects neighbours, so what a crash can
/// leave behind is a half doubled block rather than a half written
/// value. The reopen rebuilds the whole plane from the log, and nothing
/// so far has asked it to do that over a log a crash ended in the
/// middle of.
#[test]
fn a_graph_survives_a_kill_with_its_edges() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, acknowledged) = kill_a_writer(dir.path(), &["ZU2_CRASH_GRAPH"]);
    let acknowledged = last_per_key(acknowledged, NODES);

    let db = Db::open(&path, options(false)).expect("reopen");
    let mut session = db.session();
    let mut scratch = Vec::new();
    assert_eq!(
        db.core().graph().nodes(),
        NODES as u32,
        "the id counter did not survive the kill"
    );
    let hub = session.neighbours(zu2::Direction::Out, 0, |n| n.to_vec());
    for i in &acknowledged {
        let n = (i % NODES) as u32;
        assert_eq!(
            session
                .node_of(&key(u64::from(n)), &mut scratch)
                .expect("node_of"),
            Some(n),
            "the key of node {n} did not survive the kill"
        );
        // The ring edge, which is the one written first, so an
        // acknowledged round means both of that round's edges landed.
        let out = session.neighbours(zu2::Direction::Out, n, |v| v.to_vec());
        let next = (n + 1) % NODES as u32;
        assert!(
            out.contains(&next),
            "node {n} acknowledged its ring edge and came back without it"
        );
        assert!(
            out.windows(2).all(|w| w[0] < w[1]),
            "node {n} came back with its neighbours out of order"
        );
        // And the reverse, which is what an in-hop reads.
        let back = session.neighbours(zu2::Direction::In, next, |v| v.to_vec());
        assert!(
            back.contains(&n),
            "node {n} acknowledged its ring edge and the reverse is gone"
        );
        assert!(
            hub.contains(&n) || n == 0,
            "node {n} acknowledged its hub edge and the hub came back without it"
        );
    }
}
