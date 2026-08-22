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
//!
//! Three more rows draw keys the way YCSB draws them, zipfian at theta
//! 0.99, because the five above deliberately have no hot set and an
//! engine cannot be asked to keep one it was never shown. Those rows are
//! what price `Options::promote_reads`: with it on, the first read of a
//! record the tier holds pays the device and the rest do not.

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

/// A number in [0, 1) from the same mixer, so the skewed draw below is
/// as repeatable as the uniform one.
fn unit(i: u64) -> f64 {
    let mut x = i.wrapping_add(1).wrapping_mul(0xd1b5_4a32_d192_ed03);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// YCSB's zipfian, which is Gray's rejection free draw at theta 0.99.
///
/// It is here because the uniform draw above answers a question nobody
/// has: a workload that touches ten million records evenly has no hot
/// set, and every engine's cache is useless against it by construction.
/// YCSB does not do that and neither does anything real. The zeta sum is
/// taken once and handed in.
struct Zipf {
    n: u64,
    zetan: f64,
    eta: f64,
    alpha: f64,
}

impl Zipf {
    fn new(n: u64) -> Self {
        const THETA: f64 = 0.99;
        let zeta = |upto: u64| (1..=upto).map(|i| 1.0 / (i as f64).powf(THETA)).sum::<f64>();
        let zetan = zeta(n);
        let zeta2 = zeta(2);
        Self {
            n,
            zetan,
            eta: (1.0 - (2.0 / n as f64).powf(1.0 - THETA)) / (1.0 - zeta2 / zetan),
            alpha: 1.0 / (1.0 - THETA),
        }
    }

    fn at(&self, i: u64) -> u64 {
        let u = unit(i);
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(0.99) {
            return 1;
        }
        let rank = (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
        // The rank is the popularity order, and handing it to the key
        // function straight would put the hot set in one place in the
        // key space. Scattering it is what YCSB's scrambled zipfian does
        // and it is what keeps this from also being a locality test.
        scatter(rank, self.n)
    }
}

fn us(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1000.0
}

/// How the sample picks its keys.
#[derive(Clone, Copy, PartialEq)]
enum Draw {
    Uniform,
    Zipfian,
}

fn run(what: &str, compact: bool, cold: bool, promote: bool, draw: Draw, settle: Duration) {
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
            promote_reads: promote,
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
    let zipf = Zipf::new(RECORDS);
    for i in 0..SAMPLES {
        let k = key(match draw {
            Draw::Uniform => scatter(i, RECORDS),
            Draw::Zipfian => zipf.at(i),
        });
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
        "{what:<22} {:>7.2} {:>7.2} {:>8.2} {:>8.2} {:>9.2} {:>7} {:>7.0} {:>7.0} {:>8}",
        at(0.50),
        at(0.99),
        at(0.999),
        at(0.9999),
        us(reads[reads.len() - 1]),
        over(100.0),
        100.0 * tail.as_nanos() as f64 / total.as_nanos() as f64,
        share,
        db.promoted(),
    );
}

fn main() {
    println!(
        "\n{RECORDS} records, {VALUE} byte values, table presized, async, {SAMPLES} sampled reads\n"
    );
    println!(
        "{:<22} {:>7} {:>7} {:>8} {:>8} {:>9} {:>7} {:>7} {:>7} {:>8}",
        "what", "p50", "p99", "p999", "p9999", "max", ">100us", "top1%", "cold%", "moved"
    );
    let u = Draw::Uniform;
    run("everything on", true, true, true, u, Duration::ZERO);
    run(
        "everything on, settled",
        true,
        true,
        true,
        u,
        Duration::from_secs(20),
    );
    run("compaction off", false, true, true, u, Duration::ZERO);
    run("cold tier off", true, false, true, u, Duration::ZERO);
    run("both off", false, false, true, u, Duration::ZERO);

    // The rows that matter to a workload rather than to the engine. A
    // uniform draw over ten million records has no hot set, so nothing
    // any engine does about hot data can show up in the five above.
    let z = Draw::Zipfian;
    run("zipfian, promoting", true, true, true, z, Duration::ZERO);
    run("zipfian, no promotion", true, true, false, z, Duration::ZERO);
    run("zipfian, no tier", true, false, false, z, Duration::ZERO);
    println!("\nMicroseconds. >100us is how many of the sampled reads took longer");
    println!("than that, and top1% is the share of the whole sample's time that the");
    println!("slowest one percent of reads accounts for. cold% is the share of the");
    println!("file that has been migrated to the cold tier, which is the share of");
    println!("reads that go to the device rather than to a page in memory. moved is");
    println!("how many records a read put back in the log, which is what the last");
    println!("three rows are here to price.");
}
