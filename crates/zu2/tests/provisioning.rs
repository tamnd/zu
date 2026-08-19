//! The write path provisions the file ahead of itself, and gives the
//! reservation back before anyone looks at it.
//!
//! A durable commit is a write and a barrier, and the barrier costs more
//! when the write also grew the file, because then the filesystem has an
//! inode size and an extent to commit as well as the bytes. The log
//! keeps a megabyte allocated past its frontier so that the common
//! commit writes into blocks the file already owns.
//!
//! That reservation is not data. These tests are about the two places it
//! has to be invisible: what the database reports as its size, and what a
//! reopen does with a file that is longer than its log.

use std::fs::OpenOptions;

use zu2::{Db, Durability, Options};

/// A megabyte, which is what `log.rs` provisions ahead of the frontier.
const CHUNK: u64 = 1 << 20;

fn options(durability: Durability) -> Options {
    Options {
        durability,
        index_buckets: 1 << 10,
        max_pages: 64,
        max_vertices: 1 << 16,
        // Compaction off, so the file's length is the write path's doing
        // and nothing else's.
        compact_below: 0,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).expect("metadata").len()
}

#[test]
fn a_durable_commit_leaves_the_file_provisioned_past_the_tail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let db = Db::create(&path, options(Durability::Durable)).expect("create");
    let mut s = db.session();
    for i in 0..100u32 {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    let tail = db.log_bytes();
    let len = file_len(&path);
    assert!(
        len >= CHUNK && len > tail,
        "the file is {len} bytes with a tail at {tail}, so nothing was provisioned"
    );
}

#[test]
fn the_reported_size_does_not_include_the_reservation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let db = Db::create(&path, options(Durability::Durable)).expect("create");
    let mut s = db.session();
    for i in 0..100u32 {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    let reserved = file_len(&path);
    let bytes = db.disk_bytes().expect("disk bytes");
    let tail = db.log_bytes();
    assert!(
        file_len(&path) < reserved,
        "the reservation was not given back"
    );
    assert!(
        file_len(&path) >= tail,
        "the file was cut below its own tail"
    );
    assert!(
        bytes < tail + CHUNK,
        "reported {bytes} bytes for a log of {tail}, which is the reservation counted as data"
    );
    // And the next commit provisions again rather than growing per write
    // for the rest of the run.
    for i in 100..200u32 {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    assert!(
        file_len(&path) >= CHUNK && file_len(&path) > db.log_bytes(),
        "the log did not provision again after the trim"
    );
}

#[test]
fn a_closed_database_keeps_no_reservation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let tail;
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..100u32 {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        tail = db.log_bytes();
    }
    let closed = file_len(&path);
    assert!(closed >= tail, "the file is shorter than its log");
    assert!(
        closed < tail + CHUNK,
        "closed at {closed} bytes for a log of {tail}, so the reservation outlived the database"
    );
}

#[test]
fn a_reopen_appends_at_the_tail_and_not_after_the_reservation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let tail;
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..100u32 {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        tail = db.log_bytes();
    }
    // What a crash mid-run leaves behind: a file whose length is the
    // reservation rather than the log, here two pages of it so the
    // recovery scan has to cross a page boundary of zeros to get it
    // wrong.
    let handle = OpenOptions::new().write(true).open(&path).expect("open");
    handle.set_len(tail + 8 * 1024 * 1024).expect("extend");
    drop(handle);

    let db = Db::open(&path, options(Durability::Durable)).expect("reopen");
    assert_eq!(
        db.log_bytes(),
        tail,
        "recovery put the tail past the records rather than after them"
    );
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..100u32 {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out, value(i), "wrong value for {i}");
    }
    s.upsert(&key(1000), &value(1000)).expect("upsert");
    assert!(
        db.log_bytes() < tail + 8 * 1024 * 1024,
        "the append landed past the reservation, leaving a hole in the log"
    );
    assert!(
        s.read(&key(1000), &mut out).expect("read"),
        "lost the append"
    );
}
