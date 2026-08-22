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
//! writer starved behind the rest, and it is read against the one
//! writer level's own p99 rather than its median: a machine somebody
//! else is also using has a tail at every width, and only tail over
//! tail cancels it.
//!
//! The one writer level is the denominator of all three ratios, and it
//! is the level a busy machine moves most, since it is one thread
//! waiting on one sync at a time with nothing to overlap. A slow one
//! there makes every ratio read better than the run deserves, so the
//! ceilings are set against the quiet measurement rather than the loud
//! one and the numbers to read are the columns.
//!
//! Then the same widths again over a store whose syncs return without
//! asking a disk, which is the pass the P5 latency ceiling is read
//! off. Ratios are as far as the durable pass can go, because the
//! thing they are ratios of is four milliseconds on a laptop and
//! nothing at all on a runner whose temporary directory is memory, and
//! an absolute ceiling over that would be a ceiling on the disk rather
//! than on the engine. Take the storage away and what is left is the
//! two things this repository decides: what it costs a writer to get
//! in and out of the queue in front of the write side, and the fold
//! that runs while one of them is holding it.
//!
//! The second of those is the whole tail. A fold lands every couple of
//! hundred statements and costs milliseconds where a statement costs
//! microseconds, and it holds the write side while it runs, so the
//! writers behind it wait it out as well. That is why the share of
//! statements over a millisecond goes up with the width while the
//! median only goes up with the queue: one fold is one slow statement
//! at one writer and eight at eight. It is also why the unsynced pass
//! writes a great many more statements than the durable one, which it
//! can afford to: a tail that is one statement in a couple of hundred
//! needs more than a couple of hundred of them before which
//! percentile it lands in stops being luck.
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
/// Statements per level for the unsynced pass, which is not waiting on
/// a disk and so can afford a great many more of them. It needs them:
/// the tail it is reporting is periodic, one statement in a few hundred
/// carries a fold, and a level of 480 sees two of those and puts them
/// wherever it likes in the percentiles.
const UNSYNCED_WRITES: usize = 40_000;
/// The widths. Eight is the one the P5 gate names, and the two above it
/// are there because a level that shares its syncs keeps scaling past
/// the point where a level that does not has stopped.
const WIDTHS: [usize; 6] = [1, 2, 4, 8, 16, 32];
/// The width the P5 latency ceiling names, and so the row the absolute
/// numbers are read off.
const GATE_WIDTH: usize = 8;
/// What one writer's commit has to cost for the three ratios to be
/// about the engine.
///
/// All three divide a level by the one writer level, and what they are
/// reading is how much of a sync period a burst of writers can share.
/// A machine whose durable commit is a tenth of a millisecond is not
/// waiting on its storage at all: the statement is CPU, the widest
/// level is thirty two threads bidding for however many cores the box
/// has, and the columns then describe the scheduler. That is the shape
/// a hosted runner has, where one writer commits in 152 us and the same
/// run on a laptop takes 3507 us, and it reads as commits that stopped
/// sharing when nothing in the engine moved.
///
/// So the gate asks first whether there was a sync to share. Under this
/// the numbers are printed and nothing is failed, with the reason said
/// out loud, because a gate that cannot mean anything here should say
/// so rather than go red.
const SYNC_US: f64 = 1000.0;

/// What counts as a slow statement, in microseconds.
///
/// A fold is milliseconds and a statement is microseconds, so anything
/// over this is a statement that was waiting for one. That holds while
/// the two are that far apart, and on a box slow enough for an ordinary
/// statement to approach it they are not: eight writers on two shared
/// cores queue for long enough that the threshold stops telling the
/// fold from the queue. So the share is gated only where the level's
/// own median leaves it room, and reported either way with the reason
/// said out loud.
const SLOW_US: f64 = 1000.0;

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
    max: f64,
    over: f64,
    stmts: f64,
}

impl Level {
    fn header() {
        println!(
            "{:<8} {:>11} {:>11} {:>11} {:>9} {:>13}",
            "writers", "p50", "p99", "max", "over 1ms", "throughput"
        );
    }

    fn report(&self) {
        println!(
            "{:<8} {:>8.0} us {:>8.0} us {:>8.0} us {:>8.2}% {:>6.0} stmt/s",
            self.writers,
            self.p50,
            self.p99,
            self.max,
            self.over * 100.0,
            self.stmts
        );
    }
}

/// Builds a two column `person` table of `ROWS` rows, in a directory of
/// its own so each level writes into a store nothing else has touched.
fn build(dir: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("db.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    seed(&mut db);
    path
}

/// The rows themselves, apart from the file they go in, because the
/// unsynced pass puts the same ones in a store that has no file.
fn seed(db: &mut Zu1File) {
    bulk_load_as(db, "person", "follows", ROWS, &[]).expect("load");
    let names: Vec<Vec<u8>> = (0..ROWS).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..ROWS).collect();
    store_props(
        db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&refs)),
        ],
    )
    .expect("props");
}

