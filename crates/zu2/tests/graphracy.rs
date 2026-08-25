//! Several sessions writing edges at once while a compaction runs under
//! them and other threads walk the neighbourhoods they are changing.
//!
//! `tests/graphmodel.rs` is the single threaded half and it is where the
//! ordering and reopen bugs turn up. Nothing was writing edges from more
//! than one thread, which leaves the plane's one moving part uncovered:
//! a neighbourhood is a block that doubles in place, so a hop that races
//! a doubling is reading storage that is being replaced underneath it.
//! `Session::neighbours` hands the caller a slice of that storage and
//! keeps it alive with the epoch, and this is the shape that leans on
//! it.
//!
//! Each thread owns a contiguous run of node ids and writes edges only
//! inside its own run, so its model is exact without a lock anywhere
//! near the engine: nobody else can touch a node it is checking. The
//! first node of each run is a hub and collects an edge to most of the
//! run, which is what makes a block double over and over rather than
//! once.
//!
//! One node is not owned by anybody. Every thread links its own nodes
//! to the sink, so four threads write the sink's inward neighbourhood at
//! once, through four different edge order stripes, and the only thing
//! keeping them off each other is the neighbourhood's own lock. That is
//! the case the rest of this file does not reach: elsewhere a
//! neighbourhood has one writer and many readers, and here it has four
//! writers. The model stays exact because a thread only ever links its
//! own node ids, so no two threads write the same neighbour.
//!
//! The threads also read each other's hubs, and there the model says
//! nothing, since the owner is changing it. What is asserted there is
//! what has to hold of any answer whatever the writer is doing: sorted,
//! no duplicates, and every id inside the owner's run. A block half
//! replaced, a stale length against a new block or a doubling that
//! copied the wrong half all break one of those three.
//!
//! `ZU2_GRAPH_OPS` sets how many operations a thread does.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zu2::{Db, Direction, Durability, Options};

const THREADS: u32 = 4;
/// Nodes a thread owns. Wide enough that a hub's block doubles about
/// eight times over the run.
const PER: u32 = 256;

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 8,
        // Four writers ahead of one compactor, over a live set of a
        // few megabytes. It only fits because a re-add of an edge that
        // is there no longer goes on the log: before #784 the rest of
        // these pages were duplicate add records that no pass could
        // reclaim, and a long run ended in `LogFull` rather than in an
        // answer.
        max_pages: 16,
        mutable_pages: 1,
        max_nodes: 1 << 12,
        // Low enough that the background thread is always passing over
        // the log, so a reclaim lands mid flight rather than between
        // rounds.
        compact_below: 1 << 20,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("n{i:07}").into_bytes()
}

/// How many padding keys a thread cycles over. Bounded, because a live
/// set that only grows fills the log rather than lapping it, and then
/// `LogFull` is the right answer and nothing here gets tested.
const PADS: u64 = 400;

/// The padding key of thread `t`, cycled.
fn pad_key(t: u32, i: u64) -> Vec<u8> {
    format!("p{t:02}_{:07}", i % PADS).into_bytes()
}

