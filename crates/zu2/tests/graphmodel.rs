//! A seeded random edge mix against a pair of sets, which is to the
//! graph plane what `tests/model.rs` is to the record plane.
//!
//! The scripted graph tests each build one shape and check it: a ring, a
//! hub, a grid, a ring with its chords removed. What none of them do is
//! add and remove edges in an order nobody chose while the log laps
//! underneath, and that interleaving is where the bugs have been. An
//! edge is a bit in a block the next edge rewrites and the block doubles
//! in place, so the paths that matter are the ones a fixed shape never
//! takes twice: a remove that empties a block, an add that refills one
//! that was emptied, a doubling under a node whose old block a reader
//! still holds, and a reopen over the middle of all of it.
//!
//! The model is exact. Every edge this test believes in it wrote, so a
//! neighbour list that disagrees is the plane losing an edge rather than
//! a race in the test.
//!
//! `ZU2_GRAPH_SEEDS` and `ZU2_GRAPH_OPS` widen it for a soak run.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use zu2::{Db, Direction, Durability, Options};

/// Small enough that the log laps several times over during a run, so
/// the edge records the reopen replays are ones compaction has already
/// been over rather than ones still sitting in the tail.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 8,
        max_pages: env("ZU2_GRAPH_PAGES", 8) as usize,
        mutable_pages: 1,
        max_nodes: 1 << 12,
        // Low enough that the background thread is always passing over
        // the log rather than waiting for it to grow.
        compact_below: 1 << 20,
        ..Options::default()
    }
}

/// xorshift64, so a failure is a seed and not a story.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn env(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn key(i: u32) -> Vec<u8> {
    format!("node{i:07}").into_bytes()
}

/// An edge record is nine bytes, so a run of them moves the tail by a
/// page every few hundred thousand operations and nothing this test
/// cares about would ever happen. These carry the log instead: one fat
/// record an operation over a small key set, so the tail runs, the old
/// pages go read only, compaction has something to do, and the edge
/// records end up behind a pass rather than sitting in the tail where a
/// reopen would find them untouched.
fn pad(i: u64) -> Vec<u8> {
    format!("pad{:05}", i % 512).into_bytes()
}

/// The whole graph, both directions, against the model. Both are asked
/// for because an edge lives in two blocks and a plane that wrote one of
/// them and not the other answers an out hop correctly and an in hop
/// with nothing, which is the failure a one directional check misses.
fn agrees(db: &Db, out: &[BTreeSet<u32>], into: &[BTreeSet<u32>], nodes: u32, seed: u64, at: &str) {
    let mut s = db.session();
    let mut got = Vec::new();
    for n in 0..nodes {
        for (direction, model) in [(Direction::Out, out), (Direction::In, into)] {
            s.neighbours_into(direction, n, &mut got);
            let want: Vec<u32> = model[n as usize].iter().copied().collect();
            assert_eq!(
                got,
                want,
                "seed {seed} at {at}: node {n} {direction:?} disagreed, \
                 {} neighbours against {}",
                got.len(),
                want.len()
            );
            assert_eq!(
                s.degree(direction, n),
                want.len() as u32,
                "seed {seed} at {at}: node {n} {direction:?} degree disagreed with its own list"
            );
        }
    }
}

/// The distinct nodes two hops out, worked out from the model the slow
/// way. The plane has its own bitmap for this and reuses it across
/// probes, so what is being checked is that the reuse leaves nothing
/// behind as much as that the walk visits the right nodes.
fn two_hop_model(out: &[BTreeSet<u32>], node: u32) -> BTreeSet<u32> {
    let mut far = BTreeSet::new();
    for near in &out[node as usize] {
        far.extend(out[*near as usize].iter().copied());
    }
    far
}

