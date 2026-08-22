//! One device write for a group of commits, rather than one each.
//!
//! A durable commit is a write and a barrier, and the barrier is the
//! expensive half. Several threads committing at once should pay for one
//! barrier between them: whoever gets to the device takes everything
//! appended so far, and the threads waiting behind that write find
//! themselves already durable and go back to work without a barrier of
//! their own.
//!
//! `Log::syncs` and `Log::commits` are counted for exactly this
//! question. Their ratio is what one device write bought, and it was
//! 1.0 to 1.05 at every writer count until the leader stopped holding
//! the flush lock across its own barrier.

use std::sync::Barrier;

use zu2::{Db, Durability, Options};

fn options() -> Options {
    Options {
        durability: Durability::Durable,
        index_buckets: 1 << 12,
        max_pages: 1 << 10,
        // Compaction off, so the syncs counted are the commits' own and
        // not a pass making its copies durable.
        compact_below: 0,
        ..Options::default()
    }
}

/// The ratio a run of `writers` threads achieved, each committing `ops`
/// records of its own keys.
fn ratio(db: &Db, writers: usize, ops: u64, round: u64) -> f64 {
    let commits_before = db.commits();
    let syncs_before = db.syncs();
    let start = Barrier::new(writers);
    std::thread::scope(|scope| {
        for w in 0..writers {
            let start = &start;
            scope.spawn(move || {
                let mut s = db.session();
                let value = vec![b'v'; 200];
                // Every thread appends before any thread commits, so the
                // first group has something to group.
                start.wait();
                for i in 0..ops {
                    let key = format!("w{w:03}r{round:03}k{i:09}");
                    s.upsert(key.as_bytes(), &value).expect("upsert");
                }
            });
        }
    });
    let commits = (db.commits() - commits_before) as f64;
    let syncs = (db.syncs() - syncs_before).max(1) as f64;
    commits / syncs
}

/// A timing test, and worth having anyway.
///
/// It cannot force the interleaving it is about: whether a second thread
/// reaches the commit path while the first is inside its barrier is the
/// scheduler's decision, not the test's. So it takes the best of six
/// attempts and asks for less than the machines give. Three was enough
/// on a quiet machine and failed once in a full suite run on a busy one,
/// which is the #435 lesson again: the bar is not the fragile part, the
/// number of chances at it is.
///
/// The bar is 3.0 because the shape it is guarding against is not zero.
/// The old arrangement, where the leader held the flush lock across its
/// own barrier, still grouped a little by accident: a thread that
/// happened to get the lock while its record was already covered
/// returned without a barrier of its own. Eight writers in this shape
/// measured 1.89, 1.91 and 1.93 that way, against 4.37 to 4.64 with the
/// leader releasing the lock first. Anything at or below two is the old
/// shape and anything above three is this one.
///
/// Eight writers whatever the machine has, because the group forms in
/// the commit path and not on the cpus. Fewer cores than writers makes
/// the group larger rather than smaller, since the threads waiting are
/// waiting on a condition and not on a core. Under four cores the test
/// stands down anyway, because a machine that small is usually a
/// container with a share of a cpu and the ratio there measures the
/// scheduler.
#[test]
fn a_group_of_commits_pays_for_one_device_write() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < 4 {
        println!("{cores} cores is too few to say anything about a group, skipping");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("group.zu2"), options()).expect("create");
    let writers = 8;
    let mut best = 0.0f64;
    for round in 0..6 {
        best = best.max(ratio(&db, writers, 400, round));
        // Six chances, but no reason to spend them once one has landed.
        if best >= 3.0 {
            break;
        }
    }
    assert!(
        best >= 3.0,
        "{writers} writers got {best:.2} commits per device write, which is the old shape"
    );
}

/// A commit whose record is already below the durable boundary does not
/// go to the device at all. This is the cheapest half of the same rule
/// and, unlike the ratio above, it is exact.
#[test]
fn a_commit_of_a_record_that_is_already_durable_does_not_sync() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("late.zu2"), options()).expect("create");
    let mut s = db.session();
    s.upsert(b"k", b"v").expect("upsert");
    let syncs = db.syncs();
    let commits = db.commits();
    // Nothing has been appended since, so this commit is asking for a
    // boundary that has already been passed.
    db.sync().expect("sync");
    assert_eq!(db.syncs(), syncs, "the log went to the device for nothing");
    assert!(db.commits() > commits, "the commit was not counted");
}
