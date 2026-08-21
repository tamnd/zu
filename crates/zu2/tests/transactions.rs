//! Multi-statement transactions: a group of writes that lands all at
//! once or not at all.
//!
//! What the log gives on its own is atomicity and durability, so that is
//! what these check: nothing is visible before the commit, everything is
//! visible after it, a dropped transaction leaves no trace, and a reopen
//! sees the same thing the writer saw. Isolation is not claimed and is
//! not tested for, because claiming it would need a version to read at
//! and a validation at commit.

use std::sync::atomic::Ordering;

use zu2::{Db, Durability, Options};

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 12,
        max_pages: 64,
        max_nodes: 1 << 12,
        compact_below: 0,
        ..Options::default()
    }
}

fn read(s: &mut zu2::Session<'_>, key: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    s.read(key, &mut out).expect("read").then_some(out)
}

#[test]
fn a_committed_group_is_all_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut s = db.session();
    let mut t = s.transaction();
    for i in 0..64u32 {
        t.upsert(format!("k{i:04}").as_bytes(), b"one")
            .expect("stage");
    }
    assert_eq!(t.len(), 64);
    t.commit().expect("commit");

    for i in 0..64u32 {
        assert_eq!(
            read(&mut s, format!("k{i:04}").as_bytes()).as_deref(),
            Some(b"one".as_slice()),
            "key {i} did not land"
        );
    }
}

#[test]
fn a_dropped_transaction_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut s = db.session();
    s.upsert(b"kept", b"before").expect("upsert");
    let before = db.log_bytes();
    {
        let mut t = s.transaction();
        t.upsert(b"gone", b"never").expect("stage");
        t.upsert(b"kept", b"never").expect("stage");
    }
    assert_eq!(
        db.log_bytes(),
        before,
        "a transaction that never committed put bytes on the log"
    );
    assert_eq!(read(&mut s, b"gone"), None);
    assert_eq!(read(&mut s, b"kept").as_deref(), Some(b"before".as_slice()));
}

/// The write set answers before the index does, so a transaction sees
/// what it has staged and a key staged twice is one record.
#[test]
fn a_transaction_reads_its_own_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut s = db.session();
    s.upsert(b"k", b"old").expect("upsert");

    let mut out = Vec::new();
    let mut t = s.transaction();
    assert!(t.read(b"k", &mut out).expect("read"));
    assert_eq!(out, b"old");
    t.upsert(b"k", b"new").expect("stage");
    assert!(t.read(b"k", &mut out).expect("read"));
    assert_eq!(out, b"new");
    t.upsert(b"k", b"newer").expect("stage");
    assert_eq!(t.len(), 1, "a key staged twice should be one record");
    t.delete(b"k").expect("stage");
    assert!(!t.read(b"k", &mut out).expect("read"));
    assert_eq!(t.len(), 1);
    t.commit().expect("commit");

    assert_eq!(read(&mut s, b"k"), None, "the last write of the group wins");
}

/// A delete inside a group, over a key another session put there.
#[test]
fn a_group_can_remove_a_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut s = db.session();
    for i in 0..32u32 {
        s.upsert(format!("k{i:04}").as_bytes(), b"one")
            .expect("upsert");
    }
    let mut t = s.transaction();
    for i in (0..32u32).step_by(2) {
        t.delete(format!("k{i:04}").as_bytes()).expect("stage");
    }
    t.commit().expect("commit");

    for i in 0..32u32 {
        let got = read(&mut s, format!("k{i:04}").as_bytes());
        if i % 2 == 0 {
            assert_eq!(got, None, "key {i} survived the group's delete");
        } else {
            assert_eq!(got.as_deref(), Some(b"one".as_slice()), "key {i}");
        }
    }
}

/// Nothing a transaction has staged is in the index, so a second session
/// sees the group appear at the commit and not before it.
#[test]
fn another_session_sees_the_group_only_once_it_commits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut watcher = db.session();
    let mut writer = db.session();
    let mut t = writer.transaction();
    t.upsert(b"a", b"one").expect("stage");
    t.upsert(b"b", b"two").expect("stage");
    assert_eq!(read(&mut watcher, b"a"), None, "staged, not committed");
    assert_eq!(read(&mut watcher, b"b"), None);
    t.commit().expect("commit");
    assert_eq!(read(&mut watcher, b"a").as_deref(), Some(b"one".as_slice()));
    assert_eq!(read(&mut watcher, b"b").as_deref(), Some(b"two".as_slice()));
}

