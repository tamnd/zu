//! What a table that is too small costs a point read, and what growing
//! it gives back.
//!
//! A bucket is eight entries and no overflow pointer, so a key that
//! arrives at a full one takes an entry over and chains behind it. The
//! displaced key is still there and still correct, but it is no longer
//! named by its own tag: a lookup has to walk the chain, and every step
//! of that walk is a dereference into the log, which on a database
//! larger than the last level cache is a random memory read and on a
//! cold one is a random device read. That is the whole argument for
//! Z9. A table four times too small turns a point read into four of
//! them, and a hash index over a hybrid log stops being worth having.
//!
//! Three tables over the same key set:
//!
//! - roomy: the sizing the option asks for, records / 4 buckets, which
//!   is under half full and where nothing is displaced
//! - crowded: a tenth of that, growth off, which is the shape a caller
//!   who guessed low used to be stuck with
//! - grown: a tenth of that, growth on, which is the same guess with Z9
//!   allowed to fix it
//!
//! The grown row is the one that has to land on the roomy row. If it
//! does not, the doubling is not relieving crowding and the split is
//! carrying buckets over whole rather than splitting them by key.
//!
//! Reads are in a shuffled order, because a scan in key order reads the
//! log in the order it was written and measures the prefetcher instead.

use std::time::Instant;

use zu2::{Db, Durability, Options};

const RECORDS: u64 = 2_000_000;
const VALUE_BYTES: usize = 100;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

fn options(buckets: usize, grow: bool) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: buckets,
        grow_index: grow,
        max_pages: 1 << 16,
        max_nodes: 1 << 10,
        // Off, because what is being measured is how many dereferences
        // a lookup makes and not what a compaction pass happened to
        // have moved out from under it.
        compact_below: 0,
        ..Options::default()
    }
}

/// A shuffled 0..RECORDS, from a seed rather than from the clock so two
/// runs of this read the same order.
fn shuffled() -> Vec<u64> {
    let mut order: Vec<u64> = (0..RECORDS).collect();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for i in (1..order.len()).rev() {
        // xorshift64star, which is enough for a permutation and is one
        // line rather than a dependency.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let j = (state.wrapping_mul(0x2545_f491_4f6c_dd1d) % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    order
}

fn run(name: &str, buckets: usize, grow: bool, order: &[u64]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("c.zu2"), options(buckets, grow)).expect("create");
    let mut session = db.session();
    let value = vec![b'x'; VALUE_BYTES];
    for i in 0..RECORDS {
        session.upsert(&key(i), &value).expect("upsert");
    }

    let mut out = Vec::with_capacity(VALUE_BYTES);
    let started = Instant::now();
    for i in order {
        assert!(session.read(&key(*i), &mut out).expect("read"), "lost {i}");
    }
    let elapsed = started.elapsed().as_secs_f64();

    // Entries divided by keys is the fraction of the key set the table
    // names directly, so one minus it is the fraction that is only
    // reachable by walking somebody else's chain.
    let named = db.index_occupancy() as f64 / RECORDS as f64;
    println!(
        "{name:8} buckets {:>9}  grows {:>2}  named {:>5.1}%  reads {:>8.0}/s  {:>6.0} ns",
        db.index_buckets(),
        db.index_grows(),
        named * 100.0,
        RECORDS as f64 / elapsed,
        elapsed * 1e9 / RECORDS as f64,
    );
}

fn main() {
    let order = shuffled();
    let roomy = (RECORDS as usize / 4).next_power_of_two();
    println!("{RECORDS} records, {VALUE_BYTES} byte values, one session, shuffled reads");
    run("roomy", roomy, false, &order);
    run("crowded", roomy / 16, false, &order);
    run("grown", roomy / 16, true, &order);
}
