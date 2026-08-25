//! Re-adding an edge that is already there.
//!
//! An add record is live when the adjacency has the edge, and nothing
//! anywhere asks whether the adjacency got it from this record or from
//! one of the two hundred below it. So a workload that re-asserts its
//! edges, which is what every `MERGE` shaped write does, leaves a live
//! record per operation rather than per edge, every pass copies all of
//! them forward, and the log grows without bound over a graph that is
//! not growing at all.

use std::sync::atomic::Ordering;

use zu2::{Db, Durability, Options};

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 8,
        max_pages: 64,
        mutable_pages: 1,
        max_nodes: 1 << 8,
        compact_below: 1 << 20,
        ..Options::default()
    }
}

/// The live set of a fixed graph does not grow with the number of times
/// its edges are written.
#[test]
fn re_adding_an_edge_does_not_grow_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("e.zu2"), options()).expect("create");
    {
        let mut s = db.session();
        for i in 0..2u32 {
            s.add_node(format!("n{i}").as_bytes()).expect("node");
        }
    }

    // One edge, and then the same edge again a hundred thousand times.
    // Compacting throughout, so the span below is what a pass could not
    // get rid of rather than what has not been looked at yet.
    let mut s = db.session();
    s.add_edge(0, 1).expect("add");
    db.compact().expect("compact");
    let settled = db.log_span();
    for i in 0..100_000 {
        s.add_edge(0, 1).expect("add");
        if i % 10_000 == 0 {
            db.compact().expect("compact");
        }
    }
    drop(s);
    for _ in 0..8 {
        db.compact().expect("compact");
    }

    println!(
        "span after one edge {settled}, after a hundred thousand re-adds {}, \
         migrated {}",
        db.log_span(),
        db.compaction().migrated.load(Ordering::Relaxed),
    );
    assert!(
        db.log_span() <= settled + (1 << 20),
        "a hundred thousand adds of one edge left {} bytes of log against \
         the {settled} one add left, so a re-add costs a live record rather \
         than nothing",
        db.log_span()
    );
}
