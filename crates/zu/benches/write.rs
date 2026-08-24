//! What one write statement costs (G3, zu#118), in every resource a
//! write spends: latency, throughput, bytes pushed to the disk, bytes
//! the store grew by, and memory.
//!
//! A `SET` of one property of one element, an `INSERT` of one element,
//! a `DELETE` of one element and a `DETACH DELETE` of one that still
//! has edges on it, all four through the query engine, all four one
//! statement per commit. That is the shape of a linkbench update and of an SNB
//! insert, so it is the number those workloads are made of. The read
//! benches say what finding the row costs; what is left in here is the
//! write path, which is the log append, the fdatasync the commit is,
//! and the fold that makes the new value readable.
//!
//! One number would hide most of that. A write path can be quick and
//! still be wrong to ship, so every run reports six: `us` is what a
//! caller waits, `cpu` is the processor time inside it, `stmt/s` is
//! what a stream of them sustains, `written` is what the statement
//! pushed at the disk, `growth` is what it added to the store for good,
//! and the two memory columns are what it cost to hold.
//!
//! The clock and the processor time are worth reading as a pair. A
//! commit ends in an fsync, and an fsync on the laptop this was written
//! on is 3.9 ms whatever it is syncing, so the latency column of a
//! single threaded run is mostly the storage and it will sit near that
//! floor however cheap the write path gets. `cpu` is the part this
//! repository can move. Latency comes down when commits stop syncing
//! one at a time, which is group commit and a different change from
//! this one.
//!
//! `cpu-sync` is that column with the sync's own processor cost taken
//! out. A sync is not only a wait: on this laptop it is `fcntl` with
//! `F_FULLFSYNC`, which burns 20 to 80 us before the disk is even
//! asked, several times what the write path spends, and it lands in the
//! same counter. So `cpu` on a durable one statement commit is mostly
//! the storage too, and it moves with whatever else the machine is
//! doing to its disk.
//!
//! What `cpu-sync` is not is the P5 point write budget, though it used
//! to be read as it. It takes off one sync measured on a scratch file,
//! and a sustained statement pays its own commit sync plus its share of
//! the two a checkpoint costs, on a nine megabyte store with a fold's
//! worth of dirty pages behind it. So it is the wrong number of syncs
//! of the wrong size, and three runs of one commit put it at 53, 122
//! and 201 us. It is kept because a statement that suddenly burns
//! milliseconds shows up in it whatever the sync is doing, and it is
//! read as the loose thing it is.
//!
//! The budget is read off `SET unsynced` instead, which is the same
//! sustained window over a store on the memory filesystem. That store
//! runs the identical write path: same log frames, same overlay, same
//! fold schedule, same checkpoint threshold, and syncs that return
//! without asking a disk. So the gap between it and the durable run is
//! durability and nothing else, and what is left is the write path. It
//! reads 19 us against the durable window's 107, and it reads 19 again
//! next time.
//!
//! A one cell change that writes a megabyte is a real defect and the
//! clock alone would call it fine; a write path that leaked a block per
//! statement would pass every latency ceiling there is and show up only
//! in `growth`.
//!
//! The fold is why the same statement runs at two table sizes. A fold
//! rewrites the columns of the table it touched out of their old values
//! and the cells the overlay holds, so a one cell change costs the
//! width of the table, not the width of the change. Running at 10 K
//! rows and again at 100 K rows turns that from a claim into two
//! ratios: `set_fold_x` in time and `set_write_x` in bytes. A write
//! path that did not read the table would hold both near 1, and this
//! one does not, so those ceilings bound how badly it scales rather
//! than promising that it does not. They are the numbers to watch when
//! the read path learns to consult overlays and the fold stops running
//! once per statement.
//!
//! An edge is the shape that has stopped folding, and it is measured
//! twice: as the statement a client sends, and again staged straight
//! onto a transaction. The two answer different questions. The
//! statement carries a MATCH over two patterns to find its endpoints,
//! and that search is linear in the rows of the table, so at 100 K it
//! is most of what the statement costs and `insert_edge_x` mostly
//! reports the read plane. The transaction rows are the write on its
//! own, which is what `insert_edge_write_x` watches.
//!
//! Both statements are checked rather than only timed: the rows are
//! counted and read back after the loop, so a write path that got
//! faster by writing less fails instead of scoring.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench write

use std::path::Path;
use std::time::Instant;

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_labels, store_props, store_rel_props};
use zu::zu1::txn::Cell;
use zu::{Config, Database};

/// The small table, where the fold is cheap enough that the statement
/// itself is most of the number.
const SMALL: u64 = 10_000;
/// The large table, ten times the small one, so the fold's share of
/// both the time and the bytes is ten times what it was.
const LARGE: u64 = 100_000;
/// Writes per measured run. A commit is an fdatasync, so this is small
/// enough to keep the bench in seconds and large enough that one slow
/// sync does not decide the number.
const WRITES: u64 = 200;
/// Folds the sustained run goes through before its clock starts.
///
/// A store that has just been loaded has never folded and has nothing
/// on its free list, so its first statements are a ramp: the file grows
/// because no checkpoint has published anything to hand back yet, and
/// what those statements cost is not what a running store costs. Six
/// folds is past the point where the small store stops growing.
const FOLDS_RAMP: u64 = 6;
/// Folds in the measured sustained window.
///
/// Long enough to hold folds at the rate they happen and more than one
/// checkpoint, which is a checkpoint every few folds. The length is the
/// whole point of the run: a window short enough to fall between two
/// folds measures a write path with its housekeeping taken out, and the
/// housekeeping is not optional.
///
/// Both of these count folds rather than statements, and they used to
/// count statements. The trouble with that is that a statement count is
/// a fold count times a rate this file cannot see, and the rate moved:
/// the write path's deferral bound went from 256 to 4096, the ramp and
/// the window stayed at the 1500 and 2500 statements that had held six
/// folds and ten, and they went on calling themselves a ramp and a
/// window while holding one fold and two. So the run measures the rate
/// first and multiplies. See [`fold_every`].
const FOLDS_WINDOW: u64 = 10;
/// Statements the fold rate is measured over.
///
/// Long enough to hold folds at any rate the bound is likely to be set
/// to, and cheap because it runs with no disk under it: a statement
/// there is tens of microseconds, so the whole probe is a fraction of a
/// second against the minute of syncs it sizes.
const PROBE: u64 = 16_384;
/// How often the sustained run stops to look at the two files.
///
/// A stat is a microsecond against a statement that is thousands of
/// them, and the run needs the shape of the two curves rather than
/// every point on them.
const SAMPLE: u64 = 25;
/// Timed passes over the `SET` loop, of which the fastest is the
/// number. Three, because the two clocks it protects are divided by
/// each other and the gate machines are shared vCPUs.
const PASSES: u64 = 3;
const MB: f64 = 1024.0 * 1024.0;
/// The store's block, which is the granularity everything the fold
/// takes and gives back is counted in.
const BLOCK: u32 = zu::zu1::BLOCK_SIZE;

/// How many rows the store [`calibrate`] reads holds.
///
/// The same [`SMALL`] the write runs use, so the proxy walks a store the
/// size of the one the gated window walks.
const CALIBRATION_ROWS: u64 = SMALL;

/// How many reads one round of [`calibrate`] makes.
const CALIBRATION_ROUNDS: u64 = 2000;

/// What one round of [`calibrate`] costs on the machine the target was
/// written for, in microseconds. A laptop M4 with its cores to itself.
///
/// Three consecutive runs on an idle one read 13.7, 13.9 and 14.0, so
/// the middle one is the figure and the spread is two percent, which the
/// floor of the clamp absorbs: a host at or under the reference reads
/// the target exactly and never anything tighter. The same three runs
/// read 22.4, 22.2 and 22.3 on the gated number, so both ends of the
/// ratio are steady on a quiet box.
///
/// Take it on an idle machine or not at all. A stray `cargo test` in the
/// background moved the read to 21.2 us and the write to 36.2, and one
/// run under ten concurrent rustc read 4122 us a statement.
///
/// This is not a number to tune. It is a property of one host, and the
/// only reason to change it is that the host it was taken on has been
/// replaced.
const CALIBRATION_REFERENCE_US: f64 = 13.9;

/// The most the host calibration is allowed to relax the write ceiling.
///
/// A hosted runner reads about three times the reference and a bad
/// minute on one reads more, so four leaves room for the bad minute.
/// Past that the box is not slow, it is broken or it is swapping, and a
/// gate that keeps stretching for it stops being a gate.
const CALIBRATION_CAP: f64 = 4.0;

