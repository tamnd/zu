//! What a durable commit waits for besides the device. Not a benchmark,
//! a probe.
//!
//! A commit has to know that every record below the address it is about
//! to write is complete. It used to learn that by waiting for the epoch
//! to turn over, which waits for every session to leave whatever it was
//! doing, readers included, so a commit on a busy database cost what the
//! other threads were doing rather than what the device charges. Each
//! session now publishes the lowest address it may be writing instead.
//!
//! The reading is the ratio rather than the rate. One writer committing
//! on its own is the control, the same writer with readers beside it is
//! the question, and a machine that gets busy halfway through moves both
//! of them. A ratio near one means a reader is free.
//!
//! The readers are deliberately few and deliberately cold. Few, because
//! a probe that runs more threads than the machine has cores measures
//! the scheduler; the default is a quarter of the cores and one either
//! way. Cold, because the database is opened with a memory window
//! smaller than the data, so a read misses and goes to the device
//! inside its epoch, which is what a reader on a database larger than
//! memory does all day. That is the case the old shape was worst at: a
//! commit had to wait out somebody else's `pread`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use zu2::{Db, Durability, Options};

const LOADED: u64 = 20000;

fn env<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn median(rates: &[f64]) -> f64 {
    let mut sorted = rates.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn report(what: &str, rates: &[f64], batches: &[f64]) {
    let each: Vec<String> = rates.iter().map(|r| format!("{r:.0}")).collect();
    println!(
        "{what:26} {:9.0} op/s median  {:9.1} us/commit  {:5.2} commits per sync  rounds: {}",
        median(rates),
        1e6 / median(rates),
        median(batches),
        each.join(" ")
    );
}

/// A database loaded with the records the rounds update, so that no
/// round is measuring an insert.
fn loaded(dir: &std::path::Path, memory_pages: usize) -> Db {
    let db = Db::create(
        &dir.join("commitwait.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 14,
            max_pages: 1 << 14,
            memory_pages,
            ..Options::default()
        },
    )
    .expect("create");
    {
        let mut s = db.session();
        let v = vec![b'v'; 1000];
        for i in 0..LOADED {
            s.upsert(format!("user{i:019}").as_bytes(), &v)
                .expect("load");
        }
    }
    db.sync().expect("sync");
    db
}

/// One round: `readers` threads reading throughout, `writers` threads
/// committing `ops` records each, and the rate is the commits over the
/// time the writers took. The readers are started first and stopped
/// after, so a commit never runs without them.
fn round(
    db: &Db,
    readers: usize,
    writers: usize,
    ops: u64,
    nth: usize,
    value: &[u8],
) -> (f64, f64) {
    let syncs_before = db.syncs();
    let commits_before = db.commits();
    let stop = AtomicBool::new(false);
    let running = std::sync::atomic::AtomicUsize::new(0);
    let elapsed = std::thread::scope(|scope| {
        for _ in 0..readers {
            scope.spawn(|| {
                let mut s = db.session();
                let mut out = Vec::new();
                let mut i = 0u64;
                running.fetch_add(1, Ordering::Release);
                while !stop.load(Ordering::Relaxed) {
                    let key = i % LOADED;
                    s.read(format!("user{key:019}").as_bytes(), &mut out)
                        .expect("read");
                    i += 1;
                }
            });
        }
        while running.load(Ordering::Acquire) < readers {
            std::hint::spin_loop();
        }
        let started = Instant::now();
        let mut handles = Vec::new();
        for w in 0..writers {
            handles.push(scope.spawn(move || {
                let mut s = db.session();
                s.set_durability(Durability::Durable);
                for i in 0..ops {
                    let key = (nth as u64 * 7919 + w as u64 * ops + i) % LOADED;
                    s.upsert(format!("user{key:019}").as_bytes(), value)
                        .expect("update");
                }
            }));
        }
        for h in handles {
            h.join().expect("writer");
        }
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Release);
        elapsed
    });
    let commits = (db.commits() - commits_before) as f64;
    let syncs = (db.syncs() - syncs_before).max(1) as f64;
    let done = (writers as u64 * ops) as f64;
    (done / elapsed.as_secs_f64(), commits / syncs)
}

fn main() {
    let rounds: usize = env("ZU2_PROBE_ROUNDS", 5_usize);
    let ops: u64 = env("ZU2_PROBE_OPS", 400_u64);
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let writers: usize = env("ZU2_PROBE_WRITERS", (cores / 2).max(2));
    let memory_pages: usize = env("ZU2_PROBE_MEMORY_PAGES", 2_usize);
    let dir = match std::env::args().nth(1) {
        Some(path) => tempfile::tempdir_in(path),
        None => tempfile::tempdir(),
    }
    .expect("tempdir");
    println!(
        "in {}  {rounds} rounds of {ops} commits  {cores} cores  {memory_pages} pages in memory",
        dir.path().display()
    );

    let db = loaded(dir.path(), memory_pages);
    let value = vec![b'v'; 1000];

    // A sweep rather than one reader count, because what a commit used
    // to wait for was the slowest session of however many there were,
    // so one reader hides the effect and eight show it.
    let counts: Vec<usize> = env::<String>("ZU2_PROBE_READERS", "0 1 2 4 8".to_string())
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); counts.len()];
    let mut batches: Vec<Vec<f64>> = vec![Vec::new(); counts.len()];
    let mut many = Vec::new();
    let mut many_batches = Vec::new();
    for nth in 0..rounds {
        for (i, readers) in counts.iter().enumerate() {
            let (rate, batch) = round(&db, *readers, 1, ops, nth, &value);
            rates[i].push(rate);
            batches[i].push(batch);
        }
        let (rate, batch) = round(&db, 0, writers, ops, nth, &value);
        many.push(rate);
        many_batches.push(batch);
    }
    for (i, readers) in counts.iter().enumerate() {
        report(
            &format!("one writer, {readers} readers"),
            &rates[i],
            &batches[i],
        );
    }
    report(
        &format!("{writers} writers, no readers"),
        &many,
        &many_batches,
    );
    let alone = median(&rates[0]);
    let costs: Vec<String> = counts
        .iter()
        .zip(&rates)
        .map(|(readers, rate)| format!("{readers}: {:.2}x", alone / median(rate)))
        .collect();
    println!("readers cost {}", costs.join("  "));
    println!(
        "{writers} writers are worth {:.2}x one",
        median(&many) / alone
    );
}
