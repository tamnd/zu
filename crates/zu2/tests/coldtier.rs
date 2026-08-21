//! The cold tier: records that survive a pass move to the second file,
//! and everything about them keeps working from there.
//!
//! The shape every test here needs is a log that has lapped, because a
//! record only becomes cold by surviving a pass over the oldest region,
//! and a pass can only take pages below the read-only boundary. So the
//! rounds are sized to clear the mutable window several times over, the
//! same way `tests/compaction.rs` sizes its own.

use zu2::{Db, Durability, Options};

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 14,
        max_pages: 64,
        max_nodes: 1 << 16,
        // Passes happen where the test asks for one.
        compact_below: 0,
        mutable_pages: 1,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

/// Alternating lengths, for the reason `tests/compaction.rs` gives: an
/// update of the same length over a record above the boundary is an in
/// place rewrite and appends nothing, so a constant length makes how much
/// log a round produces depend on how busy the machine is.
fn value(i: u32, round: u32) -> Vec<u8> {
    let mut v = format!("{i:09}-{round:09}").into_bytes();
    v.resize(1000 + (round as usize % 2) * 8, b'x');
    v
}

/// What a key reads as, `None` when it is not there.
fn read(s: &mut zu2::Session<'_>, key: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    s.read(key, &mut out).expect("read").then_some(out)
}

/// A key in the hot namespace, which is what pushes the log forward.
fn churn(i: u32) -> Vec<u8> {
    format!("hot_{i:09}").into_bytes()
}

/// Loads `records` keys once and then rewrites a small hot set until the
/// log has lapped, which is the only shape that produces a cold record:
/// a pass has to reach a region whose records are still live. A workload
/// that rewrites everything leaves nothing live down there, which is why
/// the compaction tests never migrate a byte.
///
/// Ends with a durable write so the pass has a flushed region to work on
/// rather than racing the background thread, and then compacts.
fn lapped(db: &Db, records: u32) -> u32 {
    let mut s = db.session();
    for i in 0..records {
        s.upsert(&key(i), &value(i, 0)).expect("upsert");
    }
    for round in 0..40u32 {
        for i in 0..1000u32 {
            s.upsert(&churn(i), &value(i, round)).expect("upsert");
        }
    }
    s.set_durability(Durability::Durable);
    s.upsert(&churn(0), &value(0, 39)).expect("upsert");
    drop(s);
    db.compact().expect("compact");
    records
}

#[test]
fn a_record_that_survives_a_pass_moves_to_the_cold_tier() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options()).expect("create");
    let records = lapped(&db, 3000);

    assert!(
        db.cold_span() > 0,
        "a pass over a lapped log should have migrated something"
    );
    assert!(
        db.compaction()
            .migrated
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "and should have counted it"
    );

    let mut s = db.session();
    for i in 0..records {
        let got = read(&mut s, &key(i));
        assert_eq!(
            got.as_deref(),
            Some(value(i, 0).as_slice()),
            "key {i} came back wrong from the tier"
        );
    }
}

#[test]
fn an_update_to_a_cold_key_lands_in_the_hot_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options()).expect("create");
    let records = lapped(&db, 3000);
    assert!(db.cold_span() > 0, "nothing migrated");
    let cold = db.cold_span();

    let mut s = db.session();
    for i in 0..records {
        s.upsert(&key(i), b"rewritten").expect("upsert");
    }
    for i in 0..records {
        assert_eq!(
            read(&mut s, &key(i)).as_deref(),
            Some(b"rewritten".as_slice()),
            "key {i} did not take its update"
        );
    }
    assert_eq!(
        db.cold_span(),
        cold,
        "an update to a cold key should append in the hot log"
    );
}

/// A tombstone over a cold record, which is the case where the chain
/// crosses tiers and the answer is the hot record rather than the one
/// underneath it.
#[test]
fn a_removed_cold_key_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options()).expect("create");
    let records = lapped(&db, 3000);
    assert!(db.cold_span() > 0, "nothing migrated");

    let mut s = db.session();
    for i in (0..records).step_by(2) {
        s.delete(&key(i)).expect("delete");
    }
    drop(s);
    db.sync().expect("sync");

    let mut s = db.session();
    for i in 0..records {
        let got = read(&mut s, &key(i));
        if i % 2 == 0 {
            assert_eq!(got, None, "key {i} came back after being removed");
        } else {
            assert_eq!(got.as_deref(), Some(value(i, 0).as_slice()), "key {i}");
        }
    }
}

