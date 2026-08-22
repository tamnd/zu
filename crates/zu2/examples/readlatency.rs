//! Point read latency, zu2 against sqlite, with no client in the way.
//!
//! This exists because the milestone's latency claim cannot be measured
//! through go-ycsb and it took a while to see why. The YCSB client and
//! the cgo crossing cost about 390 ns an operation before any engine is
//! reached, and every engine pays it. A zu2 read is a few hundred
//! nanoseconds and a sqlite read a few thousand, so the harness compares
//! (390 + ours) against (390 + theirs) and the floor does not cancel. It
//! compresses every ratio toward 1, and it compresses hardest exactly
//! when the engine is fastest, so the better zu2 gets the more the
//! harness understates it. No amount of re-running fixes that. The
//! measurement has to move below the client.
//!
//! So: one process, both engines, the same histogram, the same clock,
//! the same calibration, the same keys in the same order. The only
//! difference between the two sides is the engine call in the middle of
//! the loop. That is the strongest form of this comparison available,
//! and it is worth more than a faster number taken two different ways.
//!
//! sqlite is given its fastest configuration rather than its default
//! one, because a comparison against a rival's slow mode is not worth
//! printing, and getting that right took two corrections that each moved
//! sqlite by a large factor.
//!
//! WAL, synchronous off, a page cache, an 8 GiB mmap over the file,
//! `WITHOUT ROWID` so a lookup is one B-tree descent rather than two, a
//! prepared statement reused for the life of each thread, and the blob
//! copied into a buffer that is reused too.
//!
//! Then `SQLITE_CONFIG_MEMSTATUS` off. sqlite's default allocator takes
//! one process wide mutex per malloc to maintain a byte counter, so at
//! eight threads every read serialises on it and sqlite gets slower with
//! more threads rather than faster. Turning it off moved sqlite from
//! 83000 reads a second to 635000 at eight threads on an M4, which is
//! 7.6x, and without it this table would have been reporting a lock
//! artefact as though it were a lookup cost.
//!
//! Then a second sqlite row, `sqlite+txn`, which holds one read
//! transaction open per thread instead of letting every read be its own
//! implicit one. That is what sqlite's own documentation recommends for
//! many small reads and it is the closer analogue of a zu2 session,
//! which holds an epoch for its lifetime. It is worth another 2.1x at
//! eight threads. Both rows are printed and the ratio worth quoting is
//! against the better of them.
//!
//! Each engine is measured twice, zu2, sqlite, sqlite+txn, sqlite+txn,
//! sqlite, zu2. If a host drifts under the run, or if one engine leaves
//! the machine in a state that helps or hurts the next, the two passes
//! for an engine disagree and the table says so instead of hiding it in
//! an average.
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

/// Turn off sqlite's memory statistics before anything opens a
/// connection.
///
/// This is the difference between sqlite scaling and sqlite not scaling,
/// and it is not obvious. sqlite's default allocator takes a single
/// process wide mutex on every malloc and free so it can keep a running
/// total for `sqlite3_memory_used`. A point read does several
/// allocations, so at n threads every read serialises n ways on one lock
/// that exists only to maintain a counter nobody in this benchmark
/// reads. Measured at eight threads it costs sqlite most of its
/// throughput, and without this the table would be reporting a lock
/// contention artefact as if it were a lookup cost.
///
/// `SQLITE_CONFIG_MEMSTATUS` has to be set before `sqlite3_initialize`,
/// and rusqlite may already have run it, so shut the library down first.
/// Nothing is open at this point.
fn sqlite_drop_memstatus() {
    // SQLITE_CONFIG_MEMSTATUS is 9. rusqlite's ffi does not name it.
    const SQLITE_CONFIG_MEMSTATUS: i32 = 9;
    unsafe {
        rusqlite::ffi::sqlite3_shutdown();
        let rc = rusqlite::ffi::sqlite3_config(SQLITE_CONFIG_MEMSTATUS, 0);
        rusqlite::ffi::sqlite3_initialize();
        assert_eq!(rc, rusqlite::ffi::SQLITE_OK, "sqlite3_config memstatus");
    }
}

fn open_sqlite(path: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).expect("sqlite open");
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

    sqlite_drop_memstatus();

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
    let footprint = records * (VALUE as u64 + 64) * 2 + ((threads as u64 * 64) << 20);
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
        "# sqlite: WAL, synchronous off, 64 MiB page cache a connection, 8 GiB mmap, WITHOUT ROWID, prepared statement reused, memstatus off"
    );
    println!(
        "# ns/op is the mean including thread joins; p50 is the median of the sampled reads. They are not the same measurement."
    );
    println!("pass  engine      ops/s        ns/op   p50   p95    p99   p999");

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
    // sqlite twice, because at more than one thread the two shapes are
    // an order of magnitude apart and publishing only the slower one
    // would be a comparison against a rival's bad day.
    //
    // `sqlite` gives every read its own implicit transaction, which is
    // what a YCSB style client does. `sqlite+txn` holds one read
    // transaction open per thread for the life of the pass, which is
    // what sqlite's documentation recommends for many small reads and
    // is the closer analogue of a zu2 session holding an epoch. At one
    // thread it is worth about 1.4x and at eight about 2.1x, so it is
    // not a rounding difference and printing only the first would be
    // quoting a rival's slower shape.
    // Borrowed once, because the closure below is called twice and a
    // `move` closure that captured the `PathBuf` itself would be
    // callable only once. A `&Path` is Copy and can be handed to both.
    let sq_ref: &Path = &sq_path;
    let sq_reader_at = |held: bool| {
        move |_t: usize| {
            let c = open_sqlite(sq_ref);
            if held {
                c.execute_batch("BEGIN").expect("begin");
            }
            let mut out = Vec::with_capacity(VALUE);
            move |k: &str| {
                // The statement is prepared once per read rather than
                // once per thread only because rusqlite ties a
                // Statement's lifetime to its Connection and both live
                // in this closure. sqlite caches prepared statements
                // internally, which is what `prepare_cached` uses, so
                // this is a hash lookup and not a parse.
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
        }
    };
    let sq_reader = sq_reader_at(false);
    let sq_txn_reader = sq_reader_at(true);

    // A warm pass over each engine that is not printed, so neither one
    // pays the other's first touch faults.
    pass(threads, ops / 8, records, overhead, zu2_reader);
    pass(threads, ops / 8, records, overhead, sq_reader);

    // zu2, sqlite, sqlite, zu2, with the held transaction shape sitting
    // inside that. See the header comment: the outer pair and the inner
    // pair bracket each other, so drift shows up as the two rows for one
    // engine disagreeing.
    for (n, engine) in [
        (1, "zu2"),
        (1, "sqlite"),
        (1, "sqlite+txn"),
        (2, "sqlite+txn"),
        (2, "sqlite"),
        (2, "zu2"),
    ] {
        let (h, rate) = match engine {
            "zu2" => pass(threads, ops, records, overhead, zu2_reader),
            "sqlite" => pass(threads, ops, records, overhead, sq_reader),
            _ => pass(threads, ops, records, overhead, sq_txn_reader),
        };
        println!(
            "{n:4}  {engine:10}  {rate:9.0}  {:6.0}  {:4}  {:4}  {:5}  {:5}",
            1e9 / rate * threads as f64,
            h.quantile(0.50),
            h.quantile(0.95),
            h.quantile(0.99),
            h.quantile(0.999)
        );
    }
}
