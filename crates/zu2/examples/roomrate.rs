//! What a bounded log costs a sustained write rate.
//!
//! `max_pages` is a cap on the span between the compaction floor and the
//! tail, and a caller who sets it small is saying the database is
//! allowed a fixed amount of disk. What that costs is not obvious: the
//! live set still fits many times over, so the honest answer ought to be
//! a bit of write amplification and nothing else. A pass runs, the floor
//! moves, the writers carry on.
//!
//! The crash tests found otherwise. The same child that gets fifty
//! thousand durable writes out in a second and a half at eight pages
//! gets four hundred at four, with the same live set and the same value
//! size, which is not amplification. So this walks the page count over
//! the same workload and prints the rate with the compaction counters
//! beside it, which is what says whether the writers are paying for
//! copying or paying for waiting.
//!
//! The live set here is about two megabytes against a page of four, so
//! at four pages the log is twelve percent live and at thirty two it is
//! under two. Nothing in that range should need a stall.

use std::time::Instant;

use zu2::{Db, Durability, Options};

/// A bounded key space, so the log has something to reclaim. A run that
/// only writes new keys fills it instead and measures the error path.
const KEYS: u64 = 8000;
const VALUE_BYTES: usize = 200;
const WRITES: u64 = 200_000;

fn key(i: u64) -> Vec<u8> {
    format!("key{:016}", i % KEYS).into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    let mut v = format!("value{i:016}").into_bytes();
    v.resize(VALUE_BYTES, b'v');
    v
}

fn options(max_pages: usize, durability: Durability) -> Options {
    Options {
        durability,
        index_buckets: 1,
        max_pages,
        max_nodes: 1 << 16,
        mutable_pages: 1,
        compact_below: 1 << 20,
        ..Options::default()
    }
}

fn run(max_pages: usize, durability: Durability, writes: u64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("r.zu2"), options(max_pages, durability)).expect("create");
    let mut session = db.session();
    let started = Instant::now();
    let mut done = 0;
    // A writer that outruns compaction is told so rather than retried,
    // which is #566, so the walk records how far it got instead of
    // stopping the run.
    let mut full = "";
    for i in 0..writes {
        match session.upsert(&key(i), &value(i)) {
            Ok(()) => done += 1,
            Err(zu2::Error::LogFull { .. }) => {
                full = " full";
                break;
            }
            Err(other) => panic!("upsert: {other:?}"),
        }
    }
    let took = started.elapsed();
    let writes = done.max(1);
    let counters = db.compaction();
    let load = std::sync::atomic::Ordering::Relaxed;
    let passes = counters.passes.load(load);
    let copied = counters.copied.load(load);
    let written = writes * (VALUE_BYTES + 32) as u64;
    println!(
        "{max_pages:>4} {:>6} {:>12.0} {:>10.2} {passes:>8} {:>12} {:>8.2}{full}",
        format!("{durability:?}"),
        writes as f64 / took.as_secs_f64(),
        took.as_secs_f64() * 1e6 / writes as f64,
        copied,
        copied as f64 / written as f64,
    );
}

fn main() {
    println!(
        "{:>4} {:>6} {:>12} {:>10} {:>8} {:>12} {:>8}",
        "pages", "mode", "ops/s", "us/op", "passes", "copied", "amp"
    );
    // The durable column is the one the crash tests run at. The async
    // column is there to separate a stall from a flush: if only the
    // durable rows fall away then what the writers are waiting on is the
    // device, and if both do then it is the pass.
    for durability in [Durability::Async, Durability::Durable] {
        for max_pages in [4, 8, 16, 32, 128] {
            run(max_pages, durability, WRITES);
        }
    }
}