/// The whole point of the second file being a file: what is in it has to
/// come back. Both ways in, since a checkpointed reopen restores the
/// planes with cold addresses already in them and a scanned one has to
/// read the tier itself.
#[test]
fn cold_records_survive_a_reopen() {
    for checkpoint in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("z.zu2");
        let records = {
            let db = Db::create(
                &path,
                Options {
                    checkpoint_on_close: checkpoint,
                    ..options()
                },
            )
            .expect("create");
            let records = lapped(&db, 3000);
            assert!(db.cold_span() > 0, "nothing migrated");
            db.sync().expect("sync");
            records
        };

        let db = Db::open(&path, options()).expect("open");
        assert!(
            db.cold_span() > 0,
            "the tier came back empty with checkpoint {checkpoint}"
        );
        let mut s = db.session();
        for i in 0..records {
            assert_eq!(
                read(&mut s, &key(i)).as_deref(),
                Some(value(i, 0).as_slice()),
                "key {i} did not survive a reopen with checkpoint {checkpoint}"
            );
        }
    }
}

/// An update after the reopen, so the chain that crosses tiers is one
/// the reopen built rather than one the writes built.
#[test]
fn a_reopened_cold_key_takes_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    let records = {
        let db = Db::create(&path, options()).expect("create");
        let records = lapped(&db, 3000);
        assert!(db.cold_span() > 0, "nothing migrated");
        db.sync().expect("sync");
        records
    };

    {
        let db = Db::open(&path, options()).expect("open");
        let mut s = db.session();
        for i in (0..records).step_by(3) {
            s.upsert(&key(i), b"after the reopen").expect("upsert");
        }
        drop(s);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut s = db.session();
    for i in 0..records {
        let got = read(&mut s, &key(i));
        if i % 3 == 0 {
            assert_eq!(
                got.as_deref(),
                Some(b"after the reopen".as_slice()),
                "key {i} lost an update made after a reopen"
            );
        } else {
            assert_eq!(got.as_deref(), Some(value(i, 0).as_slice()), "key {i}");
        }
    }
}

/// The tier reclaims too, on its own schedule. Rewriting every key makes
/// everything in the tier garbage, and a pass over it should hand the
/// blocks back without losing a key.
#[test]
fn a_stale_tier_gives_its_blocks_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options()).expect("create");
    let records = lapped(&db, 3000);
    assert!(db.cold_span() > 0, "nothing migrated");
    let before = db.cold_disk_bytes().expect("cold disk bytes");

    let mut s = db.session();
    for round in 1..9 {
        for i in 0..records {
            s.upsert(&key(i), &value(i, round)).expect("upsert");
        }
    }
    s.set_durability(Durability::Durable);
    s.upsert(&key(0), &value(0, 8)).expect("upsert");
    drop(s);
    while db.compact().expect("compact") > 0 {}

    let after = db.cold_disk_bytes().expect("cold disk bytes");
    assert!(
        after < before,
        "a tier that is all garbage should shrink: {after} against {before}"
    );
    let mut s = db.session();
    for i in 0..records {
        assert_eq!(
            read(&mut s, &key(i)).as_deref(),
            Some(value(i, 8).as_slice()),
            "key {i} was lost by a cold pass"
        );
    }
}

/// The tier can be turned off, and then the engine is the one that was
/// there before it existed.
#[test]
fn without_the_tier_nothing_migrates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("z.zu2"),
        Options {
            cold_tier: false,
            ..options()
        },
    )
    .expect("create");
    let records = lapped(&db, 3000);
    assert_eq!(db.cold_span(), 0, "the tier was off");

    let mut s = db.session();
    for i in 0..records {
        assert_eq!(
            read(&mut s, &key(i)).as_deref(),
            Some(value(i, 0).as_slice()),
            "key {i}"
        );
    }
}