/// How fast this host is on the read path, in microseconds a statement.
///
/// The write ceiling below is a product target and not a number fitted
/// to a box, so it cannot simply be raised until the runners pass. The
/// alternative used to be a second ceiling written for the shared runner
/// class, which drifted out of date without anyone noticing and had to
/// be refitted twice, and that is what #648 asked to remove. So instead:
/// measure the host on something, divide by what that something costs on
/// the box the target was written for, and scale the ceiling by the
/// ratio. A box twice as slow gets twice the ceiling and there is no per
/// box class key to keep current.
///
/// The whole difficulty is what to measure. Five synthetic loops were
/// tried first, and here is what each read against the write path on the
/// same run, laptop against hosted runner:
///
/// | loop | proxy | write path |
/// |---|---|---|
/// | a few hundred bytes of buffer | 1.17x | 2.19x |
/// | 4 MiB table, 64 keys | 1.78x | 3.05x |
/// | 64 MiB table, 64 keys | 0.89x | 3.05x |
/// | 64 KiB table, a million keys | 1.15x | 3.05x |
/// | 4 MiB table, a million keys | 1.23x | 3.05x |
/// | 4 MiB table and an unsynced append, on processor time | 0.33x | 2.10x |
///
/// None of them reaches two. A loop small enough to sit in cache only
/// measures how fast the core retires instructions; one sized past the
/// last level cache measures the DIMM, and the laptop's advantage is in
/// the core rather than in its memory, so that ratio collapses or
/// inverts. The append was worse still, because on processor time a
/// Linux kernel absorbs an unsynced write far more cheaply than a Darwin
/// one and the proxy read the difference between two kernels.
///
/// What is actually three times slower on the runner is zu's own code,
/// which is branchy, allocates, and has an instruction footprint no
/// twenty line loop has. So the proxy is zu's own code: the same
/// `MATCH ... WHERE` the gated statement makes, without the `SET` on the
/// end of it. It runs the same optimizer, the same scan and the same
/// storage, and it is not the path the ceiling gates, so a write that
/// got slower cannot relax its own ceiling by making this slower too.
///
/// A read that got slower would relax it, which is the one thing this
/// gives up. That is a trade worth making: the read path has ceilings of
/// its own in `read.rs`, so a regression there is caught there, and it
/// is caught before this ever runs.
fn calibrate() -> f64 {
    let db = Database::memory_with(Config::new().threads(1)).expect("memory");
    let mut conn = db.connect().expect("connect");
    seed(
        conn.session_mut().file_mut().expect("the store"),
        CALIBRATION_ROWS,
        &ring(CALIBRATION_ROWS),
    );
    let read = |conn: &mut zu::Connection, age: u64| {
        one(
            conn,
            &format!("MATCH (p:person) WHERE p.age = {age} RETURN count(p) AS n"),
        )
    };
    let mut age = 0;
    for _ in 0..CALIBRATION_ROUNDS / 10 {
        read(&mut conn, age % CALIBRATION_ROWS);
        age += 1;
    }
    // Best of a few, the same way every measurement below is taken: on a
    // shared box a single pass reads whatever else was on the core at
    // the time, and the cheapest pass is the one that got the fewest of
    // those.
    let mut us = f64::MAX;
    for _ in 0..PASSES {
        let start = Instant::now();
        for _ in 0..CALIBRATION_ROUNDS {
            read(&mut conn, age % CALIBRATION_ROWS);
            age += 1;
        }
        us = us.min(start.elapsed().as_nanos() as f64 / 1e3 / CALIBRATION_ROUNDS as f64);
    }
    us
}

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

/// What this process has spent so far, as the kernel accounts for it.
///
/// A field reads zero when the platform was not asked rather than when
/// nothing was spent, which is why the zero is printed as it comes and
/// never turned into a ratio.
#[derive(Default, Clone, Copy)]
struct Usage {
    /// Resident bytes right now.
    rss: u64,
    /// The high water mark of resident bytes, which only ever rises.
    peak_rss: u64,
    /// Bytes this process has pushed at the disk since it started.
    written: u64,
    /// Microseconds of processor time, user and system together, that
    /// this process has burned since it started. A commit waits on an
    /// fsync and waiting is not burning, so this is the work the write
    /// path does and not the storage it does it on.
    cpu: u64,
}

/// The tail of `struct rusage` this bench does not read, sized so the
/// kernel writes inside the allocation rather than past it.
const RUSAGE_TAIL: usize = 14;

/// As much of `struct rusage` as the peak resident size needs. The two
/// timevals in front of that field are two words each on both platforms
/// this runs on, so it lands at the same offset on either, and the tail
/// is there for the kernel to fill rather than to be read.
#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    tail: [i64; RUSAGE_TAIL],
}

unsafe extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// The process peak resident size in bytes and the processor time it
/// has spent in microseconds. `ru_maxrss` is bytes on macOS and
/// kilobytes on Linux, which is a difference in the kernels rather than
/// in the call; the two timevals in front of it mean the same thing on
/// both.
fn peak_rss_and_cpu() -> (u64, u64) {
    let mut usage = Rusage::default();
    // RUSAGE_SELF is 0 on every platform that has the call.
    if unsafe { getrusage(0, &mut usage) } != 0 {
        return (0, 0);
    }
    let maxrss = usage.maxrss.max(0) as u64;
    let peak = if cfg!(target_os = "macos") {
        maxrss
    } else {
        maxrss * 1024
    };
    let micros = |t: [i64; 2]| (t[0].max(0) as u64) * 1_000_000 + t[1].max(0) as u64;
    (peak, micros(usage.utime) + micros(usage.stime))
}

#[cfg(target_os = "macos")]
mod platform {
    /// `struct rusage_info_v2` read as the words it is: sixteen bytes of
    /// uuid and then u64 fields, with room past the ones this reads so
    /// the kernel fills the buffer rather than the stack behind it.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Info {
        uuid: [u8; 16],
        words: [u64; 32],
    }

    /// Resident size is the seventh word of `rusage_info_v0`, and the
    /// bytes written counter is the second word `v2` adds after the six
    /// child counters `v1` added.
    const RESIDENT: usize = 6;
    const BYTES_WRITTEN: usize = 17;
    /// `RUSAGE_INFO_V2`, the oldest flavor carrying the disk counters.
    const FLAVOR: i32 = 2;

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buf: *mut Info) -> i32;
    }

    /// Resident bytes and bytes written, or zeros if the call refuses.
    pub(super) fn rss_and_written() -> (u64, u64) {
        let mut info = Info::default();
        if unsafe { proc_pid_rusage(std::process::id() as i32, FLAVOR, &mut info) } != 0 {
            return (0, 0);
        }
        (info.words[RESIDENT], info.words[BYTES_WRITTEN])
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Resident bytes from `statm`, whose second field is resident pages,
    /// and bytes written from `io`, whose `write_bytes` is what reached
    /// the storage layer rather than what was handed to the page cache.
    pub(super) fn rss_and_written() -> (u64, u64) {
        let rss = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096)
            .unwrap_or(0);
        let written = std::fs::read_to_string("/proc/self/io")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("write_bytes:")?.trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        (rss, written)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    pub(super) fn rss_and_written() -> (u64, u64) {
        (0, 0)
    }
}

fn usage() -> Usage {
    let (rss, written) = platform::rss_and_written();
    let (peak_rss, cpu) = peak_rss_and_cpu();
    Usage {
        rss,
        peak_rss,
        written,
        cpu,
    }
}

/// What one commit's durability costs the processor on this machine,
/// in microseconds, measured rather than assumed.
///
/// An fsync is not only a wait. On this laptop the call is `fcntl`
/// with `F_FULLFSYNC`, which burns 60 to 80 us of processor time before
/// the disk is even asked, and that lands in the same `getrusage`
/// counter the write path does, three times what the write path spends.
/// So the `cpu` column of a durable single statement commit is mostly
/// the sync and it moves with whatever else the machine is doing to its
/// storage, which is no way to watch a write path.
///
/// The measurement is the difference between a loop that syncs and the
/// same loop that does not, so what comes out is the sync and not the
/// small write in front of it. It runs on the same directory the
/// database is in, because a sync costs what the filesystem under it
/// costs.
fn sync_cpu(dir: &Path) -> f64 {
    use std::io::Write;
    const ROUNDS: u64 = 100;
    let path = dir.join("sync-cost");
    let pass = |sync: bool| -> u64 {
        let mut file = std::fs::File::create(&path).expect("a writable scratch file");
        let before = peak_rss_and_cpu().1;
        for i in 0..ROUNDS {
            file.write_all(&i.to_le_bytes()).expect("a write");
            if sync {
                file.sync_data().expect("a sync");
            }
        }
        peak_rss_and_cpu().1 - before
    };
    // Warm: the first create and the first sync on a fresh file pay for
    // metadata neither of the timed passes should carry.
    pass(true);
    let synced = pass(true).min(pass(true));
    let plain = pass(false).min(pass(false));
    let _ = std::fs::remove_file(&path);
    synced.saturating_sub(plain) as f64 / ROUNDS as f64
}

