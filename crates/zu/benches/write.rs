//! What one write statement costs (G3, zu#118), in every resource a
//! write spends: latency, throughput, bytes pushed to the disk, bytes
//! the store grew by, and memory.
//!
//! A `SET` of one property of one element, an `INSERT` of one element
//! and a `DELETE` of one element, all three through the query engine,
//! all three one statement per commit. That is the shape of a linkbench update and of an SNB
//! insert, so it is the number those workloads are made of. The read
//! benches say what finding the row costs; what is left in here is the
//! write path, which is the log append, the fdatasync the commit is,
//! and the fold that makes the new value readable.
//!
//! One number would hide most of that. A write path can be quick and
//! still be wrong to ship, so every run reports five: `us` is what a
//! caller waits, `stmt/s` is what a stream of them sustains, `written`
//! is what the statement pushed at the disk, `growth` is what it added
//! to the store for good, and the two memory columns are what it cost
//! to hold. A one cell change that writes a megabyte is a real defect
//! and the clock alone would call it fine; a write path that leaked a
//! block per statement would pass every latency ceiling there is and
//! show up only in `growth`.
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
//! Both statements are checked rather than only timed: the rows are
//! counted and read back after the loop, so a write path that got
//! faster by writing less fails instead of scoring.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench write

use std::path::Path;
use std::time::Instant;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};
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
const MB: f64 = 1024.0 * 1024.0;

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

/// The process peak resident size in bytes. `ru_maxrss` is bytes on
/// macOS and kilobytes on Linux, which is a difference in the kernels
/// rather than in the call.
fn peak_rss() -> u64 {
    let mut usage = Rusage::default();
    // RUSAGE_SELF is 0 on every platform that has the call.
    if unsafe { getrusage(0, &mut usage) } != 0 {
        return 0;
    }
    let maxrss = usage.maxrss.max(0) as u64;
    if cfg!(target_os = "macos") {
        maxrss
    } else {
        maxrss * 1024
    }
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
    Usage {
        rss,
        peak_rss: peak_rss(),
        written,
    }
}

/// Every byte the database occupies, which is the file and the log
/// beside it.
fn disk(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// What one run of a statement cost, per statement where that is the
/// sensible unit and over the whole run where it is not.
struct Cost {
    /// Latency a caller waits, in microseconds.
    us: f64,
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
    fn header() {
        println!(
            "{:<22} {:>10} {:>12} {:>13} {:>12} {:>10} {:>12}",
            "statement", "latency", "throughput", "written", "growth", "RSS", "peak growth"
        );
    }

    fn report(&self, what: &str) {
        println!(
            "{what:<22} {:>7.0} us {:>7.0} stmt/s {:>8.1} kB/st {:>7.0} B/st {:>7.1} MB {:>9.1} MB",
            self.us,
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
/// `DELETE` run needs: GQL refuses to take away an element that still
/// has edges on it (G1001), and `DETACH DELETE`, which takes them with
/// it, is not in yet.
fn build_bare(dir: &Path, rows: u64) -> std::path::PathBuf {
    build_with(dir, rows, &[])
}

fn build_with(dir: &Path, rows: u64, edges: &[(u32, u32)]) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("dir");
    let path = dir.join("db.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "follows", rows, edges).expect("load");
    let names: Vec<Vec<u8>> = (0..rows).map(|i| format!("seed{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let ages: Vec<u64> = (0..rows).collect();
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
    let start = Instant::now();
    for i in 0..WRITES {
        let age = i % rows;
        conn.query(&format!(
            "MATCH (p:person) WHERE p.age = {age} SET p.age = {age}"
        ))
        .expect("set");
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
            "MATCH (p:person) WHERE p.age = 7 RETURN count(p) AS n"
        ),
        1,
        "and the value written back is the value that was there"
    );
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
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
    let start = Instant::now();
    for i in 0..WRITES {
        conn.query(&format!(
            "INSERT (p:person {{age: {}, name: 'new'}})",
            rows + i
        ))
        .expect("insert");
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows + WRITES + 1) as i64,
        "every element written is readable"
    );
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
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
/// row keeps its place and the fold merges the offset into the table's
/// tombstone chain, which every scan after that filters by. That makes
/// this the one write of the three whose cost has two halves, and both
/// are in the number: the statement itself, and the chain growing by
/// one offset a statement so every read pays a little more.
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
    let start = Instant::now();
    for i in 0..WRITES {
        conn.query(&format!("MATCH (p:person) WHERE p.age = {i} DELETE p"))
            .expect("delete");
    }
    let elapsed = start.elapsed();
    let after = usage();
    let growth = disk(dir).saturating_sub(disk_before);

    assert_eq!(
        one(&mut conn, "MATCH (p:person) RETURN count(p) AS n"),
        (rows - WRITES - 1) as i64,
        "every element deleted is gone, and no other one is"
    );
    Cost {
        us: elapsed.as_nanos() as f64 / 1e3 / WRITES as f64,
        written: after.written.saturating_sub(before.written) as f64 / WRITES as f64,
        growth: growth as f64 / WRITES as f64,
        rss: after.rss,
        peak: after.peak_rss.saturating_sub(before.peak_rss),
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let root = tempfile::tempdir().expect("tempdir");

    Cost::header();
    let set_small = run_set(&root.path().join("set-small"), SMALL);
    set_small.report(&format!("SET, {SMALL} rows"));

    let set_large = run_set(&root.path().join("set-large"), LARGE);
    set_large.report(&format!("SET, {LARGE} rows"));

    let insert = run_insert(&root.path().join("insert"), SMALL);
    insert.report(&format!("INSERT, {SMALL} rows"));

    let delete = run_delete(&root.path().join("delete"), SMALL);
    delete.report(&format!("DELETE, {SMALL} rows"));

    // How much of a one cell write is the table it sits in, in time and
    // in bytes. One means the write path does not read the table; ten
    // means it reads all of it, since the large table is ten times the
    // small one.
    let fold_x = set_large.us / set_small.us.max(0.001);
    let write_x = set_large.written / set_small.written.max(1.0);
    println!("set_fold_x:  {fold_x:.2}x in time from {SMALL} to {LARGE} rows");
    println!("set_write_x: {write_x:.2}x in bytes written from {SMALL} to {LARGE} rows");

    let mut failed = false;
    let checks = [
        ("set_stmt_us", set_small.us),
        ("insert_stmt_us", insert.us),
        ("delete_stmt_us", delete.us),
        ("set_stmt_kb", set_small.written / 1024.0),
        ("delete_stmt_kb", delete.written / 1024.0),
        ("set_stmt_growth_b", set_small.growth),
        ("set_peak_rss_mb", set_small.peak as f64 / MB),
        ("set_fold_x", fold_x),
        ("set_write_x", write_x),
    ];
    for (key, got) in checks {
        if let Some(ceiling) = budget(key)
            && got > ceiling
        {
            println!("GATE FAIL {key}: {got:.2} > ceiling {ceiling}");
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
