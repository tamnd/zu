//! What a workload E scan costs inside the engine, so the harness
//! number has something to be compared against.
//!
//! go-ycsb reports 21573 scans a second for zu2 on server2 and 2883 for
//! sqlite, which is 7.5x and short of the 10x #377 asks for. That number
//! includes everything the Go side does with a row: a copy out of the C
//! buffer, a decode, and a `map[string][]byte` with ten entries in it,
//! all of which sqlite pays too. So the question this answers is how
//! much of a scan is the engine and how much is the harness, because
//! they take different work to fix.
//!
//! Both engines are here rather than zu2 alone, because a number with
//! nothing beside it settles nothing. #647 asks that a milestone claim
//! quote the in process example and not the harness, and a claim of the
//! form "10x sqlite" needs sqlite measured in the same process, on the
//! same rows, from the same start keys, with the same work done to each
//! row that comes back. That is what this does.
//!
//! The workload is YCSB E as `core/workload_e.go` runs it: a start key
//! drawn uniformly over the key space and a length drawn uniformly from
//! 1 to 100, over ten fields of a hundred bytes. Every engine gets the
//! same list of draws, generated once before any of them runs, so the
//! comparison is not also a comparison of two random sequences.
//!
//! Neither side copies a row. zu2 hands the callback a borrow of the
//! record and sqlite hands out a borrow of the column, and this counts
//! the length of each without taking a copy of it. Asking rusqlite for a
//! `Vec<u8>` instead would put an allocation and a memcpy a row on the
//! sqlite side that zu2 does not pay, and it would be measuring a
//! binding rather than an engine.
//!
//!     cargo run --release --example scanbench
//!     RECORDS=1000000 SCANS=100000 cargo run --release --example scanbench

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use zu2::{Db, Durability, Options};

/// The draw sequence. Fixed, so two runs of this example on the same
/// host are comparable and so every engine within one run sees the same
/// scans in the same order.
const SEED: u64 = 0x2545_f491_4f6c_dd1d;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

/// Ten fields of a hundred bytes, the shape a YCSB row has once the
/// harness has encoded it.
fn value(i: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1100);
    for field in 0..10u64 {
        out.extend_from_slice(format!("field{field}=").as_bytes());
        let seed = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ field;
        for byte in 0..100u64 {
            out.push(b'a' + ((seed >> (byte % 56)) % 26) as u8);
        }
    }
    out
}

/// The same rejection free draw the other examples use, minus the skew:
/// a multiply and a mask is enough for a start key.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Every scan the run will do, as a start key index and a length.
fn draws(scans: u64, records: u64) -> Vec<(u64, usize)> {
    let mut state = SEED;
    (0..scans)
        .map(|_| {
            let start = next(&mut state) % records;
            let count = (next(&mut state) % 100 + 1) as usize;
            (start, count)
        })
        .collect()
}

/// What one engine did, in the two phases worth timing.
struct Took {
    load: Duration,
    scan: Duration,
    rows: u64,
    bytes: u64,
}

/// The order the keys go in.
///
/// go-ycsb's default `insertorder` is `hashed`, so a real load hands
/// the keys over in an order that has nothing to do with the key order,
/// and both of these engines end up with their rows on the device in
/// that order too. Loading ascending instead puts a zu2 log in key
/// order, which makes a scan a walk along the log rather than a walk
/// around it, and a number taken that way is a number about the
/// benchmark and not about the engine.
///
/// Both engines get the same order, whichever it is. 7919 is prime, so
/// stepping by it covers every key exactly once for any record count
/// that is not a multiple of it.
fn order(records: u64, scattered: bool) -> impl Iterator<Item = u64> {
    (0..records).map(move |i| if scattered { (i * 7919) % records } else { i })
}

