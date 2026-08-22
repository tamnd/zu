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

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use zu2::{Db, Durability, Options};

/// Small enough that the child laps the log several times a second and
/// the index doubles from one bucket, so the kill has a fair chance of
/// landing in the middle of a compaction pass rather than always in the
/// middle of a plain append.
fn options() -> Options {
    Options {
        durability: Durability::Durable,
        index_buckets: 1,
        max_pages: 2,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        compact_below: 1 << 20,
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

/// The child. Returns without doing anything when it is the parent's
/// own run of this test, which is how one binary is both.
#[test]
fn writes_until_it_is_killed() {
    let Some(path) = std::env::var_os("ZU2_CRASH_CHILD") else {
        return;
    };
    let db = Db::create(std::path::Path::new(&path), options()).expect("create");
    let mut session = db.session();
    // Until it is killed, which at these rates is somewhere in the first
    // few hundred thousand.
    for i in 0..=u64::MAX {
        session.upsert(&key(i), &value(i)).expect("upsert");
        // After the write returned, so the parent only ever counts on
        // what the engine has already acknowledged.
        println!("{i}");
    }
}

#[test]
fn a_durable_write_survives_a_kill() {
    if std::env::var_os("ZU2_CRASH_CHILD").is_some() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.zu2");
    let mut child = Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", "writes_until_it_is_killed", "--nocapture"])
        .env("ZU2_CRASH_CHILD", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

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
    // in the middle of: a page still being filled, a flush, a checkpoint.
    // `ZU2_CRASH_MS` is how a soak walks it over all of them.
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

    // The child cycles over `KEYS` keys, so what matters for each one is
    // the last write to it the parent saw acknowledged. Walking backwards
    // keeps the first sighting of a key, which is that write.
    let mut seen = std::collections::HashSet::new();
    acknowledged.reverse();
    acknowledged.retain(|i| seen.insert(i % KEYS));
    println!(
        "{} acknowledged writes checked after the kill",
        acknowledged.len()
    );
    assert!(
        acknowledged.len() as u64 == KEYS,
        "the child did not get through a full cycle of the key space, only {} of {KEYS}",
        acknowledged.len()
    );
    let last = acknowledged.iter().copied().max().unwrap_or(0);
    let db = Db::open(&path, options()).expect("reopen");
    let mut session = db.session();
    let mut out = Vec::new();
    let mut ahead = 0;
    for i in &acknowledged {
        out.clear();
        assert!(
            session.read(&key(*i), &mut out).expect("read"),
            "write {i} was acknowledged as durable and its key is not there after the kill"
        );
        // Not `value(i)` exactly: the child's stdout is a pipe and so is
        // block buffered, so it had gone on writing past the last line
        // the parent got to read. A later write to the same key is a
        // write that was never acknowledged and is allowed either way, so
        // the contract is that what is there is write `i` or something
        // after it, never anything before it.
        let got = round(&out);
        assert!(
            got >= *i && got % KEYS == i % KEYS,
            "write {i} was acknowledged as durable and key {} came back holding write {got}",
            i % KEYS
        );
        ahead = ahead.max(got.saturating_sub(last));
    }
    println!("furthest unacknowledged write that survived: {ahead} past the last line read");
}
