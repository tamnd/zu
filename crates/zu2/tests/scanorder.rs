//! A scan running over keys several threads are inserting between each
//! other's keys.
//!
//! `tests/racy.rs` gives each thread a prefix of its own, which is what
//! makes its expectations exact and also means two threads almost never
//! insert into the same gap in the key order. That gap is where #590
//! lived: the scan plane linked a node in front of a smaller key and
//! level zero came back out of order with a key in it twice.
//!
//! This is in a file of its own rather than beside the other one because
//! it runs eight writers and a scanner flat out, and a test binary runs
//! its tests in parallel: sharing a binary with the compaction test made
//! that one fail on a machine that was busy enough, which is a real
//! answer to the wrong question.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zu2::{Db, Durability, Options};

/// Small enough that the log laps and compaction runs underneath the
/// scan, since a scan that only ever walks the mutable window is not the
/// scan worth testing.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1,
        max_pages: 16,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        compact_below: 1 << 20,
        ordered: true,
        ..Options::default()
    }
}

/// Thread `t` owns every key whose number is `t` modulo the thread
/// count, so every insert lands between two keys other threads are
/// inserting. The scanner cannot say which
/// keys should be there, because it races the writers, but it can say
/// the two things that are true of a scan whatever it races: the keys
/// come back in order, and none of them comes back twice.
#[test]
fn a_scan_stays_in_order_while_threads_fill_each_other_s_gaps() {
    const THREADS: u32 = 8;
    const KEYS: u32 = 60_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::create(&dir.path().join("i.zu2"), options()).expect("create"));
    let stop = Arc::new(AtomicBool::new(false));

    let scanner = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut session = db.session();
            let mut rounds = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut last: Option<Vec<u8>> = None;
                let mut out_of_order = None;
                session
                    .scan(b"", KEYS as usize, |k, _| {
                        if let Some(last) = &last
                            && last.as_slice() >= k
                            && out_of_order.is_none()
                        {
                            out_of_order = Some((last.clone(), k.to_vec()));
                        }
                        last = Some(k.to_vec());
                    })
                    .expect("scan");
                assert!(
                    out_of_order.is_none(),
                    "a scan gave back {:?} and then {:?}",
                    out_of_order
                        .as_ref()
                        .map(|(a, _)| String::from_utf8_lossy(a).into_owned()),
                    out_of_order
                        .as_ref()
                        .map(|(_, b)| String::from_utf8_lossy(b).into_owned()),
                );
                rounds += 1;
            }
            rounds
        })
    };

    let writers: Vec<_> = (0..THREADS)
        .map(|t| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let mut session = db.session();
                let value = vec![b'v'; 16];
                let mut i = t;
                while i < KEYS {
                    session
                        .upsert(format!("k{i:07}").as_bytes(), &value)
                        .expect("upsert");
                    i += THREADS;
                }
            })
        })
        .collect();

    for (t, writer) in writers.into_iter().enumerate() {
        writer.join().unwrap_or_else(|_| panic!("writer {t}"));
    }
    stop.store(true, Ordering::Relaxed);
    let rounds = scanner.join().expect("scanner");
    println!("{rounds} scans while the writers ran");

    // And at rest, the whole key set once, in order and each key once.
    let mut session = db.session();
    let mut seen = Vec::new();
    session
        .scan(b"", KEYS as usize + 1, |k, _| seen.push(k.to_vec()))
        .expect("scan");
    let want: Vec<Vec<u8>> = (0..KEYS).map(|i| format!("k{i:07}").into_bytes()).collect();
    assert_eq!(seen.len(), want.len(), "the scan lost or repeated a key");
    assert_eq!(seen, want, "the scan came back out of order");
}