/// `reopen` closes the database after the load and opens it again
/// before the scan. It is not a durability test, it is the state every
/// benchmark run phase is actually in: go-ycsb loads in one process and
/// runs in another, so every published run phase number is a number
/// taken on a database that has just been opened.
///
/// It went in to ask about the plane's arena order, and it cannot
/// answer that on its own: a node is allocated when its key is first
/// written, so it takes `ZU2_SCATTER` for the written plane's nodes to
/// be in an order the walk does not follow. What it does show without
/// any help is that a reopened database scans three times slower on the
/// first pass and half again slower after that, which is #665.
fn run_zu2(dir: &Path, records: u64, plan: &[(u64, usize)], reopen: bool, scattered: bool) -> Took {
    let path = dir.join("e.zu2");
    let options = Options {
        durability: Durability::Async,
        ordered: true,
        checkpoint_on_close: reopen,
        ..Options::default()
    };
    let db = Db::create(&path, options).expect("create");

    let started = Instant::now();
    {
        let mut s = db.session();
        for i in order(records, scattered) {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
    }
    let load = started.elapsed();

    let db = if reopen {
        db.sync().expect("sync");
        drop(db);
        Db::open(&path, options).expect("reopen")
    } else {
        db
    };
    let mut s = db.session();

    // ZU2_WARM runs the plan once before the clock starts. A reopened
    // database holds none of its log in memory and reads a page in when
    // something asks for a record in it, so the first pass over a range
    // pays for the pages under it and the second does not. Off by
    // default: a published number is the one a caller gets on the pass
    // it asks for.
    if std::env::var("ZU2_WARM").is_ok() {
        for &(start, count) in plan {
            s.scan(&key(start), count, |_, _| {}).expect("warm");
        }
    }

    let mut rows = 0u64;
    let mut bytes = 0u64;
    let started = Instant::now();
    for &(start, count) in plan {
        rows += s
            .scan(&key(start), count, |_, value| bytes += value.len() as u64)
            .expect("scan") as u64;
    }
    let scan = started.elapsed();

    Took {
        load,
        scan,
        rows,
        bytes,
    }
}

/// sqlite at its fastest: write ahead log, nothing waiting for the
/// device, a big page cache and the file mapped. The same settings the
/// point bench loads at, which is the setting this comparison would be
/// accused of leaving off if it ran anything slower.
fn connect(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open sqlite");
    conn.execute_batch(
        "PRAGMA busy_timeout=60000;
         PRAGMA synchronous=OFF;
         PRAGMA cache_size=-262144;
         PRAGMA mmap_size=268435456;
         PRAGMA temp_store=MEMORY;",
    )
    .expect("pragmas");
    conn
}

/// One sqlite database of one table shape, loaded and then scanned.
///
/// The shape matters more here than it does on a point read. A rowid
/// table keeps the key in an index and the row in a B tree of its own,
/// so a range scan walks the index in key order and then goes to the
/// table once a row. A `WITHOUT ROWID` table is the index, so the walk
/// is the rows. Both are run and the faster one is the one zu2 is
/// compared against.
fn run_sqlite(
    dir: &Path,
    records: u64,
    plan: &[(u64, usize)],
    rowid: bool,
    scattered: bool,
) -> Took {
    let shape = if rowid { "rowid" } else { "norowid" };
    let path = dir.join(format!("e-{shape}.db"));
    let conn = connect(&path);
    let suffix = if rowid { "" } else { " WITHOUT ROWID" };
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE usertable (ykey TEXT PRIMARY KEY, yvalue BLOB NOT NULL){suffix};"
    ))
    .expect("create table");

    let started = Instant::now();
    {
        let mut stmt = conn
            .prepare("INSERT OR REPLACE INTO usertable (ykey, yvalue) VALUES (?1, ?2)")
            .expect("prepare insert");
        for i in order(records, scattered) {
            stmt.execute(rusqlite::params![key(i), value(i)])
                .expect("insert");
        }
    }
    let load = started.elapsed();

    let mut rows = 0u64;
    let mut bytes = 0u64;
    let started = Instant::now();
    {
        let mut stmt = conn
            .prepare("SELECT yvalue FROM usertable WHERE ykey >= ?1 ORDER BY ykey LIMIT ?2")
            .expect("prepare scan");
        for &(start, count) in plan {
            let mut got = stmt
                .query(rusqlite::params![key(start), count as i64])
                .expect("scan");
            while let Some(row) = got.next().expect("row") {
                let blob = row.get_ref(0).expect("column").as_blob().expect("blob");
                bytes += blob.len() as u64;
                rows += 1;
            }
        }
    }
    let scan = started.elapsed();

    Took {
        load,
        scan,
        rows,
        bytes,
    }
}

