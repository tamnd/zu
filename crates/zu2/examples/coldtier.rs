//! What one compaction schedule costs a workload that is mostly cold.
//!
//! F2 (Kanellis, Chandramouli, Hart, Venkataraman, PVLDB 18(12), 2025)
//! splits the log in two and compacts the halves on different schedules.
//! zu2 has one log and one schedule, and 08 says the trigger for
//! changing that is a table showing a workload where the one schedule is
//! wrong rather than a wish to match the paper. This is that table.
//!
//! The shape that should hurt is skew. Compaction is lookup based: it
//! reads the oldest region and re-appends whatever the index still
//! reaches, so a pass copies the live records in the region and reclaims
//! the rest. A record nobody updates is live on every pass that reaches
//! it, and a copy puts it back at the tail, where it waits to be reached
//! again. So a cold record is copied once per lap of the log, forever,
//! and it pays that for garbage that the hot records made.
//!
//! Two numbers say whether that is a problem. Amplification is the bytes
//! compaction copied over the bytes the workload appended, so one means
//! the database wrote everything twice. Laps is the same thing per
//! record: how many times the average record was picked up and put down
//! again over the measured half of the run.
//!
//! The passes are driven from here rather than left to the flusher. The
//! flusher's schedule is asynchronous, so what a run ends up with
//! depends on where the background thread happened to be, and three runs
//! of the same workload disagreed by a factor of three. Here each round
//! is a fixed number of updates followed by passes until the file is
//! back under the target, which is the same policy the flusher applies
//! and is reproducible. The first half of the rounds is warmup and only
//! the second half is reported, because a log that has not lapped yet
//! has no cold records to copy and would flatter every row.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use zu2::{Db, Durability, Options};

const VALUE_BYTES: usize = 100;
const RECORDS: u64 = 400_000;
const ROUNDS: u64 = 12;
const PER_ROUND: u64 = 400_000;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

/// Roughly what one record occupies, for turning copied bytes into
/// copied records. The header is fixed and so are the key and the value
/// in this workload, so this is exact rather than an estimate.
fn record_bytes() -> u64 {
    (VALUE_BYTES + key(0).len() + 64) as u64
}

/// A cheap deterministic generator. Not a Zipfian, on purpose: what is
/// being measured is what a hot fraction does to the copy cost, and a
/// named fraction says that more plainly than a theta does.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn options(memory: usize) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: (RECORDS as usize).next_power_of_two(),
        max_pages: 1 << 12,
        max_nodes: 1 << 10,
        memory_pages: memory,
        // Off. The rounds below run the passes, for the reason in the
        // module comment.
        compact_below: 0,
        checkpoint_on_close: false,
        ..Options::default()
    }
}

/// What one row of the table holds, over the measured rounds.
#[derive(Default)]
struct Row {
    appended: u64,
    copied: u64,
    passes: u64,
    updates: f64,
    compacting: f64,
}

/// One run: load every key, then `ROUNDS` rounds of updates with `hot`
/// of the keys taking four fifths of them, each round followed by
/// however many passes it takes to get back under `target` percent of
/// the live set.
fn run(hot: f64, target: u64, memory: usize) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold.zu2");
    let db = Db::create(&path, options(memory)).expect("create");
    let value = vec![b'x'; VALUE_BYTES];
    let live = RECORDS * record_bytes();
    let ceiling = live * target / 100;

    let mut session = db.session();
    for i in 0..RECORDS {
        session.upsert(&key(i), &value).expect("upsert");
    }
    drop(session);
    db.sync().expect("sync");

    let hot_keys = ((RECORDS as f64) * hot).max(1.0) as u64;
    let mut rng = Rng(0x2026_0821);
    let mut row = Row::default();
    for round in 0..ROUNDS {
        let mark = (db.log_bytes(), db.compaction().copied.load(Relaxed));
        let at = Instant::now();
        let mut session = db.session();
        for _ in 0..PER_ROUND {
            let r = rng.next();
            let i = if hot < 1.0 && !r.is_multiple_of(5) {
                r % hot_keys
            } else {
                r % RECORDS
            };
            session.upsert(&key(i), &value).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
        let updates = at.elapsed().as_secs_f64();

        let at = Instant::now();
        let mut passes = 0;
        while db.log_span() > ceiling {
            let reclaimed = db.compact().expect("compact");
            passes += 1;
            if reclaimed == 0 {
                // Nothing left that a pass is allowed to take, which is
                // the mutable window and whatever is not flushed yet.
                // Waiting for those would be measuring the flusher.
                break;
            }
        }
        let compacting = at.elapsed().as_secs_f64();

        if round >= ROUNDS / 2 {
            let copied = db.compaction().copied.load(Relaxed) - mark.1;
            // The tail moved for two reasons and only one of them is the
            // workload, so the copies come out of the total rather than
            // being compared against it.
            row.appended += (db.log_bytes() - mark.0).saturating_sub(copied);
            row.copied += copied;
            row.passes += passes;
            row.updates += updates;
            row.compacting += compacting;
        }
    }

    let laps = row.copied as f64 / record_bytes() as f64 / RECORDS as f64;
    println!(
        "{:>9}  {:>7}  {:>7}  {:>7}  {:>10.0}  {:>10.0}  {:>7.2}  {:>7.2}  {:>7.0}  {:>9.2}  {:>9.2}",
        format!("{:.0}%", hot * 100.0),
        format!("{target}%"),
        if memory == usize::MAX {
            "all".to_string()
        } else {
            format!("{} MiB", memory * 4)
        },
        row.passes,
        row.appended as f64 / (1 << 20) as f64,
        row.copied as f64 / (1 << 20) as f64,
        row.copied as f64 / row.appended.max(1) as f64,
        laps,
        db.log_span() as f64 / live as f64 * 100.0,
        row.updates,
        row.compacting,
    );
}

fn main() {
    println!(
        "{:>9}  {:>7}  {:>7}  {:>7}  {:>10}  {:>10}  {:>7}  {:>7}  {:>7}  {:>9}  {:>9}",
        "hot",
        "target",
        "memory",
        "passes",
        "wrote MiB",
        "copied MiB",
        "amp",
        "laps",
        "span %",
        "update s",
        "compact s"
    );
    for hot in [1.0, 0.2, 0.01] {
        for target in [150, 200, 400] {
            run(hot, target, usize::MAX);
        }
    }
    // The live set is about 62 MiB, so four pages is a memory budget of
    // a quarter of it and a pass has to go to the device for most of
    // what it copies.
    println!();
    for hot in [1.0, 0.2, 0.01] {
        run(hot, 200, 4);
    }
}
