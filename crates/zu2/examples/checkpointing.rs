//! What a checkpoint costs to take and what it saves on the way back,
//! which is the whole of Z10 in two numbers.
//!
//! Recovery without one reads every record in the file: a checksum, a
//! hash and an index probe each, plus whatever links have to be repaired
//! and the pages those repairs put back. Compaction bounds that by the
//! live set rather than by everything ever written, so it does not grow
//! without limit, but it does grow with the database, and an open that
//! costs seconds is an open nobody can do casually.
//!
//! A checkpoint replaces it with a read of the two planes and a walk of
//! whatever was appended after the capture. What it costs is a pause,
//! because the capture takes the barrier rather than moving sessions
//! between versions the way Concurrent Prefix Recovery does, and the
//! reason is in `src/checkpoint.rs`. A pause has to be measured or it is
//! an assumption, so this measures it, at rest and under four writers.
//!
//! Four sizes, each reopened twice: once with the checkpoint the close
//! wrote, and once with it taken away, which is the same file recovered
//! the old way.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use zu2::{Db, Durability, Options};

const VALUE_BYTES: usize = 100;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

fn options(records: u64, checkpoint: bool) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: (records as usize / 4).next_power_of_two(),
        max_pages: 1 << 14,
        max_nodes: 1 << 10,
        // Off, so what is reopened is the log the writes left rather
        // than one a pass happened to have rewritten. Compaction and
        // checkpointing meet in `tests/checkpointing.rs`.
        compact_below: 0,
        checkpoint_on_close: checkpoint,
        ..Options::default()
    }
}

fn sidecar(path: &Path) -> std::path::PathBuf {
    let mut beside = path.to_path_buf().into_os_string();
    beside.push(".ckpt");
    std::path::PathBuf::from(beside)
}

/// A database of `records` keys, closed with a checkpoint.
fn load(path: &Path, records: u64) {
    let db = Db::create(path, options(records, true)).expect("create");
    let mut session = db.session();
    let value = vec![b'x'; VALUE_BYTES];
    for i in 0..records {
        session.upsert(&key(i), &value).expect("upsert");
    }
    drop(session);
    db.sync().expect("sync");
}

/// Opens the database and reads every key, and says how long each of
/// those took.
fn reopen(path: &Path, records: u64) -> (f64, f64, u64, usize) {
    let at = Instant::now();
    let db = Db::open(path, options(records, false)).expect("open");
    let open = at.elapsed().as_secs_f64();
    let walked = db
        .recovered()
        .records
        .load(std::sync::atomic::Ordering::Relaxed);
    let pages = db.resident_pages();
    let at = Instant::now();
    let mut session = db.session();
    let mut out = Vec::new();
    for i in 0..records {
        assert!(session.read(&key(i), &mut out).expect("read"), "lost {i}");
    }
    let read = at.elapsed().as_secs_f64();
    (open, read, walked, pages)
}

/// The pause a capture costs, at rest and with four writers running.
fn pauses(records: u64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pause.zu2");
    let options = Options {
        sessions: 16,
        ..options(records, false)
    };
    let db = Db::create(&path, options).expect("create");
    {
        let mut session = db.session();
        let value = vec![b'x'; VALUE_BYTES];
        for i in 0..records {
            session.upsert(&key(i), &value).expect("upsert");
        }
    }
    db.sync().expect("sync");

    let mut quiet = Vec::new();
    for _ in 0..5 {
        quiet.push(db.checkpoint().expect("checkpoint").pause.as_secs_f64());
    }

    let stop = AtomicBool::new(false);
    let mut busy = Vec::new();
    std::thread::scope(|scope| {
        for w in 0..4u64 {
            let db = &db;
            let stop = &stop;
            scope.spawn(move || {
                let mut session = db.session();
                let value = vec![b'y'; VALUE_BYTES + 1];
                let mut i = w;
                while !stop.load(Ordering::Relaxed) {
                    session.upsert(&key(i % records), &value).expect("upsert");
                    i += 4;
                }
            });
        }
        for _ in 0..5 {
            busy.push(db.checkpoint().expect("checkpoint").pause.as_secs_f64());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
    });

    let worst = |v: &[f64]| v.iter().cloned().fold(0.0, f64::max) * 1e6;
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).expect("no nans"));
        v[v.len() / 2] * 1e6
    };
    println!(
        "{records:>9}  pause at rest {:>8.0} us median {:>8.0} us worst   under four writers {:>8.0} us median {:>8.0} us worst",
        median(&mut quiet.clone()),
        worst(&quiet),
        median(&mut busy.clone()),
        worst(&busy),
    );
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    println!(
        "{:>9}  {:>12}  {:>10}  {:>9}  {:>8}  {:>9}",
        "records", "reopen", "open s", "walked", "pages", "read s"
    );
    for records in [100_000u64, 400_000, 1_600_000, 6_400_000] {
        let path = dir.path().join(format!("{records}.zu2"));
        load(&path, records);
        let bytes = std::fs::metadata(sidecar(&path)).expect("checkpoint").len();
        let log = std::fs::metadata(&path).expect("log").len();

        let (open, read, walked, pages) = reopen(&path, records);
        println!(
            "{records:>9}  {:>12}  {open:>10.3}  {walked:>9}  {pages:>8}  {read:>9.3}",
            "checkpoint"
        );
        let saved = open;

        std::fs::remove_file(sidecar(&path)).expect("take it away");
        let (open, read, walked, pages) = reopen(&path, records);
        println!(
            "{records:>9}  {:>12}  {open:>10.3}  {walked:>9}  {pages:>8}  {read:>9.3}   {:.1}x  checkpoint {} MiB beside a log of {} MiB",
            "scan",
            open / saved.max(1e-9),
            bytes / (1 << 20),
            log / (1 << 20),
        );
        std::fs::remove_file(&path).expect("tidy up");
    }

    println!();
    for records in [100_000u64, 1_600_000] {
        pauses(records);
    }
}
