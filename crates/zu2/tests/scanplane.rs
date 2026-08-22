//! The scan plane end to end: a range scan over a real database, what
//! a delete does to one, and what a reopen owes it. See #548.

use zu2::{Db, Durability, Options};

fn key(i: usize) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn value(i: usize) -> Vec<u8> {
    format!("field0={i}").into_bytes()
}

fn options() -> Options {
    Options {
        durability: Durability::Async,
        ordered: true,
        index_buckets: 1 << 12,
        compact_below: 0,
        checkpoint_on_close: false,
        ..Options::default()
    }
}

fn loaded(path: &std::path::Path, records: usize) -> Db {
    let db = Db::create(path, options()).unwrap();
    {
        let mut s = db.session();
        // Scattered, so the plane is doing the ordering rather than the
        // insertion order doing it.
        for i in 0..records {
            let at = (i * 7919) % records;
            s.upsert(&key(at), &value(at)).unwrap();
        }
    }
    db
}

fn collect(db: &Db, start: &[u8], count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut got = Vec::new();
    let mut s = db.session();
    s.scan(start, count, |k, v| got.push((k.to_vec(), v.to_vec())))
        .unwrap();
    got
}

#[test]
fn a_scan_hands_back_the_records_that_follow_a_key_in_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = loaded(&dir.path().join("scan.zu2"), 2000);
    let got = collect(&db, &key(500), 50);
    let want: Vec<(Vec<u8>, Vec<u8>)> = (500..550).map(|i| (key(i), value(i))).collect();
    assert_eq!(got, want);
}

#[test]
fn a_scan_that_starts_between_two_keys_starts_at_the_one_above() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("between.zu2"), options()).unwrap();
    {
        let mut s = db.session();
        for i in (0..100).step_by(2) {
            s.upsert(&key(i), &value(i)).unwrap();
        }
    }
    let got = collect(&db, &key(11), 3);
    let want: Vec<(Vec<u8>, Vec<u8>)> = [12, 14, 16].iter().map(|&i| (key(i), value(i))).collect();
    assert_eq!(got, want);
}

#[test]
fn a_scan_stops_at_the_end_of_the_key_set_rather_than_at_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = loaded(&dir.path().join("short.zu2"), 100);
    let got = collect(&db, &key(90), 50);
    assert_eq!(got.len(), 10);
    assert_eq!(got[0].0, key(90));
    assert_eq!(got[9].0, key(99));
    // And past the end there is nothing at all.
    assert!(collect(&db, &key(1000), 50).is_empty());
}

#[test]
fn a_deleted_key_is_walked_past_and_does_not_count_against_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = loaded(&dir.path().join("deleted.zu2"), 200);
    {
        let mut s = db.session();
        for i in 50..60 {
            assert!(s.delete(&key(i)).unwrap());
        }
    }
    // Ten records asked for from a range whose first ten are gone, so
    // the answer is the ten above them and not an answer of nothing.
    let got = collect(&db, &key(50), 10);
    let want: Vec<Vec<u8>> = (60..70).map(key).collect();
    assert_eq!(got.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), want);
    // The node stays, which is what the plane says about itself.
    assert_eq!(db.ordered_keys(), Some(200));
}

#[test]
fn a_scan_sees_the_newest_value_of_every_key_it_reaches() {
    let dir = tempfile::tempdir().unwrap();
    let db = loaded(&dir.path().join("updated.zu2"), 200);
    {
        let mut s = db.session();
        for i in 100..110 {
            s.upsert(&key(i), b"rewritten").unwrap();
        }
    }
    let got = collect(&db, &key(100), 10);
    for (_, v) in &got {
        assert_eq!(v.as_slice(), b"rewritten");
    }
}

#[test]
fn a_database_opened_without_the_plane_refuses_a_scan_rather_than_answering_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(
        &dir.path().join("plain.zu2"),
        Options {
            durability: Durability::Async,
            checkpoint_on_close: false,
            ..Options::default()
        },
    )
    .unwrap();
    let mut s = db.session();
    s.upsert(b"a", b"1").unwrap();
    let refused = s.scan(b"", 10, |_, _| unreachable!("nothing to hand over"));
    assert!(refused.is_err(), "a scan with no plane has to be an error");
    assert!(db.ordered_bytes().is_none());
}

