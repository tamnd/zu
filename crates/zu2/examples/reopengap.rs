//! Where the reopen gap goes. See #665.
//!
//! A database that has just been opened scans slower than the same
//! database in the process that wrote it, and it stays slower after
//! every page is back in memory, which is the part #665 has no
//! explanation for. This loads one database, scans it, closes it, opens
//! it again, warms it and scans it again, and prints what the two
//! databases say about themselves either side of that.
//!
//!     RECORDS=100000 cargo run --release --example reopengap
//!
//! The numbers to read are the ones that differ between the two blocks.
//! Anything the same in both is not where the time is going.

use std::path::Path;
use std::time::Instant;

use zu2::{Db, Durability, Options};

fn key(i: u64) -> Vec<u8> {
    format!("user{i:012}").into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    let mut out = format!("field0={i}:").into_bytes();
    out.resize(1070, b'x');
    out
}

/// Scattered, the way go-ycsb's default `insertorder` of hashed hands
/// its keys over. 7919 is prime, so the walk is a permutation.
fn order(records: u64) -> impl Iterator<Item = u64> {
    (0..records).map(move |i| (i * 7919) % records)
}

fn options(records: u64) -> Options {
    Options {
        durability: Durability::Async,
        ordered: true,
        index_buckets: (records as usize / 4).next_power_of_two(),
        compact_below: 0,
        checkpoint_on_close: true,
        ..Options::default()
    }
}

/// What a database says about itself, printed the same way for both
/// sides so the two blocks can be read against each other.
fn describe(what: &str, db: &Db) {
    println!("{what}:");
    println!("  index buckets      {}", db.index_buckets());
    println!("  index keys         {}", db.index_keys());
    println!("  index occupancy    {}", db.index_occupancy());
    println!("  index foreign      {}", db.index_foreign());
    println!("  index grows        {}", db.index_grows());
    println!("  plane keys         {:?}", db.ordered_keys());
    println!("  plane bytes        {:?}", db.ordered_bytes());
    println!("  log bytes          {}", db.log_bytes());
    println!("  log span           {}", db.log_span());
    println!("  resident pages     {}", db.resident_pages());
    println!("  promoted           {}", db.promoted());
}

/// Scans the whole key range in runs of 50, the way workload E does,
/// and gives back nanoseconds a row.
fn scan_all(db: &Db, records: u64) -> u64 {
    let mut session = db.session();
    let mut rows = 0u64;
    let mut sink = 0u64;
    let started = Instant::now();
    let mut at = 0u64;
    while at < records {
        session
            .scan(&key(at), 50, |_, value| {
                rows += 1;
                sink += value[0] as u64;
            })
            .unwrap();
        at += 50;
    }
    let took = started.elapsed();
    assert!(sink > 0);
    took.as_nanos() as u64 / rows.max(1)
}

/// Reads every key in key order, which is the same three misses a scan
/// takes without the plane walk in front of them.
fn read_all(db: &Db, records: u64) -> u64 {
    let mut session = db.session();
    let mut out = Vec::new();
    let started = Instant::now();
    for i in 0..records {
        session.read(&key(i), &mut out).unwrap();
    }
    let took = started.elapsed();
    took.as_nanos() as u64 / records.max(1)
}

fn main() {
    let records: u64 = std::env::var("RECORDS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(100_000);
    let dir = tempfile::tempdir().unwrap();
    let path: &Path = &dir.path().join("gap.zu2");

    {
        let db = Db::create(path, options(records)).unwrap();
        {
            let mut session = db.session();
            for i in order(records) {
                session.upsert(&key(i), &value(i)).unwrap();
            }
        }
        // Once through untimed, so both sides are measured on a database
        // whose pages something has already touched.
        scan_all(&db, records);
        describe("written", &db);
        println!("  scan               {} ns a row", scan_all(&db, records));
        println!("  read               {} ns a key", read_all(&db, records));
        db.sync().unwrap();
    }

    let db = Db::open(path, options(records)).unwrap();
    // The warm thread is reading the log back while this runs, so wait
    // for it to stop moving before anything is timed. Otherwise the
    // first pass measures the race and not the database.
    let mut last = 0;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let now = db.resident_pages();
        if now == last {
            break;
        }
        last = now;
    }
    scan_all(&db, records);
    describe("reopened", &db);
    println!("  scan               {} ns a row", scan_all(&db, records));
    println!("  read               {} ns a key", read_all(&db, records));
}