/// Every byte the database occupies, which is the file and the log
/// beside it.
fn disk(dir: &Path) -> u64 {
    let (store, log) = parts(dir);
    store + log
}

/// The same, with the log told apart from the store.
///
/// The two move for different reasons and the sustained run watches
/// both: the store grows when a fold takes blocks no checkpoint has
/// published yet, and the log shrinks when one finally does. A run that
/// wants to know whether it churned asks the second, because cutting
/// the log back is the one thing only a checkpoint does.
fn parts(dir: &Path) -> (u64, u64) {
    let (mut store, mut log) = (0, 0);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        match entry.path().extension().and_then(|e| e.to_str()) {
            Some("wal") => log += meta.len(),
            _ => store += meta.len(),
        }
    }
    (store, log)
}

/// The median statement of a run and its tail, in microseconds, off
/// the per statement times in `lat`. Sorts in place.
fn tail(lat: &mut [std::time::Duration]) -> (f64, f64) {
    lat.sort_unstable();
    let us = |d: std::time::Duration| d.as_nanos() as f64 / 1e3;
    (us(lat[lat.len() / 2]), us(lat[lat.len() * 99 / 100]))
}

/// What one run of a statement cost, per statement where that is the
/// sensible unit and over the whole run where it is not.
struct Cost {
    /// Latency a caller waits, in microseconds, averaged over the run.
    us: f64,
    /// The median statement of the run and its tail, in microseconds.
    ///
    /// The average above is what the run cost divided by the statements
    /// in it, so one statement that stalled for a hundred milliseconds
    /// moves it by half a millisecond and hides inside it. These two
    /// come from timing each statement on its own. A run is [`WRITES`]
    /// statements, so the p99 is the second slowest of them: coarse,
    /// but a checkpoint or a fold landing on one caller is exactly the
    /// thing it is there to show, and the ceilings on it are set with
    /// that coarseness in mind.
    p50: f64,
    p99: f64,
    /// Processor time the statement burned, in microseconds. A commit
    /// is a log append and an fsync, and on this laptop that fsync is
    /// 3.9 ms on its own, so the latency column is mostly the storage
    /// and this one is the write path. It is the column to watch when
    /// the fold gets cheaper, because the clock beside it will not move
    /// until commits stop syncing one at a time.
    cpu: f64,
    /// Bytes pushed at the disk.
    written: f64,
    /// Bytes the store is bigger by afterwards.
    growth: f64,
    /// Resident bytes when the run finished.
    rss: u64,
    /// Bytes the peak resident size rose by over the run.
    peak: u64,
}

impl Cost {
    /// The processor time of the statement with its commit's sync taken
    /// out, given what a sync costs on this machine.
    ///
    /// One statement is one commit is one sync, so what comes off is one
    /// `sync`. A statement that syncs twice keeps the second one, which
    /// is the right way round: an extra sync is a regression and should
    /// be visible rather than subtracted away.
    fn cpu_less_sync(&self, sync: f64) -> f64 {
        (self.cpu - sync).max(0.0)
    }

    fn header() {
        println!(
            "{:<26} {:>10} {:>9} {:>9} {:>9} {:>11} {:>12} {:>13} {:>12} {:>10} {:>12}",
            "statement",
            "latency",
            "p50",
            "p99",
            "cpu",
            "cpu-sync",
            "throughput",
            "written",
            "growth",
            "RSS",
            "peak growth"
        );
    }

    fn report(&self, what: &str, sync: f64) {
        println!(
            "{what:<26} {:>7.0} us {:>6.0} us {:>6.0} us {:>6.0} us {:>8.0} us {:>7.0} stmt/s {:>8.1} kB/st {:>7.0} B/st {:>7.1} MB {:>9.1} MB",
            self.us,
            self.p50,
            self.p99,
            self.cpu,
            self.cpu_less_sync(sync),
            1e6 / self.us.max(0.001),
            self.written / 1024.0,
            self.growth,
            self.rss as f64 / MB,
            self.peak as f64 / MB,
        );
    }
}

/// Builds a database with a two column `person` table of `rows` rows
/// and the `follows` rel table over it, in a directory of its own so
/// what it occupies can be told apart from every other run's.
fn build(dir: &Path, rows: u64) -> std::path::PathBuf {
    let edges: Vec<(u32, u32)> = (0..rows as u32)
        .map(|i| (i, (i * 7 + 1) % rows as u32))
        .collect();
    build_with(dir, rows, &edges)
}

/// The same table with nothing following anybody, which is the table a
/// plain `DELETE` run needs: GQL refuses to take away an element that
/// still has edges on it (G1001), so a `DELETE` run over the followed
/// table would measure the refusal and not the write.
fn build_bare(dir: &Path, rows: u64) -> std::path::PathBuf {
    build_with(dir, rows, &[])
}

/// The followed table with a property on the edges, which is what a
/// `SET` on an edge needs: a table that stores no edge column has no
/// place to put a value and refuses the statement.
///
/// Every row follows exactly one other, so the edges sort by their
/// source and the edge in the order the table holds them is the row it
/// leaves, which is what lets the run write back the value it found.
fn build_edge_props(dir: &Path, rows: u64) -> std::path::PathBuf {
    let path = build(dir, rows);
    let mut db = Zu1File::open(&path).expect("open");
    let since: Vec<u64> = (0..rows).collect();
    store_rel_props(&mut db, "follows", &[("since", PropValues::Int(&since))]).expect("rel props");
    path
}

/// The same again with a string on the edges as well, which is the
/// table LinkBench actually writes: its LINK carries a payload of 64
/// random characters, and a string column is the expensive one to
/// rewrite because the blob has to be re-encoded and not just moved.
fn build_edge_payload(dir: &Path, rows: u64) -> std::path::PathBuf {
    let path = build(dir, rows);
    let mut db = Zu1File::open(&path).expect("open");
    let since: Vec<u64> = (0..rows).collect();
    let loads: Vec<Vec<u8>> = (0..rows).map(payload).collect();
    let refs: Vec<&[u8]> = loads.iter().map(Vec::as_slice).collect();
    store_rel_props(
        &mut db,
        "follows",
        &[
            ("since", PropValues::Int(&since)),
            ("payload", PropValues::Str(&refs)),
        ],
    )
    .expect("rel props");
    path
}

/// 64 characters that look like the payload LinkBench hands an edge:
/// drawn from a small alphabet so the column compresses the way a real
/// one does, and varying per edge so a symbol table cannot cheat it.
fn payload(i: u64) -> Vec<u8> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = i
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..64)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ALPHABET[(seed >> 33) as usize % ALPHABET.len()]
        })
        .collect()
}

/// The same table with a label beyond its own name declared on it,
/// which is what a `SET` of a label is measured against: a label the
/// table has not declared is one the statement declares, and a catalog
/// written once at the head of a run is not what the per statement cost
/// of putting a bit on a row is.
///
/// The label goes on and comes straight back off, so the table has
/// declared it and no row carries it, which is where the run starts.
fn build_labels(dir: &Path, rows: u64) -> std::path::PathBuf {
    let path = build(dir, rows);
    let mut db = Zu1File::open(&path).expect("open");
    let all: Vec<Vec<&str>> = (0..rows as usize).map(|_| vec!["Bot"]).collect();
    store_labels(&mut db, "person", &all).expect("labels");
    let none: Vec<Vec<&str>> = (0..rows as usize).map(|_| Vec::new()).collect();
    store_labels(&mut db, "person", &none).expect("labels");
    path
}

