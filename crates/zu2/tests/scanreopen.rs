//! What a reopen owes the key order.
//!
//! `tests/scanorder.rs` covers the scan plane while threads race in it,
//! which is where #590 lived. This covers the other way a key gets into
//! the plane twice, and it needs no threads at all: the plane is rebuilt
//! on open from the checkpoint's key list, and that rebuild goes through
//! [`Ordered::builder`], which appends without comparing. Its whole
//! point is to avoid the comparison, so anything that hands it a key it
//! already holds gets two nodes with the same key and a scan that
//! returns the same row twice.
//!
//! Found through go-ycsb: a single threaded workload E load followed by
//! a separate run process, which is a reopen, failed the scan integrity
//! check with "keys do not climb, user628... then user628...", the same
//! key twice in one scan. It reproduced on both scan paths, so it is not
//! the harness.

use zu2::{Db, Durability, Options};

/// The shape the workload generator makes: a fixed prefix and a hashed
/// number, so the keys arrive in no order and share long prefixes with
/// their neighbours, which is what the checkpoint's front coding is for.
fn key(i: u64) -> Vec<u8> {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 32;
    format!("usertable:user{:019}", x >> 1).into_bytes()
}

fn options() -> Options {
    Options {
        durability: Durability::Async,
        ordered: true,
        ..Options::default()
    }
}

/// Write a key set, close, open again, and walk the whole thing. Every
/// key once and in order, which is what the plane is for.
#[test]
fn a_reopened_scan_gives_every_key_once() {
    const N: u64 = 20_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("r.zu2");

    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        let value = vec![b'v'; 100];
        for i in 0..N {
            session.upsert(&key(i), &value).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut session = db.session();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    session
        .scan(b"", N as usize * 2, |k, _| seen.push(k.to_vec()))
        .expect("scan");

    let mut repeated = Vec::new();
    let mut backwards = Vec::new();
    for pair in seen.windows(2) {
        if pair[0] == pair[1] {
            repeated.push(String::from_utf8_lossy(&pair[0]).into_owned());
        } else if pair[0] > pair[1] {
            backwards.push((
                String::from_utf8_lossy(&pair[0]).into_owned(),
                String::from_utf8_lossy(&pair[1]).into_owned(),
            ));
        }
    }
    assert!(
        repeated.is_empty(),
        "{} keys came back twice, first {:?}",
        repeated.len(),
        repeated.first()
    );
    assert!(
        backwards.is_empty(),
        "{} steps went backwards, first {:?}",
        backwards.len(),
        backwards.first()
    );

    let mut want: Vec<Vec<u8>> = (0..N).map(key).collect();
    want.sort();
    want.dedup();
    assert_eq!(
        seen.len(),
        want.len(),
        "the reopened scan lost or gained keys"
    );
    assert_eq!(seen, want, "the reopened scan is not the key set");
}

/// The same again with the log lapping under the writes, so compaction
/// runs and records go out to the cold tier before the close. That is the
/// state the benchmark reopens in and the plain one above is not: ten
/// thousand kilobyte rows do not fit in the window, so what the
/// checkpoint captures and what the replay puts back are two different
/// sets of keys meeting in one plane.
#[test]
fn a_reopened_scan_gives_every_key_once_after_compaction() {
    const N: u64 = 20_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.zu2");
    let options = || Options {
        durability: Durability::Async,
        ordered: true,
        index_buckets: 1,
        max_pages: 16,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        compact_below: 1 << 20,
        ..Options::default()
    };

    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        let value = vec![b'v'; 1000];
        for i in 0..N {
            session.upsert(&key(i), &value).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut session = db.session();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    session
        .scan(b"", N as usize * 2, |k, _| seen.push(k.to_vec()))
        .expect("scan");

    let repeated: Vec<_> = seen
        .windows(2)
        .filter(|p| p[0] == p[1])
        .map(|p| String::from_utf8_lossy(&p[0]).into_owned())
        .collect();
    assert!(
        repeated.is_empty(),
        "{} keys came back twice, first {:?}",
        repeated.len(),
        repeated.first()
    );

    let mut want: Vec<Vec<u8>> = (0..N).map(key).collect();
    want.sort();
    want.dedup();
    assert_eq!(seen, want, "the reopened scan is not the key set");
}

/// The same, with a write after the reopen. A key written again after
/// the plane was rebuilt goes through insert rather than the builder, so
/// this is the pair of paths meeting: if the rebuild left the plane in a
/// state insert cannot see through, the second write adds a node beside
/// the first one instead of finding it.
#[test]
fn a_write_after_a_reopen_does_not_add_a_second_node() {
    const N: u64 = 5_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.zu2");

    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        for i in 0..N {
            session.upsert(&key(i), b"first").expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    {
        let mut session = db.session();
        for i in 0..N {
            session.upsert(&key(i), b"second").expect("upsert");
        }
    }

    let mut session = db.session();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    session
        .scan(b"", N as usize * 2, |k, _| seen.push(k.to_vec()))
        .expect("scan");

    let mut want: Vec<Vec<u8>> = (0..N).map(key).collect();
    want.sort();
    want.dedup();
    assert_eq!(
        seen.len(),
        want.len(),
        "rewriting every key changed how many the scan gives back"
    );
    assert_eq!(seen, want, "the scan is not the key set after a rewrite");
}

/// The shape the benchmark actually reopens: ten thousand kilobyte rows
/// under the default options, which is where the duplicate showed up.
/// Twenty thousand hundred byte rows do not reach it and neither do ten
/// thousand under a hand shrunk log, so the size that matters is the
/// live set against the defaults and not the key count.
#[test]
fn a_reopened_scan_of_a_benchmark_sized_load_gives_every_key_once() {
    const N: u64 = 10_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("b.zu2");

    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        let value = vec![b'v'; 1000];
        for i in 0..N {
            session.upsert(&key(i), &value).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut session = db.session();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    session
        .scan(b"", N as usize * 2, |k, _| seen.push(k.to_vec()))
        .expect("scan");

    let repeated: Vec<_> = seen
        .windows(2)
        .filter(|p| p[0] == p[1])
        .map(|p| String::from_utf8_lossy(&p[0]).into_owned())
        .collect();
    assert!(
        repeated.is_empty(),
        "{} keys came back twice, first {:?}",
        repeated.len(),
        repeated.first()
    );

    let mut want: Vec<Vec<u8>> = (0..N).map(key).collect();
    want.sort();
    want.dedup();
    assert_eq!(seen, want, "the reopened scan is not the key set");
}
