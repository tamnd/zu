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
//! The workload is YCSB E as `core/workload_e.go` runs it: a start key
//! drawn uniformly over the key space and a length drawn uniformly from
//! 1 to 100, over ten fields of a hundred bytes.

use std::time::Instant;

use zu2::{Db, Durability, Options};

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

fn main() {
    let records: u64 = std::env::var("RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let scans: u64 = std::env::var("SCANS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("e.zu2"),
        Options {
            durability: Durability::Async,
            ordered: true,
            ..Options::default()
        },
    )
    .expect("create");

    let mut s = db.session();
    let started = Instant::now();
    for i in 0..records {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    let load = started.elapsed();

    let mut state = 0x2545_f491_4f6c_dd1d;
    let mut rows = 0u64;
    let mut bytes = 0u64;
    let started = Instant::now();
    for _ in 0..scans {
        let start = key(next(&mut state) % records);
        let count = (next(&mut state) % 100 + 1) as usize;
        rows += s
            .scan(&start, count, |_, value| bytes += value.len() as u64)
            .expect("scan") as u64;
    }
    let took = started.elapsed();

    println!();
    println!(
        "{records} records of {} bytes, ordered, async",
        value(0).len()
    );
    println!(
        "load    {:.0} records a second",
        records as f64 / load.as_secs_f64()
    );
    println!(
        "scan    {:.0} scans a second, {:.0} rows a second, {:.1} rows a scan",
        scans as f64 / took.as_secs_f64(),
        rows as f64 / took.as_secs_f64(),
        rows as f64 / scans as f64
    );
    println!(
        "        {:.0} ns a row, {:.0} MiB a second out of the engine",
        took.as_nanos() as f64 / rows as f64,
        bytes as f64 / took.as_secs_f64() / (1024.0 * 1024.0)
    );
    println!();
    println!(
        "A row here is a plane step, a hash, a chain walk and one copy \
         into the callback's\nbuffer. Whatever the harness reports below \
         this is the Go side and the C boundary."
    );
}