fn build_with(dir: &Path, rows: u64, edges: &[(u32, u32)]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("db.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    seed(&mut db, rows, edges);
    path
}

/// The rows themselves, apart from the file they go in, because the
/// unsynced run puts the same ones in a store that has no file.
fn seed(db: &mut Zu1File, rows: u64, edges: &[(u32, u32)]) {
    bulk_load_as(db, "person", "follows", rows, edges).expect("load");
    let names: Vec<Vec<u8>> = (0..rows).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..rows).collect();
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

fn one(conn: &mut zu::Connection, text: &str) -> i64 {
    let r = conn.query(text).expect("query");
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// A `SET` of one property of one element, `WRITES` times over, each
/// one a statement of its own and so a commit and a fold of its own.
///
/// Every write names a different row, because a write path that only
/// ever touched the row it touched last could keep something warm that
/// a real update stream would not. Each write puts back the value it
/// found, so the table it leaves is the table it started with: the
/// growth column is what changing nothing cost the store, which should
/// be nothing.
fn run_set(dir: &Path, rows: u64) -> Cost {
    let path = build(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    // One write before the clock starts, so the number carries neither
    // the first compile of the statement nor the first extend of the
    // log.
    conn.query("MATCH (p:person) WHERE p.age = 0 SET p.age = 0")
        .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    // The pass is timed [`PASSES`] times and the fastest one is the
    // number, because `set_fold_x` is this clock at one table size over
    // this clock at another and a single slow pass moves a ratio that
    // is not about speed at all. On a hosted runner the large table's
    // pass has come in anywhere from 7096 to 19735 us a statement,
    // which put a ratio the laptop measures at 2.1 to 2.5 between 4.18
    // and 6.18 on the same commit.
    //
    // The bytes, the disk growth and the processor time are the first
    // pass only. They are counts and not clocks, so nothing about them
    // wants a best of, and a store that grows once on first touch would
    // look like a store that grows a third as much if the same growth
    // were spread over three passes.
    let mut elapsed = std::time::Duration::MAX;
    let mut lat = Vec::with_capacity(WRITES as usize);
    let mut once: Option<(Usage, u64)> = None;
    for _ in 0..PASSES {
        let mut pass = Vec::with_capacity(WRITES as usize);
        let start = Instant::now();
        for i in 0..WRITES {
            let age = i % rows;
            let at = Instant::now();
            conn.query(&format!(
                "MATCH (p:person) WHERE p.age = {age} SET p.age = {age}"
            ))
            .expect("set");
            pass.push(at.elapsed());
        }
        // The median and the tail come off whichever pass was fastest
        // end to end, the same pass the average above is taken from, so
        // the three numbers on the line describe one run rather than
        // three different ones.
        let took = start.elapsed();
        if took < elapsed {
            elapsed = took;
            lat = pass;
        }
        if once.is_none() {
            once = Some((usage(), disk(dir)));
        }
    }
    let (after, disk_after) = once.expect("a pass ran");
    let growth = disk_after.saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        rows as i64,
        "no row was added or lost"
    );
    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person) WHERE p.age = 7 RETURN count(p) AS n"
        ),
        1,
        "and the value written back is the value that was there"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `SET` of the whole record of one element, `WRITES` times over.
///
/// This is the form that says what the element holds afterwards rather
/// than what to change about it, so it writes every column of the table
/// and not one of them. Two columns here, one of each kind, so the number
/// beside the one property run is what the second column and the wider
/// log record cost, and it is the run that says a string is carried the
/// way a word is: the name goes over the name the row holds and the
/// blob range is rebuilt around it, where it used to mean rewriting the
/// column. Each write puts back the values it found, as the other runs
/// do, so the growth column is again what changing nothing cost the
/// store. It is what a linkbench update of a node's payload costs.
fn run_set_record(dir: &Path, rows: u64) -> Cost {
    let path = build(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.query("MATCH (p:person) WHERE p.age = 0 SET p = {age: 0, name: 'seed0'}")
        .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        let age = i % rows;
        conn.query(&format!(
            "MATCH (p:person) WHERE p.age = {age} SET p = {{age: {age}, name: 'seed{age}'}}"
        ))
        .expect("set a record");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        rows as i64,
        "no row was added or lost"
    );
    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person) WHERE p.name = 'seed7' RETURN count(p) AS n"
        ),
        1,
        "and both values written back are the values that were there"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `SET` of a label on one element, `WRITES` times over.
///
/// A label is not a property: it is one bit of one word beside the row,
/// and the word is one per row rather than one per column. So this is
/// the cheapest write the statement has, and the number beside the one
/// property run is what a column costs that a bit does not. Each write
/// puts the label on and the next takes it off again, so the table ends
/// where it started and the growth column is what changing nothing cost.
///
/// It no longer folds. A change is a pair of masks, the bits to put on
/// the row and the bits to take off it, and a reader wants the word, so
/// the writer reads the word the row was carrying, puts the masks over
/// it and hands the answer on. The bitset itself is not rewritten and
/// neither are the columns of the table beside it.
fn run_set_label(dir: &Path, rows: u64) -> Cost {
    let path = build_labels(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.query("MATCH (p:person) WHERE p.age = 0 SET p:Bot")
        .expect("warmup");
    conn.query("MATCH (p:person) WHERE p.age = 0 REMOVE p:Bot")
        .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        let age = i % rows;
        let verb = match i % 2 {
            0 => "SET",
            _ => "REMOVE",
        };
        conn.query(&format!(
            "MATCH (p:person) WHERE p.age = {age} {verb} p:Bot"
        ))
        .expect("set a label");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        rows as i64,
        "no row was added or lost"
    );
    assert_eq!(
        one(&mut conn, "MATCH (p:Bot) RETURN count(p) AS n"),
        (WRITES / 2) as i64,
        "every other write put the label on and the one after took it off"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `SET` of one property of one edge, `WRITES` times over, which is
/// the same statement as the run above against the other half of the
/// store.
///
/// It is worth its own number because an edge property is kept in a
/// different shape. A node column is in row order, so a change to one
/// cell is a change to one place in it, whereas an edge column is dense
/// over the edges in the order the table holds them and an edge has no
/// offset of its own to name that place by. So the change names the pair
/// of rows the edge runs between, and that used to mean a fold, which
/// rewrote the column through the edge order it rebuilt. The writer now
/// works the pair into the ordinal it holds and puts the word in the
/// patch, so what is left in here is the search for the edge and the log
/// frame. It is what a linkbench update of a link's payload costs.
///
/// Each write puts back the value it found, as the node run does, so the
/// growth column is what changing nothing cost the store.
fn run_set_edge(dir: &Path, rows: u64) -> Cost {
    let path = build_edge_props(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.query("MATCH (p:person)-[f:follows]->(q:person) WHERE p.age = 0 SET f.since = 0")
        .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        let age = i % rows;
        conn.query(&format!(
            "MATCH (p:person)-[f:follows]->(q:person) WHERE p.age = {age} SET f.since = {age}"
        ))
        .expect("set");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person)-[f:follows]->(q:person) RETURN count(*) AS n"
        ),
        rows as i64,
        "no edge was added or lost"
    );
    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person)-[f:follows]->(q:person) WHERE f.since = 7 RETURN count(*) AS n"
        ),
        1,
        "and the value written back is the value that was there"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `MERGE` over two rows, one the walk finds and one it does not,
/// `WRITES` times over.
///
/// This is the only statement shape that writes twice: the row the walk
/// missed is an insert and the row it found is a `SET`, and until they
/// shared a transaction the statement paid a commit frame, an epoch and
/// a fold for each of them. They are different rows by definition, so
/// neither half reads what the other wrote, which is what lets them go
/// in together. The number to watch is the bytes: a second commit for
/// the same work is a second frame, and the row count at the end is
/// what says both halves still happened.
///
/// The walk itself is a scan of the table, the same as the `SET` run
/// above, so most of the clock here is the read and the bytes are the
/// part of the number that is about the write.
fn run_merge(dir: &Path, rows: u64) -> Cost {
    let path = build(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let merge = |found: u64, made: u64| {
        format!(
            "UNWIND [{found}, {made}] AS a MERGE (p:person {{age: a}}) \
             ON CREATE SET p.name = 'made' ON MATCH SET p.name = 'found'"
        )
    };
    conn.query(&merge(0, rows)).expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        conn.query(&merge(i % rows, rows + 1 + i)).expect("merge");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows + WRITES + 1) as i64,
        "the half that writes wrote a row a statement and no more"
    );
    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person) WHERE p.name = 'found' RETURN count(p) AS n"
        ),
        WRITES.min(rows) as i64,
        "and the half that matched changed the rows it matched"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// An `INSERT` of one element, `WRITES` times over, for the same reason
