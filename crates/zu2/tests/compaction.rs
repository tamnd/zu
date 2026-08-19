//! Compaction tests: the log gives blocks back and loses nothing doing
//! it.
//!
//! Every test here writes more than one page and then rewrites it, which
//! is the only shape where compaction has anything to do. The numbers are
//! chosen so the log crosses several 4 MiB pages, because a pass can only
//! take pages that are already flushed and already below the read-only
//! boundary, and a test that stayed inside the mutable window would pass
//! without compacting a byte.

use zu2::{Db, Direction, Durability, Options};

/// Compaction off in the background, so a pass happens exactly where the
/// test asks for one and the assertions are about a known state.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 14,
        max_pages: 64,
        max_nodes: 1 << 16,
        compact_below: 0,
        // One page of mutable window rather than the default four. The
        // window is the part of the log compaction is not allowed to
        // touch, so with the default it would be the floor these tests
        // measure against and the live set would be lost inside it.
        mutable_pages: 1,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

/// A key in its own namespace, for the graph test, where the node keys
/// are the mapping from key to dense id and writing a property over one
/// would replace the mapping rather than sit beside it.
fn property(i: u32) -> Vec<u8> {
    format!("prop{i:09}").into_bytes()
}

/// About a thousand bytes, the YCSB record size, so the page arithmetic
/// in these tests matches what the benchmark sees.
///
/// The length alternates by round, and that is load bearing rather than
/// decoration. An update whose value is the same length as the one it
/// replaces, on a record that is still above the read-only boundary, is
/// an in-place rewrite that appends nothing. With a constant length the
/// amount of log these tests produce therefore depends on where the
/// boundary happens to have got to, which depends on how much has been
/// appended, which is a feedback loop and moves with how busy the
/// machine is: the same sixteen rounds gave anywhere from 6 to 19 MiB
/// under parallel load, and at the bottom of that range there was
/// nothing below the boundary to compact and the test failed saying
/// compaction had not worked. Alternating the length means consecutive
/// rounds can never be in place, so the log is what the record count
/// says it is on any machine (#435).
fn value(i: u32, round: u32) -> Vec<u8> {
    let mut v = format!("{i:09}-{round:09}").into_bytes();
    v.resize(1000 + (round as usize % 2) * 8, b'x');
    v
}

/// The most a record of ours can take on the log, value plus key plus
/// header, rounded up. The live set bounds in these tests are written
/// against it.
const RECORD: u64 = 1064;

/// Pushes everything appended so far to the device, so the next pass has
/// a flushed region to work on rather than racing the background thread.
fn flush(session: &mut zu2::Session<'_>, i: u32, round: u32) {
    session.set_durability(Durability::Durable);
    session
        .upsert(&property(i), &value(i, round))
        .expect("flush");
    session.set_durability(Durability::Async);
}

#[test]
fn a_rewritten_log_gives_its_blocks_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options()).expect("create");
    let mut s = db.session();
    let records = 3000u32;
    let rounds = 16u32;
    for round in 0..rounds {
        for i in 0..records {
            s.upsert(&key(i), &value(i, round)).expect("upsert");
        }
    }
    flush(&mut s, 0, rounds - 1);

    let before = db.disk_bytes().expect("disk bytes");
    let written = db.log_bytes();
    assert!(
        before >= written / 2,
        "the file should be about as big as the log before compaction: {before} against {written}"
    );

    // The setup, checked rather than assumed. Compaction can only take
    // pages that are below the read-only boundary, so a log that has not
    // cleared the mutable window by several pages leaves it nothing to
    // do and the assertions below would blame the engine for a rewrite
    // that never happened (#435).
    let mutable = options().mutable_pages as u64 * (4 << 20);
    assert!(
        written > mutable * 4,
        "the rounds did not produce enough log to compact: {written} against a {mutable} byte window"
    );

    db.compact().expect("compact");
    let after = db.disk_bytes().expect("disk bytes");
    let live = u64::from(records) * RECORD;
    // What compaction cannot take: the mutable window, which is above the
    // read-only boundary and so out of reach by construction, plus the
    // page the copies landed in. That is the floor, and the live set is
    // small next to it, which is why the bound is written this way.
    let window = (options().mutable_pages as u64 + 2) * (4 << 20);

    assert!(
        after < before / 2,
        "compaction should have returned most of the file: {after} against {before}"
    );
    assert!(
        after < live * 2 + window,
        "the file should be the live set plus the mutable window: {after} against {live} plus {window}"
    );
    assert!(
        db.compaction()
            .reclaimed
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "no blocks were punched, so this platform is not returning space"
    );

    // Everything is still there, at its newest value.
    let mut out = Vec::new();
    for i in 0..records {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out, value(i, rounds - 1), "wrong value for {i}");
    }
}

