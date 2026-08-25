//! Several sessions writing and reading at once while the log laps, the
//! index doubles and a pass moves records to the cold tier underneath
//! them.
//!
//! `tests/model.rs` compares a single session against a `BTreeMap` and
//! that is where the ordering bugs turn up. This is the other half: the
//! bugs that need two threads and a compaction to line up, which is the
//! shape #562, #563 and #564 all had. Each thread owns a disjoint slice
//! of the key space and keeps its own record of what it wrote, so the
//! expectation is exact without a lock anywhere near the engine: nobody
//! else can touch a key this thread is checking, so a read of it that
//! disagrees is the engine losing a write, not a race in the test.
//!
//! `ZU2_RACY_OPS` sets how many operations a thread does. The default is
//! what a run on a laptop gets through quickly, and it is a floor rather
//! than a ceiling now: a writer that outran compaction used to end the
//! run with `LogFull` at 100000, which was #566, and every write path
//! makes room and goes round again since #566 and #593. A soak at
//! 1000000 a thread passes in ten seconds in release, which is the run
//! worth doing after anything touches the log or a pass.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zu2::{Db, Durability, Options};

/// Small enough that the log laps several times over during a run and
/// the index has to double from one bucket, which is what puts a reader
/// on an old table and a compaction under it at the same time.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1,
        max_pages: 16,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        // Low enough that the background thread is always passing over
        // the log, and the thread below piles on top of that so a
        // reclaim lands while the workers are mid flight rather than
        // between rounds.
        compact_below: 1 << 20,
        ordered: true,
        ..Options::default()
    }
}

fn key(thread: u32, i: u32) -> Vec<u8> {
    format!("t{thread:02}_{i:07}").into_bytes()
}

/// A key written once and never again, so a pass over the region it is
/// in finds it live and has to carry it to the cold tier. A workload
/// that rewrites everything leaves nothing down there to migrate, which
/// is the point `tests/coldtier.rs` makes about its own shape.
fn resident(thread: u32, i: u32) -> Vec<u8> {
    format!("r{thread:02}_{i:07}").into_bytes()
}

/// Alternating length for the reason the compaction tests give: a same
/// length update over a record above the boundary is rewritten in place
/// and appends nothing.
fn value(thread: u32, i: u32, round: u32) -> Vec<u8> {
    let mut v = format!("{thread:02}-{i:07}-{round:07}").into_bytes();
    v.resize(1000 + (round as usize % 2) * 8, b'v');
    v
}

