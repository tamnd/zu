//! What a burst of writers costs, which is the shape the P5 gate at
//! c=8 is about (zu#78).
//!
//! `write.rs` measures one statement on one connection, and a statement
//! on its own is one commit and one fsync, so the clock it reports is
//! mostly the storage. That number cannot say what a server does,
//! because a server has eight clients writing at once and the whole
//! point of group commit is that eight commits in the air cost about
//! what one costs. Nothing in this repository measured that, so nothing
//! would have noticed the day the writers went back to syncing one at a
//! time.
//!
//! The run is the same `INSERT` of one element at six widths, one
//! writer up to thirty two, each on a connection of its own, each
//! writing its own share of the same total. Every level writes the same
//! number of statements, so the levels are the same work handed to a
//! different number of hands.
//!
//! Two numbers say whether the commits share their syncs. `commit_x` is
//! what the widest level gets through against what one writer does:
//! writers that each waited for a sync of their own do the same work in
//! the same time and it reads 1, and writers that landed in one sync
//! read near the width. `commit_p50_x` is the other side of the same
//! question, the latency one statement sees at the widest level against
//! what it saw alone, and it says whether the throughput was bought by
//! making every client wait for all the others.
//!
//! What the two of them together describe is the floor a durable commit
//! has and cannot go under. A writer that stages its frames while a
//! sync is already in the air cannot be covered by that sync, because
//! the sync was told how far to reach before those bytes existed. So it
//! waits out the one in flight and then waits for the next one, which
//! is between one and two sync periods however many writers there are,
//! and the width only decides how many of them ride in the second sync.
//! That is why the latency column is flat and the throughput column is
//! a straight line: the shape to watch for is the latency column
//! climbing with the width, which is what serialised commits look like.
//!
//! Latency is reported per statement rather than per level, because a
//! writer that has to queue for the write side spends most of a burst
//! waiting and the mean hides it. The p99 is the column that catches a
//! writer starved behind the rest.
//!
//! The one writer level is the denominator of all three ratios, and it
//! is the level a busy machine moves most, since it is one thread
//! waiting on one sync at a time with nothing to overlap. A slow one
//! there makes every ratio read better than the run deserves, so the
//! ceilings are set against the quiet measurement rather than the loud
//! one and the numbers to read are the columns.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench commit

use std::path::Path;
use std::time::Instant;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
use zu::{Config, Database};

/// The table the writers write into. Small, because what is measured is
/// the commit and not the table: an `INSERT` adds a row past the end of
/// every column and folds nothing, so the width of the table is not in
/// the number.
const ROWS: u64 = 10_000;
/// Statements per level, shared out over that level's writers. Enough
/// that one slow sync does not decide the number, small enough that six
/// levels of it stay inside a minute.
const WRITES: usize = 480;
/// The widths. Eight is the one the P5 gate names, and the two above it
/// are there because a level that shares its syncs keeps scaling past
/// the point where a level that does not has stopped.
const WIDTHS: [usize; 6] = [1, 2, 4, 8, 16, 32];

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

/// What one level cost: the latency a statement saw and the rate the
/// level as a whole sustained.
struct Level {
    writers: usize,
    p50: f64,
    p99: f64,
    stmts: f64,
}

impl Level {
    fn header() {
        println!(
            "{:<10} {:>12} {:>12} {:>14}",
            "writers", "p50", "p99", "throughput"
        );
    }

    fn report(&self) {
        println!(
            "{:<10} {:>9.0} us {:>9.0} us {:>7.0} stmt/s",
            self.writers, self.p50, self.p99, self.stmts
        );
    }
}

/// Builds a two column `person` table of `ROWS` rows, in a directory of
/// its own so each level writes into a store nothing else has touched.
fn build(dir: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("db.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "follows", ROWS, &[]).expect("load");
    let names: Vec<Vec<u8>> = (0..ROWS).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..ROWS).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&refs)),
        ],
    )
    .expect("props");
    path
}

/// What one writer did: how long each of its statements waited, and the
/// window it ran them in.
struct Ran {
    waits: Vec<f64>,
    from: Instant,
    to: Instant,
}