#[test]
fn compacting_a_live_set_larger_than_the_window_terminates() {
    // Twelve megabytes of live records against a one page window, so
    // every copy a pass makes lands below the read-only boundary and is
    // compactable again on the next pass. That is the shape that used to
    // walk its own copies up the address space and never return, and the
    // test is simply that this function ends.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("w.zu2"), options()).expect("create");
    let mut s = db.session();
    let records = 12000u32;
    for round in 0..3u32 {
        for i in 0..records {
            s.upsert(&key(i), &value(i, round)).expect("upsert");
        }
    }
    flush(&mut s, 0, 2);

    db.compact().expect("compact");
    let mut out = Vec::new();
    for i in 0..records {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out, value(i, 2), "wrong value for {i}");
    }
}

#[test]
fn a_compacted_database_reopens_with_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let records = 3000u32;
    let rounds = 12u32;
    {
        let db = Db::create(&path, options()).expect("create");
        let mut s = db.session();
        for round in 0..rounds {
            for i in 0..records {
                s.upsert(&key(i), &value(i, round)).expect("upsert");
            }
        }
        // A few keys gone, so the reopen has to agree about deletes that
        // were written before the compaction as well as after it.
        for i in (0..records).step_by(97) {
            assert!(s.delete(&key(i)).expect("delete"), "delete missed {i}");
        }
        flush(&mut s, 1, rounds - 1);
        db.compact().expect("compact");
    }

    let db = Db::open(&path, options()).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..records {
        let gone = i % 97 == 0;
        let found = s.read(&key(i), &mut out).expect("read");
        if gone {
            assert!(!found, "{i} was deleted and came back");
            continue;
        }
        assert!(found, "lost {i} across a compaction and a reopen");
        assert_eq!(out, value(i, rounds - 1), "wrong value for {i}");
    }
}

#[test]
fn a_compacted_graph_reopens_with_every_edge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("g.zu2");
    let nodes = 2000u32;
    let rounds = 10u32;
    {
        let db = Db::create(&path, options()).expect("create");
        let mut s = db.session();
        for i in 0..nodes {
            s.add_node(&key(i)).expect("node");
        }
        // The properties are what makes the log long enough to have a
        // compactable region. The edges are what the test is about.
        for round in 0..rounds {
            for i in 0..nodes {
                s.upsert(&property(i), &value(i, round)).expect("upsert");
            }
        }
        for i in 0..nodes {
            s.add_edge(i, (i + 1) % nodes).expect("ring");
            s.add_edge(i, (i * 7 + 1) % nodes).expect("chord");
        }
        // A hub, so at least one adjacency is out of line and large.
        for i in 1..nodes {
            s.add_edge(0, i).expect("hub");
        }
        s.remove_edge(0, 3).expect("remove");
        flush(&mut s, 2, rounds - 1);
        db.compact().expect("compact");
    }

    let db = Db::open(&path, options()).expect("reopen");
    let mut s = db.session();
    let mut scratch = Vec::new();
    assert_eq!(
        s.node_of(&key(11), &mut scratch).expect("node_of"),
        Some(11),
        "the key to id mapping did not survive the compaction"
    );
    assert_eq!(
        db.core().graph().nodes(),
        nodes,
        "the id counter did not survive the compaction"
    );
    let hub = s.neighbours(Direction::Out, 0, |n| n.to_vec());
    assert_eq!(
        hub.len(),
        (nodes - 2) as usize,
        "the hub lost edges, or kept the removed one"
    );
    assert!(
        !hub.contains(&3),
        "the removed edge came back, so the remove record was compacted away"
    );
    for i in 0..nodes {
        let out = s.neighbours(Direction::Out, i, |n| n.to_vec());
        assert!(
            out.contains(&((i + 1) % nodes)),
            "node {i} lost its ring edge"
        );
        assert!(
            out.contains(&((i * 7 + 1) % nodes)),
            "node {i} lost its chord"
        );
    }
}

