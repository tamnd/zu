//! What a checkpoint owes the run that reads it.
//!
//! Recovery without one reads every record in the file. A checkpoint is
//! the two planes written down at a log address, so the reopen installs
//! them and replays only what was written above that address. The
//! promise is therefore exactly the promise the scan makes, and these
//! are the shapes of it: everything the run before acknowledged comes
//! back, whether it was below the boundary, above it, or written while
//! the capture was running.
//!
//! The other half is what happens when the checkpoint cannot be used. A
//! compaction pass since the capture, a pinned index of another size, a
//! file that is not the file the checkpoint was taken of: every one of
//! them falls back to the scan, and the test for each is that the keys
//! are all still there and the scan is what found them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use zu2::{Db, Durability, Options};

const N: u32 = 20_000;

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 13,
        max_pages: 64,
        max_nodes: 1 << 16,
        // Off, so the file a reopen reads is the one the writes left.
        // Compaction has a test of its own below.
        compact_below: 0,
        ..Options::default()
    }
}

/// Fills a database and closes it, which takes the checkpoint.
fn written(path: &std::path::Path, options: Options) {
    let db = Db::create(path, options).expect("create");
    let mut session = db.session();
    for i in 0..N {
        session.upsert(&key(i), &value(i)).expect("upsert");
    }
    drop(session);
    db.sync().expect("sync");
}

/// Where a database's checkpoint lives.
fn sidecar(path: &std::path::Path) -> std::path::PathBuf {
    let mut beside = path.to_path_buf().into_os_string();
    beside.push(".ckpt");
    std::path::PathBuf::from(beside)
}

/// Reads every key back and says how many were missing or wrong.
fn checked(db: &Db) -> u32 {
    let mut session = db.session();
    let mut out = Vec::new();
    let mut wrong = 0;
    for i in 0..N {
        if !session.read(&key(i), &mut out).expect("read") || out != value(i) {
            wrong += 1;
        }
    }
    wrong
}

/// The whole point. A database that closed cleanly comes back without
/// its log being read, and the record count is what says so: nothing was
/// written above the boundary, so the reopen walked nothing.
#[test]
fn a_clean_close_reopens_without_reading_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("clean.zu2");
    written(&path, options());

    let db = Db::open(&path, options()).expect("reopen");
    assert_eq!(
        db.recovered().records.load(Ordering::Relaxed),
        0,
        "the reopen walked records, so the checkpoint was not used"
    );
    assert_eq!(checked(&db), 0, "the checkpoint lost keys");
}

/// The other half of a prefix: what was written after the capture is
/// replayed on top of it. The close takes no checkpoint of its own here,
/// so what the reopen has is the one the run took by hand and the tail
/// above it, which is the shape a crash leaves.
#[test]
fn what_was_written_after_the_capture_is_replayed_on_top_of_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tail.zu2");
    let no_close = Options {
        checkpoint_on_close: false,
        ..options()
    };
    let after = 1_000u32;
    {
        let db = Db::create(&path, no_close).expect("create");
        let mut session = db.session();
        for i in 0..N - after {
            session.upsert(&key(i), &value(i)).expect("upsert");
        }
        drop(session);
        let taken = db.checkpoint().expect("checkpoint");
        assert!(taken.entries > 0, "the capture wrote an empty index");

        let mut session = db.session();
        for i in N - after..N {
            session.upsert(&key(i), &value(i)).expect("upsert");
        }
        // And a second version of a key from below the boundary, which
        // is the case that says the replay wins over the checkpoint
        // rather than the other way round.
        session.upsert(&key(0), b"newer").expect("update");
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, no_close).expect("reopen");
    let replayed = db.recovered().records.load(Ordering::Relaxed);
    assert_eq!(
        replayed,
        u64::from(after) + 1,
        "the replay read {replayed} records, not the ones above the boundary"
    );
    let mut session = db.session();
    let mut out = Vec::new();
    assert!(session.read(&key(0), &mut out).expect("read"), "k0 is gone");
    assert_eq!(out, b"newer".to_vec(), "the checkpoint won over the replay");
    drop(session);
    for i in 1..N {
        let mut session = db.session();
        assert!(
            session.read(&key(i), &mut out).expect("read") && out == value(i),
            "user{i:09} did not come back"
        );
    }
}