#[test]
fn a_random_edge_mix_agrees_with_a_pair_of_sets_across_a_reopen() {
    let seeds = env("ZU2_GRAPH_SEEDS", 3);
    let ops = env("ZU2_GRAPH_OPS", 20_000);
    // Small enough that a remove lands on an edge that is there most of
    // the time, and large enough that the blocks double a few times.
    const NODES: u32 = 192;

    for seed in 0..seeds {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("g.zu2");
        let mut db = Db::create(&path, options()).expect("create");
        let mut out: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); NODES as usize];
        let mut into: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); NODES as usize];
        let mut state = 0x2545_f491_4f6c_dd1d ^ (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut seen = Vec::new();
        let mut first = Vec::new();
        let mut far = Vec::new();
        let padding = vec![b'p'; 4096];

        {
            let mut s = db.session();
            for i in 0..NODES {
                let id = s.add_node(&key(i)).expect("node");
                assert_eq!(id, i, "ids are handed out in creation order");
            }
        }

        for op in 0..ops {
            // Halfway through, close and open again, so the second half
            // runs over a graph that was rebuilt from the log and the
            // model carries across the boundary.
            if op == ops / 2 {
                drop(db);
                db = Db::open(&path, options()).expect("reopen");
                agrees(&db, &out, &into, NODES, seed, "the reopen");
            }

            let roll = next(&mut state);
            let src = (roll >> 8) as u32 % NODES;
            // Skewed, so a few nodes carry most of the edges and their
            // blocks double while the rest stay in the first one.
            let dst = match roll % 4 {
                0 => (roll >> 40) as u32 % 8,
                _ => (roll >> 40) as u32 % NODES,
            };
            let mut s = db.session();
            // The length alternates from one lap of the padding keys to
            // the next, because an update of the same length over a
            // record in the mutable window is rewritten where it lies
            // and appends nothing, so a padding key of a fixed length
            // stops moving the tail after the first lap. Per lap rather
            // than per operation, since a key comes round every 512 and
            // per operation would hand it the same length every time.
            let fat = &padding[..padding.len() - (op as usize / 512 % 2) * 8];
            s.upsert(&pad(op), fat).expect("padding");
            match roll % 100 {
                0..=54 => {
                    s.add_edge(src, dst).expect("add");
                    out[src as usize].insert(dst);
                    into[dst as usize].insert(src);
                }
                55..=79 => {
                    s.remove_edge(src, dst).expect("remove");
                    out[src as usize].remove(&dst);
                    into[dst as usize].remove(&src);
                }
                80..=89 => {
                    s.neighbours_into(Direction::Out, src, &mut far);
                    let want: Vec<u32> = out[src as usize].iter().copied().collect();
                    assert_eq!(far, want, "seed {seed} op {op}: node {src} out");
                }
                90..=95 => {
                    s.neighbours_into(Direction::In, dst, &mut far);
                    let want: Vec<u32> = into[dst as usize].iter().copied().collect();
                    assert_eq!(far, want, "seed {seed} op {op}: node {dst} in");
                }
                _ => {
                    s.two_hop(Direction::Out, src, &mut seen, &mut first, &mut far);
                    let mut got = far.clone();
                    got.sort_unstable();
                    let want: Vec<u32> = two_hop_model(&out, src).into_iter().collect();
                    assert_eq!(
                        got, want,
                        "seed {seed} op {op}: two hops from {src} disagreed"
                    );
                    assert_eq!(
                        far.len(),
                        got.len(),
                        "seed {seed} op {op}: two hops from {src} gave a node twice"
                    );
                }
            }
        }

        // The run is only worth anything if the log moved far enough for
        // a pass to have happened, which depends on the padding rather
        // than on the edges, so this says so rather than assuming it.
        let passes = db.compaction().passes.load(Ordering::Relaxed);
        println!(
            "seed {seed}: passes {passes}, copied {}, span {} bytes",
            db.compaction().copied.load(Ordering::Relaxed),
            db.log_span(),
        );
        assert!(
            passes > 0,
            "seed {seed}: the log never lapped, so nothing here was tested over a compaction"
        );

        // A pass over the log, so what it decided about the edge records
        // is checked rather than assumed, and then the whole graph.
        db.compact().expect("compact");
        agrees(&db, &out, &into, NODES, seed, "the compaction");

        // And once more from the file, which is the version a crash
        // would have left behind.
        drop(db);
        let db = Db::open(&path, options()).expect("reopen");
        agrees(&db, &out, &into, NODES, seed, "the last reopen");
        let mut s = db.session();
        let mut scratch = Vec::new();
        for i in 0..NODES {
            assert_eq!(
                s.node_of(&key(i), &mut scratch).expect("node_of"),
                Some(i),
                "seed {seed}: the key of node {i} did not survive the reopen"
            );
        }
    }
}

/// The regression for #587: an edge write that arrives at a full log
/// pays for a pass and goes round again, the way a record write has
/// since #566, rather than handing the caller a failure it can only
/// answer by sleeping and asking for the same edge.
///
/// The background compactor is off, so the only thread that can make
/// room here is the writer itself, and the padding is what keeps the
/// span pressed against the cap. Before the fix this failed with
/// `LogFull { span: 4, max: 3 }` somewhere in the first few thousand
/// edges.
#[test]
fn an_edge_write_on_a_full_log_makes_room_rather_than_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("full.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 8,
            max_pages: 8,
            mutable_pages: 1,
            max_nodes: 1 << 10,
            // Off, so nothing but this thread can reclaim anything.
            compact_below: 0,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    const NODES: u32 = 64;
    for i in 0..NODES {
        s.add_node(&key(i)).expect("node");
    }
    // First the log is filled with records, which make room for
    // themselves as they go and leave the span pressed against the cap.
    let padding = vec![b'p'; 4096];
    for op in 0..6_000u64 {
        let fat = &padding[..padding.len() - (op as usize / 512 % 2) * 8];
        s.upsert(&pad(op), fat).expect("padding");
    }
    let filled = db.log_span();
    // Then nothing but edges, which is the case that had no retry: an
    // edge is nine bytes of payload, so it is never the write that fills
    // a page on its own, but a long enough run of them walks the tail
    // over the cap that the records left it at.
    for op in 0..400_000u64 {
        let src = (op % u64::from(NODES)) as u32;
        let dst = ((op * 7 + 1) % u64::from(NODES)) as u32;
        s.add_edge(src, dst)
            .unwrap_or_else(|e| panic!("edge at op {op}, span was {filled}: {e}"));
        if op % 3 == 0 {
            s.remove_edge(src, dst)
                .unwrap_or_else(|e| panic!("remove at op {op}: {e}"));
        }
    }
    // Says what it saw rather than only that it was unhappy. This test
    // failed once in a loaded full suite run and could not be reproduced
    // afterwards, and what was kept of the output did not include the
    // assertion text, so it is not known whether this is the line that
    // fired or one of the unwraps above it. The counters cost nothing to
    // print and the next failure will not be a guess. #763.
    let passes = db.compaction().passes.load(Ordering::Relaxed);
    assert!(
        passes > 0,
        "the log never filled, so nothing here was tested against a full one: \
         span {} after the padding filled it to {filled}",
        db.log_span(),
    );
}
