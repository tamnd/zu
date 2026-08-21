//! Whether an operation costs more when the database holds more.
//!
//! This is the question the Y program was opened on and it is the one
//! zu1 answered wrong. A single row insert into zu1 costs a flat 143 ms
//! once the table passes about a thousand rows, because a commit folds
//! and a fold rebuilds every column the table has, so the cost of adding
//! a row is the size of the table (#292, #373, #391). A point read on a
//! string property is a full label scan, so it is the size of the table
//! too (#374). The pass condition those issues wrote down is a shape and
//! not a value: the curve has to be flat across decades of record count.
//!
//! zu2 should be flat by construction. An insert is an append and a
//! chain link. A read is a hash, a bucket, and a chain walk that is one
//! step deep until the table crowds. Neither of those has the record
//! count in it. "Should be" is not a measurement, so this is the
//! measurement.
//!
//! Two sweeps, because the two issues ask over different ranges. The
//! YCSB record is ten fields of a hundred bytes, so the first sweep runs
//! that size from a thousand records to a million, which is where zu1
//! was measured. The second runs a hundred byte record out to ten
//! million, which is the range #374 names, at a size that fits a laptop.
//!
//! Everything runs at the default options and `Durability::Async`, which
//! is what the benchmark harness calls the fastest default mode, and the
//! table says so rather than leaving it to be assumed. Compaction is at
//! its default trigger and running, because a curve measured with
//! reclamation turned off is a curve of something nobody runs.

use std::time::{Duration, Instant};

use zu2::{Db, Durability, Options};

/// Latency percentiles in microseconds, over one sample of operations.
struct Took {
    p50: f64,
    p99: f64,
    mean: f64,
}

impl Took {
    fn of(mut samples: Vec<Duration>) -> Took {
        samples.sort_unstable();
        let us = |d: Duration| d.as_nanos() as f64 / 1000.0;
        let at = |q: f64| us(samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)]);
        Took {
            p50: at(0.50),
            p99: at(0.99),
            mean: samples.iter().sum::<Duration>().as_nanos() as f64
                / samples.len() as f64
                / 1000.0,
        }
    }
}

/// A key that does not sort the way it hashes, so the index sees the
/// same spread a real key set gives it.
fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

/// A cheap deterministic shuffle of `0..n`, so the sampled operations
/// touch the whole key space rather than the part still in memory. Not
/// random, on purpose: the same run twice gives the same keys.
fn scatter(i: u64, n: u64) -> u64 {
    let mut x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 31;
    x.wrapping_mul(0xbf58_476d_1ce4_e5b9) % n
}

/// One row of the table: build to `n` records of `value` bytes, then
/// sample each operation over the built database.
fn row(n: u64, value: usize, samples: u64, presize: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("scaling.zu2");
    let db = Db::create(
        &path,
        Options {
            durability: Durability::Async,
            // Off by default, which is the shape a caller who does not
            // know their key count has: the table starts at 65536
            // buckets and doubles under them as they load.
            index_buckets: if presize {
                (n as usize).next_power_of_two()
            } else {
                Options::default().index_buckets
            },
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let payload = vec![b'x'; value];

    // The build, timing only the last `samples` inserts. The interesting
    // number is what an insert costs when the database is already full,
    // not the average over filling it.
    let from = n.saturating_sub(samples);
    let mut inserts = Vec::with_capacity(samples as usize);
    let began = Instant::now();
    for i in 0..n {
        if i >= from {
            let at = Instant::now();
            s.upsert(&key(i), &payload).expect("insert");
            inserts.push(at.elapsed());
        } else {
            s.upsert(&key(i), &payload).expect("insert");
        }
    }
    let load = began.elapsed();

    let mut out = Vec::with_capacity(value);
    let mut reads = Vec::with_capacity(samples as usize);
    for i in 0..samples {
        let k = key(scatter(i, n));
        let at = Instant::now();
        let found = s.read(&k, &mut out).expect("read");
        reads.push(at.elapsed());
        assert!(found, "a key that was loaded is not there");
    }

    // An update of the same length, which is what YCSB does when it
    // rewrites one field, and the shape zu2's in-place path takes.
    let mut updates = Vec::with_capacity(samples as usize);
    let wrote = db.log_bytes();
    for i in 0..samples {
        let k = key(scatter(i.wrapping_add(1 << 20), n));
        let at = Instant::now();
        s.upsert(&k, &payload).expect("update");
        updates.push(at.elapsed());
    }
    let per_update = (db.log_bytes() - wrote) as f64 / samples as f64;

    let mut deletes = Vec::with_capacity(samples as usize);
    for i in 0..samples {
        let k = key(scatter(i.wrapping_add(1 << 40), n));
        let at = Instant::now();
        s.delete(&k).expect("delete");
        deletes.push(at.elapsed());
    }

    drop(s);
    let disk = db.disk_bytes().unwrap_or(0);
    let logical = n * (value as u64 + 16);

    let insert = Took::of(inserts);
    let read = Took::of(reads);
    let update = Took::of(updates);
    let delete = Took::of(deletes);
    println!(
        "{n:>10} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.0} {:>7.2} {:>8.2}",
        insert.p50,
        insert.p99,
        read.p50,
        read.p99,
        update.p50,
        update.p99,
        delete.p50,
        delete.p99,
        insert.mean.max(read.mean).max(update.mean).max(delete.mean),
        per_update,
        disk as f64 / logical as f64,
        load.as_secs_f64(),
    );
}

fn sweep(what: &str, value: usize, sizes: &[u64], samples: u64, presize: bool) {
    println!("\n{what}, {value} byte values, async\n");
    println!(
        "{:>10} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7} {:>8}",
        "records",
        "ins p50",
        "ins p99",
        "rd p50",
        "rd p99",
        "upd p50",
        "upd p99",
        "del p50",
        "del p99",
        "worst",
        "b/upd",
        "amp",
        "load s"
    );
    for &n in sizes {
        row(n, value, samples.min(n / 10).max(1), presize);
    }
    println!("\nMicroseconds. worst is the highest of the four means, which is the");
    println!("number that has to stay flat down the column for the milestone to pass.");
    println!("b/upd is log bytes per update, amp is the file over the logical payload.");
}

fn main() {
    sweep(
        "the YCSB record",
        1000,
        &[1_000, 10_000, 100_000, 1_000_000],
        10_000,
        false,
    );
    sweep(
        "a small record",
        100,
        &[10_000, 100_000, 1_000_000, 10_000_000],
        10_000,
        false,
    );
    // The same two top rows with the table sized for the keys up front,
    // which is the control for the tail. A doubling migration is the
    // only thing in the read path whose cost has the record count in it,
    // so if the p99 comes down here it was the migration and if it does
    // not it is the device.
    sweep("the tail, table presized", 1000, &[1_000_000], 10_000, true);
    sweep("the tail, table presized", 100, &[10_000_000], 10_000, true);
}
