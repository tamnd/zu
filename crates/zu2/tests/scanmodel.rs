//! A seeded random mix of writes, deletes and scans against a
//! `BTreeMap`, which is to the scan plane what `tests/model.rs` is to
//! the record plane and `tests/graphmodel.rs` is to the graph plane.
//!
//! The scripted scan tests each build one key set and walk it. What
//! none of them do is delete a key and put it back, scan across the
//! hole either way round, and do it while the log laps and compaction
//! moves the records the scan reads through. That is where #590 was,
//! and a scan is the one read in this database that touches two planes
//! at once: the key comes from the skip list and the value comes from
//! the hash index, so a scan is wrong if either one is.
//!
//! `ZU2_SCAN_SEEDS` and `ZU2_SCAN_OPS` widen it for a soak run.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use zu2::{Db, Durability, Options};

fn env(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

/// Small enough that the log laps several times during a run, so a scan
/// reads records compaction has already moved rather than records still
/// sitting in the tail.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        ordered: true,
        index_buckets: 1 << 8,
        max_pages: 8,
        mutable_pages: 1,
        compact_below: 1 << 20,
        ..Options::default()
    }
}

/// xorshift64, so a failure is a seed and not a story.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Fixed width and zero padded, so the byte order the plane sorts on
/// and the number order the model sorts on are the same order.
fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

/// Long enough that a thousand of them lap the log, and a length
/// that varies with the value so an update is an append rather than a
/// rewrite where the record lies.
fn value(i: u64, salt: u64) -> Vec<u8> {
    let mut v = format!("field0={i}:{salt}").into_bytes();
    v.resize(3000 + (salt % 7) as usize * 16, b'v');
    v
}

/// A pair as it is compared and as it is printed when it disagrees.
/// The padding is dropped, because three thousand bytes of the letter v
/// in a failure message hides the one field that says which write this
/// value came from.
fn shown(key: &[u8], value: &[u8]) -> (String, String) {
    let head = value.iter().position(|b| *b == b'v').unwrap_or(value.len());
    (
        String::from_utf8_lossy(key).into_owned(),
        format!(
            "{}+{}",
            String::from_utf8_lossy(&value[..head]),
            value.len() - head
        ),
    )
}

/// What a scan from `start` for `count` records should give back.
fn expected(model: &BTreeMap<u64, Vec<u8>>, start: u64, count: usize) -> Vec<(String, String)> {
    model
        .range(start..)
        .take(count)
        .map(|(k, v)| shown(&key(*k), v))
        .collect()
}

fn scanned(db: &Db, start: u64, count: usize) -> Vec<(String, String)> {
    let mut got = Vec::new();
    let mut s = db.session();
    s.scan(&key(start), count, |k, v| got.push(shown(k, v)))
        .expect("scan");
    got
}

#[test]
fn a_random_scan_mix_agrees_with_a_map_across_a_reopen() {
    let seeds = env("ZU2_SCAN_SEEDS", 3);
    let ops = env("ZU2_SCAN_OPS", 30_000);
    // Small enough that a delete lands on a key that is there most of
    // the time, and a scan of a few dozen crosses several holes.
    const KEYS: u64 = 4096;

    for seed in 0..seeds {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scan.zu2");
        let mut db = Db::create(&path, options()).expect("create");
        let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut state = 0x2545_f491_4f6c_dd1d ^ (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);

        for op in 0..ops {
            // Halfway through, close and open again, so the second half
            // runs over a plane rebuilt from the log rather than one
            // this run's writes built.
            if op == ops / 2 {
                drop(db);
                db = Db::open(&path, options()).expect("reopen");
                let want = expected(&model, 0, model.len());
                assert_eq!(
                    scanned(&db, 0, model.len()),
                    want,
                    "seed {seed}: the whole key set disagreed after the reopen"
                );
            }

            let roll = next(&mut state);
            let k = (roll >> 8) % KEYS;
            let mut s = db.session();
            match roll % 100 {
                0..=49 => {
                    let v = value(k, op);
                    s.upsert(&key(k), &v).expect("upsert");
                    model.insert(k, v);
                }
                50..=69 => {
                    let gone = s.delete(&key(k)).expect("delete");
                    assert_eq!(
                        gone,
                        model.remove(&k).is_some(),
                        "seed {seed} op {op}: remove of {k} disagreed about whether it was there"
                    );
                }
                _ => {
                    // A short scan most of the time and a long one now
                    // and then, because a short one stays inside a page
                    // and a long one crosses the tail into the pages
                    // compaction has been over.
                    let count = if roll % 1000 < 20 {
                        1000
                    } else {
                        1 + (roll >> 40) as usize % 40
                    };
                    let got = scanned(&db, k, count);
                    let want = expected(&model, k, count);
                    assert_eq!(
                        got.len(),
                        want.len(),
                        "seed {seed} op {op}: scan from {k} for {count} gave {} records against {}",
                        got.len(),
                        want.len()
                    );
                    assert_eq!(got, want, "seed {seed} op {op}: scan from {k} for {count}");
                }
            }
        }

        // The run is only worth anything if the log moved far enough for
        // a pass to have happened, so this says so rather than assuming
        // it.
        let passes = db.compaction().passes.load(Ordering::Relaxed);
        println!(
            "seed {seed}: passes {passes}, {} keys live, plane {} keys",
            model.len(),
            db.ordered_keys().unwrap_or(0)
        );
        assert!(
            passes > 0,
            "seed {seed}: the log never lapped, so no scan here read a compacted record"
        );

        // A pass over the log, so what it decided about the records a
        // scan reads is checked rather than assumed.
        db.compact().expect("compact");
        assert_eq!(
            scanned(&db, 0, model.len()),
            expected(&model, 0, model.len()),
            "seed {seed}: the key set disagreed after a compaction"
        );

        // And once more from the file, which is the version a crash
        // would have left behind.
        drop(db);
        let db = Db::open(&path, options()).expect("reopen");
        assert_eq!(
            scanned(&db, 0, model.len()),
            expected(&model, 0, model.len()),
            "seed {seed}: the key set disagreed after the last reopen"
        );
        // Every key deleted along the way still has a node in the plane,
        // since nothing is ever unlinked, so the plane knows more keys
        // than the map has and a scan walks past the difference. That is
        // the design and this is what says it is still true.
        assert!(
            db.ordered_keys().expect("a plane") >= model.len(),
            "seed {seed}: the plane forgot a key"
        );
    }
}
