//! The table doubling while the database is being used.
//!
//! A resize is the one thing in the record plane that replaces a
//! structure rather than adding to it, and it does it under readers and
//! writers that hold no lock. The protocol is in `index.rs`: the
//! migration is published before the new table, so an operation that
//! saw the new table also sees the migration; the grower waits for the
//! operations that were already running before anything migrates; and
//! an operation that arrives at a migration that is not open yet leaves
//! its epoch and comes back rather than waiting inside it, which is the
//! difference between a wait and a deadlock.
//!
//! These tests are the protocol from the outside: nothing is lost,
//! nothing comes back stale, and the table really did double while the
//! traffic was running rather than after it stopped.

use std::sync::Arc;

use zu2::{Db, Durability, Options};

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

/// One bucket to start with, so the table has to double all the way up
/// from eight slots while the workers are writing into it.
fn tiny() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1,
        max_pages: 64,
        max_nodes: 1 << 10,
        compact_below: 0,
        ..Options::default()
    }
}

#[test]
fn a_table_that_doubles_under_writers_keeps_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::create(&dir.path().join("r.zu2"), tiny()).expect("create"));
    let threads = 4u32;
    let per_thread = 4000u32;
    let mut handles = Vec::new();
    for t in 0..threads {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut s = db.session();
            for i in 0..per_thread {
                let k = key(t * per_thread + i);
                s.upsert(&k, &value(i)).expect("upsert");
                // Reading what this thread just wrote, while the others
                // are writing and the table is moving under all of them.
                let mut out = Vec::new();
                assert!(s.read(&k, &mut out).expect("read"), "lost {k:?}");
                assert_eq!(out, value(i), "{k:?} came back wrong");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }

    assert!(
        db.index_grows() >= 8,
        "the table only doubled {} times, so this proves little",
        db.index_grows()
    );
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..threads * per_thread {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out, value(i % per_thread), "key {i} has the wrong value");
    }
}

/// A reader that is inside a bucket when the table is replaced has to
/// come out with an answer and not with a hole. Half the workers here
/// only read, so they spend the whole run arriving at buckets that are
/// mid migration.
#[test]
fn a_reader_across_a_doubling_never_misses_a_key_that_is_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::create(&dir.path().join("d.zu2"), tiny()).expect("create"));
    let seeded = 2000u32;
    {
        let mut s = db.session();
        for i in 0..seeded {
            s.upsert(&key(i), &value(i)).expect("seed");
        }
    }

    let mut handles = Vec::new();
    for t in 0..2u32 {
        // Writers, pushing the table through several more doublings.
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut s = db.session();
            for i in 0..8000u32 {
                let k = key(seeded + t * 8000 + i);
                s.upsert(&k, &value(i)).expect("upsert");
            }
        }));
    }
    for _ in 0..2 {
        // Readers, over and over, on keys that were all there before
        // any of this started.
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut s = db.session();
            let mut out = Vec::new();
            for round in 0..10 {
                for i in 0..seeded {
                    assert!(
                        s.read(&key(i), &mut out).expect("read"),
                        "key {i} disappeared during a doubling, round {round}"
                    );
                    assert_eq!(out, value(i), "key {i} came back wrong");
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    assert!(db.index_grows() > 0, "the table never grew");
}

/// A doubling has to leave the table naming every key once. It cannot
/// give a key an entry on both sides of a split, because a lookup takes
/// the first slot whose chain holds the key and the older of two entries
/// would then win as often as not (#454).
#[test]
fn a_doubling_gives_a_key_one_entry_and_not_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("o.zu2"), tiny()).expect("create");
    let keys = 6000u32;
    {
        let mut s = db.session();
        for i in 0..keys {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        // Second and third versions, so the chains the split walks hold
        // more than one record per key.
        for i in (0..keys).step_by(2) {
            s.upsert(&key(i), &value(i + keys)).expect("update");
        }
        for i in (0..keys).step_by(5) {
            s.upsert(&key(i), &value(i + 2 * keys)).expect("update");
        }
    }
    // The flusher drains whatever the traffic left and doubles again if
    // that leaves the table over half full, so the count only means
    // something once it has stopped moving.
    let mut occupancy = db.index_occupancy();
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let now = db.index_occupancy();
        if now == occupancy && !db.index_resizing() {
            break;
        }
        occupancy = now;
    }
    assert!(
        occupancy <= keys as usize,
        "{occupancy} entries for {keys} keys, so a split named one of them twice"
    );
    // And not so few that the keys are sitting in chains rather than in
    // entries of their own, which is what the doubling is for.
    assert!(
        occupancy > keys as usize - keys as usize / 10,
        "{occupancy} entries for {keys} keys, so the split left them crowded"
    );
}

/// Issue #537: a load whose whole key set fits inside the mutable
/// window gives the log no page to flush, and the doubling check only
/// runs on the maintenance thread. This is what says whether the table
/// still catches up, and how far behind it is when the load stops.
#[test]
fn a_load_that_never_flushes_still_doubles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = Options {
        durability: Durability::Async,
        index_buckets: 1,
        // Room for the whole load, so no page ever seals and the log
        // never wakes the maintainer with work of its own.
        max_pages: 1 << 12,
        max_nodes: 1 << 16,
        compact_below: 0,
        ..Options::default()
    };
    let db = Db::create(&dir.path().join("m.zu2"), options).expect("create");
    let keys = 60000u32;
    {
        let mut s = db.session();
        for i in 0..keys {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
    }
    let straight_after = db.index_buckets();
    let mut buckets = straight_after;
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let now = db.index_buckets();
        if now == buckets && !db.index_resizing() {
            break;
        }
        buckets = now;
    }
    // 60000 keys over eight slots a bucket wants at least 8192 buckets
    // to stay under the load factor.
    assert!(
        buckets >= 8192,
        "{buckets} buckets for {keys} keys, so the table never doubled"
    );
    assert!(
        straight_after >= 8192,
        "{straight_after} buckets when the load returned, {buckets} once the \
         maintainer caught up, so the doubling is behind the writer"
    );
}
