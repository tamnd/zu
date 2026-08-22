//! End to end tests: the four record operations, the graph plane, and
//! what survives a close and reopen.
//!
//! These run against a real file, because the thing worth testing is the
//! boundary between memory and disk. A test that only exercised the
//! in-memory tail would pass whatever the log did.

use std::sync::Arc;

use zu2::{Db, Direction, Durability, Options};

fn options(durability: Durability) -> Options {
    Options {
        durability,
        // Small enough that the tests cross page boundaries and exercise
        // the read-only region rather than staying in the mutable window.
        index_buckets: 1 << 10,
        max_pages: 64,
        max_nodes: 1 << 16,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

#[test]
fn the_four_operations_agree_with_a_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    let mut expected = std::collections::HashMap::new();
    let mut out = Vec::new();

    for i in 0..2000u32 {
        s.upsert(&key(i), &value(i)).expect("upsert");
        expected.insert(key(i), value(i));
    }
    // Overwrite half of them with a value of the same length, which is
    // the case the in-place path takes, and a quarter with a longer one,
    // which it cannot.
    for i in (0..2000u32).step_by(2) {
        let fresh = value(i + 100_000);
        s.upsert(&key(i), &fresh).expect("update");
        expected.insert(key(i), fresh);
    }
    for i in (0..2000u32).step_by(4) {
        let long = vec![b'x'; 200];
        s.upsert(&key(i), &long).expect("grow");
        expected.insert(key(i), long);
    }
    for i in (0..2000u32).step_by(7) {
        assert!(s.delete(&key(i)).expect("delete"), "delete missed {i}");
        expected.remove(&key(i));
    }

    for i in 0..2000u32 {
        let found = s.read(&key(i), &mut out).expect("read");
        match expected.get(&key(i)) {
            Some(want) => {
                assert!(found, "key {i} went missing");
                assert_eq!(&out, want, "key {i} has the wrong value");
            }
            None => assert!(!found, "deleted key {i} came back"),
        }
    }
    assert!(!s.read(b"absent", &mut out).expect("read"), "phantom key");
}

#[test]
fn a_read_modify_write_counts_without_losing_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    let mut scratch = Vec::new();
    for _ in 0..500 {
        s.rmw(b"counter", &mut scratch, |current, out| {
            let n = match current {
                Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("eight bytes")),
                None => 0,
            };
            out.extend_from_slice(&(n + 1).to_le_bytes());
        })
        .expect("rmw");
    }
    let mut out = Vec::new();
    assert!(s.read(b"counter", &mut out).expect("read"));
    assert_eq!(
        u64::from_le_bytes(out.try_into().expect("eight bytes")),
        500
    );
}

#[test]
fn what_was_acknowledged_is_there_after_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..500u32 {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        for i in (0..500u32).step_by(5) {
            s.delete(&key(i)).expect("delete");
        }
    }
    let db = Db::open(&path, options(Durability::Durable)).expect("open");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..500u32 {
        let found = s.read(&key(i), &mut out).expect("read");
        if i % 5 == 0 {
            assert!(!found, "deleted key {i} came back from the log");
        } else {
            assert!(found, "key {i} did not survive the reopen");
            assert_eq!(out, value(i), "key {i} came back wrong");
        }
    }
    // The reopened database has to be writable, and a key written after
    // recovery has to be findable next to the recovered ones.
    s.upsert(b"after", b"recovery").expect("upsert");
    assert!(s.read(b"after", &mut out).expect("read"));
    assert_eq!(out, b"recovery");
}

#[test]
fn a_graph_survives_a_reopen_with_its_edges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("g.zu2");
    let nodes = 400u32;
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..nodes {
            let id = s.add_node(&key(i)).expect("node");
            assert_eq!(id, i, "ids are handed out in creation order");
        }
        // A ring plus a chord, so every node has degree two out and a
        // few have more, which puts some neighbourhoods out of line.
        for i in 0..nodes {
            s.add_edge(i, (i + 1) % nodes).expect("edge");
            s.add_edge(i, (i * 7 + 1) % nodes).expect("edge");
        }
        // A hub, to force a block that doubles several times. From one,
        // because a self edge is a different question than this test is
        // asking.
        for i in 1..nodes {
            s.add_edge(0, i).expect("hub edge");
        }
        s.remove_edge(0, 3).expect("remove");
    }

    let db = Db::open(&path, options(Durability::Durable)).expect("open");
    let mut s = db.session();
    let mut scratch = Vec::new();
    assert_eq!(
        s.node_of(&key(11), &mut scratch).expect("node_of"),
        Some(11),
        "the key to id mapping did not survive"
    );
    assert_eq!(
        db.core().graph().nodes(),
        nodes,
        "the id counter did not survive"
    );
    let hub = s.neighbours(Direction::Out, 0, |n| n.to_vec());
    assert_eq!(
        hub.len(),
        (nodes - 2) as usize,
        "the hub lost edges: {} of {nodes}",
        hub.len()
    );
    assert!(!hub.contains(&3), "the removed edge came back");
    assert!(!hub.contains(&0), "a self edge appeared");
    assert!(hub.windows(2).all(|w| w[0] < w[1]), "neighbours unsorted");
    assert_eq!(s.degree(Direction::Out, 0), hub.len() as u32);
    // Every ring edge has a reverse, which is what makes an in-hop cost
    // the same as an out-hop.
    for i in 1..nodes {
        let back = s.neighbours(Direction::In, (i + 1) % nodes, |n| n.to_vec());
        assert!(back.contains(&i), "node {i} lost its reverse edge");
    }
}

