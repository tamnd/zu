//! What a point read gains from more threads, with no client in the way.
//!
//! The YCSB sweep on gamingpc reads 712158 ops a second from workload c
//! at one thread and 1125818 at sixteen, on a machine with thirty two
//! cores. That is 1.58x for sixteen times the threads, and it is the
//! whole of the remaining gap to ten times sqlite on the read heavy
//! workloads (#613). sqlite on the same host and the same pass gains
//! 2.40x, so the shape is zu2's and not the machine's.
//!
//! Every thread scaling number so far has a cgo crossing and a go-ycsb
//! client in it, and neither of those is the engine. This sweep has
//! neither. It loads once, then reads point keys from a growing number
//! of threads, each thread on its own slice of the key space so nothing
//! is contending on a key. What is left in the read path is an epoch
//! entry, a hash, a bucket load and a chain walk, and if the curve here
//! is flat the shared write is in one of those.
//!
//! Two key distributions, because they answer different questions. The
//! disjoint pass gives every thread its own range, which is the best
//! case: separate buckets, separate cache lines, no reason to interfere.
//! The shared pass has every thread read the same range in the same
//! order, which is the case where the bucket lines are shared and the
//! page cache is warm for everybody. A read only workload should scale
//! on both, so a difference between them is itself the answer.
//!
//! Default options and `Durability::Async`, the same as every other
//! measurement in this directory, and the record is the YCSB shape of
//! ten fields of a hundred bytes so the numbers sit beside the sweep's.

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use zu2::{Db, Durability, Options};

/// The YCSB record, ten fields of a hundred bytes.
const VALUE: usize = 1000;

/// The YCSB key, written into `into` rather than allocated, since an
/// allocation a read would be a per thread malloc in the hot loop and
/// this sweep is about what the threads share.
fn key(i: u64, into: &mut Vec<u8>) {
    into.clear();
    write!(into, "user{i:019}").expect("format");
}

/// Reads `ops` keys drawn uniformly from `[lo, lo + span)`, starting the
/// sequence at `seed` so two threads on the same range are not in step.
fn read_range(db: &Db, lo: u64, span: u64, ops: u64, seed: u64) {
    let mut s = db.session();
    let mut out = Vec::with_capacity(VALUE);
    let mut k = Vec::with_capacity(32);
    // splitmix64, so the key order is scattered rather than the order
    // the log was written in. Reading in insertion order is sequential
    // on the device and flatter than anything a client does, and it is
    // the mistake that makes a read benchmark look like a memcpy.
    let mut state = seed;
    for _ in 0..ops {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        let at = (z ^ (z >> 31)) % span;
        key(lo + at, &mut k);
        out.clear();
        if !s.read(&k, &mut out).expect("read") {
            panic!("missing key {}", lo + at);
        }
    }
}

fn main() {
    let records: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let ops: u64 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000_000);
    let top = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(
            &dir.path().join("readscale.zu2"),
            Options {
                durability: Durability::Async,
                // Sized for the load so the sweep is not measuring the
                // doublings, the same hint the benchmark harness gives.
                index_buckets: (records / 4 + 1) as usize,
                ..Options::default()
            },
        )
        .expect("create"),
    );
    {
        let mut s = db.session();
        let value = vec![b'v'; VALUE];
        let mut k = Vec::with_capacity(32);
        for i in 0..records {
            key(i, &mut k);
            s.upsert(&k, &value).expect("upsert");
        }
    }
    // Read the whole database once before timing anything. Without
    // this the one thread row pays every first touch page fault and
    // every cold tier promotion the load left behind, and the rows
    // after it read a database somebody else warmed. That shows up as
    // per thread throughput rising with the thread count, which is not
    // a thing that happens, and it makes the speedup column a measure
    // of the warm up order.
    {
        let warm: Vec<_> = (0..top)
            .map(|t| {
                let db = Arc::clone(&db);
                let span = records / top as u64;
                std::thread::spawn(move || {
                    read_range(&db, t as u64 * span, span, span * 2, t as u64)
                })
            })
            .collect();
        for w in warm {
            w.join().expect("warm");
        }
    }

    println!("# records {records}, ops {ops}, value {VALUE} bytes, cores {top}");
    println!("threads  keys      ops/s        per thread   speedup   ns/op");

    // The sweep runs up and then measures one thread again at the end.
    // If the two one thread rows disagree the host drifted under the
    // run and the speedup column is that drift, not the engine, so the
    // control row is printed rather than averaged away.
    let mut base = [0.0f64; 2];
    let mut counts: Vec<usize> = Vec::new();
    let mut threads = 1usize;
    while threads <= top.max(1) {
        counts.push(threads);
        threads *= 2;
    }
    counts.push(1);
    for threads in counts {
        for (which, disjoint) in [(0usize, true), (1, false)] {
            let each = ops / threads as u64;
            let started = Instant::now();
            let workers: Vec<_> = (0..threads)
                .map(|t| {
                    let db = Arc::clone(&db);
                    let (lo, span) = if disjoint {
                        let span = records / threads as u64;
                        (t as u64 * span, span)
                    } else {
                        (0, records)
                    };
                    let seed = 0x5eed_0000 ^ t as u64;
                    std::thread::spawn(move || read_range(&db, lo, span, each, seed))
                })
                .collect();
            for w in workers {
                w.join().expect("worker");
            }
            let took = started.elapsed().as_secs_f64();
            let done = each * threads as u64;
            let rate = done as f64 / took;
            if base[which] == 0.0 {
                base[which] = rate;
            }
            println!(
                "{threads:7}  {:8}  {rate:9.0}  {:11.0}  {:8.2}x  {:6.0}",
                if disjoint { "disjoint" } else { "shared" },
                rate / threads as f64,
                rate / base[which],
                took * 1e9 / done as f64
            );
        }
    }
}