fn report(name: &str, records: u64, scans: u64, took: &Took) {
    println!("{name}");
    println!(
        "  load  {:.0} records a second",
        records as f64 / took.load.as_secs_f64()
    );
    println!(
        "  scan  {:.0} scans a second, {:.0} rows a second, {:.1} rows a scan",
        scans as f64 / took.scan.as_secs_f64(),
        took.rows as f64 / took.scan.as_secs_f64(),
        took.rows as f64 / scans as f64
    );
    println!(
        "        {:.0} ns a row, {:.0} MiB a second out of the engine",
        took.scan.as_nanos() as f64 / took.rows as f64,
        took.bytes as f64 / took.scan.as_secs_f64() / (1024.0 * 1024.0)
    );
}

fn main() {
    let records: u64 = std::env::var("RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let scans: u64 = std::env::var("SCANS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    // ZU2_ONLY skips the sqlite side. It is for the case where two
    // builds of zu2 are being compared against each other rather than
    // against another engine, where the sqlite runs are two thirds of
    // the wall clock and none of the answer. A published number is a
    // full run.
    let only = std::env::var("ZU2_ONLY").is_ok();

    let plan = draws(scans, records);
    let dir = tempfile::tempdir().expect("tempdir");

    // ZU2_REOPEN loads, closes and opens again before the scan, so the
    // plane's nodes are in the arena in key order rather than in the
    // order the keys were written. See [`run_zu2`].
    let reopen = std::env::var("ZU2_REOPEN").is_ok();

    // ZU2_SCATTER hands the keys over in an order that has nothing to
    // do with the key order, which is what go-ycsb's default does. See
    // [`order`].
    let scattered = std::env::var("ZU2_SCATTER").is_ok();

    let zu2 = run_zu2(dir.path(), records, &plan, reopen, scattered);

    println!();
    println!(
        "{records} records of {} bytes, {scans} scans, one thread, in process",
        value(0).len()
    );
    println!();
    let name = match (reopen, scattered) {
        (true, true) => "zu2 async, ordered, reopened, scattered load",
        (true, false) => "zu2 async, ordered, reopened",
        (false, true) => "zu2 async, ordered, scattered load",
        (false, false) => "zu2 async, ordered",
    };
    report(name, records, scans, &zu2);
    if only {
        return;
    }

    let rowid = run_sqlite(dir.path(), records, &plan, true, scattered);
    let norowid = run_sqlite(dir.path(), records, &plan, false, scattered);

    // A scan that hands back fewer rows than another engine handed back
    // from the same draws is a faster scan for the wrong reason, and it
    // has happened here before: an engine that returns nothing returns
    // it very quickly (tamnd/zu#560). The rows are the same draws, so
    // the counts have to agree.
    assert_eq!(
        zu2.rows, rowid.rows,
        "zu2 and rowid sqlite disagree on rows"
    );
    assert_eq!(
        rowid.rows, norowid.rows,
        "the two sqlite shapes disagree on rows"
    );

    report("sqlite wal/off, rowid", records, scans, &rowid);
    report("sqlite wal/off, without rowid", records, scans, &norowid);

    let best = if norowid.scan < rowid.scan {
        ("without rowid", &norowid)
    } else {
        ("rowid", &rowid)
    };
    println!();
    println!(
        "zu2 is {:.1}x the faster sqlite shape ({}) on scan throughput, \
         {:.0} ns a row against {:.0}.",
        best.1.scan.as_secs_f64() / zu2.scan.as_secs_f64(),
        best.0,
        zu2.scan.as_nanos() as f64 / zu2.rows as f64,
        best.1.scan.as_nanos() as f64 / best.1.rows as f64,
    );
    println!(
        "A row on the zu2 side is a plane step, a hash, a chain walk and a \
         borrow of the\nrecord. Whatever go-ycsb reports for either engine \
         below this is the Go side\nand, for zu2, the C boundary."
    );
}
