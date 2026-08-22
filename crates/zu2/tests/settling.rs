//! A record that has settled into the cold tier is still a record the
//! index has to be able to find. See #596.

use std::collections::BTreeMap;

use zu2::{Db, Durability, Options};

/// xorshift64, so a failure is a seed and not a story.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

/// Long enough that a run of them laps the log, and a length that
/// varies so an update appends rather than being rewritten where the
/// record lies.
fn value(i: u64, salt: u64) -> Vec<u8> {
    let mut v = format!("field0={i}:{salt}").into_bytes();
    v.resize(3000 + (salt % 7) as usize * 16, b'v');
    v
}

/// The three things this needs, and it needs all three.
///
/// A small index, so the table doubles under the key set and buckets
/// fill on the way; a small log, so a pass runs and records settle into
/// the tier; and no read promotion, because a read that pulls a settled
/// record back into the log is a read that hides this.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 8,
        max_pages: 8,
        mutable_pages: 1,
        compact_below: 1 << 20,
        promote_reads: false,
        ..Options::default()
    }
}

/// A full bucket buries the key it displaces under the record doing the
/// displacing, which is fine when the buried record is in the log and
/// wrong when it is in the cold tier: a cold address is above every hot
/// one and has no `previous`, so the only route to it is the hot record
/// hanging above it, and that record can die and be dropped by a pass
/// rather than copied. The key is then in neither the index nor any
/// chain.
///
/// Before the fix this lost a key every second or third run, somewhere
/// past twenty thousand operations. It is not a race between two
/// threads: one writer, one background compactor, and the compactor is
/// only what settles the records and moves the floor.
#[test]
fn a_key_that_settled_into_the_tier_is_still_there() {
    const KEYS: u64 = 4096;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("settle.zu2"), options()).expect("create");
    let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut state = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::new();
    for op in 0..30_000u64 {
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
                assert_eq!(gone, model.remove(&k).is_some(), "op {op}: delete of {k}");
            }
            _ => {
                let _ = s.read(&key(k), &mut out).expect("read");
            }
        }
        // Often, because the window between the burial and the pass that
        // drops the record above is not wide.
        if op % 100 == 99 {
            for (k, v) in &model {
                assert!(
                    s.read(&key(*k), &mut out).expect("read"),
                    "op {op}: key {k} went missing"
                );
                assert_eq!(&out, v, "op {op}: key {k} came back a different value");
            }
        }
    }
}
