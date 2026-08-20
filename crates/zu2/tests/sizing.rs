//! What happens at the edges of how a database was sized.
//!
//! The graph plane has one number that cannot be changed after the fact,
//! `max_nodes`, because the array of chunk pointers is sized once so a
//! node lookup stays two loads. Everything here is about a call or a
//! reopen that runs into it, and about none of them costing more than
//! the call itself.

use zu2::{Db, Direction, Durability, Error, Options};

/// A small graph, so the ceiling is reachable in a test. `max_nodes` is
/// rounded up to the chunk size of 16384, which is what the capacity in
/// these assertions is.
fn options(max_nodes: usize) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 10,
        max_pages: 16,
        max_nodes,
        compact_below: 0,
        mutable_pages: 1,
        ..Options::default()
    }
}

#[test]
fn an_edge_the_graph_has_no_room_for_leaves_nothing_behind() {
    // #455. The record went down before the ids were checked, so a
    // rejected edge left its record on the log, and a replay could not
    // get past that record either. One call that failed the way it was
    // supposed to and the file could never be opened again.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("range.zu2");
    let nodes = 100u32;
    {
        let db = Db::create(&path, options(1 << 14)).expect("create");
        let mut session = db.session();
        for i in 0..nodes {
            session.add_node(format!("n{i}").as_bytes()).expect("node");
        }
        for i in 0..nodes {
            session.add_edge(i, (i + 1) % nodes).expect("ring");
        }
        let refused = session.add_edge(0, 99_999);
        assert!(
            matches!(refused, Err(Error::NodeOutOfRange { node: 99_999, .. })),
            "an id past the end was accepted: {refused:?}"
        );
        // Both ends, and both directions, since the check is on the pair.
        assert!(session.add_edge(99_999, 0).is_err(), "the source too");
        assert!(session.remove_edge(0, 99_999).is_err(), "removes too");
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options(1 << 14)).expect("the file did not survive a rejected edge");
    let mut session = db.session();
    for i in 0..nodes {
        let out = session.neighbours(Direction::Out, i, |n| n.to_vec());
        assert_eq!(out, vec![(i + 1) % nodes], "node {i} lost its ring edge");
    }
}

#[test]
fn reopening_with_less_room_than_the_file_needs_says_so() {
    // The other way in, and it needs no bad call at all. Every edge in
    // the file is fine and the options are the problem, so the error
    // names the number that would open it rather than repeating the one
    // the write path uses.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shrink.zu2");
    {
        let db = Db::create(&path, options(1 << 15)).expect("create");
        let mut session = db.session();
        // Past the first chunk, so a smaller table genuinely cannot hold
        // it rather than being rounded back up to the same capacity.
        for i in 0..20_000u32 {
            session.add_node(format!("n{i}").as_bytes()).expect("node");
        }
        session.add_edge(0, 19_999).expect("edge");
        db.sync().expect("sync");
    }
    match Db::open(&path, options(1 << 14)) {
        Err(Error::GraphTooSmall { needs, max }) => {
            assert_eq!(needs, 20_000, "the number it asked for is not usable");
            assert_eq!(max, 1 << 14);
        }
        Err(other) => panic!("a file that needs more room failed with {other:?}"),
        Ok(_) => panic!("a file that needs more room opened anyway"),
    }
    // And the file is not the problem, so the right options still work.
    let db = Db::open(&path, options(1 << 15)).expect("reopen");
    let mut session = db.session();
    assert_eq!(
        session.neighbours(Direction::Out, 0, |n| n.to_vec()),
        vec![19_999]
    );
}

#[test]
fn a_node_past_the_end_does_not_move_the_counter() {
    // `allocate` used to add and then look, so an `add_node` that could
    // not be served still moved the id counter, and a caller that kept
    // asking walked `nodes()` up past `capacity()` reporting nodes that
    // cannot exist.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("full.zu2"), options(1 << 14)).expect("create");
    let capacity = db.core().graph().capacity() as u32;
    let mut session = db.session();
    for i in 0..capacity {
        let node = session.add_node(format!("n{i}").as_bytes()).expect("node");
        assert_eq!(node, i, "ids are supposed to be dense");
    }
    for i in 0..10u32 {
        let refused = session.add_node(format!("over{i}").as_bytes());
        assert!(
            matches!(refused, Err(Error::NodeOutOfRange { .. })),
            "a node past the end was accepted: {refused:?}"
        );
    }
    assert_eq!(
        db.core().graph().nodes(),
        capacity,
        "refused allocations moved the counter anyway"
    );
}

/// `max_pages` has the same shape as `max_nodes`: a ceiling picked at
/// open time that a running database can walk into. Walking into it has
/// to cost the writes that do not fit and nothing else.
fn small_log() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 10,
        max_pages: 2,
        mutable_pages: 4,
        max_nodes: 1 << 10,
        compact_below: 0,
        ..Options::default()
    }
}

#[test]
fn a_full_log_can_still_be_made_durable() {
    // The tail allocator claimed its bytes and then found out the page
    // was past the end of the page table, so a refused append left the
    // tail sitting in a page that cannot exist. Every flush and every
    // durable commit takes the tail as its target, so from the first
    // refusal on, the whole database was undurable: `sync` reported the
    // tail page as missing, the background flusher took the same error
    // and stopped, and under `Async` that is every record since the last
    // flush, gone with no way to ask for them back.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("full.zu2");
    let value = vec![7u8; 1000];
    let mut wrote = 0u64;
    {
        let db = Db::create(&path, small_log()).expect("create");
        let mut session = db.session();
        for i in 0..100_000u64 {
            match session.upsert(&i.to_be_bytes(), &value) {
                Ok(()) => wrote += 1,
                Err(Error::LogFull { .. }) => break,
                Err(other) => panic!("{other}"),
            }
        }
        assert!(wrote > 0, "the log was full before it was written to");
        assert!(wrote < 100_000, "the log never filled up");
        db.sync().expect("a full log still has to reach the device");
    }
    let db = Db::open(&path, small_log()).expect("reopen");
    let mut session = db.session();
    let mut out = Vec::new();
    for i in 0..wrote {
        assert!(
            session.read(&i.to_be_bytes(), &mut out).expect("read"),
            "key {i} was accepted and then lost when the log filled"
        );
    }
}
