//! Point read latency, zu2 against sqlite, with no client in the way.
//!
//! This exists because the milestone's latency claim cannot be measured
//! through go-ycsb and it took a while to see why. The YCSB client and
//! the cgo crossing cost about 390 ns an operation before any engine is
//! reached, and every engine pays it. A zu2 read is around 34 ns and a
//! sqlite read around 2500, so the true ratio is about 73x, but what
//! YCSB reports is (390 + 34) against (390 + 2500), which is 6.8x. The
//! floor does not cancel. It compresses every ratio toward 1, and it
//! compresses hardest exactly when the engine is fastest, so the better
//! zu2 gets the more the harness understates it. No amount of re-running
//! fixes that. The measurement has to move below the client.
//!
//! So: one process, both engines, the same histogram, the same clock,
//! the same calibration, the same keys in the same order. The only
//! difference between the two sides is the engine call in the middle of
//! the loop. That is the strongest form of this comparison available,
//! and it is worth more than a faster number taken two different ways.
//!
//! sqlite is given its fastest configuration rather than its default
//! one, because a comparison against a rival's slow mode is not worth
//! printing. WAL, synchronous off, a page cache, and an 8 GiB mmap over
//! the file on top of it, with a prepared statement reused for the life
//! of each thread and the blob copied into a buffer that is reused too.
//! The mapping is the part that matters for a read: it is file backed
//! and shared between connections, so the whole database is reachable
//! without a copy into any one thread's cache. If sqlite loses here it
//! is not because it was holding a hand behind its back.
//!
//! Each engine is measured twice, in the order zu2, sqlite, sqlite, zu2.
//! If a host drifts under the run, or if one engine leaves the machine
//! in a state that helps or hurts the next, the two passes for an engine
//! disagree and the table says so instead of hiding it in an average.
//!
//! usage: readlatency [records] [ops] [threads]

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

#[path = "common/hist.rs"]
mod hist;
use hist::{Hist, SAMPLE, clock_granularity, clock_overhead};

use zu2::{Db, Durability, Options};

/// The YCSB record, ten fields of a hundred bytes, same as the sweep.
const VALUE: usize = 1000;

/// The YCSB key.
fn key(i: u64, into: &mut String) {
    into.clear();
    write!(into, "user{i:019}").expect("format");
}

/// The key sequence both engines walk. splitmix64, so the order is
/// scattered rather than the order the rows were written, which is what
/// keeps this a point read benchmark rather than a sequential scan with
/// extra steps. Both engines get the same seed and therefore the same
/// keys in the same order.
struct Keys {
    state: u64,
    span: u64,
    lo: u64,
}

impl Keys {
    fn new(seed: u64, lo: u64, span: u64) -> Self {
        Keys {
            state: seed,
            span,
            lo,
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.lo + ((z ^ (z >> 31)) % self.span)
    }
}

/// Both read loops have this shape, and it is written once so the two
/// engines cannot drift apart in how they sample or where they put the
/// clock calls. `read` returns the number of bytes it found, and zero
/// means the key was missing, which is a bug in the load rather than a
/// result and is treated as one.
fn measure<F>(
    mut read: F,
    ops: u64,
    seed: u64,
    lo: u64,
    span: u64,
    phase: u64,
    overhead: u64,
) -> Hist
where
    F: FnMut(&str) -> usize,
{
    let mut keys = Keys::new(seed, lo, span);
    let mut k = String::with_capacity(32);
    let mut h = Hist::new();
    for i in 0..ops {
        let at = keys.next();
        key(at, &mut k);
        if i % SAMPLE == phase {
            let started = Instant::now();
            let got = read(&k);
            let ns = (started.elapsed().as_nanos() as u64).saturating_sub(overhead);
            h.add(ns);
            assert!(got > 0, "missing key {at}");
        } else {
            assert!(read(&k) > 0, "missing key {at}");
        }
    }
    h
}

/// One pass over one engine at `threads` threads, returning the merged
/// histogram and the wall clock rate.
fn pass<S, F>(threads: usize, ops: u64, records: u64, overhead: u64, open: S) -> (Hist, f64)
where
    S: Fn(usize) -> F + Sync,
    F: FnMut(&str) -> usize + Send,
{
    let each = ops / threads as u64;
    let open = &open;
    let started = Instant::now();
    let hists: Vec<Hist> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    let reader = open(t);
                    // Every thread reads the whole key space, not a
                    // slice of it. A slice per thread is the friendlier
                    // shape for both engines and it is the one
                    // readscale uses to isolate contention, but a
                    // latency table wants the shape a client actually
                    // produces, which is uniform over everything.
                    measure(
                        reader,
                        each,
                        0x5eed_0000 ^ t as u64,
                        0,
                        records,
                        t as u64 % SAMPLE,
                        overhead,
                    )
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("worker"))
            .collect()
    });
    let took = started.elapsed().as_secs_f64();
    let mut merged = Hist::new();
    for h in &hists {
        merged.merge(h);
    }
    (merged, (each * threads as u64) as f64 / took)
}