/// A cheap deterministic mix per thread, so a failure is reproducible
/// from the thread number and the operation count alone.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn concurrent_sessions_keep_their_own_keys_under_compaction() {
    const THREADS: u32 = 4;
    const KEYS: u32 = 400;
    const RESIDENT: u32 = 400;
    let ops: u32 = std::env::var("ZU2_RACY_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40000);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::create(&dir.path().join("r.zu2"), options()).expect("create"));
    let stop = Arc::new(AtomicBool::new(false));

    // A pass every so often, so the reclaim races the readers rather
    // than waiting for them to finish. The background thread compacts on
    // its own schedule and this one leans on it.
    let compactor = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                db.compact().expect("compact");
                std::thread::yield_now();
            }
        })
    };

    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                // What this thread last wrote for each of its keys, and
                // `None` for a key it has deleted or never written.
                let mut held: Vec<Option<Vec<u8>>> = vec![None; KEYS as usize];
                let mut session = db.session();
                let mut state = 0x9e37_79b9_7f4a_7c15 ^ u64::from(t) << 32;
                let mut out = Vec::new();
                let prefix = format!("t{t:02}_").into_bytes();
                for i in 0..RESIDENT {
                    session
                        .upsert(&resident(t, i), &value(t, i, 0))
                        .expect("resident");
                }
                for op in 0..ops {
                    let draw = next(&mut state);
                    let i = (draw >> 32) as u32 % KEYS;
                    let k = key(t, i);
                    match draw % 100 {
                        95..=99 => {
                            // The keys that end up cold, read from
                            // wherever the pass has left them.
                            let i = (draw >> 16) as u32 % RESIDENT;
                            out.clear();
                            assert!(
                                session.read(&resident(t, i), &mut out).expect("read"),
                                "thread {t} resident key {i} at op {op} is gone"
                            );
                            assert_eq!(
                                out.as_slice(),
                                value(t, i, 0).as_slice(),
                                "thread {t} resident key {i} at op {op}: wrong value"
                            );
                        }
                        90..=94 => {
                            // A scan over this thread's own stretch of
                            // the key order. Nobody else writes into it,
                            // so what comes back has to be exactly what
                            // this thread believes is live, in order,
                            // with the values it last wrote. The walk
                            // stops at the first key outside the prefix,
                            // since the order is global and the next
                            // thread's keys sit right after these.
                            let mut seen = Vec::new();
                            session
                                .scan(&k, 16, |k, v| seen.push((k.to_vec(), v.to_vec())))
                                .expect("scan");
                            let mut want = (i..KEYS).filter_map(|j| {
                                held[j as usize].as_ref().map(|v| (key(t, j), v.clone()))
                            });
                            for (got_key, got_value) in seen {
                                if !got_key.starts_with(&prefix) {
                                    break;
                                }
                                let (want_key, want_value) =
                                    want.next().unwrap_or_else(|| {
                                        panic!(
                                            "thread {t} at op {op}: scan gave back a key it should have run out at"
                                        )
                                    });
                                assert_eq!(
                                    got_key, want_key,
                                    "thread {t} at op {op}: scan is out of order or skipped a key"
                                );
                                assert_eq!(
                                    got_value, want_value,
                                    "thread {t} at op {op}: scan gave a stale value"
                                );
                            }
                        }
                        0..=49 => {
                            let v = value(t, i, op);
                            session.upsert(&k, &v).expect("upsert");
                            held[i as usize] = Some(v);
                        }
                        50..=59 => {
                            let gone = session.delete(&k).expect("delete");
                            assert_eq!(
                                gone,
                                held[i as usize].is_some(),
                                "thread {t} key {i} at op {op}: delete said {gone}"
                            );
                            held[i as usize] = None;
                        }
                        _ => {
                            out.clear();
                            let there = session.read(&k, &mut out).expect("read");
                            let want = held[i as usize].as_deref();
                            assert_eq!(
                                there,
                                want.is_some(),
                                "thread {t} key {i} at op {op}: read said {there}"
                            );
                            if let Some(want) = want {
                                assert_eq!(
                                    out.as_slice(),
                                    want,
                                    "thread {t} key {i} at op {op}: wrong value"
                                );
                            }
                        }
                    }
                }
                held
            })
        })
        .collect();

    let held: Vec<_> = workers
        .into_iter()
        .enumerate()
        .map(|(t, worker)| worker.join().unwrap_or_else(|_| panic!("thread {t}")))
        .collect();
    stop.store(true, Ordering::Relaxed);
    compactor.join().expect("compactor");

    // The run is only worth anything if the machinery it is aimed at
    // actually ran, and how much of it runs depends on how fast the
    // machine got through the operations, so this says so rather than
    // assuming it.
    println!(
        "buckets {}, doublings {}, cold {} bytes, migrated {}, promoted {}",
        db.index_buckets(),
        db.index_grows(),
        db.cold_span(),
        db.compaction()
            .migrated
            .load(std::sync::atomic::Ordering::Relaxed),
        db.promoted(),
    );
    assert!(db.index_grows() > 0, "the index never doubled");
    // Migrated rather than what is down there now, since a cold pass can
    // take back everything it was given and leave the span at zero on a
    // run where the tier did its whole job. A pass at rest first: which
    // of them the threads managed between them depends on how fast the
    // machine got through the operations, and the tier has to be on the
    // path of every run rather than of most of them.
    let migrated = || {
        db.compaction()
            .migrated
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    if migrated() == 0 {
        // The sync is the part that makes this deterministic and the
        // compaction on its own was not. A pass may only touch pages
        // below `compact::ceiling`, which is the flushed frontier and
        // the read-only boundary, whichever is lower, and the durability
        // here is async. On a loaded machine the flusher is behind at
        // the moment the threads stop, so the ceiling sits below the
        // page the log starts in, the pass has nothing it is allowed to
        // read, and the counter stays at zero on a run where every other
        // thing worked. That is #763, seen once in about eight full
        // suite runs and never in isolation. Flushing first gives the
        // pass the whole log up to the mutable window, which is the same
        // thing a quiet machine was handing it by luck.
        db.sync().expect("sync");
        db.compact().expect("compact");
    }
    assert!(migrated() > 0, "nothing ever reached the cold tier");

    // And the whole of it is still there once the threads are done and
    // nothing is moving, which is what says a lost write was lost rather
    // than merely late.
    let mut session = db.session();
    let mut out = Vec::new();
    for (t, held) in held.iter().enumerate() {
        let t = t as u32;
        for (i, want) in held.iter().enumerate() {
            let k = key(t, i as u32);
            out.clear();
            let there = session.read(&k, &mut out).expect("read");
            assert_eq!(there, want.is_some(), "thread {t} key {i} at rest");
            if let Some(want) = want {
                assert_eq!(
                    out.as_slice(),
                    want.as_slice(),
                    "thread {t} key {i} at rest"
                );
            }
        }
        for i in 0..RESIDENT {
            out.clear();
            assert!(
                session.read(&resident(t, i), &mut out).expect("read"),
                "thread {t} resident key {i} at rest is gone"
            );
            assert_eq!(out.as_slice(), value(t, i, 0).as_slice());
        }
    }
}
