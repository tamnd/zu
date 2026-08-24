//! Which half of a scan stops scaling with threads.
//!
//! tamnd/zu#732 measures zu2 gaining 1.34x on workload e going from
//! eight threads to thirty two while lmdb gains 1.77x, on a box with
//! thirty two cores that is otherwise idle. The same zu2 build on
//! workload c in the same run gains 2.52x across the same step, so it is
//! not the C ABI crossing and it is not a general threading limit. It is
//! the scan path.
//!
//! The first check on that issue removed every writer and the knee did
//! not move, which rules out one in twenty operations holding a
//! structure the other nineteen want. That leaves readers contending
//! with each other, and this is the bench that says where.
//!
//! Two variants, the same two [`scan`](../scan.rs) uses and for the same
//! reason:
//!
//! - `walk`, seek and step and touch each key, never looking at a
//!   record. The scan plane on its own.
//! - `scan`, the real [`zu2::Session::scan`], plane and records
//!   together.
//!
//! If `walk` scales and `scan` does not then the plane is fine and the
//! contention is in reading records out of the log, which is a page
//! cache or a file handle problem and not an ordered structure problem.
//! If `walk` is the one that flattens then it is the plane, and #708's
//! finding that the walk is only 17% to 39% of a single threaded scan
//! stops being the whole story, because a share that small at one thread
//! can still be the entire ceiling at thirty two.
//!
//! Every thread gets its own session and its own cursor and its own
//! disjoint slice of the start keys, so nothing here shares anything the
//! engine did not intend to be shared. The keys are split by stride
//! rather than by block so no worker gets a contiguous and therefore
//! easier region.
//!
//! Shuffled log order only, unlike the single threaded bench which runs
//! both. A store under real traffic receives records in arrival order,
//! so shuffled is the one that describes it, and ascending is a ceiling
//! that #708 has already measured.
//!
//! Run: cargo bench -p zu2 --bench scanmt
//! Bigger: ZU2_RECORDS=1000000 cargo bench -p zu2 --bench scanmt
//! Other counts: ZU2_THREADS=1,4,16,32 cargo bench -p zu2 --bench scanmt

use std::path::Path;
use std::time::Instant;

use zu2::{Db, Durability, Options};

/// Bytes of value per record, which is YCSB's ten fields of a hundred.
const VALUE_BYTES: usize = 1000;

fn env<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn key(i: u64) -> String {
    format!("user{i:019}")
}

fn value(i: u64, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(format!("{i:019}").as_bytes());
    let mut rng = Rng(0x9E3779B97F4A7C15 ^ (i + 1).wrapping_mul(0x100000001B3) | 1);
    while out.len() < VALUE_BYTES {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(VALUE_BYTES);
}

fn options(records: u64) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: (records as usize / 4).next_power_of_two().max(1 << 12),
        max_pages: 1 << 17,
        ordered: true,
        ..Options::default()
    }
}