#[test]
fn a_reopen_gets_the_key_order_back_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.zu2");
    {
        let db = loaded(&path, 2000);
        let mut s = db.session();
        for i in 300..320 {
            s.delete(&key(i)).unwrap();
        }
        db.sync().unwrap();
    }
    let db = Db::open(&path, options()).unwrap();
    assert_eq!(db.ordered_keys(), Some(2000));
    let got = collect(&db, &key(295), 10);
    let want: Vec<Vec<u8>> = [295, 296, 297, 298, 299]
        .into_iter()
        .chain(320..325)
        .map(key)
        .collect();
    assert_eq!(got.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), want);
}

#[test]
fn a_checkpoint_carries_the_key_set_so_a_scanning_database_opens_from_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpointed.zu2");
    {
        let db = Db::create(
            &path,
            Options {
                checkpoint_on_close: true,
                ..options()
            },
        )
        .unwrap();
        let mut s = db.session();
        for i in 0..500 {
            s.upsert(&key(i), &value(i)).unwrap();
        }
        drop(s);
        db.sync().unwrap();
    }
    // The checkpoint carries the key set front coded, so the open adopts
    // it and reads nothing above the boundary, and the plane is whole
    // rather than empty.
    let db = Db::open(&path, options()).unwrap();
    assert_eq!(
        db.recovered()
            .records
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the open read records, so it did not take the checkpoint"
    );
    assert_eq!(db.ordered_keys(), Some(500));
    let got = collect(&db, &key(0), 500);
    assert_eq!(got.len(), 500);
    let want: Vec<Vec<u8>> = (0..500).map(key).collect();
    assert_eq!(got.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), want);
}

#[test]
fn a_checkpoint_taken_without_a_plane_is_refused_by_an_open_that_wants_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("planeless.zu2");
    {
        // Written with no plane, so the checkpoint beside it says
        // nothing about key order.
        let db = Db::create(
            &path,
            Options {
                durability: Durability::Async,
                checkpoint_on_close: true,
                ..Options::default()
            },
        )
        .unwrap();
        let mut s = db.session();
        for i in 0..300 {
            s.upsert(&key(i), &value(i)).unwrap();
        }
        drop(s);
        db.sync().unwrap();
    }
    // Adopting it would open a database whose index is full and whose
    // key order is empty, and every scan would answer nothing. So the
    // open turns it down and reads the log, which costs time and never
    // an answer.
    let db = Db::open(&path, options()).unwrap();
    assert!(
        db.recovered()
            .records
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 300,
        "the open took a checkpoint that could not describe its key order"
    );
    assert_eq!(db.ordered_keys(), Some(300));
    assert_eq!(collect(&db, &key(0), 300).len(), 300);
}

#[test]
fn the_plane_costs_what_it_says_it_costs() {
    let dir = tempfile::tempdir().unwrap();
    let db = loaded(&dir.path().join("bytes.zu2"), 50_000);
    let bytes = db.ordered_bytes().unwrap();
    // A node is a header, its links and a sixteen byte key, so fifty
    // thousand of them is well under ten megabytes and well over zero.
    assert!(bytes > 0, "the plane reports nothing");
    assert!(bytes < 10 << 20, "the plane is holding {bytes} bytes");
    assert_eq!(db.ordered_keys(), Some(50_000));
}

#[test]
fn threads_scanning_while_others_write_see_an_order_and_never_a_gap() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(loaded(&dir.path().join("racing.zu2"), 5000));
    let mut threads = Vec::new();
    for t in 0..4 {
        let db = Arc::clone(&db);
        threads.push(std::thread::spawn(move || {
            let mut s = db.session();
            for i in 0..2000 {
                s.upsert(&key(5000 + t * 2000 + i), b"late").unwrap();
            }
        }));
    }
    for _ in 0..4 {
        let db = Arc::clone(&db);
        threads.push(std::thread::spawn(move || {
            for _ in 0..200 {
                let mut last: Option<Vec<u8>> = None;
                let mut seen = 0;
                let mut s = db.session();
                s.scan(&key(0), 500, |k, _| {
                    if let Some(last) = &last {
                        assert!(last.as_slice() < k, "the walk went backwards");
                    }
                    last = Some(k.to_vec());
                    seen += 1;
                })
                .unwrap();
                assert_eq!(seen, 500, "a scan of a key set this big came up short");
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(db.ordered_keys(), Some(13_000));
}
