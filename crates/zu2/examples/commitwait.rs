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
//! of them. A ratio near one means a reader is free. The old shape gave
//! about a fifth of that.

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

fn report(what: &str, rates: &[f64]) {
    let each: Vec<String> = rates.iter().map(|r| format!("{r:.0}")).collect();
    println!(
        "{what:26} {:9.0} op/s median  {:9.1} us/commit  rounds: {}",
        median(rates),
        1e6 / median(rates),
        each.join(" ")
    );
}

/// A database loaded with the records the rounds update, so that no
/// round is measuring an insert.
fn loaded(dir: &std::path::Path) -> Db {
    let db = Db::create(
        &dir.join("commitwait.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 14,
            max_pages: 1 << 14,
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
fn round(db: &Db, readers: usize, writers: usize, ops: u64, nth: usize, value: &[u8]) -> f64 {
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
    (writers as u64 * ops) as f64 / elapsed.as_secs_f64()
}

fn main() {
    let rounds: usize = env("ZU2_PROBE_ROUNDS", 5_usize);
    let ops: u64 = env("ZU2_PROBE_OPS", 400_u64);
    let readers: usize = env("ZU2_PROBE_READERS", 7_usize);
    let writers: usize = env("ZU2_PROBE_WRITERS", 8_usize);
    let dir = match std::env::args().nth(1) {
        Some(path) => tempfile::tempdir_in(path),
        None => tempfile::tempdir(),
    }
    .expect("tempdir");
    println!(
        "in {}  {rounds} rounds of {ops} commits",
        dir.path().display()
    );

    let db = loaded(dir.path());
    let value = vec![b'v'; 1000];

    let mut alone = Vec::new();
    let mut beside_readers = Vec::new();
    let mut many = Vec::new();
    for nth in 0..rounds {
        alone.push(round(&db, 0, 1, ops, nth, &value));
        beside_readers.push(round(&db, readers, 1, ops, nth, &value));
        many.push(round(&db, 0, writers, ops, nth, &value));
    }
    report("one writer alone", &alone);
    report(&format!("one writer, {readers} readers"), &beside_readers);
    report(&format!("{writers} writers"), &many);
    println!(
        "readers cost {:.2}x, and {writers} writers are worth {:.2}x one",
        median(&alone) / median(&beside_readers),
        median(&many) / median(&alone)
    );
}
