//! Compaction looks before it moves anything.
//!
//! #736. The first pass of a database's life is triggered by
//! `Options::compact_below`, which is a size and knows nothing about how
//! much of the log is stale. A database that has only ever been loaded
//! therefore compacted at 128 MiB, found every record live, reclaimed
//! nothing, and moved the whole region to the cold tier, because
//! surviving a pass is what this engine means by cold. A gigabyte load
//! ended up with a third of itself in the tier, and a cold read is a
//! `pread`, so scanning it cost a syscall a row.
//!
//! Two things have to hold at once and the tests here are one for each.
//! A load that never updates anything must not be moved, and a workload
//! that does update must still be compacted. The second is the one that
//! would fail quietly: a survey that always says no is a database whose
//! log grows forever, and nothing in the first test would notice.
//!
//! These drive the background maintainer rather than calling
//! `Db::compact`, because the survey is on the background path.
//! `Db::compact` is a host asking for a pass explicitly and it makes the
//! one it was asked for.

use std::time::{Duration, Instant};

use zu2::{Db, Durability, Options};

const VALUE: usize = 1000;

/// A small `compact_below` so the trigger is reached in a test sized
/// database. Everything else is the default, because the defaults are
/// what #736 was found under.
fn options(rows: u32) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: (rows as usize / 4 + 1).next_power_of_two(),
        compact_below: 8 << 20,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

/// Waits until `done` or the deadline, whichever is first, letting the
/// background maintainer run. Returns whether it got there.
///
/// A deadline rather than a fixed sleep because the two tests want
/// opposite things from it: one waits for compaction to happen and one
/// waits to be sure it has not, and a fixed sleep is either too short
/// for the first on a busy machine or too long for the second on every
/// machine.
fn within(seconds: u64, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

#[test]
fn a_load_that_updates_nothing_is_never_moved_to_the_cold_tier() {
    // Enough to cross `compact_below` several times over, so the
    // maintainer has every chance to run a pass.
    const ROWS: u32 = 60_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("load.zu2"), options(ROWS)).expect("create");
    {
        let mut s = db.session();
        let value = [b'v'; VALUE];
        for i in 0..ROWS {
            s.upsert(&key(i), &value).expect("upsert");
        }
    }
    db.sync().expect("sync");

    // The survey has to have looked, or this test passes for the wrong
    // reason on a machine where the maintainer never woke up.
    assert!(
        within(20, || db
            .compaction()
            .skipped
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0),
        "no survey ran in twenty seconds, so this test proved nothing"
    );

    let migrated = db
        .compaction()
        .migrated
        .load(std::sync::atomic::Ordering::Relaxed);
    let copied = db
        .compaction()
        .copied
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        migrated, 0,
        "a load that updated nothing moved {migrated} bytes to the cold tier"
    );
    assert_eq!(
        copied, 0,
        "a load that updated nothing copied {copied} bytes back to the tail"
    );
    assert_eq!(
        db.cold_disk_bytes().expect("cold disk bytes"),
        0,
        "the cold tier has bytes in it after a load that updated nothing"
    );

    // And every record is still there, which is the thing all of this is
    // in service of.
    let mut s = db.session();
    let mut value = Vec::new();
    for i in (0..ROWS).step_by(997) {
        assert!(
            s.read(&key(i), &mut value).expect("read"),
            "key {i} is gone"
        );
        assert_eq!(value.len(), VALUE);
    }
}

#[test]
fn a_workload_that_rewrites_the_same_keys_is_still_compacted() {
    // A key set rewritten many times, so almost everything on the log is
    // a superseded version and a pass has a great deal to reclaim. This
    // is the case the survey must not talk itself out of.
    //
    // The live set is deliberately several times the mutable window,
    // which is four pages of four megabytes by default. A smaller one is
    // quicker and it measures the wrong thing: at four thousand keys the
    // log settles at sixteen megabytes for a four megabyte live set and
    // that ratio is the window, not the target. `crates/zu2/tests/
    // compaction.rs` solves the same problem by setting `mutable_pages:
    // 1`, which is not available here because #736 is a claim about the
    // defaults.
    const KEYS: u32 = 40_000;
    const ROUNDS: u32 = 6;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("churn.zu2"), options(KEYS)).expect("create");
    {
        let mut s = db.session();
        let value = [b'v'; VALUE];
        for _ in 0..ROUNDS {
            for i in 0..KEYS {
                s.upsert(&key(i), &value).expect("upsert");
            }
        }
    }
    db.sync().expect("sync");

    let live = u64::from(KEYS) * VALUE as u64;
    assert!(
        within(30, || {
            db.compaction()
                .reclaimed
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        }),
        "nothing was reclaimed in thirty seconds from a log that is {}x its \
         live set, so the survey is refusing every pass",
        ROUNDS
    );

    // The space target is 200 per cent by default, so the log should
    // settle near twice the live set. Three times rather than two as the
    // assertion, because the mutable window and the page the survey
    // schedules the next check past are both real and both above the
    // target, and this is a test of whether the log converges at all
    // rather than of where exactly it lands.
    assert!(
        within(60, || db.disk_bytes().unwrap_or(u64::MAX) < live * 3),
        "the log is {} bytes for a live set of {live}, which is {:.1}x",
        db.disk_bytes().unwrap_or(0),
        db.disk_bytes().unwrap_or(0) as f64 / live as f64
    );

    let mut s = db.session();
    let mut value = Vec::new();
    for i in 0..KEYS {
        assert!(
            s.read(&key(i), &mut value).expect("read"),
            "key {i} is gone"
        );
        assert_eq!(value.len(), VALUE);
    }
}
