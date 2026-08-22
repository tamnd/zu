//! A seeded random operation mix against a `BTreeMap`, which is the
//! model every other test here is a hand written special case of.
//!
//! The scripted tests each pin one path: an update of the same length,
//! a delete of a key that has moved to the cold tier, a scan across a
//! compaction. What none of them do is interleave those paths in an
//! order nobody chose, and that interleaving is where the bugs found so
//! far have been (#537 was a load whose shape no test had, #557 was a
//! reopen over a promotion). So this drives the four operations and a
//! scan from a seeded generator, checks every answer against the map,
//! and reopens the database part way through so recovery is in the mix
//! rather than tested on its own.
//!
//! The options are sized so the interesting things happen: the mutable
//! window is a page, so the log laps and records go cold; compaction
//! runs; the index doubles from one bucket; and the scan plane is on.
//!
//! `ZU2_MODEL_SEEDS` and `ZU2_MODEL_OPS` widen it for a soak run.

use std::collections::BTreeMap;

use zu2::{Db, Durability, Options};

fn options() -> Options {
    Options {
        durability: Durability::Async,
        // From one bucket, so the doubling happens under the mix.
        index_buckets: env("ZU2_MODEL_BUCKETS", 1) as usize,
        max_pages: env("ZU2_MODEL_PAGES", 1 << 9) as usize,
        mutable_pages: 1,
        max_nodes: 1 << 16,
        compact_below: 0,
        ordered: true,
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

/// A key space small enough that a delete lands on something that is
/// there most of the time, and large enough to fill several pages.
fn key(i: u64) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

/// Lengths that cross the in place boundary both ways, since an update
/// of the same length is rewritten where it lies and a longer one is
/// appended, and those are different paths.
fn value(seed: u64) -> Vec<u8> {
    let len = 8 + (seed % 700) as usize;
    let mut v = format!("{seed:016x}").into_bytes();
    v.resize(len.max(16), b'v');
    v
}

fn env(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

/// Every key the model holds, read back one at a time, plus a read of a
/// key the model says is not there.
fn agrees(db: &Db, model: &BTreeMap<Vec<u8>, Vec<u8>>, keys: u64, seed: u64) {
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..keys {
        let k = key(i);
        let found = s.read(&k, &mut out).expect("read");
        match model.get(&k) {
            Some(want) => {
                assert!(found, "seed {seed}: key {i} went missing");
                assert_eq!(&out, want, "seed {seed}: key {i} has the wrong value");
            }
            None => assert!(!found, "seed {seed}: deleted key {i} came back"),
        }
    }
}

/// A scan from a random start, against what the map says the same range
/// holds. This is the check the plane exists for: the keys in order,
/// none skipped, none repeated, and each with its newest value.
fn scan_agrees(
    db: &Db,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: &[u8],
    count: usize,
    seed: u64,
    at: &str,
) {
    let mut s = db.session();
    let mut got: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    s.scan(start, count, |k, v| got.push((k.to_vec(), v.to_vec())))
        .expect("scan");
    let want: Vec<(Vec<u8>, Vec<u8>)> = model
        .range(start.to_vec()..)
        .take(count)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Keys first and by name, because a values first failure prints two
    // pages of bytes and says nothing about which record moved.
    let names = |rows: &[(Vec<u8>, Vec<u8>)]| -> Vec<String> {
        rows.iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            .collect()
    };
    assert_eq!(
        names(&got),
        names(&want),
        "seed {seed} at {at}: the keys of a scan of {count} from {} disagreed with the map",
        String::from_utf8_lossy(start)
    );
    for ((k, gv), (_, wv)) in got.iter().zip(want.iter()) {
        if gv != wv {
            // What the point read says about the same key, because a
            // scan reads through the hash index and a disagreement
            // between the two is a different bug from a disagreement
            // between both of them and the map.
            let mut point = Vec::new();
            let found = s.read(k, &mut point).expect("read");
            panic!(
                "seed {seed} at {at}: {} came back from the scan as {} bytes, from a point read as {} \
                 ({found}), and the map has {}",
                String::from_utf8_lossy(k),
                gv.len(),
                point.len(),
                wv.len()
            );
        }
    }
}

#[test]
fn a_random_mix_agrees_with_a_map_across_a_reopen() {
    let seeds = env("ZU2_MODEL_SEEDS", 4);
    let ops = env("ZU2_MODEL_OPS", 20_000);
    let keys = 4000u64;

    for seed in 0..seeds {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("model.zu2");
        let mut db = Db::create(&path, options()).expect("create");
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut state = 0x2545_f491_4f6c_dd1d ^ (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut out = Vec::new();

        for op in 0..ops {
            // Halfway through, close and open again, so everything the
            // second half does runs over a recovered database and the
            // model carries across the boundary.
            if op == ops / 2 {
                drop(db);
                db = Db::open(&path, options()).expect("reopen");
                agrees(&db, &model, keys, seed);
            }

            let roll = next(&mut state);
            let k = key(roll % keys);
            let mut s = db.session();
            match roll % 100 {
                // Writes, weighted so the key set grows and the log laps.
                0..=49 => {
                    let v = value(next(&mut state));
                    s.upsert(&k, &v).expect("upsert");
                    model.insert(k, v);
                }
                50..=59 => {
                    let gone = s.delete(&k).expect("delete");
                    assert_eq!(
                        gone,
                        model.remove(&k).is_some(),
                        "seed {seed}: delete of {} disagreed",
                        String::from_utf8_lossy(&k)
                    );
                }
                60..=94 => {
                    let found = s.read(&k, &mut out).expect("read");
                    match model.get(&k) {
                        Some(want) => {
                            assert!(
                                found,
                                "seed {seed}: {} went missing",
                                String::from_utf8_lossy(&k)
                            );
                            assert_eq!(
                                &out,
                                want,
                                "seed {seed}: {} has the wrong value",
                                String::from_utf8_lossy(&k)
                            );
                        }
                        None => assert!(
                            !found,
                            "seed {seed}: {} came back deleted",
                            String::from_utf8_lossy(&k)
                        ),
                    }
                }
                _ => {
                    let count = (next(&mut state) % 100 + 1) as usize;
                    drop(s);
                    scan_agrees(&db, &model, &k, count, seed, &format!("op {op}"));
                }
            }
        }

        // A pass over the log, then the whole key set again, so what
        // compaction moved is checked rather than assumed.
        db.compact().expect("compact");
        agrees(&db, &model, keys, seed);
        scan_agrees(
            &db,
            &model,
            b"",
            model.len().min(500),
            seed,
            "after the compaction",
        );

        // And once more from the file, which is the version a crash
        // would have left behind.
        drop(db);
        let db = Db::open(&path, options()).expect("reopen");
        agrees(&db, &model, keys, seed);
        scan_agrees(
            &db,
            &model,
            b"",
            model.len().min(500),
            seed,
            "after the last reopen",
        );
    }
}
