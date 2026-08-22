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

/// Readers walking the tier while a pass reclaims the front of it.
///
/// A pass copies what is still live above the region it is about to
/// release and then moves `begin` past the region, and a reader that
/// resolved an address just before that has an address that is about to
/// be a hole. The floor test in the chain walk is not enough on its own,
/// because it reads `begin` and then reads the record, and the pass can
/// land between the two. What closes it is waiting for the sessions that
/// were already inside an operation to leave before `begin` moves, and
/// this is the test that they are waited for.
///
/// Timing, so it is written to make the window as wide as it can: as
/// many readers as the session table allows, all of them over the cold
/// key set, and passes back to back for the whole of it.
#[test]
fn a_cold_pass_does_not_reclaim_under_a_reader() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(
            &dir.path().join("z.zu2"),
            Options {
                sessions: 32,
                ..options()
            },
        )
        .expect("create"),
    );
    let records = lapped(&db, 3000);
    assert!(
        db.cold_span() > 0,
        "nothing went cold, so this proves nothing"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..8u32)
        .map(|which| {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut s = db.session();
                let mut rounds = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..records {
                        let i = (i + which) % records;
                        assert_eq!(
                            read(&mut s, &key(i)).as_deref(),
                            Some(value(i, 0).as_slice()),
                            "key {i} came back wrong under a pass"
                        );
                    }
                    rounds += 1;
                }
                rounds
            })
        })
        .collect();

    // Enough churn between the passes to keep giving them something to
    // do, since a pass over a tier nothing has been added to is a pass
    // that returns immediately and never touches begin.
    {
        let mut s = db.session();
        for round in 0..20u32 {
            for i in 0..1000u32 {
                s.upsert(&churn(i), &value(i, round)).expect("upsert");
            }
            s.set_durability(Durability::Durable);
            s.upsert(&churn(0), &value(0, round)).expect("flush");
            s.set_durability(Durability::Async);
            db.compact().expect("compact");
        }
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        let rounds = reader.join().expect("reader");
        assert!(rounds > 0, "a reader did not finish a single round");
    }
}

/// A doubling that runs after a cold pass has taken space back.
///
/// The split reads the log to find out which side of the new table each
/// key belongs on, and the address it starts that read from can name a
/// region a pass has already reclaimed. Which floor it tests against is
/// the whole of it: a cold address is numerically above every hot one,
/// so the hot log's floor drops nothing, and the split walks into a hole
/// and fails the whole operation with `outside the cold tier`. #535.
///
/// The shape is a load big enough that the table doubles several times
/// after the tier has a floor above zero, which is what the scaling
/// example was doing when it found this.
#[test]
fn a_doubling_after_a_cold_pass_does_not_read_reclaimed_space() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("z.zu2"),
        Options {
            // Small enough that the load below doubles it several times,
            // which is the only way into the split path.
            index_buckets: 1 << 8,
            ..options()
        },
    )
    .expect("create");
    let records = lapped(&db, 3000);
    assert!(db.cold_span() > 0, "nothing migrated");

    let mut s = db.session();
    for round in 1..8u32 {
        for i in 0..records {
            // Rewriting the cold set is what puts a reclaimed address
            // under a live entry, and the fresh keys are what make the
            // table double while those entries are in it.
            s.upsert(&key(i), &value(i, round)).expect("upsert");
            s.upsert(&key(records + round * records + i), &value(i, round))
                .expect("upsert");
        }
        drop(s);
        db.compact().expect("compact");
        s = db.session();
    }
    for i in 0..records {
        assert!(read(&mut s, &key(i)).is_some(), "cold key {i} is gone");
    }
}

/// A scan after a reopen finds the keys whose only copy is cold.
///
/// A reopen without a checkpoint rebuilds the scan plane from what it
/// reads, and what it reads is two files. A key that has settled has its
/// hot copy reclaimed, so if the cold pass of recovery did not tell the
/// plane about it the database would come back with that key readable by
/// name and invisible to every scan, which is the worst shape a wrong
/// answer takes: nothing fails and the answer is short. See #548 and the
/// note_key in recover's install_cold.
#[test]
fn a_scan_after_a_reopen_finds_the_keys_that_only_the_cold_tier_holds() {
    for checkpoint in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scan.zu2");
        let records = {
            let db = Db::create(
                &path,
                Options {
                    ordered: true,
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

        let db = Db::open(
            &path,
            Options {
                ordered: true,
                ..options()
            },
        )
        .expect("open");
        let mut s = db.session();
        let mut seen = Vec::new();
        s.scan(&key(0), records as usize, |k, _| seen.push(k.to_vec()))
            .expect("scan");
        let want: Vec<Vec<u8>> = (0..records).map(key).collect();
        assert_eq!(
            seen.len(),
            want.len(),
            "a scan came back {} keys short with checkpoint {checkpoint}",
            want.len() - seen.len().min(want.len())
        );
        assert_eq!(seen, want, "with checkpoint {checkpoint}");
    }
}
