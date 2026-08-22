//! What a value's size does to what compaction costs.
//!
//! WiscKey (Lu, Pillai, Arpaci-Dusseau, Arpaci-Dusseau, FAST 2016) keeps
//! values out of the tree and stores a pointer in their place, because
//! an LSM re-merges a record once per level and a large value is
//! therefore written a dozen times over its life. Z12 asks whether that
//! argument transfers to a hybrid log, and this is the table that has to
//! answer it before a line of separation gets written.
//!
//! The reason to doubt that it does: an LSM's copy count comes from the
//! shape of the tree, so it grows with the data and is the same whatever
//! the space budget is. A hybrid log has no levels. Compaction reads the
//! oldest region and re-appends whatever the index still reaches, so a
//! record is copied once per lap and the number of laps comes out of the
//! space target, not out of the record. If that is right then
//! amplification is flat in the value size, separating the value moves
//! the same bytes from one file to another and buys nothing, and a point
//! read pays a second device trip for it.
//!
//! So every row here holds the live set and the update volume in bytes
//! fixed and changes only how those bytes are divided into records. A
//! row with a hundred byte value has six hundred thousand records and a
//! row with sixteen kilobytes has under four thousand, and both write
//! about the same number of megabytes per round. If amplification tracks
//! the value size, separation is worth building. If it is flat, the
//! table says so and Z12 is answered rather than implemented.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use zu2::{Db, Durability, Options};

/// The live set every row holds, so that the space target means the same
/// number of bytes in each of them.
const LIVE_BYTES: u64 = 64 << 20;

/// What one round of updates writes, for the same reason.
const ROUND_BYTES: u64 = 64 << 20;

const ROUNDS: u64 = 8;

/// The fraction of the keys that take four fifths of the updates.
const HOT: f64 = 0.2;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

/// A cheap deterministic generator, the same one `coldtier.rs` uses.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn options(records: u64, memory: usize) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: (records as usize).next_power_of_two(),
        max_pages: 1 << 12,
        memory_pages: memory,
        max_nodes: 1 << 10,
        // Passes are driven from the loop below rather than left to the
        // flusher, for the reason `coldtier.rs` gives: an asynchronous
        // schedule makes a run depend on where the background thread
        // happened to be.
        compact_below: 0,
        checkpoint_on_close: false,
        ..Options::default()
    }
}

/// What one measured run produced, before the repeats are folded.
#[derive(Clone, Copy, Default)]
struct Took {
    appended: f64,
    moved: f64,
    amp: f64,
    laps: f64,
    updating: f64,
    compacting: f64,
    read_ns: f64,
}

