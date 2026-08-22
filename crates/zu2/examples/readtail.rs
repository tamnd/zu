//! What the read tail at ten million records is made of.
//!
//! `scaling` says a read is flat at the median and is not flat at the
//! tail: at ten million records of a hundred bytes the p50 is under a
//! microsecond and the p99 is hundreds of them, and presizing the table
//! does not bring it down, so it is not the doubling. That leaves the
//! things a read can wait on that a small database does not have, and
//! this turns them off one at a time to find out which.
//!
//! Four runs over the same shape, table presized so a migration is out
//! of the picture in all of them:
//!
//! - everything on, which is the default and is the row `scaling` prints
//! - compaction off, so nothing scans the log and nothing moves a record
//! - the cold tier off with compaction still on, so records are dropped
//!   rather than migrated and a read never reaches cold space
//! - both off and the flusher given nothing to chase, which is the floor
//!
//! The percentiles go further out than `scaling` prints, because a p99
//! over ten thousand samples is a hundred operations and the question is
//! whether those hundred are one stall or a hundred slow reads.
//!
//! The answer is the cold tier and it is not close: with it on the p99
//! is hundreds of microseconds and thousands of reads pass a hundred,
//! and with it off, running the same compaction, the p99 is single
//! digits and the count is zero. The settled row rules out contention
//! with a pass that is still running, which leaves the read itself. See
//! #557, which this is the measurement for.

use std::time::{Duration, Instant};

use zu2::{Db, Durability, Options};

const RECORDS: u64 = 10_000_000;
const VALUE: usize = 100;
const SAMPLES: u64 = 100_000;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn scatter(i: u64, n: u64) -> u64 {
    let mut x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 31;
    x.wrapping_mul(0xbf58_476d_1ce4_e5b9) % n
}

fn us(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1000.0
}

fn run(what: &str, compact: bool, cold: bool, settle: Duration) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("readtail.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: (RECORDS as usize).next_power_of_two(),
            compact_below: if compact {
                Options::default().compact_below
            } else {
                0
            },
            cold_tier: cold,
            checkpoint_on_close: false,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let payload = vec![b'x'; VALUE];
    for i in 0..RECORDS {
        s.upsert(&key(i), &payload).expect("insert");
    }

    // A pass that is still running is a different thing from a database
    // whose records have already moved, and waiting tells them apart.
    std::thread::sleep(settle);

    let mut out = Vec::with_capacity(VALUE);
    let mut reads = Vec::with_capacity(SAMPLES as usize);
    for i in 0..SAMPLES {
        let k = key(scatter(i, RECORDS));
        let at = Instant::now();
        let found = s.read(&k, &mut out).expect("read");
        reads.push(at.elapsed());
        assert!(found, "a key that was loaded is not there");
    }

    // How much of the total sits in the slow ones, which is the
    // difference between a stall everybody pays a share of and a stall a
    // few operations pay all of.
    let cold = db.cold_disk_bytes().unwrap_or(0);
    let hot = db.disk_bytes().unwrap_or(0).saturating_sub(cold);
    let share = if cold + hot > 0 {
        100.0 * cold as f64 / (cold + hot) as f64
    } else {
        0.0
    };
    let total: Duration = reads.iter().sum();
    reads.sort_unstable();
    let at = |q: f64| us(reads[((reads.len() as f64 * q) as usize).min(reads.len() - 1)]);
    let over = |limit: f64| reads.iter().filter(|d| us(**d) > limit).count();
    let tail: Duration = reads[reads.len() - reads.len() / 100..].iter().sum();
    println!(
        "{what:<22} {:>7.2} {:>7.2} {:>8.2} {:>8.2} {:>9.2} {:>7} {:>7.0} {:>7.0}",
        at(0.50),
        at(0.99),
        at(0.999),
        at(0.9999),
        us(reads[reads.len() - 1]),
        over(100.0),
        100.0 * tail.as_nanos() as f64 / total.as_nanos() as f64,
        share,
    );
}

fn main() {
    println!(
        "\n{RECORDS} records, {VALUE} byte values, table presized, async, {SAMPLES} sampled reads\n"
    );
    println!(
        "{:<22} {:>7} {:>7} {:>8} {:>8} {:>9} {:>7} {:>7} {:>7}",
        "what", "p50", "p99", "p999", "p9999", "max", ">100us", "top1%", "cold%"
    );
    run("everything on", true, true, Duration::ZERO);
    run("everything on, settled", true, true, Duration::from_secs(20));
    run("compaction off", false, true, Duration::ZERO);
    run("cold tier off", true, false, Duration::ZERO);
    run("both off", false, false, Duration::ZERO);
    println!("\nMicroseconds. >100us is how many of the sampled reads took longer");
    println!("than that, and top1% is the share of the whole sample's time that the");
    println!("slowest one percent of reads accounts for. cold% is the share of the");
    println!("file that has been migrated to the cold tier, which is the share of");
    println!("reads that go to the device rather than to a page in memory.");
}