/// Loads `records` rows in shuffled key order.
///
/// A full permutation rather than a random draw, so the store holds
/// exactly the record set the single threaded bench holds and the two
/// can be read against each other.
fn load(path: &Path, records: u64) -> Db {
    let db = Db::create(path, options(records)).expect("create");
    let mut order: Vec<u64> = (0..records).collect();
    let mut rng = Rng(0x243F6A8885A308D3);
    for i in (1..order.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let mut s = db.session();
    let mut buf = Vec::with_capacity(VALUE_BYTES);
    for i in order {
        value(i, &mut buf);
        s.upsert(key(i).as_bytes(), &buf).expect("upsert");
    }
    drop(s);
    db.sync().expect("sync");
    db
}

/// Start keys, uniform rather than YCSB's zipfian, for the reason the
/// single threaded bench gives: a skewed start set lives in cache and in
/// the mutable tail and would flatter us, and here it would also hand
/// every thread the same few pages and invent contention that is not
/// the engine's.
fn starts(records: u64, scan: u64, n: usize) -> Vec<String> {
    let mut rng = Rng(0xB5026F5AA96619E9);
    (0..n)
        .map(|_| key(rng.next() % records.saturating_sub(scan).max(1)))
        .collect()
}

/// Runs `body` on `threads` workers and returns rows done and wall
/// clock. Wall clock and not the sum of the workers': what is being
/// measured is throughput, so a worker that finished early still counts
/// as time the machine had and did not use.
fn phase(threads: usize, body: impl Fn(usize) -> u64 + Sync) -> (u64, f64) {
    let started = Instant::now();
    let rows: u64 = std::thread::scope(|scope| {
        let body = &body;
        let handles: Vec<_> = (0..threads).map(|t| scope.spawn(move || body(t))).collect();
        handles.into_iter().map(|h| h.join().expect("worker")).sum()
    });
    (rows, started.elapsed().as_secs_f64())
}

fn main() {
    let records: u64 = env("ZU2_RECORDS", 200_000);
    let scan: u64 = env("ZU2_SCAN", 50);
    let iterations: usize = env("ZU2_ITERS", 20_000);
    // Best of, for the reason the single threaded bench gives: one pass
    // moves by more than the effect on a machine doing anything else,
    // and the fastest pass is the closest this gets to the engine alone.
    let repeats: usize = env("ZU2_REPEATS", 3);
    let threads: Vec<usize> = std::env::var("ZU2_THREADS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 2, 4, 8, 16, 32]);

    let dir = std::env::temp_dir().join(format!("zu2-scanmt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    println!(
        "# zu2 scan scaling, {records} records, {scan} rows a scan, \
         {iterations} scans a thread count, best of {repeats}, shuffled log order"
    );
    println!("# see tamnd/zu#732 and tamnd/zu#708");

    let keys = starts(records, scan, iterations);
    let path = dir.join("shuffled.db");
    let t = Instant::now();
    let db = load(&path, records);
    println!(
        "# loaded in {:.1}s, {:.1} MiB on device",
        t.elapsed().as_secs_f64(),
        db.disk_bytes().expect("disk bytes") as f64 / (1 << 20) as f64
    );
    println!("variant\tthreads\trows\tseconds\tus a row\trows a second");

    // Held so the scaling factors can be printed at the end rather than
    // left for a reader to divide out.
    let mut walk_rate: Vec<(usize, f64)> = Vec::new();
    let mut scan_rate: Vec<(usize, f64)> = Vec::new();

    for &n in &threads {
        for variant in ["walk", "scan"] {
            let mut best = f64::MAX;
            let mut best_rows = 0u64;
            for _ in 0..repeats {
                let (rows, seconds) = phase(n, |t| {
                    // Stride rather than block, so no worker gets a
                    // contiguous and therefore cheaper region.
                    let mine = keys.iter().skip(t).step_by(n);
                    let mut done = 0u64;
                    match variant {
                        "walk" => {
                            let ordered = db.core().ordered().expect("ordered");
                            for start in mine {
                                let mut cursor = ordered.seek(start.as_bytes());
                                for _ in 0..scan {
                                    let Some(k) = cursor.key() else { break };
                                    std::hint::black_box(k);
                                    cursor.step();
                                    done += 1;
                                }
                            }
                        }
                        _ => {
                            let mut s = db.session();
                            for start in mine {
                                done += s
                                    .scan(start.as_bytes(), scan as usize, |_k, v| {
                                        std::hint::black_box(v);
                                    })
                                    .expect("scan") as u64;
                            }
                        }
                    }
                    done
                });
                // Compared per row and not per pass, because a pass that
                // was cut short would otherwise win for having done less.
                let per_row = seconds / (rows.max(1) as f64);
                let best_per_row = best / (best_rows.max(1) as f64);
                if rows > 0 && per_row < best_per_row {
                    best = seconds;
                    best_rows = rows;
                }
            }
            let rate = best_rows as f64 / best;
            println!(
                "{variant}\t{n}\t{best_rows}\t{best:.2}\t{:.3}\t{rate:.0}",
                best * 1e6 / best_rows as f64
            );
            if variant == "walk" {
                walk_rate.push((n, rate));
            } else {
                scan_rate.push((n, rate));
            }
        }
    }

    // The number #732 asks for, stated rather than left to be worked out.
    println!("#");
    println!("# scaling against one thread");
    println!("threads\twalk\tscan");
    let walk1 = walk_rate.first().map(|x| x.1).unwrap_or(1.0);
    let scan1 = scan_rate.first().map(|x| x.1).unwrap_or(1.0);
    for (i, &(n, w)) in walk_rate.iter().enumerate() {
        let s = scan_rate.get(i).map(|x| x.1).unwrap_or(0.0);
        println!("{n}\t{:.2}x\t{:.2}x", w / walk1, s / scan1);
    }
    println!(
        "# if walk keeps climbing and scan does not, the plane is fine and \
         the contention is in reading records out of the log"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