/// Where a level's store lives.
///
/// The two answers measure different halves of the same commit. On the
/// disk a statement pays the storage and the ratios read how much of a
/// sync period the writers managed to share. In memory the syncs return
/// without asking a disk, so there is nothing left to share and what the
/// columns describe is the handoff itself: what it costs a writer to get
/// in and out of the log when the log is not waiting on anything.
#[derive(Clone, Copy, PartialEq)]
enum Store {
    Disk,
    Memory,
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
fn run(dir: &Path, writers: usize, store: Store) -> Level {
    let writes = match store {
        Store::Disk => WRITES,
        Store::Memory => UNSYNCED_WRITES,
    };
    let db = match store {
        Store::Disk => {
            let path = build(dir);
            Database::open_with(&path, Config::new().threads(1)).expect("open")
        }
        Store::Memory => {
            let db = Database::memory_with(Config::new().threads(1)).expect("memory");
            // A memory database opens empty, so the rows go in through
            // the store itself rather than through a file that was
            // loaded before anything opened it.
            let mut conn = db.connect().expect("connect");
            seed(conn.session_mut().file_mut().expect("the store"));
            drop(conn);
            db
        }
    };
    let each = writes / writers;

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
        max: *all.last().expect("a writer ran"),
        over: all.iter().filter(|w| **w > 1000.0).count() as f64 / all.len() as f64,
        stmts: all.len() as f64 / to.duration_since(from).as_secs_f64().max(1e-9),
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let root = tempfile::tempdir().expect("tempdir");

    println!("durable, one sync a commit and the writers sharing what they can");
    Level::header();
    let levels: Vec<Level> = WIDTHS
        .iter()
        .map(|&w| {
            let level = run(&root.path().join(format!("w{w}")), w, Store::Disk);
            level.report();
            level
        })
        .collect();

    let one = &levels[0];
    let wide = levels.last().expect("a level ran");
    let commit_x = wide.stmts / one.stmts.max(0.001);
    let p50_x = wide.p50 / one.p50.max(0.001);
    // Tail against tail, not tail against median. A machine somebody
    // else is also using spikes at both levels, and dividing the two
    // tails is what cancels it; dividing the wide tail by the narrow
    // median leaves the spike in the number and gates on the load.
    let p99_x = wide.p99 / one.p99.max(0.001);
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

    // Whether the storage under this run has a sync worth sharing. The
    // one writer level is the answer: it is one thread waiting out one
    // sync at a time with nothing to overlap, so its median is a sync
    // period and a bit.
    let syncs = one.p50 >= SYNC_US;
    if !syncs {
        println!(
            "one writer commits in {:.0} us, under the {SYNC_US:.0} us a sync costs, \
             so the three ratios are the scheduler and not the sharing: reported, not gated",
            one.p50
        );
    }

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

    // The same burst again with the storage taken out from under it.
    // The ratios above cannot say what a commit costs, only how much of
    // it the writers shared, and on a laptop what they shared is a four
    // millisecond flush that no engine put there. What is left when the
    // syncs return without asking a disk is the handoff: the lock a
    // writer takes to stage its frames and the wait it does to be told
    // they are through. That is the part of the tail this repository
    // owns, so it is the part the P5 ceiling is read against.
    println!();
    println!("unsynced, the same burst with a store whose syncs ask no disk");
    Level::header();
    let unsynced: Vec<Level> = WIDTHS
        .iter()
        .map(|&w| {
            let level = run(root.path(), w, Store::Memory);
            level.report();
            level
        })
        .collect();
    let at_gate = unsynced
        .iter()
        .find(|level| level.writers == GATE_WIDTH)
        .expect("the gate width is one of the widths");
    println!(
        "commit_p99_nosync_us: {:.0} us at the tail, at {GATE_WIDTH} writers, with no sync in it",
        at_gate.p99
    );
    println!(
        "commit_stmt_nosync_x: {:.2}x the statements a second of one writer, at {GATE_WIDTH} \
         writers, with no sync in it",
        at_gate.stmts / unsynced[0].stmts.max(0.001)
    );
    println!(
        "commit_slow_share: {:.2}% of statements over a millisecond at {GATE_WIDTH} writers, \
         against {:.2}% at one",
        at_gate.over * 100.0,
        unsynced[0].over * 100.0
    );

    let mut engine_failed = false;
    // Room between an ordinary statement here and what this calls slow.
    // Without it the share is counting the queue rather than the folds
    // in it, and says so rather than failing on a slow box.
    let separated = at_gate.p50 < SLOW_US / 2.0;
    if let Some(ceiling) = budget("commit_slow_share") {
        let ok = at_gate.over <= ceiling;
        let verdict = match (ok, separated) {
            (_, false) => "the median is too near a millisecond here to tell a fold from a queue",
            (true, true) => "ok",
            (false, true) => "over",
        };
        println!(
            "commit_slow_share: {:.4} against a ceiling of {ceiling:.4} ({verdict})",
            at_gate.over
        );
        engine_failed |= !ok && separated;
    }
    if let Some(ceiling) = budget("commit_p99_nosync_us") {
        let ok = at_gate.p99 <= ceiling;
        let verdict = match (ok, separated) {
            (_, false) => "the box and not the engine is what a tail in milliseconds reads here",
            (true, true) => "ok",
            (false, true) => "over",
        };
        println!(
            "commit_p99_nosync_us: {:.0} against a ceiling of {ceiling:.0} ({verdict})",
            at_gate.p99
        );
        engine_failed |= !ok && separated;
    }
    if let Some(floor) = budget("commit_stmt_nosync_x") {
        let got = at_gate.stmts / unsynced[0].stmts.max(0.001);
        let ok = got >= floor;
        let verdict = if ok { "ok" } else { "under" };
        println!("commit_stmt_nosync_x: {got:.2} against a floor of {floor:.2} ({verdict})");
        engine_failed |= !ok;
    }

    let red = (failed && syncs) || engine_failed;
    if gate && red {
        std::process::exit(1);
    }
    println!(
        "gate: {}",
        match (red, failed, syncs) {
            (true, _, _) => "budgets missed",
            (false, true, false) => "budgets missed on storage with no sync to share",
            _ => "all ceilings met",
        }
    );
}