#[test]
fn two_hops_are_the_distinct_neighbours_of_the_neighbours() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("h.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    for i in 0..64u32 {
        s.add_node(&key(i)).expect("node");
    }
    // A grid of eight by eight, linked right and down, so the two hop
    // set from a corner is small enough to write out by hand.
    for row in 0..8u32 {
        for col in 0..8u32 {
            let here = row * 8 + col;
            if col + 1 < 8 {
                s.add_edge(here, here + 1).expect("right");
            }
            if row + 1 < 8 {
                s.add_edge(here, here + 8).expect("down");
            }
        }
    }
    let mut seen = Vec::new();
    let mut first = Vec::new();
    let mut out = Vec::new();
    s.two_hop(Direction::Out, 0, &mut seen, &mut first, &mut out);
    out.sort_unstable();
    // From 0 the first hop is {1, 8} and the second is {2, 9} from 1 and
    // {9, 16} from 8, deduplicated.
    assert_eq!(out, vec![2, 9, 16]);
    assert!(
        seen.iter().all(|&w| w == 0),
        "the bitmap was left dirty for the next probe"
    );
    // Run it again on the same buffers, which is what a benchmark does,
    // and it has to give the same answer.
    s.two_hop(Direction::Out, 0, &mut seen, &mut first, &mut out);
    out.sort_unstable();
    assert_eq!(out, vec![2, 9, 16]);
}

#[test]
fn concurrent_writers_and_readers_agree_on_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(&dir.path().join("c.zu2"), options(Durability::Durable)).expect("create"),
    );
    let threads = 4u32;
    let per_thread = 1000u32;
    let mut handles = Vec::new();
    for t in 0..threads {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut s = db.session();
            for i in 0..per_thread {
                let k = key(t * per_thread + i);
                s.upsert(&k, &value(i)).expect("upsert");
            }
            // Read back what this thread wrote while the others are
            // still writing, which is the case the tentative bit and the
            // foreign chains exist for.
            let mut out = Vec::new();
            for i in 0..per_thread {
                let k = key(t * per_thread + i);
                assert!(s.read(&k, &mut out).expect("read"), "lost {k:?}");
                assert_eq!(out, value(i));
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..threads * per_thread {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out, value(i % per_thread), "key {i} has the wrong value");
    }
}

#[test]
fn a_record_that_left_memory_still_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("e.zu2"),
        Options {
            durability: Durability::Durable,
            // Two pages resident out of the eight or so this writes, so
            // most of what it reads back has to come off disk.
            memory_pages: 2,
            max_pages: 64,
            index_buckets: 1 << 12,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let big = vec![b'v'; 8192];
    let records = 1500u32;
    for i in 0..records {
        s.upsert(&key(i), &big).expect("upsert");
    }
    let mut out = Vec::new();
    for i in 0..records {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), big.len(), "key {i} came back short");
    }
}

#[test]
fn an_overflowing_bucket_keeps_every_key_through_updates() {
    // Sixteen buckets, eight entries each, so 128 slots hold 4000 keys.
    // Almost every write displaces an entry and chains behind it, which
    // is the case where a record that chains to the wrong address drops
    // whole keys out of the index without anything else noticing.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("o.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 16,
            max_pages: 1 << 12,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let records = 4000u32;
    for i in 0..records {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    // Three rounds of updates, so a key gets rewritten while it is deep
    // in someone else's chain as well as while it is at the head. The
    // value length changes every round, which refuses the in-place path
    // and forces the append and swing that the chaining rule governs.
    // The order matters: newest first, so that a chain truncated by an
    // update is not quietly repaired by a later insert of the keys it
    // dropped, which is what an ascending sweep would do.
    for round in 1..=3u32 {
        let expect = |i: u32| {
            let mut v = value(i);
            v.resize(v.len() + round as usize, b'r');
            v
        };
        for i in (0..records).rev() {
            s.upsert(&key(i), &expect(i)).expect("update");
        }
        let mut out = Vec::new();
        for i in 0..records {
            assert!(
                s.read(&key(i), &mut out).expect("read"),
                "lost {i} in {round}"
            );
            assert_eq!(out, expect(i), "stale {i} in {round}");
        }
    }
}
