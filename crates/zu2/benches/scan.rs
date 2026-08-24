//! Where the time goes in a range scan.
//!
//! Workload e is the weakest thing zu2 does. It is 3.2x over pebble where
//! the point read is ten and more, and since a 10x claim is settled by
//! whichever rival does best, workload e is the one that decides it. See
//! tamnd/zu#708.
//!
//! The reason it is harder than the point read is structural rather than
//! a tuning miss. A hash index answers a point lookup in one probe and an
//! LSM has to look in several places, which is why we win there. A range
//! scan inverts that: pebble and badger are sorted on disk, so an ordered
//! iterator is a walk over pages that are already in the order the caller
//! asked for, and the compaction they pay for on every write is exactly
//! what buys them it. Our scan plane is a key ordered structure standing
//! beside the index, and the records it names are wherever the log put
//! them.
//!
//! So the question is not "is the scan slow", it is which of these two it
//! is, because they want opposite fixes:
//!
//! - the walk itself costs, in which case the plane is the thing to work
//!   on, or
//! - the walk is cheap and the fifty records it names are fifty random
//!   reads, in which case the plane is fine and the layout is the problem
//!   and no amount of tuning the plane touches it.
//!
//! Four measurements, run over the same loaded database, all of them one
//! thread so nothing here is queueing:
//!
//! - `walk`, seek and step and touch each key, and never look at a
//!   record. The plane on its own.
//! - `scan`, the real [`Session::scan`], with its lookahead and its
//!   prefetch.
//! - `points`, take the fifty keys off the plane first and then read them
//!   one at a time through the ordinary point read path. The same work
//!   with the pipeline taken away.
//! - `dense`, fifty point reads of keys that are adjacent by construction
//!   rather than found on the plane. No plane at all.
//!
//! And the single variable that separates the two explanations: the same
//! four run against two databases holding the same records, one loaded in
//! ascending key order so that plane order and log order agree, and one
//! loaded in shuffled order so they do not. If the scan is paying for
//! locality then the ascending database is much faster and the shuffled
//! one is not, and the gap between them is the size of the prize. If the
//! two come out level then the address the record sits at is not what
//! costs and the probe is, and the plane is where to look after all.
//!
//! Run: cargo bench -p zu2 --bench scan
//! Bigger: ZU2_RECORDS=1000000 cargo bench -p zu2 --bench scan
//! Longer scans: ZU2_SCAN=100 cargo bench -p zu2 --bench scan

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
        // The scan plane is the whole subject here.
        ordered: true,
        ..Options::default()
    }
}