/// and at the same shape: a statement, a commit and a fold each.
///
/// Unlike the `SET` run this one does grow the table, so its growth
/// carries the row itself as well as whatever the write path rewrote
/// around it.
fn run_insert(dir: &Path, rows: u64) -> Cost {
    let path = build(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.query("INSERT (p:person {age: -1, name: 'warmup'})")
        .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        conn.query(&format!(
            "INSERT (p:person {{age: {}, name: 'new'}})",
            rows + i
        ))
        .expect("insert");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows + WRITES + 1) as i64,
        "every element written is readable"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// An `INSERT` of one edge between two rows that are already there,
/// `WRITES` times over.
///
/// This is the shape LinkBench writes most of, and it is the one that
/// costs the most: a rel table holds its edges sorted by the row they
/// leave, so an edge added in the middle moves every edge behind it,
/// and the fold rebuilds the CSR and rewrites the edge columns into the
/// new order. Run at two sizes, the number says whether the statement
/// pays for the edge it added or for every edge the table already had.
///
/// With `strings` on, the edges carry a payload as well, which is the
/// LinkBench shape: the rewrite then has to re-encode a blob and not
/// just shuffle fixed width values.
fn run_insert_edge(dir: &Path, rows: u64, strings: bool) -> Cost {
    let path = if strings {
        build_edge_payload(dir, rows)
    } else {
        build_edge_props(dir, rows)
    };
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let insert = |i: u64| {
        let props = if strings {
            format!(
                "since: {i}, payload: '{}'",
                String::from_utf8(payload(i)).expect("ascii")
            )
        } else {
            format!("since: {i}")
        };
        format!(
            "MATCH (a:person), (b:person) WHERE a.age = {} AND b.age = {} \
             INSERT (a)-[:follows {{{props}}}]->(b)",
            i % rows,
            (i * 7 + 3) % rows,
        )
    };
    conn.query(&insert(0)).expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        conn.query(&insert(i + 1)).expect("insert");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(
            &mut conn,
            "MATCH (p:person)-[f:follows]->(q:person) RETURN count(*) AS n"
        ),
        (rows + WRITES + 1) as i64,
        "every edge written is readable"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// The same edge inserts staged straight onto a transaction, with no
/// statement in front of them.
///
/// This is the one that answers the size question. The statement above
/// finds its two endpoints with a MATCH over two patterns, and that
/// search is linear in the rows of the table, so at 100000 rows it is
/// most of what the statement costs and the ratio it gives says more
/// about the read plane than about the write. Staging the pair
/// directly leaves the write on its own, which is what wants watching:
/// a rel table holds its edges in the order the CSR lays them out, so
/// an edge that folds pays for every edge already there, and one that
/// commits without folding does not.
fn run_insert_edge_txn(dir: &Path, rows: u64, strings: bool) -> Cost {
    let path = if strings {
        build_edge_payload(dir, rows)
    } else {
        build_edge_props(dir, rows)
    };
    let mut session = Session::open(&path).expect("open");
    let rel = session
        .catalog()
        .rel_by_name("follows")
        .expect("follows")
        .id;
    let cells = |i: u64| match strings {
        true => vec![(0, Cell::Int(i)), (1, Cell::Str(payload(i)))],
        false => vec![(0, Cell::Int(i))],
    };
    // The row a pair leaves is a fresh one every time and the row it
    // arrives at is scattered, so no two writes share a pair and none
    // of them is an edge the table already holds.
    let write = |session: &mut Session, i: u64| {
        let (src, dst) = (i % rows, (i * 7 + 3) % rows);
        session
            .write(|txn| {
                txn.insert_rel_carrying(rel, src, dst, cells(i));
                Ok(())
            })
            .expect("insert");
    };
    write(&mut session, 0);

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        write(&mut session, i + 1);
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    let read = session
        .run(
            "MATCH (p:person)-[f:follows]->(q:person) RETURN count(*) AS n",
            &[],
        )
        .expect("read back");
    assert_eq!(
        read.rows.first().and_then(|row| row.first()),
        Some(&Value::Int((rows + WRITES + 1) as i64)),
        "every edge written is readable"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `DELETE` of one element, `WRITES` times over, at the same shape as
/// the other two.
///
/// A delete does not compact, because every edge in the file names its
/// endpoints by row offset, so what a delete writes is a tombstone: the
/// row keeps its place and the offset joins the table's tombstone
/// chain, which every scan after that filters by. That makes this the
/// one write of the three whose cost has two halves, and both are in
/// the number: the statement itself, and the chain growing by one
/// offset a statement so every read pays a little more. The offset is
/// carried in the patch until a fold takes it, so what the statement
/// pays is the log frame and the readers merge the two lists.
///
/// The count is read back afterwards, so a path that timed well by not
/// taking the row away fails instead of scoring.
fn run_delete(dir: &Path, rows: u64) -> Cost {
    let path = build_bare(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.query(&format!(
        "MATCH (p:person) WHERE p.age = {} DELETE p",
        rows - 1
    ))
    .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        conn.query(&format!("MATCH (p:person) WHERE p.age = {i} DELETE p"))
            .expect("delete");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows - WRITES - 1) as i64,
        "every element deleted is gone, and no other one is"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// A `DETACH DELETE` of one element, `WRITES` times over, over the
/// table where everybody follows somebody, which is what makes it
/// different from the `DELETE` run: the element has edges on it, and
/// they go with it.
///
/// This used to be the expensive shape of a write, because an edge has
/// no offset a reader could filter it out by and so cannot be
/// tombstoned the way a row is, which left the fold to drop it out of
/// the CSR it rebuilds over the whole table's edges. What a reader is
/// handed now is the pair the edge runs between, which is the whole
/// name of it, and the adjacency reader takes it off the two lists it
/// is in on the way past, so the statement costs what a DELETE does and
/// the bytes column says the same at the disk.
///
/// Both ends are checked afterwards: the rows are counted, and so are
/// the edges, so a path that timed well by leaving an edge behind fails
/// instead of scoring.
fn run_detach(dir: &Path, rows: u64) -> Cost {
    let path = build(dir, rows);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let edges = one(
        &mut conn,
        "MATCH (p:person)-[:follows]->(q:person) RETURN count(*) AS n",
    );
    conn.query(&format!(
        "MATCH (p:person) WHERE p.age = {} DETACH DELETE p",
        rows - 1
    ))
    .expect("warmup");

    let before = usage();
    let disk_before = disk(dir);
    let mut lat = Vec::with_capacity(WRITES as usize);
    let start = Instant::now();
    for i in 0..WRITES {
        let at = Instant::now();
        conn.query(&format!(
            "MATCH (p:person) WHERE p.age = {i} DETACH DELETE p"
        ))
        .expect("detach delete");
        lat.push(at.elapsed());
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows - WRITES - 1) as i64,
        "every element deleted is gone, and no other one is"
    );
    let left = one(
        &mut conn,
        "MATCH (p:person)-[:follows]->(q:person) RETURN count(*) AS n",
    );
    assert!(
        left < edges - WRITES as i64,
        "every deleted element took its edges with it: {left} left of {edges}"
    );
    let (p50, p99) = tail(&mut lat);
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        p50,
        p99,
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

/// The statement both sustained runs make. It writes one cell over a
/// row the store already holds and touches nothing else, so what the
/// run costs above the cell is the housekeeping.
fn set(conn: &mut zu::Connection, age: u64) {
    conn.query(&format!(
        "MATCH (p:person) WHERE p.age = {age} SET p.age = {age}"
    ))
    .expect("set");
}

/// One edge out of every node, which is what a seeded store carries so
/// that a fold has a graph to rebuild and not just a column to rewrite.
fn ring(rows: u64) -> Vec<(u32, u32)> {
    (0..rows as u32)
        .map(|i| (i, (i * 7 + 1) % rows as u32))
        .collect()
}

/// Statements from one fold to the next, measured on a store whose
/// syncs ask no disk.
///
/// The two runs below have to hold folds at the rate they happen, and
/// that rate is a bound inside the write path rather than anything this
/// file can see. Counting them is possible anyway. A fold rewrites a
/// column and writes two segments where a deferred commit appends a log
/// frame, so it is milliseconds where a statement is microseconds, and
/// with no sync in the way it is the only thing that is. The slow
/// statements are therefore the folds, and the statements over the
/// number of them is the rate.
///
/// The line between the two is drawn off the probe's own median rather
/// than at a number of microseconds, because the median moves by a
/// factor of ten between a quiet laptop and a shared runner while the
/// gap between a statement and a fold stays two orders of magnitude.
fn fold_every(rows: u64) -> u64 {
    let db = Database::memory_with(Config::new().threads(1)).expect("memory");
    let mut conn = db.connect().expect("connect");
    seed(
        conn.session_mut().file_mut().expect("the store"),
        rows,
        &ring(rows),
    );

    let before = folds_so_far(&mut conn);
    for age in 0..PROBE {
        set(&mut conn, age % rows);
    }
    let folds = folds_so_far(&mut conn) - before;

    // A probe with no fold in it has not measured a rate, and a window
    // sized off the answer would run for as long as the bound is wrong
    // by, so the rate is capped at what makes the window the length of
    // the probe. A bound that outran the probe fails the fold check in
    // `main` and says so, which is the right way to find out.
    (PROBE / folds.max(1)).min(PROBE / FOLDS_WINDOW)
}

/// How many folds the store behind this connection has run since it
/// was opened.
///
/// Asking costs the writer lock and gives it straight back, so it is
/// something to do between windows rather than inside one.
fn folds_so_far(conn: &mut zu::Connection) -> u64 {
    conn.session_mut().fold_count().expect("fold count")
}

/// What a sustained run cost, which is a [`Cost`] and the two things
/// only a long window can say.
struct Sustained {
    cost: Cost,
    /// The store at its biggest over the run against the store as it
    /// was loaded.
    file_x: f64,
    /// The store at its biggest inside the measured window against what
    /// it was when the window opened.
    window_x: f64,
    /// The store as the window opened and at its biggest inside it, in
    /// bytes. The ratio above says how much it grew, these say what it
    /// grew from, and the checkpoint slack that bounds the growth is a
    /// share of the file held between a floor and a ceiling, so which
    /// of the three is in force cannot be read off the ratio alone.
    opened: u64,
    peak: u64,
    /// The store as it was loaded, before the ramp. The pair above
    /// bounds the measured window, this bounds the whole run.
    loaded: u64,
}

/// The write path measured across its own housekeeping rather than
/// between two rounds of it.
///
/// Every other run in this file is [`WRITES`] statements on a store
/// that was loaded a moment earlier, and that window falls entirely
/// inside the first deferred batch: no fold happens in it, no
/// checkpoint happens in it, and the growth column reads zero because
/// nothing that grows a file has run yet. That is an honest number for
/// the deferred path and a misleading one for a store being written to,
/// which is the whole reason this run exists. It ramps past the point
/// where the file stops growing, then measures a window long enough to
/// hold folds at their own rate and more than one checkpoint, and says
/// what a statement costs with its share of both inside it.
///
/// It also says what the store was at its worst against what it was
/// loaded as. A fold takes fresh blocks for whatever it rewrites and
/// gives back none of them until a checkpoint publishes, so that ratio
/// is the transient garbage the deferred path carries, and it is the
/// number that says whether the bound on it is set anywhere near right.
/// It was eighteen once, for a store that fit in four megabytes and ran
/// to seventy-one.
///
/// What the caller checks the run against is the pair of numbers that
/// says the housekeeping was inside the window rather than beside it.
/// The bytes are the first: a statement that only commits pushes its
/// log frame and nothing else, so bytes above what the short run
/// pushes are the folds and there is nothing else they can be. The
/// second is that the store held still while that was going on, which
/// is not the same as the zero the short runs report. A fold that
/// nobody published would show up as a file getting steadily bigger,
/// one block per column it rewrote; a file that folds all window and
/// ends the size it started is a file whose blocks are coming back,
/// and a checkpoint publishing them is the only thing that hands a
/// block back. Neither number can be had by a run that quietly stopped
/// churning, which is the point of checking them.
fn run_sustained(dir: &Path, rows: u64, ramp: u64, window: u64) -> Sustained {
    let path = build(dir, rows);
    let loaded = parts(dir).0;
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    let mut age = 0;
    for _ in 0..ramp {
        set(&mut conn, age % rows);
        age += 1;
    }

    let before = usage();
    let store_before = parts(dir).0;
    let mut peak = store_before;
    let mut lat = Vec::with_capacity(window as usize);
    let start = Instant::now();
    for i in 0..window {
        let at = Instant::now();
        set(&mut conn, age % rows);
        lat.push(at.elapsed());
        age += 1;
        if (i + 1) % SAMPLE == 0 {
            peak = peak.max(parts(dir).0);
        }
    }
    let elapsed = start.elapsed();
    let after = usage();
    let store_after = parts(dir).0;

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        rows as i64,
        "no row was added or lost"
    );
    let (p50, p99) = tail(&mut lat);
    Sustained {
        cost: Cost {
            us: elapsed.as_nanos() as f64 / 1e3 / window as f64,
            p50,
            p99,
            cpu: after.cpu.saturating_sub(before.cpu) as f64 / window as f64,
            written: after.written.saturating_sub(before.written) as f64 / window as f64,
            growth: store_after.saturating_sub(store_before) as f64 / window as f64,
            rss: after.rss,
            peak: after.peak_rss.saturating_sub(before.peak_rss),
        },
        file_x: peak as f64 / loaded.max(1) as f64,
        window_x: peak as f64 / store_before.max(1) as f64,
        opened: store_before,
        peak,
        loaded,
    }
}

/// What the unsynced window cost and how many folds were in it.
struct Unsynced {
    cost: Cost,
    /// Statements in the window that took a fold's worth of time. On a
    /// store with no sync in the way nothing else does, so this is the
    /// count of folds the window held.
    folds: u64,
}

/// The same sustained window with nothing to sync to, which is what
/// "processor time excluding fsync" means when it is measured rather
/// than estimated.
///
/// The `cpu-sync` column beside every other run is the processor time
/// with one calibrated sync subtracted from it, and the subtraction
/// does not work. A sustained statement pays its own commit sync plus
/// its share of the two a checkpoint costs, so one is the wrong number
/// of them to take off; and the calibration syncs a scratch file with a
/// few bytes behind it while the statement syncs a nine megabyte store
/// with a fold's worth of dirty pages, so one sync is the wrong size as
/// well. Three runs of the same commit on this laptop put that column
/// at 53, 122 and 201 us, which is not a measurement of anything.
///
/// A store on the memory filesystem runs the identical write path. It
/// encodes the same log frames, keeps the same overlay, folds on the
/// same schedule and checkpoints on the same threshold, and the only
/// thing it does differently is that its syncs return without asking a
/// disk. So the difference between the two runs is the durability and
/// nothing else, and what is left is the write path. It reads 22.8 us
/// against the file's 73.3, and it reads it again the next time.
///
/// The fold is inside it, which is the thing worth checking rather than
/// assuming: the same store measured over a window too short to fold
/// reads 19.1, so the folds in the sustained window are about four
/// microseconds of the number and the run is not quietly measuring a
/// store that stopped working.
fn run_unsynced(rows: u64, ramp: u64, window: u64) -> Unsynced {
    let db = Database::memory_with(Config::new().threads(1)).expect("memory");
    let mut conn = db.connect().expect("connect");
    // A memory database opens empty, so the rows go in through the
    // store itself rather than through a file that was loaded before
    // anything opened it. This is the one place a bench reaches for the
    // file under the session, and it is at seeding time, before
    // anything is being measured.
    seed(
        conn.session_mut().file_mut().expect("the store"),
        rows,
        &ring(rows),
    );

    let mut age = 0;
    for _ in 0..ramp {
        set(&mut conn, age % rows);
        age += 1;
    }

    let before = usage();
    let folds_before = folds_so_far(&mut conn);
    let start = Instant::now();
    // Timed one at a time as well as end to end, for the median and the
    // tail. The folds in the window are counted off the writer rather
    // than read out of these times: they used to be whatever statement
    // came in over twenty times the median, and on a box with a
    // neighbour that line catches statements that are not folds and
    // misses folds that are. Three runs on this laptop put the rate at
    // one fold every 91, 153 and 356 statements for the same store and
    // the same statements.
    let mut spent = Vec::with_capacity(window as usize);
    for _ in 0..window {
        let at = Instant::now();
        set(&mut conn, age % rows);
        spent.push(at.elapsed().as_nanos() as f64 / 1e3);
        age += 1;
    }
    let elapsed = start.elapsed();
    let after = usage();

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        rows as i64,
        "no row was added or lost"
    );
    let mut sorted = spent.clone();
    sorted.sort_by(f64::total_cmp);
    let cost = Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / window as f64,
        p50: sorted[sorted.len() / 2],
        p99: sorted[sorted.len() * 99 / 100],
        cpu: after.cpu.saturating_sub(before.cpu) as f64 / window as f64,
        // A store with no file under it pushes nothing at a disk and
        // grows no file, so both columns are zero by construction
        // rather than by measurement and neither is gated.
        written: 0.0,
        growth: 0.0,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    };
    Unsynced {
        cost,
        folds: folds_so_far(&mut conn) - folds_before,
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    // How fast this box is, on a proxy shaped like a write. One ceiling
    // here is a product target rather than a number fitted to a box, and
    // this is what lets that one target be enforced everywhere: it is
    // read at what the target says on the reference host and at the
    // ratio above it on a slower one. See #648 for what this replaced.
    let root = tempfile::tempdir().expect("tempdir");
    let cal_us = calibrate();
    let raw = cal_us / CALIBRATION_REFERENCE_US;
    let scale = raw.clamp(1.0, CALIBRATION_CAP);
    println!(
        "host calibration: {cal_us:.1} us a read, {raw:.2}x the reference \
         {CALIBRATION_REFERENCE_US:.1} us"
    );
    if raw > scale {
        println!(
            "host calibration: {raw:.2}x is past the {CALIBRATION_CAP:.0}x cap, the write \
             ceiling is read at {CALIBRATION_CAP:.0}x"
        );
    }
    let sync = sync_cpu(root.path());
    println!("one sync costs this machine {sync:.0} us of processor time");

    Cost::header();
    let set_small = run_set(&root.path().join("set-small"), SMALL);
    set_small.report(&format!("SET, {SMALL} rows"), sync);

    let set_large = run_set(&root.path().join("set-large"), LARGE);
    set_large.report(&format!("SET, {LARGE} rows"), sync);

    let set_edge = run_set_edge(&root.path().join("set-edge"), SMALL);
    set_edge.report(&format!("SET on an edge, {SMALL} rows"), sync);

    let set_record = run_set_record(&root.path().join("set-record"), SMALL);
    set_record.report(&format!("SET a record, {SMALL} rows"), sync);

    let set_label = run_set_label(&root.path().join("set-label"), SMALL);
    set_label.report(&format!("SET a label, {SMALL} rows"), sync);

    let insert = run_insert(&root.path().join("insert"), SMALL);
    insert.report(&format!("INSERT, {SMALL} rows"), sync);

    // The same statement over ten times the table, which is the one
    // question the `SET` pair above cannot answer. A `SET` leaves the
    // row domain where it was, so the fold rewrites the chunks the
    // statement touched and leaves every other column alone. An
    // `INSERT` grows the domain, and a column that has to grow is a
    // column the fold has to rewrite, so the work an append leaves for
    // the fold is set by the table and not by the append. Whether that
    // is what happens is `insert_fold_x`.
    let insert_large = run_insert(&root.path().join("insert-large"), LARGE);
    insert_large.report(&format!("INSERT, {LARGE} rows"), sync);

    let merge = run_merge(&root.path().join("merge"), SMALL);
    merge.report(&format!("MERGE, one found one made, {SMALL} rows"), sync);

    let edge_small = run_insert_edge(&root.path().join("insert-edge-small"), SMALL, false);
    edge_small.report(&format!("INSERT an edge, {SMALL} edges"), sync);

    let edge_large = run_insert_edge(&root.path().join("insert-edge-large"), LARGE, false);
    edge_large.report(&format!("INSERT an edge, {LARGE} edges"), sync);

    let edge_str = run_insert_edge(&root.path().join("insert-edge-str"), SMALL, true);
    edge_str.report(
        &format!("INSERT an edge with a payload, {SMALL} edges"),
        sync,
    );

    let txn_small = run_insert_edge_txn(&root.path().join("txn-edge-small"), SMALL, false);
    txn_small.report(&format!("edge on a txn, {SMALL} edges"), sync);

    let txn_large = run_insert_edge_txn(&root.path().join("txn-edge-large"), LARGE, false);
    txn_large.report(&format!("edge on a txn, {LARGE} edges"), sync);

    let txn_str = run_insert_edge_txn(&root.path().join("txn-edge-str"), SMALL, true);
    txn_str.report(
        &format!("edge on a txn with a payload, {SMALL} edges"),
        sync,
    );

    let delete = run_delete(&root.path().join("delete"), SMALL);
    delete.report(&format!("DELETE, {SMALL} rows"), sync);

    let detach = run_detach(&root.path().join("detach"), SMALL);
    detach.report(&format!("DETACH DELETE, {SMALL} rows"), sync);

    // How long the two runs below have to be, which is a question about
    // the write path and not one this file gets to answer on its own.
    // See [`fold_every`].
    let every = fold_every(SMALL);
    let (ramp, window) = (FOLDS_RAMP * every, FOLDS_WINDOW * every);
    println!(
        "a fold every {every} statements, so the sustained window is {window} of them with \
         {ramp} ahead of it"
    );

    let sustained = run_sustained(&root.path().join("sustained"), SMALL, ramp, window);
    sustained
        .cost
        .report(&format!("SET sustained, {SMALL} rows"), sync);
    // Every run above this line reports a growth of zero, and the
    // reason is that none of them is long enough to fold: two hundred
    // statements all land in the first deferred batch. This one is long
    // enough, so its growth column is a store that is holding steady
    // rather than a store that has not started yet, and the ratio
    // beside it is how much bigger than itself the store gets while it
    // holds steady.
    println!(
        "sustained_file_x: {:.2}x store at its peak against the store as loaded",
        sustained.file_x
    );
    // A run that quietly stopped churning would score better on every
    // column above, so it fails on these two instead. The bytes say the
    // folds were inside the window: a statement that only commits
    // pushes its log frame, and the short run above is exactly that, so
    // anything over it is a fold. The file says a checkpoint published
    // them: blocks come back from nowhere else.
    let sustained_fold_x = sustained.cost.written / set_small.written.max(1.0);
    println!("sustained_fold_x: {sustained_fold_x:.2}x the bytes of a run that never folds");
    println!(
        "sustained_window_x: {:.2}x store at its peak against the store as the window opened, \
         {:.1} MB to {:.1} MB",
        sustained.window_x,
        sustained.opened as f64 / MB,
        sustained.peak as f64 / MB
    );
    // The byte counter is the operating system's, and it counts what
    // reached a block device. A store on a memory filesystem, which is
    // what /tmp is on a good many Linux boxes, reaches one never, so
    // the column reads zero for every scenario and the ratio is zero
    // over zero. That is the instrument missing rather than the folds
    // missing, and the two have to be told apart: the file check below
    // holds either way, and it is the one that would catch a run that
    // stopped churning.
    //
    // What it reads is set by the fold rate and not by how long the
    // window is, since both halves of it grow together: a fold on this
    // store pushes about a megabyte, a commit pushes sixteen kilobytes,
    // so a fold every thousand statements is a kilobyte a statement on
    // top of sixteen and the ratio is about 1.07. It read 1.27 when a
    // fold landed every 256. That is the reason the floor below is 1.03
    // rather than anything tighter: the deferral bound is what decides
    // the headroom this check has, and raising it spends some. The
    // count of folds in the unsynced window is the direct form of the
    // same question and is checked further down.
    //
    // Reported and not checked, on either kind of host. On Linux the
    // counter reads zero because nothing the run wrote reached a block
    // device inside it. On Darwin it reads 16.1 kB a statement for
    // every run in this file including the ones that write a single
    // cell, which is one block per commit gone out under the fsync, and
    // a fold's share of a statement is a few hundred bytes: under the
    // granularity of the thing measuring it. So the ratio came back
    // 1.01 for a window whose file grew by 540 B a statement, and a
    // counter that cannot resolve the signal is not evidence either
    // way. The file growth below is the same question asked of the
    // file, and the fold count in the unsynced window is it asked of
    // the folds themselves, so the check is made twice over without
    // this one.
    println!(
        "sustained_fold_x: reported, not checked. The bytes are a process counter at block \
         granularity and one commit already writes a block, so a fold's few hundred bytes \
         do not clear it"
    );
    // What the folds did to the file, which is on the file itself and
    // so is there to read wherever the store is. A run whose folds
    // stopped is a run that loaded a store and left it the size it
    // loaded it.
    assert!(
        sustained.file_x > 1.05,
        "the sustained window has to contain the folds it is measuring, and the store came \
         out of it {:.2}x the size it was loaded at",
        sustained.file_x
    );
    // A fold that nobody published takes fresh blocks for every block
    // it rewrote and gives none of them back, so what the file may grow
    // by between checkpoints is exactly the checkpoint slack. That is a
    // share of the file held between a floor and a ceiling rather than
    // a flat ratio, and for a store under the floor the floor is what
    // is in force: this store opens its window at 0.7 MB and the floor
    // is a megabyte, so a bound of 1.25x was one the rule never
    // promised and the check failed on a run that was behaving. The
    // rule itself comes from the write path so the two cannot drift,
    // and a run whose blocks stop coming back still fails, because it
    // runs past the slack instead of checkpointing at it.
    //
    // Half a block of headroom on top, for the fold that crosses the
    // threshold: the check fires at the commit after, so the last fold
    // is over the line by whatever it took.
    let slack = zu::write::checkpoint_slack_bytes(sustained.opened) + BLOCK as u64 / 2;
    let allowed = sustained.opened + slack;
    println!(
        "sustained_window_slack: {:.1} MB grown against the {:.1} MB the checkpoint rule \
         allows a store this size",
        (sustained.peak - sustained.opened) as f64 / MB,
        slack as f64 / MB
    );
    // The same question over the whole run rather than over the
    // measured window, and the one that is gated. The ramp folds too,
    // so the file is already carrying churn when the window opens and
    // a bound on the window alone would not see it.
    let run_slack = zu::write::checkpoint_slack_bytes(sustained.loaded);
    let slack_x = (sustained.peak - sustained.loaded) as f64 / run_slack as f64;
    println!(
        "sustained_slack_x: {slack_x:.2}x the checkpoint slack, {:.1} MB loaded to {:.1} MB at \
         its biggest against the {:.1} MB the rule allows a store this size",
        sustained.loaded as f64 / MB,
        sustained.peak as f64 / MB,
        run_slack as f64 / MB
    );
    assert!(
        sustained.peak <= allowed,
        "a fold nobody published grows the file by every block it rewrote, and this one \
         went from {:.1} MB to {:.1} MB over the window against the {:.1} MB the \
         checkpoint slack allows, so the blocks are not coming back",
        sustained.opened as f64 / MB,
        sustained.peak as f64 / MB,
        allowed as f64 / MB
    );

    // Best of PASSES, the way every other number in this file is taken.
    // This one was a single pass and it is the one the gate reads, which
    // is why it kept coming back with a different answer: three runs of
    // the same hosted runner read 53.5, 75.1 and 50.3 us a statement,
    // and no ceiling scaled off a host measurement can absorb a fifty
    // percent spread that is contention on the box rather than the write
    // path. The lowest pass is the one that got the fewest neighbours.
    let mut unsynced = run_unsynced(SMALL, ramp, window);
    for _ in 1..PASSES {
        let again = run_unsynced(SMALL, ramp, window);
        if again.cost.cpu < unsynced.cost.cpu {
            unsynced = again;
        }
    }
    unsynced
        .cost
        .report(&format!("SET unsynced, {SMALL} rows"), 0.0);
    // The folds counted rather than inferred. The window was sized to
    // hold [`FOLDS_WINDOW`] of them and a run that quietly stopped
    // folding is the thing every check around here is looking for, so
    // this is that question asked of the folds themselves. Half is the
    // floor because the rate was measured on one store and used on
    // another, and because a checkpoint lands among them and takes a
    // fold's worth of time of its own.
    println!(
        "sustained_folds: {} folds in the window with no sync in it, against the {FOLDS_WINDOW} \
         it was sized for",
        unsynced.folds
    );
    assert!(
        unsynced.folds >= FOLDS_WINDOW / 2,
        "the sustained window has to contain the folds it is measuring, and {window} statements \
         with no sync in them held {} of them",
        unsynced.folds
    );
    // The write path on its own, which is the same window over a store
    // whose syncs return without asking a disk. This is the number the
    // P5 point write budget is read against, because it is measured
    // rather than arrived at by subtracting an estimate of a sync from
    // a number that contains several.
    println!(
        "write_cpu_nosync_us: {:.1} us of processor time a statement, against the {:.1} the \
         same window costs with the syncs in it",
        unsynced.cost.cpu, sustained.cost.cpu
    );
    // And its tail, which is the same window read at the other end.
    // Every other line in this file prints a p99 with an fsync inside
    // it, and a ceiling on that is a ceiling on the disk: this is the
    // one whose tail is the write path's own.
    //
    // The ratio beside it is what a fold landing inside the tail would
    // move. A fold is milliseconds where these statements are tens of
    // microseconds, and the deferral bound is set so that the share of
    // statements carrying one stays under what a p99 leaves: this run
    // folds once every 240 statements, which is 0.4 percent, so the
    // folds sit just above the p99 and the tail is a statement rather
    // than a fold. A bound that shrank, or a fold that got slower,
    // pulls one in and this jumps.
    let p99_x = unsynced.cost.p99 / unsynced.cost.p50.max(0.001);
    println!(
        "write_p99_nosync_us: {:.1} us at the tail against {:.1} at the median, {p99_x:.2}x",
        unsynced.cost.p99, unsynced.cost.p50
    );

    // How much of a one cell write is the table it sits in, in time and
    // in bytes. One means the write path does not read the table; ten
    // means it reads all of it, since the large table is ten times the
    // small one.
    let fold_x = set_large.us / set_small.us.max(0.001);
    let write_x = set_large.written / set_small.written.max(1.0);
    println!("set_fold_x:  {fold_x:.2}x in time from {SMALL} to {LARGE} rows");
    println!("set_write_x: {write_x:.2}x in bytes written from {SMALL} to {LARGE} rows");

    // The same question of the append, on the processor time rather
    // than the clock for the reason the edge ratio below gives. An
    // `INSERT` puts one row past the end of every column, so what it
    // asks of the store does not depend on how many rows are already
    // there, and a ratio near one is that. A ratio near ten is the fold
    // rewriting the whole table to add a row to it, which is a cost per
    // statement that goes up forever as the table fills.
    let insert_x = insert_large.cpu / insert.cpu.max(0.001);
    println!("insert_fold_x: {insert_x:.2}x in processor time from {SMALL} to {LARGE} rows");

    // The same question of an edge insert, which is the one write that
    // rebuilds a whole structure rather than rewriting a column of it.
    // Asked of the processor time and not the clock: a machine whose
    // sync costs 5 ms carries that in both halves of the ratio and
    // reads far flatter than it is, and a gate box whose sync is free
    // does not, so the wall clock version of this number says more
    // about the storage than about the write path.
    let edge_x = edge_large.cpu / edge_small.cpu.max(0.001);
    println!("insert_edge_x: {edge_x:.2}x in processor time from {SMALL} to {LARGE} edges");

    // The same ratio of the write on its own. The statement one above
    // carries the MATCH that found the two endpoints, and that search
    // is linear in the rows of the table, so it is the bigger half of
    // the number at 100000 and the write is nearly none of it.
    let write_edge_x = txn_large.cpu / txn_small.cpu.max(0.001);
    println!(
        "insert_edge_write_x: {write_edge_x:.2}x in processor time from {SMALL} to {LARGE} edges"
    );

    let mut failed = false;
    let checks = [
        ("set_stmt_us", set_small.us),
        ("set_stmt_cpu_us", set_small.cpu),
        ("set_stmt_cpu_nosync_us", set_small.cpu_less_sync(sync)),
        ("insert_stmt_cpu_us", insert.cpu),
        ("set_edge_stmt_us", set_edge.us),
        ("set_edge_stmt_kb", set_edge.written / 1024.0),
        ("set_edge_stmt_cpu_us", set_edge.cpu),
        ("set_edge_stmt_growth_b", set_edge.growth),
        ("set_record_stmt_us", set_record.us),
        ("set_record_stmt_kb", set_record.written / 1024.0),
        ("set_record_stmt_cpu_us", set_record.cpu),
        ("set_record_stmt_growth_b", set_record.growth),
        ("set_label_stmt_us", set_label.us),
        ("set_label_stmt_kb", set_label.written / 1024.0),
        ("set_label_stmt_cpu_us", set_label.cpu),
        ("set_label_stmt_growth_b", set_label.growth),
        ("insert_stmt_us", insert.us),
        ("insert_stmt_kb", insert.written / 1024.0),
        ("insert_stmt_growth_b", insert.growth),
        ("insert_edge_stmt_us", edge_small.us),
        ("insert_edge_stmt_kb", edge_small.written / 1024.0),
        ("insert_edge_stmt_cpu_us", edge_small.cpu),
        ("insert_edge_str_stmt_us", edge_str.us),
        ("insert_edge_str_stmt_cpu_us", edge_str.cpu),
        ("insert_edge_x", edge_x),
        ("insert_edge_write_us", txn_small.cpu),
        ("insert_edge_write_kb", txn_small.written / 1024.0),
        ("insert_edge_write_str_us", txn_str.cpu),
        ("insert_edge_write_x", write_edge_x),
        ("delete_stmt_us", delete.us),
        ("delete_stmt_cpu_us", delete.cpu),
        ("delete_stmt_growth_b", delete.growth),
        ("detach_stmt_us", detach.us),
        ("detach_stmt_cpu_us", detach.cpu),
        ("detach_stmt_growth_b", detach.growth),
        ("merge_stmt_us", merge.us),
        ("merge_stmt_kb", merge.written / 1024.0),
        ("merge_stmt_cpu_us", merge.cpu),
        ("merge_stmt_growth_b", merge.growth),
        ("set_stmt_kb", set_small.written / 1024.0),
        ("delete_stmt_kb", delete.written / 1024.0),
        ("detach_stmt_kb", detach.written / 1024.0),
        ("set_stmt_growth_b", set_small.growth),
        ("set_peak_rss_mb", set_small.peak as f64 / MB),
        ("set_fold_x", fold_x),
        ("set_write_x", write_x),
        ("insert_fold_x", insert_x),
        ("sustained_stmt_us", sustained.cost.us),
        ("sustained_stmt_cpu_us", sustained.cost.cpu),
        (
            "sustained_stmt_cpu_nosync_us",
            sustained.cost.cpu_less_sync(sync),
        ),
        ("sustained_stmt_kb", sustained.cost.written / 1024.0),
        ("sustained_stmt_growth_b", sustained.cost.growth),
        ("sustained_slack_x", slack_x),
        ("write_cpu_nosync_us", unsynced.cost.cpu),
        ("write_p99_nosync_us", unsynced.cost.p99),
        ("write_p99_p50_x", p99_x),
    ];
    for (key, got) in checks {
        let Some(written) = budget(key) else { continue };
        // The one key that is a product target rather than a number
        // fitted to what was measured, so it is the one that has to be
        // read against the host rather than against a box class. Every
        // other ceiling here is either a ratio, which a slow box cannot
        // move, or was fitted on the box class that enforces it.
        let ceiling = match key {
            "write_cpu_nosync_us" | "write_p99_nosync_us" => written * scale,
            _ => written,
        };
        if key == "write_cpu_nosync_us" {
            println!("write ceiling: {written:.1} us written, {ceiling:.1} us on this host");
        }
        if got > ceiling {
            println!("GATE FAIL {key}: {got:.2} > ceiling {ceiling:.2}");
            failed = true;
        }
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