/// The group has to come back off the file, both ways in: a scan reads
/// the markers and the records itself, and a checkpoint restores an
/// index the commit already went into.
#[test]
fn a_committed_group_survives_a_reopen() {
    for checkpoint in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.zu2");
        {
            let db = Db::create(
                &path,
                Options {
                    checkpoint_on_close: checkpoint,
                    ..options()
                },
            )
            .expect("create");
            let mut s = db.session();
            s.upsert(b"outside", b"plain").expect("upsert");
            let mut t = s.transaction();
            for i in 0..128u32 {
                t.upsert(format!("k{i:04}").as_bytes(), b"grouped")
                    .expect("stage");
            }
            t.commit().expect("commit");
            drop(s);
            db.sync().expect("sync");
        }

        let db = Db::open(&path, options()).expect("open");
        let mut s = db.session();
        assert_eq!(
            read(&mut s, b"outside").as_deref(),
            Some(b"plain".as_slice()),
            "with checkpoint {checkpoint}"
        );
        for i in 0..128u32 {
            assert_eq!(
                read(&mut s, format!("k{i:04}").as_bytes()).as_deref(),
                Some(b"grouped".as_slice()),
                "key {i} did not survive a reopen with checkpoint {checkpoint}"
            );
        }
    }
}

/// A group that a compaction pass has been over. The copies are ordinary
/// records rather than provisional ones, so a reopen after a pass has to
/// find them without any marker being left to release them.
#[test]
fn a_compacted_group_survives_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.zu2");
    // Enough rounds to put several pages below the read-only boundary,
    // since a pass can only take what is under it and one that finds
    // nothing there would leave the test proving nothing (#435).
    let rounds = 48u32;
    {
        let db = Db::create(
            &path,
            Options {
                checkpoint_on_close: false,
                mutable_pages: 1,
                ..options()
            },
        )
        .expect("create");
        let mut s = db.session();
        let mut t = s.transaction();
        for i in 0..64u32 {
            t.upsert(format!("k{i:04}").as_bytes(), &[b'g'; 500])
                .expect("stage");
        }
        t.commit().expect("commit");
        for round in 0..rounds {
            // A different length each round, so the write goes on the
            // tail rather than in place over the record before it.
            let value = vec![b'h'; 1000 + (round % 2) as usize];
            for i in 0..1000u32 {
                s.upsert(format!("hot{i:04}").as_bytes(), &value)
                    .expect("upsert");
            }
        }
        // A durable write, so the region a pass wants is on the device
        // rather than still in the log's tail buffer.
        s.set_durability(Durability::Durable);
        s.upsert(b"flush", b"x").expect("flush");
        let written = db.log_bytes();
        // One page of mutable window, which is what this database was
        // opened with.
        let window = 4u64 << 20;
        assert!(
            written > window * 4,
            "the rounds did not make enough log to compact: {written} against a {window} byte window"
        );
        while db.compact().expect("compact") > 0 {}
        // Either half counts: a survivor goes to the tail or to the cold
        // tier, and both go through the copy that rewrites the kind.
        let stats = db.compaction();
        let carried = stats.copied.load(Ordering::Relaxed) + stats.migrated.load(Ordering::Relaxed);
        assert!(
            carried > 0,
            "no pass carried anything forward, so the test proves nothing"
        );
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut s = db.session();
    for i in 0..64u32 {
        assert_eq!(
            read(&mut s, format!("k{i:04}").as_bytes()).as_deref(),
            Some([b'g'; 500].as_slice()),
            "key {i} was lost by a pass over the group"
        );
    }
}

/// An empty transaction is a no-op rather than a marker pair on the log.
#[test]
fn an_empty_group_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("t.zu2"), options()).expect("create");
    let mut s = db.session();
    s.upsert(b"k", b"v").expect("upsert");
    let before = db.log_bytes();
    let t = s.transaction();
    assert!(t.is_empty());
    t.commit().expect("commit");
    assert_eq!(db.log_bytes(), before);
}