/// One run: `value` byte values, enough of them to make the live set,
/// and `ROUNDS` rounds of updates each followed by passes until the
/// files are back under `target` percent of live.
fn once(value_bytes: usize, target: u64, memory: usize) -> Took {
    let record = (value_bytes + key(0).len() + 64) as u64;
    let records = (LIVE_BYTES / record).max(1);
    let per_round = (ROUND_BYTES / record).max(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("v.zu2"), options(records, memory)).expect("create");
    let value = vec![b'x'; value_bytes];
    let live = records * record;
    let ceiling = live * target / 100;

    let mut session = db.session();
    for i in 0..records {
        session.upsert(&key(i), &value).expect("upsert");
    }
    drop(session);
    db.sync().expect("sync");

    let hot_keys = ((records as f64) * HOT).max(1.0) as u64;
    let mut rng = Rng(0x2026_0822);
    let (mut appended, mut moved, mut updating, mut compacting) = (0u64, 0u64, 0.0, 0.0);
    for round in 0..ROUNDS {
        let mark = (
            db.log_bytes(),
            db.compaction().copied.load(Relaxed) + db.compaction().migrated.load(Relaxed),
        );
        let at = Instant::now();
        let mut session = db.session();
        for _ in 0..per_round {
            let r = rng.next();
            let i = if r.is_multiple_of(5) {
                r % records
            } else {
                r % hot_keys
            };
            session.upsert(&key(i), &value).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
        let updates = at.elapsed().as_secs_f64();

        let at = Instant::now();
        while db.log_span() + db.cold_span() > ceiling {
            if db.compact().expect("compact") == 0 {
                break;
            }
        }
        let passes = at.elapsed().as_secs_f64();

        if round >= ROUNDS / 2 {
            let copied = db.compaction().copied.load(Relaxed)
                + db.compaction().migrated.load(Relaxed)
                - mark.1;
            // The tail moved for two reasons and only one of them is the
            // workload, so the copies come out of the total rather than
            // being compared against it.
            appended += (db.log_bytes() - mark.0).saturating_sub(copied);
            moved += copied;
            updating += updates;
            compacting += passes;
        }
    }

    // What a read costs, since separation would put a device trip in
    // this path and the table has to say what that trip would be
    // competing with.
    let mut session = db.session();
    let mut out = Vec::new();
    let at = Instant::now();
    let reads = 200_000u64.min(records * 8);
    for _ in 0..reads {
        let i = rng.next() % records;
        session.read(&key(i), &mut out).expect("read");
    }
    let read_ns = at.elapsed().as_secs_f64() * 1e9 / reads as f64;

    Took {
        appended: appended as f64 / (1 << 20) as f64,
        moved: moved as f64 / (1 << 20) as f64,
        amp: moved as f64 / appended.max(1) as f64,
        laps: moved as f64 / record as f64 / records as f64,
        updating,
        compacting,
        read_ns,
    }
}

/// The middle of three runs, field by field. Individual rows move by up
/// to a factor of two between runs, because how much of a round lands in
/// the mutable window depends on where the tail happened to be, so one
/// run of one row is not a number worth reasoning from.
fn run(value_bytes: usize, target: u64, memory: usize) {
    let record = (value_bytes + key(0).len() + 64) as u64;
    let records = (LIVE_BYTES / record).max(1);
    let mut runs = [Took::default(); 3];
    for run in &mut runs {
        *run = once(value_bytes, target, memory);
    }
    let middle = |pick: fn(&Took) -> f64| {
        let mut got: Vec<f64> = runs.iter().map(pick).collect();
        got.sort_by(f64::total_cmp);
        got[1]
    };

    println!(
        "{:>9}  {:>7}  {:>7}  {:>9}  {:>10.0}  {:>10.0}  {:>7.2}  {:>7.2}  {:>9.2}  {:>9.2}  {:>8.0}",
        if value_bytes >= 1024 {
            format!("{} KiB", value_bytes / 1024)
        } else {
            format!("{value_bytes} B")
        },
        format!("{target}%"),
        if memory == usize::MAX {
            "all".to_string()
        } else {
            format!("{} MiB", memory * 4)
        },
        records,
        middle(|t| t.appended),
        middle(|t| t.moved),
        middle(|t| t.amp),
        middle(|t| t.laps),
        middle(|t| t.updating),
        middle(|t| t.compacting),
        middle(|t| t.read_ns),
    );
}

fn main() {
    println!(
        "{:>9}  {:>7}  {:>7}  {:>9}  {:>10}  {:>10}  {:>7}  {:>7}  {:>9}  {:>9}  {:>8}",
        "value",
        "target",
        "memory",
        "records",
        "wrote MiB",
        "moved MiB",
        "amp",
        "laps",
        "update s",
        "compact s",
        "read ns"
    );
    for target in [150, 200, 400] {
        for value_bytes in [100, 1024, 4096, 16384, 65536] {
            run(value_bytes, target, usize::MAX);
        }
        println!();
    }
    // A quarter of the live set, so a pass has to go to the device for
    // most of what it reads. This is the row that would make the case
    // for separation if there is one: a lookup based pass reads the
    // whole record to decide whether it is live, and with a value in it
    // that read is the value's size rather than the key's.
    for value_bytes in [100, 1024, 4096, 16384, 65536] {
        run(value_bytes, 200, 4);
    }
}
