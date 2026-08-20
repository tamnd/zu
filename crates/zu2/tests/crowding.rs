//! A bucket that fills does not lose the ninth key, it chains it.
//!
//! `index.rs` spends none of its eight slots on an overflow pointer.
//! When all eight are taken the arriving record takes an entry over and
//! points its `previous` at whatever that entry was holding, so the
//! collision chain lives in the log beside the version chain. The
//! displaced key is then no longer named by its own tag, which is what
//! the foreign bit is for, and a lookup that reaches the bucket walks
//! the foreign entries whether the tag matches or not.
//!
//! The visible consequence is that `index_occupancy` stops counting a
//! key once it has been displaced, so a small table under a large key
//! set reports fewer entries than there are keys. That is an accounting
//! statement about the index and not a statement about the data, and
//! these tests are what say so: every key still reads back, and a
//! delete of a displaced key still takes only that key.

use zu2::{Db, Durability, Options};

fn options(buckets: usize) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: buckets,
        max_pages: 256,
        max_nodes: 1 << 16,
        // Off, so what is measured is the index and not what a
        // compaction happened to have rewritten.
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

#[test]
fn a_table_smaller_than_the_key_set_keeps_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    // 5001 is what scripts/bench-engine.sh in go-ycsb asks for at
    // 20000 records, rounded up to 8192 buckets, and 20000 keys over
    // 8192 buckets is a mean of 2.44 with a tail that reaches nine.
    let db = Db::create(&path, options(5001)).expect("create");
    let mut s = db.session();

    const N: u32 = 20000;
    for i in 0..N {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }

    let mut out = Vec::new();
    for i in 0..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, value(i), "key {i} read back the wrong value");
    }

    // The count is allowed to be short. It is not allowed to be short
    // by much, because a table this size should only crowd its tail.
    let occupancy = db.index_occupancy();
    assert!(
        occupancy <= N as usize,
        "occupancy {occupancy} is above the {N} keys inserted"
    );
    assert!(
        occupancy > N as usize - 100,
        "occupancy {occupancy} is {} short of {N}, which is more than crowding explains",
        N as usize - occupancy
    );
}

#[test]
fn one_bucket_holds_a_key_set_that_cannot_fit_in_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    // One bucket, so every key lands in it and all but eight of them
    // are displaced. The chain is the whole index at this point.
    let db = Db::create(&path, options(1)).expect("create");
    let mut s = db.session();

    const N: u32 = 200;
    for i in 0..N {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }

    let mut out = Vec::new();
    for i in 0..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, value(i), "key {i} read back the wrong value");
    }

    assert_eq!(
        db.index_occupancy(),
        zu2::index::SLOTS,
        "one bucket has eight slots and they should all be taken"
    );
}

#[test]
fn a_delete_under_crowding_takes_only_its_own_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let db = Db::create(&path, options(1)).expect("create");
    let mut s = db.session();

    const N: u32 = 200;
    for i in 0..N {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }

    // Key 0 went in first, so by now it is as deep in the chain as any
    // key gets.
    assert!(s.delete(&key(0)).expect("delete"));

    let mut out = Vec::new();
    assert!(!s.read(&key(0), &mut out).expect("read"), "key 0 came back");
    for i in 1..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, value(i), "key {i} read back the wrong value");
    }
}

#[test]
fn an_update_under_crowding_is_seen_by_the_next_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let db = Db::create(&path, options(1)).expect("create");
    let mut s = db.session();

    const N: u32 = 200;
    for i in 0..N {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    for i in 0..N {
        s.upsert(&key(i), b"second").expect("upsert");
    }

    let mut out = Vec::new();
    for i in 0..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, b"second", "key {i} kept its first value");
    }
}