/// `WRITES` inserts over `writers` connections at once, timed one
/// statement at a time.
///
/// Every writer takes its connection and runs one statement on it
/// before the clock starts, so what is measured is neither the first
/// compile of the statement nor the connection that had to open a file.
/// The barrier is what makes the level a burst rather than a queue: the
/// writers are all in front of the log before any of them writes.
///
/// The ages are strided by the writer, so no two writers ever hand the
/// store the same row, and a level that lost a write is caught by the
/// count at the end rather than scoring for it.
fn run(dir: &Path, writers: usize) -> Level {
    let path = build(dir);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let each = WRITES / writers;

    let start = std::sync::Barrier::new(writers);
    let ran: Vec<Ran> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..writers)
            .map(|w| {
                let db = &db;
                let start = &start;
                scope.spawn(move || {
                    let mut conn = db.connect().expect("connect");
                    conn.query(&format!(
                        "INSERT (p:person {{age: -{}, name: 'warmup'}})",
                        w + 1
                    ))
                    .expect("warmup");
                    let mut waits = Vec::with_capacity(each);
                    start.wait();
                    let from = Instant::now();
                    for i in 0..each {
                        let age = ROWS as usize + w * each + i;
                        let began = Instant::now();
                        conn.query(&format!("INSERT (p:person {{age: {age}, name: 'new'}})"))
                            .expect("insert");
                        waits.push(began.elapsed().as_nanos() as f64 / 1e3);
                    }
                    Ran {
                        waits,
                        from,
                        to: Instant::now(),
                    }
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("writer"))
            .collect()
    });

    // The level's window is the first writer starting to the last one
    // stopping, so a writer that finished early is idle time the rate
    // carries rather than idle time it is rewarded for.
    let from = ran.iter().map(|r| r.from).min().expect("a writer ran");
    let to = ran.iter().map(|r| r.to).max().expect("a writer ran");
    let mut all: Vec<f64> = ran.into_iter().flat_map(|r| r.waits).collect();
    all.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)];

    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query("MATCH (p:person) RETURN count(p) AS n")
        .expect("count")
        .rows;
    let want = ROWS as i64 + (each * writers) as i64 + writers as i64;
    assert_eq!(
        rows.first().and_then(|row| row.first()),
        Some(&Value::Int(want)),
        "every statement of every writer is in the store"
    );

    Level {
        writers,
        p50: at(0.50),
        p99: at(0.99),
        stmts: all.len() as f64 / to.duration_since(from).as_secs_f64().max(1e-9),
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let root = tempfile::tempdir().expect("tempdir");

    Level::header();
    let levels: Vec<Level> = WIDTHS
        .iter()
        .map(|&w| {
            let level = run(&root.path().join(format!("w{w}")), w);
            level.report();
            level
        })
        .collect();

    let one = &levels[0];
    let wide = levels.last().expect("a level ran");
    let commit_x = wide.stmts / one.stmts.max(0.001);
    let p50_x = wide.p50 / one.p50.max(0.001);
    let p99_x = wide.p99 / one.p50.max(0.001);
    println!(
        "commit_x: {commit_x:.2}x the statements a second of one writer, at {} writers",
        wide.writers
    );
    println!(
        "commit_p50_x: {p50_x:.2}x the latency of one writer, at {} writers",
        wide.writers
    );
    println!(
        "commit_p99_x: {p99_x:.2}x the latency of one writer, at {} writers",
        wide.writers
    );

    let mut failed = false;
    for (key, got) in [("commit_p50_x", p50_x), ("commit_p99_x", p99_x)] {
        if let Some(ceiling) = budget(key) {
            let ok = got <= ceiling;
            let verdict = if ok { "ok" } else { "over" };
            println!("{key}: {got:.2} against a ceiling of {ceiling:.2} ({verdict})");
            failed |= !ok;
        }
    }
    // A floor and not a ceiling: this is the one number here that has to
    // be large, because it is the sharing itself.
    if let Some(floor) = budget("commit_x") {
        let ok = commit_x >= floor;
        let verdict = if ok { "ok" } else { "under" };
        println!("commit_x: {commit_x:.2} against a floor of {floor:.2} ({verdict})");
        failed |= !ok;
    }
    if gate && failed {
        std::process::exit(1);
    }
    println!(
        "gate: {}",
        if failed {
            "budgets missed"
        } else {
            "all ceilings met"
        }
    );
}
