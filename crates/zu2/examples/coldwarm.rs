//! What the cold read tail is actually made of: the syscall, or the
//! device behind it.
//!
//! `readtail` says the tail is the cold tier and stops there, and #557
//! reads that as a missing cache and proposes one. Before building a
//! cache it is worth knowing what a cache would be caching. There are
//! two candidates and they want opposite fixes:
//!
//! - the `pread` itself, the call and the copy, in which case a cache in
//!   front of the tier removes it and the tail comes down
//! - the device under the `pread`, in which case the operating system's
//!   own page cache is already the cache and a second one in user space
//!   holds the same bytes twice for nothing, and the fix is to stop
//!   putting so much down there or to fetch it before it is asked for
//!
//! The way to tell them apart is to hand the whole tier to the page
//! cache before sampling. A run that reads the cold file end to end and
//! then samples pays the same number of `pread`s as one that does not,
//! and pays the device for none of them. If the tail is the call, the
//! two rows look alike. If the tail is the device, the warm row is flat.
//!
//! Both draws, because a uniform draw over ten million records has no
//! hot set and a zipfian one does, and a cache is only ever worth
//! anything against the second.

use std::io::Read;
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

fn unit(i: u64) -> f64 {
    let mut x = i.wrapping_add(1).wrapping_mul(0xd1b5_4a32_d192_ed03);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// YCSB's zipfian at theta 0.99, the same draw `readtail` uses.
struct Zipf {
    n: u64,
    zetan: f64,
    eta: f64,
    alpha: f64,
}

impl Zipf {
    fn new(n: u64) -> Self {
        const THETA: f64 = 0.99;
        let zeta = |upto: u64| {
            (1..=upto)
                .map(|i| 1.0 / (i as f64).powf(THETA))
                .sum::<f64>()
        };
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
        scatter(rank, self.n)
    }
}

fn us(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1000.0
}

#[derive(Clone, Copy, PartialEq)]
enum Draw {
    Uniform,
    Zipfian,
}

/// Reads the cold file straight through, so every byte of it is in the
/// operating system's page cache when the sample starts.
///
/// A plain sequential read and not a hint, because a hint is advice a
/// kernel is allowed to ignore and this has to be the difference between
/// the two rows rather than a request that it be.
fn warm(path: &std::path::Path) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
    }
}

fn run(what: &str, promote: bool, draw: Draw, preheat: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("coldwarm.zu2");
    let db = Db::create(
        &path,
        Options {
            durability: Durability::Async,
            index_buckets: (RECORDS as usize).next_power_of_two(),
            cold_tier: true,
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
    // Everything that was going to move has moved, so the two rows are
    // over the same file rather than over whatever a pass had reached.
    // A sleep is not that: it gives the background thread time and no
    // guarantee, and the share of the file that ends up in the tier then
    // varies from 2 to 35 percent across identical runs, which is more
    // than anything measured here. `compact` runs both passes until
    // neither moves anything, so it is the fixed point a measurement
    // wants to start from. #600.
    db.compact().expect("settle the tier");

    let mut heated = 0;
    if preheat {
        let beside = {
            let mut b = path.clone().into_os_string();
            b.push(".cold");
            std::path::PathBuf::from(b)
        };
        heated = warm(&beside).expect("warm the tier");
    }

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

    let cold = db.cold_disk_bytes().unwrap_or(0);
    let hot = db.disk_bytes().unwrap_or(0).saturating_sub(cold);
    let share = if cold + hot > 0 {
        100.0 * cold as f64 / (cold + hot) as f64
    } else {
        0.0
    };
    reads.sort_unstable();
    let at = |q: f64| us(reads[((reads.len() as f64 * q) as usize).min(reads.len() - 1)]);
    let over = |limit: f64| reads.iter().filter(|d| us(**d) > limit).count();
    println!(
        "{what:<26} {:>7.2} {:>7.2} {:>8.2} {:>9.2} {:>7} {:>7.0} {:>9}",
        at(0.50),
        at(0.99),
        at(0.999),
        us(reads[reads.len() - 1]),
        over(100.0),
        share,
        heated / (1 << 20),
    );
}

fn main() {
    println!(
        "\n{RECORDS} records, {VALUE} byte values, table presized, async, {SAMPLES} sampled reads\n"
    );
    println!(
        "{:<26} {:>7} {:>7} {:>8} {:>9} {:>7} {:>7} {:>9}",
        "what", "p50", "p99", "p999", "max", ">100us", "cold%", "warmedMiB"
    );
    run("uniform, cold file", false, Draw::Uniform, false);
    run("uniform, tier preheated", false, Draw::Uniform, true);
    run("zipfian, cold file", false, Draw::Zipfian, false);
    run("zipfian, tier preheated", false, Draw::Zipfian, true);
    println!("\nMicroseconds. Both rows in a pair pay the same number of preads and");
    println!("the preheated one pays the device for none of them, so the difference");
    println!("between them is the device and what is left is the call. Promotion is");
    println!("off in all four, because a read that puts the record back in the log");
    println!("is a read that hides the thing being measured. #557.");
}