#[test]
fn the_background_compactor_runs_under_writers_and_keeps_everything() {
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("b.zu2");
    // A low threshold and a tight space target, so passes happen during
    // the run rather than after it. This is the production path: the
    // flusher thread compacting while foreground sessions append.
    let eager = Options {
        compact_below: 8 << 20,
        space_target_percent: 150,
        ..options()
    };
    let threads = 4u32;
    let per_thread = 1500u32;
    let rounds = 8u32;
    {
        let db = Arc::new(Db::create(&path, eager).expect("create"));
        let mut handles = Vec::new();
        for t in 0..threads {
            let db = Arc::clone(&db);
            handles.push(std::thread::spawn(move || {
                let mut s = db.session();
                for round in 0..rounds {
                    for i in 0..per_thread {
                        let k = key(t * per_thread + i);
                        s.upsert(&k, &value(i, round)).expect("upsert");
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker");
        }
        // Give the background thread the chance the writers may not have
        // left it. Forty eight megabytes of upserts is under a tenth of a
        // second on a fast desktop, and on that machine the writers can
        // be done before the flusher has written a page, which leaves
        // nothing compactable and no pass to count. The sync makes the
        // pages eligible and the poll waits for the thread that acts on
        // them, so what is being tested is still the background path and
        // not the foreground one.
        db.sync().expect("sync");
        let passes = || {
            db.compaction()
                .passes
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        // No deadline. A wall clock bound here is not a test of the
        // compactor, it is a test of how busy the machine is, and it
        // failed a third of the time under a parallel hammer for that
        // reason alone (#435). A thread that never runs hangs the suite,
        // which says what went wrong far more clearly than a flake does.
        while passes() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mut s = db.session();
        let mut out = Vec::new();
        for i in 0..threads * per_thread {
            assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
            assert_eq!(
                out,
                value(i % per_thread, rounds - 1),
                "wrong value for {i}"
            );
        }
        flush(&mut s, 4, rounds - 1);
    }

    let db = Db::open(&path, eager).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..threads * per_thread {
        assert!(
            s.read(&key(i), &mut out).expect("read"),
            "lost {i} on reopen"
        );
        assert_eq!(
            out,
            value(i % per_thread, rounds - 1),
            "wrong value for {i} on reopen"
        );
    }
}

#[test]
fn a_foreground_pass_beside_writers_survives_a_reopen() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Passes from another thread while sessions write, which is the one
    // shape the other reopen tests here do not have: they compact after
    // the writers are done, so no copy of theirs can lose a race. A copy
    // that does lose one is appended before the compare and swap that
    // would publish it, so it stays on the log above the record that
    // beat it with nobody pointing at it, and a replay that ordered the
    // log by address handed the key back a round behind (#436). The
    // narrow test for that is in `recover.rs`; this is the workload it
    // came out of.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("r.zu2");
    let threads = 4u32;
    let per_thread = 800u32;
    let rounds = 6u32;
    {
        let db = Arc::new(Db::create(&path, options()).expect("create"));
        let writing = Arc::new(AtomicBool::new(true));
        let passes = {
            let db = Arc::clone(&db);
            let writing = Arc::clone(&writing);
            std::thread::spawn(move || {
                while writing.load(Ordering::Acquire) {
                    // A pass can only read what is on the device and
                    // below the read-only boundary, and on a machine
                    // busy enough that the flusher does not get a turn
                    // there is nothing there at all. Pushing first is
                    // what makes the pass have work rather than hoping
                    // it does (#435).
                    db.sync().expect("sync");
                    db.compact().expect("compact");
                }
            })
        };
        let mut handles = Vec::new();
        for t in 0..threads {
            let db = Arc::clone(&db);
            handles.push(std::thread::spawn(move || {
                let mut s = db.session();
                for round in 0..rounds {
                    for i in 0..per_thread {
                        let k = key(t * per_thread + i);
                        s.upsert(&k, &value(i, round)).expect("upsert");
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("writer");
        }
        // The precondition, waited for rather than asserted after the
        // fact: a run where no pass ever read a byte proves nothing, and
        // on a loaded machine that is decided by who got the cpu.
        let scanned = || db.compaction().scanned.load(Ordering::Relaxed);
        while scanned() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        writing.store(false, Ordering::Release);
        passes.join().expect("compactor");

        let mut s = db.session();
        let mut out = Vec::new();
        for i in 0..threads * per_thread {
            assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
            assert_eq!(out, value(i % per_thread, rounds - 1), "wrong value for {i}");
        }
        flush(&mut s, 5, rounds - 1);
    }

    let db = Db::open(&path, options()).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..threads * per_thread {
        assert!(
            s.read(&key(i), &mut out).expect("read"),
            "lost {i} on reopen"
        );
        assert_eq!(
            out,
            value(i % per_thread, rounds - 1),
            "wrong value for {i} on reopen"
        );
    }
}

#[test]
fn a_chain_through_a_full_bucket_survives_compaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.zu2");
    // Sixty four entries for five hundred keys, so every bucket
    // overflows, entries go foreign, and a read has to walk the record
    // chain rather than trust a tag. That walk is what stops at the
    // begin address after a compaction, which is the thing being tested.
    let cramped = Options {
        index_buckets: 8,
        ..options()
    };
    let records = 500u32;
    let rounds = 40u32;
    {
        let db = Db::create(&path, cramped).expect("create");
        let mut s = db.session();
        for round in 0..rounds {
            for i in 0..records {
                s.upsert(&key(i), &value(i, round)).expect("upsert");
            }
        }
        flush(&mut s, 3, rounds - 1);
        db.compact().expect("compact");

        let mut out = Vec::new();
        for i in 0..records {
            assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
            assert_eq!(out, value(i, rounds - 1), "wrong value for {i}");
        }
    }

    let db = Db::open(&path, cramped).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..records {
        assert!(
            s.read(&key(i), &mut out).expect("read"),
            "lost {i} on reopen"
        );
        assert_eq!(out, value(i, rounds - 1), "wrong value for {i} on reopen");
    }
}