/// One fat record an operation, so the log actually moves. An edge
/// record is nine bytes and a run of them alone would leave the tail
/// where it started, and then no pass would ever run.
fn pad(i: u64) -> Vec<u8> {
    let mut v = format!("p{i:015}").into_bytes();
    v.resize(3000, b'p');
    v
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The first node of thread `t`'s run, which is its hub.
fn base(t: u32) -> u32 {
    t * PER
}

/// The node every thread links into, so one neighbourhood has four
/// writers. Its id is above every run, which is why appending it to a
/// thread's expected out list keeps that list sorted.
const SINK: u32 = THREADS * PER;

#[test]
fn concurrent_sessions_keep_their_own_edges_under_compaction() {
    let ops: u64 = std::env::var("ZU2_GRAPH_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30000);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::create(&dir.path().join("g.zu2"), options()).expect("create"));

    // Every node up front and from one thread, so the ids are known:
    // they are handed out in creation order and the runs below are
    // worked out from that rather than looked up.
    {
        let mut s = db.session();
        for i in 0..=SINK {
            let id = s.add_node(&key(i)).expect("node");
            assert_eq!(id, i, "ids are handed out in creation order");
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let compactor = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                db.compact().expect("compact");
                std::thread::yield_now();
            }
        })
    };

    let workers: Vec<_> = (0..THREADS)
        .map(|t| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let mut s = db.session();
                let mut out: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); PER as usize];
                // Which of this thread's nodes are linked to the sink.
                let mut sink: BTreeSet<u32> = BTreeSet::new();
                let mut state = 0x2545_f491_4f6c_dd1d ^ u64::from(t) << 32;
                let mut got = Vec::new();
                for op in 0..ops {
                    let draw = next(&mut state);
                    // A hub edge two times in three, so the block that
                    // doubles is doing most of the work, and an ordinary
                    // edge otherwise.
                    let src = if draw.is_multiple_of(3) {
                        (draw >> 40) as u32 % PER
                    } else {
                        0
                    };
                    let dst = (draw >> 20) as u32 % PER;
                    // A quarter of the edge writes go to the sink
                    // instead, which is the neighbourhood every thread
                    // is writing at once.
                    let to_sink = ((draw >> 32) as u32).is_multiple_of(4);
                    // The log has to move for a pass to have anything to
                    // do, and edges are too small to move it.
                    s.upsert(&pad_key(t, op), &pad(op)).expect("pad");
                    match draw % 8 {
                        0..=5 if to_sink => {
                            s.add_edge(base(t) + src, SINK).expect("add sink");
                            sink.insert(src);
                        }
                        0..=5 => {
                            s.add_edge(base(t) + src, base(t) + dst).expect("add");
                            out[src as usize].insert(dst);
                        }
                        6 if to_sink => {
                            s.remove_edge(base(t) + src, SINK).expect("remove sink");
                            sink.remove(&src);
                        }
                        6 => {
                            s.remove_edge(base(t) + src, base(t) + dst).expect("remove");
                            out[src as usize].remove(&dst);
                        }
                        _ => {
                            // This thread's own hub, against the model,
                            // exactly.
                            s.neighbours_into(Direction::Out, base(t), &mut got);
                            let mut want: Vec<u32> = out[0].iter().map(|&d| base(t) + d).collect();
                            if sink.contains(&0) {
                                // Above every run, so the list stays
                                // sorted with it on the end.
                                want.push(SINK);
                            }
                            assert_eq!(
                                got, want,
                                "thread {t} at op {op}: its own hub disagrees with the model"
                            );
                            assert_eq!(
                                s.degree(Direction::Out, base(t)) as usize,
                                want.len(),
                                "thread {t} at op {op}: its own hub's degree disagrees \
                                 with its neighbours"
                            );

                            // And somebody else's, where the model says
                            // nothing because its owner is writing it.
                            let other = (t + 1 + (draw >> 8) as u32 % (THREADS - 1)) % THREADS;
                            s.neighbours_into(Direction::Out, base(other), &mut got);
                            let low = base(other);
                            let high = base(other) + PER;
                            assert!(
                                got.windows(2).all(|w| w[0] < w[1]),
                                "thread {t} at op {op}: hub of {other} came back \
                                 unsorted or with a duplicate: {got:?}"
                            );
                            assert!(
                                got.iter().all(|&n| (n >= low && n < high) || n == SINK),
                                "thread {t} at op {op}: hub of {other} has a \
                                 neighbour outside {low}..{high} that is not \
                                 the sink: {got:?}"
                            );
                            // And the sink, which all four threads are
                            // writing at once. No model here either,
                            // for the same reason, but a neighbourhood
                            // with four writers on it has to come back
                            // sorted, without a repeat, and made of
                            // node ids that exist.
                            s.neighbours_into(Direction::In, SINK, &mut got);
                            assert!(
                                got.windows(2).all(|w| w[0] < w[1]),
                                "thread {t} at op {op}: the sink came back \
                                 unsorted or with a duplicate: {got:?}"
                            );
                            assert!(
                                got.iter().all(|&n| n < SINK),
                                "thread {t} at op {op}: the sink has a \
                                 neighbour that is not a node: {got:?}"
                            );

                            // No degree check here. It is a second call
                            // and the owner writes between the two, so a
                            // count off by one is this test racing rather
                            // than the plane disagreeing with itself. The
                            // check belongs on the hub above, where
                            // nobody else writes.
                        }
                    }
                }
                (out, sink)
            })
        })
        .collect();

    let held: Vec<_> = workers
        .into_iter()
        .enumerate()
        .map(|(t, w)| w.join().unwrap_or_else(|_| panic!("thread {t}")))
        .collect();
    stop.store(true, Ordering::Relaxed);
    compactor.join().expect("compactor");

    // The run is worth something only if the machinery it aims at ran,
    // and how much of it runs depends on how fast the machine got
    // through the operations, so this says so rather than assuming it.
    println!(
        "passes {}, migrated {}, log span {}",
        db.compaction().passes.load(Ordering::Relaxed),
        db.compaction().migrated.load(Ordering::Relaxed),
        db.log_span(),
    );
    assert!(
        db.compaction().passes.load(Ordering::Relaxed) > 0,
        "no pass ever ran, so nothing here was tested against a moving log"
    );

    // And the whole of it at rest, both directions. In as well as out,
    // because an edge lives in two blocks and a plane that wrote one and
    // not the other answers an out hop correctly and an in hop with
    // nothing.
    let mut s = db.session();
    let mut got = Vec::new();
    for (t, (out, sink)) in held.iter().enumerate() {
        let t = t as u32;
        let mut into: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); PER as usize];
        for (src, dsts) in out.iter().enumerate() {
            for &dst in dsts {
                into[dst as usize].insert(src as u32);
            }
        }
        for i in 0..PER {
            s.neighbours_into(Direction::Out, base(t) + i, &mut got);
            let mut want: Vec<u32> = out[i as usize].iter().map(|&d| base(t) + d).collect();
            if sink.contains(&i) {
                want.push(SINK);
            }
            assert_eq!(got, want, "thread {t} node {i} out at rest");
            s.neighbours_into(Direction::In, base(t) + i, &mut got);
            let want: Vec<u32> = into[i as usize].iter().map(|&d| base(t) + d).collect();
            assert_eq!(got, want, "thread {t} node {i} in at rest");
        }
    }

    // And the neighbourhood four threads were writing, against the union
    // of what they say they wrote. This is the one list in the file that
    // no single thread owns.
    let want: Vec<u32> = held
        .iter()
        .enumerate()
        .flat_map(|(t, (_, sink))| sink.iter().map(move |&i| base(t as u32) + i))
        .collect();
    s.neighbours_into(Direction::In, SINK, &mut got);
    assert_eq!(got, want, "the sink at rest");
    assert_eq!(
        s.degree(Direction::In, SINK) as usize,
        want.len(),
        "the sink's degree at rest"
    );
    println!("sink degree {}", want.len());
}