/// A capture with writers running. The barrier is what makes a
/// checkpoint a prefix, and a prefix is only worth anything if every
/// write acknowledged before the capture is in it and every write
/// acknowledged after it is above the boundary. Either way the reopen
/// has to find all of them.
#[test]
fn a_capture_under_writers_still_comes_back_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("busy.zu2");
    let no_close = Options {
        checkpoint_on_close: false,
        sessions: 16,
        ..options()
    };
    let written = Arc::new(AtomicU64::new(0));
    {
        let db = Db::create(&path, no_close).expect("create");
        std::thread::scope(|scope| {
            for w in 0..4u32 {
                let db = &db;
                let written = Arc::clone(&written);
                scope.spawn(move || {
                    let mut session = db.session();
                    for i in (w..N).step_by(4) {
                        session.upsert(&key(i), &value(i)).expect("upsert");
                        written.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            // Half a dozen captures while they run, so the barrier is
            // met from both sides rather than once at a quiet moment.
            for _ in 0..6 {
                let taken = db.checkpoint().expect("checkpoint");
                assert!(
                    taken.pause < std::time::Duration::from_secs(5),
                    "the barrier held for {:?}, which is a stall and not a pause",
                    taken.pause
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        db.sync().expect("sync");
    }
    assert_eq!(written.load(Ordering::Relaxed), u64::from(N));

    let db = Db::open(&path, no_close).expect("reopen");
    assert_eq!(
        checked(&db),
        0,
        "a key acknowledged around a capture did not come back"
    );
}

/// The graph plane goes into the checkpoint as well, and it is the plane
/// that has to, since a neighbourhood is a sorted array with nothing in
/// it to walk back to.
#[test]
fn the_graph_comes_back_from_a_checkpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("graph.zu2");
    let nodes = 5_000u32;
    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        for i in 0..nodes {
            session.add_node(format!("n{i}").as_bytes()).expect("node");
        }
        // A ring, so every node has one edge each way, and a hub with a
        // neighbourhood far past what fits inline.
        for i in 0..nodes {
            session.add_edge(i, (i + 1) % nodes).expect("ring");
        }
        for i in 1_000..1_200 {
            session.add_edge(0, i).expect("hub");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("reopen");
    assert_eq!(
        db.recovered().records.load(Ordering::Relaxed),
        0,
        "the reopen walked records, so the checkpoint was not used"
    );
    let mut session = db.session();
    assert_eq!(
        db.core().graph().nodes(),
        nodes,
        "the node counter came back short"
    );
    for i in 1..nodes {
        let out = session.neighbours(zu2::Direction::Out, i, |n| n.to_vec());
        assert_eq!(out, vec![(i + 1) % nodes], "node {i} lost its ring edge");
        let back = session.neighbours(zu2::Direction::In, i, |n| n.to_vec());
        let wanted = if (1_000..1_200).contains(&i) {
            // The ring and the hub, in the order a neighbourhood keeps.
            vec![0, i - 1]
        } else {
            vec![i - 1]
        };
        assert_eq!(back, wanted, "node {i} lost an incoming edge");
    }
    // The hub, whose neighbourhood is far past what fits inline, so the
    // capture and the restore both went through a block.
    let hub = session.neighbours(zu2::Direction::Out, 0, |n| n.to_vec());
    let mut wanted: Vec<u32> = (1_000..1_200).collect();
    wanted.push(1);
    wanted.sort_unstable();
    assert_eq!(hub, wanted, "the hub's neighbourhood did not come back");
}

/// A checkpoint taken before a compaction pass names addresses the pass
/// has punched a hole in. The pass takes the file away, and a copy put
/// back by hand is refused on its own terms, which is what the test is
/// about: the refusal is a property of the checkpoint and not of whether
/// anybody remembered to delete it.
#[test]
fn a_checkpoint_that_compaction_has_moved_past_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("compacted.zu2");
    let sidecar = sidecar(&path);
    let kept = dir.path().join("kept.ckpt");
    // A page of mutable window rather than four, and records of about a
    // kilobyte in two lengths, which is the shape tests/compaction.rs
    // uses and for the reasons it gives: a pass can only take pages that
    // are below the read-only boundary, and two rewrites of the same
    // length would be in place and leave nothing dead behind.
    let wide = Options {
        mutable_pages: 1,
        ..options()
    };
    let big = |i: u32, round: u32| {
        let mut v = format!("{i:09}-{round:09}").into_bytes();
        v.resize(1000 + (round as usize % 2) * 8, b'x');
        v
    };
    {
        let db = Db::create(&path, wide).expect("create");
        let mut session = db.session();
        for round in 0..2 {
            for i in 0..N {
                session.upsert(&key(i), &big(i, round)).expect("upsert");
            }
        }
        drop(session);
        db.sync().expect("sync");
        db.checkpoint().expect("checkpoint");
        std::fs::copy(&sidecar, &kept).expect("keep a copy");
        db.compact().expect("compact");
        assert!(
            db.compaction().reclaimed.load(Ordering::Relaxed) > 0,
            "the pass reclaimed nothing, so there is nothing stale about the checkpoint"
        );
        assert!(
            !sidecar.exists(),
            "the pass left the checkpoint it moved past behind"
        );
        db.sync().expect("sync");
    }
    // Put it back, which is the state a crash between the pass and the
    // remove would leave.
    std::fs::copy(&kept, &sidecar).expect("put it back");

    let db = Db::open(
        &path,
        Options {
            checkpoint_on_close: false,
            ..wide
        },
    )
    .expect("reopen");
    assert!(
        db.recovered().records.load(Ordering::Relaxed) > 0,
        "the reopen used a checkpoint that names addresses the log no longer has"
    );
    let mut session = db.session();
    let mut out = Vec::new();
    for i in 0..N {
        assert!(
            session.read(&key(i), &mut out).expect("read") && out == big(i, 1),
            "the fall back to the scan lost user{i:09}"
        );
    }
}

/// An index the caller pinned at a size is a decision and not a hint, so
/// a checkpoint of another size is refused rather than adopted. A grown
/// table is the other way round: the size in the checkpoint is what the
/// run before it learned the key set needs, and taking it is how a
/// reopen avoids doing that learning again.
#[test]
fn a_pinned_index_keeps_the_size_it_was_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pinned.zu2");
    let grew = Options {
        index_buckets: 1 << 8,
        ..options()
    };
    written(&path, grew);

    let pinned = Options {
        index_buckets: 1 << 12,
        grow_index: false,
        checkpoint_on_close: false,
        ..options()
    };
    let db = Db::open(&path, pinned).expect("reopen pinned");
    assert_eq!(
        db.index_buckets(),
        1 << 12,
        "the checkpoint took a pinned table over"
    );
    assert!(
        db.recovered().records.load(Ordering::Relaxed) > 0,
        "a refused checkpoint has to leave the scan to do the work"
    );
    assert_eq!(checked(&db), 0, "the pinned reopen lost keys");
    drop(db);

    // And the refusal took the checkpoint away with it, because the
    // scan that ran instead repaired the links in the log to fit the
    // table it was filling. The records the checkpoint's entries point
    // at are chained for that table now, so an entry out of the
    // checkpoint that walked its chain would walk into keys that are no
    // longer under it. Ten keys out of twenty thousand went missing
    // that way before the refusal cleared up after itself.
    assert!(
        !sidecar(&path).exists(),
        "a refused checkpoint was left beside a log the scan has relinked"
    );
    let db = Db::open(&path, options()).expect("reopen again");
    assert_eq!(checked(&db), 0, "the reopen after the pinned one lost keys");
}

/// The other side of it. A table that is free to grow takes the size the
/// checkpoint holds however small the hint it was opened with, because
/// the size in the checkpoint is what the run before it learned the key
/// set needs and the hint is only ever a hint.
#[test]
fn a_growable_index_takes_the_size_the_checkpoint_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grown.zu2");
    let hint = Options {
        index_buckets: 1 << 8,
        ..options()
    };
    written(&path, hint);

    let db = Db::open(&path, hint).expect("reopen");
    assert_eq!(
        db.index_buckets(),
        1 << 13,
        "the reopen went back to its hint of 256 buckets and threw away what the last run learned"
    );
    assert_eq!(
        db.recovered().records.load(Ordering::Relaxed),
        0,
        "a table the checkpoint could be adopted into still made the scan run"
    );
    assert_eq!(checked(&db), 0, "the adopted table lost keys");
}

/// A checkpoint whose bytes are damaged says nothing at all, which is
/// the only safe thing for it to say. The reopen falls back to the scan
/// and the database is whole, because the log is still the log.
#[test]
fn a_damaged_checkpoint_is_ignored_rather_than_believed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bent.zu2");
    written(&path, options());
    let sidecar = sidecar(&path);
    let mut bytes = std::fs::read(&sidecar).expect("read the checkpoint");
    let at = bytes.len() / 2;
    bytes[at] ^= 0x40;
    std::fs::write(&sidecar, &bytes).expect("bend it");

    let db = Db::open(
        &path,
        Options {
            checkpoint_on_close: false,
            ..options()
        },
    )
    .expect("reopen");
    assert!(
        db.recovered().records.load(Ordering::Relaxed) > 0,
        "a checkpoint that fails its own checksum was used anyway"
    );
    assert_eq!(checked(&db), 0, "the fall back to the scan lost keys");
}
