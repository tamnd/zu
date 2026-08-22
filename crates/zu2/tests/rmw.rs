use std::sync::Arc;
use zu2::{Db, Durability, Options};

#[test]
fn threads_counting_on_one_key_do_not_lose_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(
            &dir.path().join("rmw.zu2"),
            Options {
                durability: Durability::Async,
                max_pages: 64,
                ..Options::default()
            },
        )
        .expect("create"),
    );
    const THREADS: u64 = 8;
    const EACH: u64 = 5000;
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let mut s = db.session();
                let mut scratch = Vec::new();
                for _ in 0..EACH {
                    s.rmw(b"counter", &mut scratch, |current, out| {
                        let n = match current {
                            Some(b) => u64::from_le_bytes(b.try_into().expect("eight")),
                            None => 0,
                        };
                        out.extend_from_slice(&(n + 1).to_le_bytes());
                    })
                    .expect("rmw");
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("worker");
    }
    let mut s = db.session();
    let mut out = Vec::new();
    assert!(s.read(b"counter", &mut out).expect("read"));
    let got = u64::from_le_bytes(out.as_slice().try_into().expect("eight"));
    assert_eq!(got, THREADS * EACH, "lost {} updates", THREADS * EACH - got);
}

/// A read modify write that arrives at a full log pays for a pass and
/// goes round again, the way an upsert has since #566. Before the fix it
/// handed the caller `LogFull` instead. Same shape as the edge case in
/// #587, and the last of the three write paths that was missing it.
#[test]
fn a_read_modify_write_on_a_full_log_makes_room_rather_than_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("full.zu2"),
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
    let mut scratch = Vec::new();
    // Records first, to press the span against the cap, and then a
    // counter whose length alternates so that every increment appends
    // rather than taking the in place path and never moving the tail.
    let padding = vec![b'p'; 4096];
    for op in 0..6_000u64 {
        let fat = &padding[..padding.len() - (op as usize / 512 % 2) * 8];
        s.upsert(format!("pad{:05}", op % 512).as_bytes(), fat)
            .expect("padding");
    }
    for op in 0..200_000u64 {
        s.rmw(b"counter", &mut scratch, |current, out| {
            let n = match current {
                Some(b) => u64::from_le_bytes(b[..8].try_into().expect("eight")),
                None => 0,
            };
            out.extend_from_slice(&(n + 1).to_le_bytes());
            if op % 2 == 0 {
                out.extend_from_slice(b"wider");
            }
        })
        .unwrap_or_else(|e| panic!("rmw at op {op}: {e}"));
    }
    let mut out = Vec::new();
    assert!(s.read(b"counter", &mut out).expect("read"));
    assert_eq!(
        u64::from_le_bytes(out[..8].try_into().expect("eight")),
        200_000
    );
    assert!(
        db.compaction()
            .passes
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "the log never filled, so nothing here was tested against a full one"
    );
}
