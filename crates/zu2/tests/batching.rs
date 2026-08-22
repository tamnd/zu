//! The batch write path against a log that is already full.
//!
//! `upsert_many` is what a loader calls and what `zu2_upsert_many` is,
//! so it is the path that sees a full log more often than any other: a
//! bulk load is a writer that never stops long enough for the
//! maintenance thread to get in front of it.

use zu2::{Db, Durability, Options};

/// A batch that arrives at a full log pays for a pass and goes on, the
/// way a single upsert has since #566, an edge since #587 and a read
/// modify write since #591. Before the fix this one handed the caller
/// `LogFull` and a count, and the caller could only answer it by
/// sleeping and offering the same pairs again.
#[test]
fn a_batch_on_a_full_log_makes_room_rather_than_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("batch.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 8,
            max_pages: 8,
            mutable_pages: 1,
            // Off, so the only thread that can reclaim anything here is
            // the one doing the writing.
            compact_below: 0,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let padding = vec![b'p'; 4096];
    // The keys cycle, and the length alternates from one lap to the
    // next, because an update of the same length inside the mutable
    // window is rewritten where it lies and never moves the tail.
    let mut written = 0usize;
    for lap in 0..80u64 {
        let fat = &padding[..padding.len() - (lap as usize % 2) * 8];
        let keys: Vec<Vec<u8>> = (0..512u64)
            .map(|i| format!("pad{:05}", i).into_bytes())
            .collect();
        let pairs: Vec<(&[u8], &[u8])> = keys.iter().map(|k| (k.as_slice(), fat)).collect();
        let (done, outcome) = s.upsert_many(&pairs);
        outcome.unwrap_or_else(|e| panic!("batch at lap {lap}, pair {done}: {e}"));
        assert_eq!(done, pairs.len(), "lap {lap} stopped short");
        written += done;
    }
    assert_eq!(written, 80 * 512);
    assert!(
        db.compaction()
            .passes
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "the log never filled, so nothing here was tested against a full one"
    );
    // And the last lap is what is there, so the retry wrote each pair
    // once rather than leaving a half applied batch behind.
    let mut out = Vec::new();
    for i in 0..512u64 {
        let key = format!("pad{:05}", i).into_bytes();
        assert!(s.read(&key, &mut out).expect("read"), "key {i} is missing");
        assert_eq!(out.len(), padding.len() - 8, "key {i} has the wrong value");
    }
}