/// Loads `records` rows, either in ascending key order or shuffled.
///
/// Shuffled is a full permutation and not a random draw, so both
/// databases hold exactly the same set of keys written exactly once. The
/// only thing that differs is the order the log received them in, which
/// is the variable.
fn load(path: &Path, records: u64, ascending: bool) -> Db {
    let db = Db::create(path, options(records)).expect("create");
    let mut order: Vec<u64> = (0..records).collect();
    if !ascending {
        let mut rng = Rng(0x243F6A8885A308D3);
        for i in (1..order.len()).rev() {
            let j = (rng.next() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
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

/// Start keys for the scans, the same list for every variant and every
/// database so no variant gets an easier set.
///
/// Uniform rather than YCSB's zipfian, for the reason the point
/// benchmark gives: a skewed start set lives in cache and in the mutable
/// tail and would flatter us.
fn starts(records: u64, scan: u64, n: usize) -> Vec<String> {
    let mut rng = Rng(0xB5026F5AA96619E9);
    (0..n)
        .map(|_| key(rng.next() % records.saturating_sub(scan).max(1)))
        .collect()
}

struct Row {
    what: &'static str,
    rows: u64,
    seconds: f64,
}

impl Row {
    fn per_row_us(&self) -> f64 {
        self.seconds * 1e6 / self.rows as f64
    }
}

fn main() {
    let records: u64 = env("ZU2_RECORDS", 200_000);
    let scan: u64 = env("ZU2_SCAN", 50);
    let iterations: usize = env("ZU2_ITERS", 20_000);
    // Repeats, because one pass of this is not stable enough to publish.
    // On a loaded machine the ascending case moved between 1.1 and 2.4
    // microseconds a row across three runs, which is wider than the
    // effect being measured. Each variant runs `repeats` times and the
    // fastest pass is the one reported: the slow passes are the machine
    // doing something else, and the fastest is the closest this gets to
    // the engine on its own.
    let repeats: usize = env("ZU2_REPEATS", 5);

    let dir = std::env::temp_dir().join(format!("zu2-scan-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");

    println!(
        "# zu2 scan decomposition, {records} records, {scan} rows a scan, \
         {iterations} scans a variant, best of {repeats}, one thread"
    );
    println!("# see tamnd/zu#708");

    let keys = starts(records, scan, iterations);

    for ascending in [true, false] {
        let name = if ascending { "ascending" } else { "shuffled" };
        let path = dir.join(format!("{name}.db"));
        let t = Instant::now();
        let db = load(&path, records, ascending);
        let loaded = t.elapsed().as_secs_f64();
        println!(
            "\n## log written in {name} key order, loaded in {loaded:.1}s, \
             {:.1} MiB on device",
            db.disk_bytes().expect("disk bytes") as f64 / (1 << 20) as f64
        );

        let mut rows: Vec<Row> = Vec::new();
        let mut keep = |what: &'static str, r: u64, seconds: f64| match rows
            .iter_mut()
            .find(|x| x.what == what)
        {
            Some(prev) => {
                if seconds / (r as f64) < prev.seconds / (prev.rows as f64) {
                    prev.rows = r;
                    prev.seconds = seconds;
                }
            }
            None => rows.push(Row {
                what,
                rows: r,
                seconds,
            }),
        };

        for _ in 0..repeats {
            // The plane on its own. `black_box` on the key so the walk is not
            // optimised down to stepping a pointer.
            {
                let ordered = db.core().ordered().expect("ordered");
                let t = Instant::now();
                let mut walked = 0u64;
                for start in &keys {
                    let mut cursor = ordered.seek(start.as_bytes());
                    for _ in 0..scan {
                        let Some(k) = cursor.key() else { break };
                        std::hint::black_box(k);
                        cursor.step();
                        walked += 1;
                    }
                }
                keep("walk", walked, t.elapsed().as_secs_f64());
            }

            // The real thing.
            {
                let mut s = db.session();
                let t = Instant::now();
                let mut got = 0u64;
                for start in &keys {
                    got += s
                        .scan(start.as_bytes(), scan as usize, |_k, v| {
                            std::hint::black_box(v);
                        })
                        .expect("scan") as u64;
                }
                keep("scan", got, t.elapsed().as_secs_f64());
            }

            // The same records, found the same way, read one at a time. The
            // keys come off the plane into a buffer first so the walk is not
            // in the timed region twice, and the reads that follow are the
            // ordinary point read path with no lookahead and no prefetch.
            {
                let ordered = db.core().ordered().expect("ordered");
                let mut s = db.session();
                let mut out = Vec::with_capacity(VALUE_BYTES);
                // One flat buffer of fixed width keys rather than a vector of
                // vectors. Every key here is `user` and nineteen digits, so
                // they are all the same length, and collecting them into
                // owned allocations meant fifty mallocs an iteration whose
                // cache churn landed on the reads being timed. That alone
                // was worth 3x against `dense` below on work that is
                // supposed to be identical, which is how it was noticed.
                let width = key(0).len();
                let mut batch = vec![0u8; scan as usize * width];
                let mut held;
                let mut got = 0u64;
                let mut seconds = 0.0;
                for start in &keys {
                    held = 0;
                    let mut cursor = ordered.seek(start.as_bytes());
                    for _ in 0..scan as usize {
                        let Some(k) = cursor.key() else { break };
                        debug_assert_eq!(k.len(), width);
                        batch[held * width..(held + 1) * width].copy_from_slice(k);
                        held += 1;
                        cursor.step();
                    }
                    let t = Instant::now();
                    for n in 0..held {
                        let k = &batch[n * width..(n + 1) * width];
                        if s.read(k, &mut out).expect("read") {
                            std::hint::black_box(&out);
                            got += 1;
                        }
                    }
                    seconds += t.elapsed().as_secs_f64();
                }
                keep("points", got, seconds);
            }

            // No plane at all. The keys are adjacent because they were built
            // that way, so this is the point read path over a dense range and
            // it is the floor the other three are measured against.
            {
                let mut s = db.session();
                let mut out = Vec::with_capacity(VALUE_BYTES);
                let mut rng = Rng(0xB5026F5AA96619E9);
                // Same care as `points`: the key is written into a buffer
                // that is already there rather than formatted into a fresh
                // String fifty times an iteration, so what is timed is the
                // read and not the allocator.
                let mut buf = key(0).into_bytes();
                let t = Instant::now();
                let mut got = 0u64;
                for _ in 0..iterations {
                    let base = rng.next() % records.saturating_sub(scan).max(1);
                    for j in 0..scan {
                        let mut n = base + j;
                        for d in (4..buf.len()).rev() {
                            buf[d] = b'0' + (n % 10) as u8;
                            n /= 10;
                        }
                        if s.read(&buf, &mut out).expect("read") {
                            std::hint::black_box(&out);
                            got += 1;
                        }
                    }
                }
                keep("dense", got, t.elapsed().as_secs_f64());
            }
        }

        println!("variant\trows\tseconds\tus a row\trows a second");
        for r in &rows {
            println!(
                "{}\t{}\t{:.2}\t{:.3}\t{:.0}",
                r.what,
                r.rows,
                r.seconds,
                r.per_row_us(),
                r.rows as f64 / r.seconds
            );
        }
        // The number #708 is actually asking for, stated rather than left
        // to be worked out from the table.
        let walk = rows[0].per_row_us();
        let full = rows[1].per_row_us();
        println!(
            "# the walk is {:.0}% of the scan, so {:.0}% of it is fetching the records",
            walk / full * 100.0,
            (full - walk) / full * 100.0
        );
        println!(
            "# a {scan} row scan costs {:.0} microseconds here",
            full * scan as f64
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