fn open_sqlite(path: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).expect("sqlite open");
    // sqlite's fastest honest read configuration.
    //
    // The cache is per connection, which is per thread here, and that is
    // the detail that matters. It was a gigabyte and at eight threads
    // that is eight gigabytes of page cache on a box with four free.
    // The first attempt at this on server3 swapped instead of running
    // and produced nothing in half an hour, which is not a slow
    // benchmark, it is a benchmark of the swap. Sixty four mebibytes,
    // and the mapping below is what covers the rest: mmap pages are
    // file backed and shared between connections, so raising that costs
    // address space rather than memory.
    for p in [
        "PRAGMA journal_mode = WAL",
        "PRAGMA synchronous = OFF",
        "PRAGMA cache_size = -65536",
        "PRAGMA mmap_size = 8589934592",
        "PRAGMA temp_store = MEMORY",
    ] {
        // query_row rather than execute, because journal_mode returns a
        // row and execute refuses statements that do.
        let _ = c.query_row(p, [], |_| Ok(()));
    }
    c
}

fn main() {
    let mut a = std::env::args().skip(1);
    let records: u64 = a.next().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let ops: u64 = a.next().and_then(|v| v.parse().ok()).unwrap_or(4_000_000);
    let threads: usize = a.next().and_then(|v| v.parse().ok()).unwrap_or(1);

    let dir = tempfile::tempdir().expect("tempdir");
    let zu_path = dir.path().join("readlatency.zu2");
    let sq_path = dir.path().join("readlatency.db");

    let value = vec![b'v'; VALUE];

    // Not behind an `Arc`. `pass` runs its workers in a scoped thread
    // group, so a plain borrow of this outlives them, and a session
    // borrows the `Db` it came from: an `Arc` clone moved into the
    // reader closure would make that closure refer to its own field.
    let db = Db::create(
        &zu_path,
        Options {
            durability: Durability::Async,
            index_buckets: (records / 4 + 1) as usize,
            ..Options::default()
        },
    )
    .expect("create");
    {
        let mut s = db.session();
        let mut k = String::with_capacity(32);
        for i in 0..records {
            key(i, &mut k);
            s.upsert(k.as_bytes(), &value).expect("upsert");
        }
    }

    {
        let c = open_sqlite(&sq_path);
        c.execute_batch(
            "CREATE TABLE usertable (ycsb_key TEXT PRIMARY KEY, field0 BLOB) WITHOUT ROWID;",
        )
        .expect("create table");
        let tx = c.unchecked_transaction().expect("tx");
        {
            let mut ins = c
                .prepare("INSERT INTO usertable (ycsb_key, field0) VALUES (?1, ?2)")
                .expect("prepare insert");
            let mut k = String::with_capacity(32);
            for i in 0..records {
                key(i, &mut k);
                ins.execute(rusqlite::params![k.as_str(), &value])
                    .expect("insert");
            }
        }
        tx.commit().expect("commit");
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); ANALYZE;")
            .expect("checkpoint");
    }

    let overhead = clock_overhead();
    let tick = clock_granularity();

    // What the run needs, said before the numbers rather than after,
    // because a host that cannot hold this swaps and then every row
    // below is a measurement of the swap and not of either engine. zu2
    // holds its whole log in the process by default (#636) and sqlite
    // keeps a bounded cache per connection on top of its file, so the
    // rough floor is one database per engine plus the caches.
    let footprint = records * (VALUE as u64 + 64) * 2 + (threads as u64 * 64 << 20);
    println!(
        "# this run wants about {} MiB of memory; on a host with less than that the rows below \
         are measuring the swap",
        footprint >> 20
    );
    println!("# records {records}, ops {ops}, threads {threads}, value {VALUE} bytes");
    println!(
        "# one read in {SAMPLE} timed, clock overhead {overhead} ns subtracted from every sample"
    );
    println!("# clock granularity {tick} ns");
    if tick > 10 {
        println!(
            "# that is coarse next to a point read, so every percentile below is a multiple of it \
             and should be read as a bound; the mean is over millions of ops and is not affected"
        );
    }
    println!(
        "# sqlite: WAL, synchronous off, 64 MiB page cache a connection, 8 GiB mmap, WITHOUT ROWID, prepared statement reused"
    );
    println!(
        "# ns/op is the mean including thread joins; p50 is the median of the sampled reads. They are not the same measurement."
    );
    println!("pass  engine   ops/s        ns/op   p50   p95    p99   p999");

    let zu2_reader = |_t: usize| {
        let mut s = db.session();
        let mut out = Vec::with_capacity(VALUE);
        move |k: &str| {
            out.clear();
            if s.read(k.as_bytes(), &mut out).expect("read") {
                out.len()
            } else {
                0
            }
        }
    };
    let sq_reader = |_t: usize| {
        let c = open_sqlite(&sq_path);
        let mut out = Vec::with_capacity(VALUE);
        move |k: &str| {
            // The statement is prepared once per read rather than once
            // per thread only because rusqlite ties a Statement's
            // lifetime to its Connection and both live in this closure.
            // sqlite caches prepared statements internally, which is
            // what `prepare_cached` uses, so this is a hash lookup and
            // not a parse.
            let mut stmt = c
                .prepare_cached("SELECT field0 FROM usertable WHERE ycsb_key = ?1")
                .expect("prepare");
            stmt.query_row([k], |r| {
                let b = r.get_ref(0)?.as_blob().expect("blob");
                out.clear();
                out.extend_from_slice(b);
                Ok(out.len())
            })
            .unwrap_or(0)
        }
    };

    // A warm pass over each engine that is not printed, so neither one
    // pays the other's first touch faults.
    pass(threads, ops / 8, records, overhead, zu2_reader);
    pass(threads, ops / 8, records, overhead, sq_reader);

    // zu2, sqlite, sqlite, zu2. See the header comment: the outer pair
    // and the inner pair bracket each other, so drift shows up as the
    // two rows for one engine disagreeing.
    for (n, engine) in [(1, "zu2"), (1, "sqlite"), (2, "sqlite"), (2, "zu2")] {
        let (h, rate) = if engine == "zu2" {
            pass(threads, ops, records, overhead, zu2_reader)
        } else {
            pass(threads, ops, records, overhead, sq_reader)
        };
        println!(
            "{n:4}  {engine:7}  {rate:9.0}  {:6.0}  {:4}  {:4}  {:5}  {:5}",
            1e9 / rate * threads as f64,
            h.quantile(0.50),
            h.quantile(0.95),
            h.quantile(0.99),
            h.quantile(0.999)
        );
    }
}
