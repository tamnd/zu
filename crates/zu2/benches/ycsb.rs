//! Z3: zu2 against sqlite, in process, on the YCSB point workloads.
//!
//! The claim zu2 exists to test is that a hash index over a hybrid log
//! beats a B-tree over pages on point operations by enough to matter.
//! This is where that gets a number instead of an argument, so the two
//! engines run in one binary, on one machine, against the same keys, in
//! the same phase order, with the same per-operation transaction model.
//!
//! What is held fixed, and why.
//!
//! Every engine holds the same number of records. An earlier version of
//! this benchmark gave every configuration the same wall clock to load
//! in, which left zu2 with a million records and a slow sqlite with
//! twenty thousand, and then compared their read rates. That is not a
//! comparison, it is a working set difference: twenty thousand rows fit
//! in cache and a million do not. Now the load runs to a fixed count for
//! everyone and the phases that follow read the same key space.
//!
//! The load runs once per engine, at that engine's fastest durability,
//! and the measured phases run afterwards on the loaded database at each
//! durability in turn. That is why durability is a property of a zu2
//! session and of a sqlite connection rather than of the file: it lets
//! one loaded database answer for every setting, so the settings differ
//! in nothing but what a commit waits for.
//!
//! Every measured operation is its own transaction. That is the YCSB
//! model and it is the only setting in which a durability mode means
//! anything. Batching a thousand updates into one sqlite transaction
//! would measure the batch, not the engine.
//!
//! Every update writes bytes the record did not already hold. That sounds
//! like it should not need saying, and it cost this benchmark a set of
//! wrong numbers: sqlite compares the new payload against the stored one
//! and skips dirtying the page when they match, so an update phase that
//! rewrote each record with the value derived from its key was measuring
//! a memcmp. No journal, no page write, no fsync, and sqlite reporting
//! twenty thousand durable updates a second on a device that can do four
//! hundred. Every value now carries a revision, so an update is an update.
//!
//! Keys are uniform, not zipfian. YCSB's default is zipfian, which would
//! flatter zu2: a skewed read set lives in the mutable tail and in cache.
//! Uniform is the conservative choice here.
//!
//! Durability is paired rather than cherry picked, and the pairing is by
//! what a commit actually waits for, not by what the setting is called.
//! sqlite `synchronous=OFF` and `synchronous=NORMAL` in WAL both return
//! before the device has the bytes, so both pair with zu2 Async. sqlite
//! `synchronous=FULL` does not return until the write is on the device,
//! so it pairs with zu2 Durable. WAL with FULL is in the sweep because
//! it is the fastest durable sqlite, which is the one worth losing to.
//!
//! sqlite gets two table shapes, because the shape is most of the point
//! read cost: a rowid table whose text primary key builds a separate
//! unique index, which is what go-ycsb creates, and a `WITHOUT ROWID`
//! table where the primary key is the table and a lookup is one descent
//! rather than two. It also gets a 256 MiB page cache and mmap window,
//! because a rival running badly proves nothing either.
//!
//! One caveat, and it is the reason the durable rows are only worth
//! reading on Linux and Windows. On macOS neither engine reaches the
//! drive: sqlite takes `F_BARRIERFSYNC` for `synchronous=FULL`, which
//! orders writes without waiting for the device, and zu2 takes plain
//! `fsync`, which on Darwin returns once the writes are in the drive's
//! own cache. The only macOS call that waits is `F_FULLFSYNC`, and the
//! device line the run prints is measured with it, which is why the
//! macOS durable rows come out above their own floor. On Linux zu2 calls
//! `fdatasync` and so does sqlite, the device line is measured with the
//! same call, and the comparison holds. The run prints which it is on.
//!
//! Storage is measured as well as speed, because an engine that is fast
//! and three times the size is not obviously the better trade. Every
//! engine is asked the same two questions: what the same records cost on
//! disk after the load, and what they cost after the update phases have
//! rewritten them. The number is what the device is holding, not what the
//! file claims: zu2 punches holes when it compacts, so its length is not
//! its size, and sqlite in WAL keeps a second file, so its main database
//! is not its size either. zu1, the previous engine, loads the same
//! records into its columnar file for a third column, and it answers the
//! load question only, because it has no update path per record.
//!
//! Run: cargo bench -p zu2 --bench ycsb
//! Longer: ZU2_BUDGET=10 ZU2_RECORDS=1000000 cargo bench -p zu2 --bench ycsb
//! Threads: ZU2_THREADS=1,4,8 cargo bench -p zu2 --bench ycsb
//! No zu1 column: ZU2_ZU1=0 cargo bench -p zu2 --bench ycsb

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::Connection;
use zu2::{Db, Durability, Options};

/// Bytes of value per record, which is YCSB's ten fields of a hundred.
const VALUE_BYTES: usize = 1000;

/// Operations per latency sample. Timing every operation costs more than
/// the operation does on the fast engine, which would flatter the slow
/// one, so the clock is read once per batch and divided.
const SAMPLE: u64 = 32;

fn env<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn key(i: u64) -> String {
    format!("user{i:019}")
}

/// A record's value: its serial number, its revision, then pseudorandom
/// bytes.
///
/// Random rather than a constant fill, and this is a storage decision
/// rather than a throughput one. A thousand identical bytes is a thousand
/// bytes an engine with a dictionary or a run length encoder does not have
/// to store, so a constant fill would put zu1's compressor in the storage
/// table instead of its layout. YCSB's own values are random for the same
/// reason. The serial number stays at the front so a read that returned
/// the wrong record is still caught.
///
/// The revision is not decoration. sqlite compares the new payload against
/// the stored one in `btreeOverwriteContent` and skips dirtying the page
/// when the bytes match, so an update phase that rewrote a record with the
/// value it already held never journalled, never wrote and never synced.
/// That is a real optimisation and it is not what this benchmark is asking
/// about, and it made sqlite look between ten and a hundred times faster at
/// update than it is. Every write carries a revision no earlier write of
/// that record used, so every update is an update.
fn value(i: u64, revision: u64, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(format!("{i:019}").as_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    let mut rng = Rng(0x9E3779B97F4A7C15
        ^ (i + 1).wrapping_mul(0x100000001B3)
        ^ revision.wrapping_mul(0xD6E8FEB86659FD93)
        | 1);
    while out.len() < VALUE_BYTES {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(VALUE_BYTES);
}

/// The revision a worker stamps on its `n`th write.
///
/// The thread index is in the high bits so no two workers ever pick the
/// same one, and the counter is in the low bits so a worker that comes back
/// to a record it wrote before never writes the same bytes twice.
fn revision(t: usize, n: u64) -> u64 {
    ((t as u64 + 1) << 40) | n
}

fn seed(t: usize) -> u64 {
    0x9E3779B97F4A7C15 ^ (t as u64 + 1).wrapping_mul(0x100000001B3)
}

/// One durable commit's floor on this filesystem, measured on the spot.
///
/// This exists so a reader does not have to take a durability setting's
/// word for it. A row that claims a commit waits for the device and
/// reports a rate well above this number did not wait, whatever the
/// setting is called, and that is worth catching in the output rather
/// than in a spec written six months later.
///
/// The writes are overwrites of a file that has already been filled and
/// synced, because an overwrite does not allocate and is the cheaper of
/// the two, so this is a floor rather than an estimate. On Linux
/// `sync_data` is `fdatasync`, which is what zu2 calls, so the two are
/// directly comparable. On macOS `sync_data` is `F_FULLFSYNC` and zu2
/// calls plain `fsync`, so the number there is stricter than what zu2
/// pays and the durable rows are not a like for like anyway.
fn device_floor(dir: &Path) -> f64 {
    use std::io::{Seek, SeekFrom, Write};
    const BLOCK: usize = 4096;
    const SPAN: u64 = 64;
    const ROUNDS: u64 = 32;
    let path = dir.join("device-floor");
    let mut f = std::fs::File::create(&path).expect("probe file");
    let block = [7u8; BLOCK];
    for _ in 0..SPAN {
        f.write_all(&block).expect("fill");
    }
    f.sync_all().expect("fill sync");
    let started = Instant::now();
    for i in 0..ROUNDS {
        f.seek(SeekFrom::Start((i % SPAN) * BLOCK as u64))
            .expect("seek");
        f.write_all(&block).expect("write");
        f.sync_data().expect("sync");
    }
    let rate = ROUNDS as f64 / started.elapsed().as_secs_f64();
    drop(f);
    let _ = std::fs::remove_file(&path);
    rate
}

/// One worker's clock. Collects per-operation seconds, batched.
struct Lat {
    batch: Instant,
    counted: u64,
    samples: Vec<f64>,
}

impl Lat {
    fn new() -> Self {
        Self {
            batch: Instant::now(),
            counted: 0,
            samples: Vec::new(),
        }
    }

    #[inline]
    fn tick(&mut self) {
        self.counted += 1;
        if self.counted == SAMPLE {
            self.samples
                .push(self.batch.elapsed().as_secs_f64() / SAMPLE as f64);
            self.counted = 0;
            self.batch = Instant::now();
        }
    }
}

/// What one worker did.
struct Work {
    ops: u64,
    samples: Vec<f64>,
}

/// One phase's result, pooled over the workers.
struct Phase {
    ops: u64,
    seconds: f64,
    p50: f64,
    p99: f64,
}

impl Phase {
    fn pool(work: Vec<Work>, seconds: f64) -> Self {
        let ops = work.iter().map(|w| w.ops).sum();
        let mut all: Vec<f64> = work.into_iter().flat_map(|w| w.samples).collect();
        all.sort_unstable_by(f64::total_cmp);
        let at = |q: f64| match all.len() {
            0 => 0.0,
            n => all[((n as f64 - 1.0) * q).round() as usize] * 1e6,
        };
        Self {
            ops,
            seconds,
            p50: at(0.50),
            p99: at(0.99),
        }
    }

    fn rate(&self) -> f64 {
        if self.seconds <= 0.0 {
            0.0
        } else {
            self.ops as f64 / self.seconds
        }
    }
}

/// Runs `body` on `threads` workers and pools what they report.
fn phase(threads: usize, body: impl Fn(usize) -> Work + Sync) -> Phase {
    let started = Instant::now();
    let work: Vec<Work> = std::thread::scope(|scope| {
        let body = &body;
        let handles: Vec<_> = (0..threads).map(|t| scope.spawn(move || body(t))).collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker"))
            .collect()
    });
    Phase::pool(work, started.elapsed().as_secs_f64())
}

/// Which operation a phase issues.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Update,
    /// YCSB workload A: half reads, half updates.
    Mixed,
}

impl Op {
    fn reads(self, rng: &mut Rng) -> bool {
        match self {
            Op::Read => true,
            Op::Update => false,
            Op::Mixed => rng.next() & 1 == 0,
        }
    }
}

/// Whether a commit returns before or after the device has the bytes.
/// This is the axis the durability pairing is drawn on, because it is
/// the only one that changes what a crash can take away.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waits {
    /// Returns before the write reaches the device.
    No,
    /// Does not return until it does.
    Device,
}

/// One configuration's measured phases.
struct Run {
    label: String,
    mine: bool,
    waits: Waits,
    read: Phase,
    update: Phase,
    mixed: Phase,
}

/// What one engine's records cost on disk.
struct Storage {
    label: String,
    /// After the load, with everything the engine owes the device paid.
    loaded: u64,
    /// After the measured phases, which is where an engine that never
    /// reclaims anything shows it. `None` for an engine the phases did
    /// not run against.
    after: Option<u64>,
}

impl Storage {
    fn per_record(bytes: u64, records: u64) -> f64 {
        bytes as f64 / records as f64
    }
}

/// Bytes a file holds on the device, zero if it is not there. Holes are
/// excluded, which matters for zu2 and costs nothing for the others.
fn file_bytes(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .map(|m| m.blocks() * 512)
            .unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

/// A sqlite database is up to three files and all of them are the
/// database, so the write ahead log counts against it exactly the way
/// zu2's log counts against zu2.
fn sqlite_bytes(path: &Path) -> u64 {
    let mut total = file_bytes(path);
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        total += file_bytes(Path::new(&side));
    }
    total
}

/// How long each measured phase runs, and how many records everyone
/// loads before they start.
#[derive(Clone, Copy)]
struct Budget {
    seconds: f64,
    records: u64,
}

impl Budget {
    #[inline]
    fn over(&self, started: &Instant) -> bool {
        started.elapsed().as_secs_f64() >= self.seconds
    }
}

// ---------------------------------------------------------------- zu2

fn zu2_options(records: u64, memory_pages: usize) -> Options {
    Options {
        // The load's setting. Every measured phase sets its own on the
        // session, so this is only what the loader gets.
        durability: Durability::Async,
        // Under half full at eight entries a bucket.
        index_buckets: (records as usize / 4).next_power_of_two().max(1 << 12),
        // 4 MiB a page, and every record is written more than once over
        // the phases, so leave the log room to grow.
        max_pages: 1 << 17,
        memory_pages,
        ..Options::default()
    }
}

/// Loads one zu2 database and measures it at each durability.
fn run_zu2(
    dir: &Path,
    threads: usize,
    budget: Budget,
    memory_pages: usize,
) -> (Phase, Vec<Run>, Vec<Storage>) {
    let path = dir.join(format!("zu2-{threads}.db"));
    let db = Db::create(&path, zu2_options(budget.records, memory_pages)).expect("create");
    let records = budget.records;

    // The load strides, so worker t writes t, t+threads, t+2*threads,
    // and every key below `records` is written exactly once.
    let load = phase(threads, |t| {
        let mut s = db.session();
        let mut buf = Vec::with_capacity(VALUE_BYTES);
        let mut lat = Lat::new();
        let mut done = 0u64;
        let mut i = t as u64;
        while i < records {
            value(i, 0, &mut buf);
            s.upsert(key(i).as_bytes(), &buf).expect("upsert");
            lat.tick();
            done += 1;
            i += threads as u64;
        }
        Work {
            ops: done,
            samples: lat.samples,
        }
    });

    // The load ran async, so the tail is ahead of the file. Pay what is
    // owed before asking what the records cost.
    db.sync().expect("sync");
    let loaded = db.disk_bytes().expect("disk bytes");

    // Crosscheck before anything is timed against it: a sample of keys
    // is there and each one carries its own serial number, so a read
    // that returned the wrong record would be caught rather than counted
    // as a fast read.
    {
        let mut s = db.session();
        let mut out = Vec::new();
        let mut rng = Rng(0x243F6A8885A308D3);
        for _ in 0..1000.min(records) {
            let i = rng.next() % records;
            assert!(
                s.read(key(i).as_bytes(), &mut out).expect("read"),
                "lost {i}"
            );
            assert_eq!(out.len(), VALUE_BYTES, "key {i} is the wrong length");
            assert_eq!(&out[..19], &key(i).as_bytes()[4..], "key {i} crossed");
        }
    }

    let run = |durability: Durability, op: Op| {
        // The backlog belongs to whoever made it. A phase that does not
        // wait for the device leaves a tail the file has not got yet,
        // and without this the first commit of the next waiting phase
        // pays to write and sync all of it. That is one operation
        // carrying a hundred megabytes and it is not what a steady
        // waiting workload costs.
        db.sync().expect("sync");
        phase(threads, |t| {
            let mut s = db.session();
            s.set_durability(durability);
            let mut out = Vec::new();
            let mut buf = Vec::with_capacity(VALUE_BYTES);
            let mut rng = Rng(seed(t));
            let mut lat = Lat::new();
            let started = Instant::now();
            let mut done = 0u64;
            loop {
                let i = rng.next() % records;
                if op.reads(&mut rng) {
                    assert!(s.read(key(i).as_bytes(), &mut out).expect("read"), "miss");
                } else {
                    value(i, revision(t, done), &mut buf);
                    s.upsert(key(i).as_bytes(), &buf).expect("upsert");
                }
                lat.tick();
                done += 1;
                if done.is_multiple_of(SAMPLE) && budget.over(&started) {
                    break;
                }
            }
            Work {
                ops: done,
                samples: lat.samples,
            }
        })
    };

    let mut runs = Vec::new();
    for (durability, waits) in [
        (Durability::Async, Waits::No),
        (Durability::Durable, Waits::Device),
    ] {
        runs.push(Run {
            label: format!("zu2 {}", format!("{durability:?}").to_lowercase()),
            mine: true,
            waits,
            read: run(durability, Op::Read),
            update: run(durability, Op::Update),
            mixed: run(durability, Op::Mixed),
        });
    }

    db.sync().expect("sync");
    let after = db.disk_bytes().expect("disk bytes");
    // What the background compactor had not got to yet. It runs on its
    // own schedule and the phases stop on a clock, so the row above is a
    // database caught mid-pass and this one is the same database asked to
    // finish. Both are worth printing: one is what a running system looks
    // like, the other is what it settles at.
    db.compact().expect("compact");
    db.sync().expect("sync");
    let compacted = db.disk_bytes().expect("disk bytes");

    let storage = vec![
        Storage {
            label: "zu2".into(),
            loaded,
            after: Some(after),
        },
        Storage {
            label: "zu2 compacted".into(),
            loaded,
            after: Some(compacted),
        },
    ];
    (load, runs, storage)
}

// ------------------------------------------------------------- sqlite

/// A sqlite durability setting. `journal` belongs to the database and
/// `synchronous` belongs to the connection, which is why they are set in
/// two different places below.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Setting {
    journal: &'static str,
    synchronous: &'static str,
    waits: Waits,
}

/// The load setting: nothing waits for the device, which is the fastest
/// sqlite can be told to go.
const LOADING: Setting = Setting {
    journal: "WAL",
    synchronous: "OFF",
    waits: Waits::No,
};

const SETTINGS: [Setting; 4] = [
    LOADING,
    // What a production sqlite runs. It still does not wait for the
    // device on commit, only at checkpoint, so it pairs with Async.
    Setting {
        journal: "WAL",
        synchronous: "NORMAL",
        waits: Waits::No,
    },
    // The fastest durable sqlite: one device write per commit, appended
    // to the WAL rather than written twice through a rollback journal.
    Setting {
        journal: "WAL",
        synchronous: "FULL",
        waits: Waits::Device,
    },
    // sqlite's own default journal, durable. Slower than WAL and still
    // the combination most people mean by a durable sqlite.
    Setting {
        journal: "DELETE",
        synchronous: "FULL",
        waits: Waits::Device,
    },
];

impl Setting {
    fn label(&self) -> String {
        format!(
            "{}/{}",
            self.journal.to_lowercase(),
            self.synchronous.to_lowercase()
        )
    }
}

/// Opens a connection at one `synchronous` setting. The journal mode is
/// the database's, set once by `set_journal`, so this never fights
/// another connection over it.
fn connect(path: &Path, synchronous: &str) -> Connection {
    let conn = Connection::open(path).expect("open sqlite");
    // Busy timeout before anything else. A pragma is a statement and a
    // statement takes a lock, so opening a connection while the other
    // workers are writing hard enough to hold one is a `database is
    // locked` on the very first line of setup. That is what killed every
    // multithreaded phase in the first run after the update phases
    // started doing real writes.
    conn.execute_batch("PRAGMA busy_timeout=60000;")
        .expect("busy timeout");
    conn.execute_batch(&format!("PRAGMA synchronous={synchronous};"))
        .expect("synchronous pragma");
    conn.execute_batch(
        "PRAGMA cache_size=-262144;
         PRAGMA mmap_size=268435456;
         PRAGMA temp_store=MEMORY;",
    )
    .expect("tuning pragmas");
    conn
}

/// Switches the database's journal mode, on a connection of its own so
/// that no other connection is open to refuse it.
fn set_journal(path: &Path, journal: &str) {
    let conn = Connection::open(path).expect("open sqlite");
    conn.execute_batch("PRAGMA busy_timeout=60000;")
        .expect("busy timeout");
    let got: String = conn
        .query_row(&format!("PRAGMA journal_mode={journal}"), [], |r| r.get(0))
        .expect("journal pragma");
    assert!(
        got.eq_ignore_ascii_case(journal),
        "sqlite refused journal_mode={journal} and stayed on {got}"
    );
}

/// Loads one sqlite database of the given table shape and measures it at
/// each durability setting.
fn run_sqlite(
    dir: &Path,
    rowid: bool,
    threads: usize,
    budget: Budget,
) -> (Phase, Vec<Run>, Storage) {
    let shape = if rowid { "rowid" } else { "norowid" };
    let path: PathBuf = dir.join(format!("sqlite-{shape}-{threads}.db"));
    let records = budget.records;
    {
        let conn = connect(&path, LOADING.synchronous);
        let suffix = if rowid { "" } else { " WITHOUT ROWID" };
        conn.execute_batch(&format!(
            "CREATE TABLE usertable (ykey TEXT PRIMARY KEY, yvalue BLOB NOT NULL){suffix};"
        ))
        .expect("create table");
    }
    set_journal(&path, LOADING.journal);

    let load = phase(threads, |t| {
        let conn = connect(&path, LOADING.synchronous);
        let mut buf = Vec::with_capacity(VALUE_BYTES);
        let mut lat = Lat::new();
        let mut done = 0u64;
        let mut i = t as u64;
        let mut stmt = conn
            .prepare("INSERT OR REPLACE INTO usertable (ykey, yvalue) VALUES (?1, ?2)")
            .expect("prepare insert");
        while i < records {
            value(i, 0, &mut buf);
            stmt.execute(rusqlite::params![key(i), buf])
                .expect("insert");
            lat.tick();
            done += 1;
            i += threads as u64;
        }
        Work {
            ops: done,
            samples: lat.samples,
        }
    });

    // The load ran at synchronous=OFF in WAL, so most of it is in the
    // write ahead log rather than the database. Both files are the
    // database, and `sqlite_bytes` counts both.
    let loaded = sqlite_bytes(&path);

    {
        let conn = connect(&path, LOADING.synchronous);
        let mut stmt = conn
            .prepare("SELECT yvalue FROM usertable WHERE ykey = ?1")
            .expect("prepare select");
        let mut rng = Rng(0x243F6A8885A308D3);
        for _ in 0..1000.min(records) {
            let i = rng.next() % records;
            let found: Vec<u8> = stmt.query_row([key(i)], |r| r.get(0)).expect("read");
            assert_eq!(found.len(), VALUE_BYTES, "key {i} is the wrong length");
            assert_eq!(&found[..19], &key(i).as_bytes()[4..], "key {i} crossed");
        }
    }

    let run = |setting: Setting, op: Op| {
        phase(threads, |t| {
            let conn = connect(&path, setting.synchronous);
            let mut read = conn
                .prepare("SELECT yvalue FROM usertable WHERE ykey = ?1")
                .expect("prepare select");
            let mut write = conn
                .prepare("UPDATE usertable SET yvalue = ?2 WHERE ykey = ?1")
                .expect("prepare update");
            let mut buf = Vec::with_capacity(VALUE_BYTES);
            let mut rng = Rng(seed(t));
            let mut lat = Lat::new();
            let started = Instant::now();
            let mut done = 0u64;
            loop {
                let i = rng.next() % records;
                if op.reads(&mut rng) {
                    // The length is taken from the borrowed blob rather
                    // than copying it out, which is the cheapest thing
                    // sqlite can be asked to do with a row.
                    let len: usize = read
                        .query_row([key(i)], |r| Ok(r.get_ref(0)?.as_blob()?.len()))
                        .expect("read");
                    assert_eq!(len, VALUE_BYTES, "short row");
                } else {
                    value(i, revision(t, done), &mut buf);
                    let n = write
                        .execute(rusqlite::params![key(i), buf])
                        .expect("update");
                    assert_eq!(n, 1, "update missed");
                }
                lat.tick();
                done += 1;
                if done.is_multiple_of(SAMPLE) && budget.over(&started) {
                    break;
                }
            }
            Work {
                ops: done,
                samples: lat.samples,
            }
        })
    };

    let mut runs = Vec::new();
    for setting in SETTINGS {
        set_journal(&path, setting.journal);
        runs.push(Run {
            label: format!("sqlite {} {shape}", setting.label()),
            mine: false,
            waits: setting.waits,
            read: run(setting, Op::Read),
            update: run(setting, Op::Update),
            mixed: run(setting, Op::Mixed),
        });
    }
    // Leave the database on WAL, so that dropping the directory does not
    // have to clean up a rollback journal. That switch also checkpoints,
    // which is why the storage figure is taken after it: it is sqlite
    // holding the same records with nothing outstanding, which is the
    // fairest thing to put next to a compacted zu2.
    set_journal(&path, "WAL");
    let storage = Storage {
        label: format!("sqlite {shape}"),
        loaded,
        after: Some(sqlite_bytes(&path)),
    };
    (load, runs, storage)
}

// ---------------------------------------------------------------- zu1

/// Loads the same records into a zu1 file and reports what they cost.
///
/// zu1 is columnar and loads in bulk, so this is not a throughput
/// comparison and is not printed as one. It is the storage baseline: the
/// engine zu2 replaces, holding the same keys and the same values, with
/// its own dictionary encoding and its own block overhead. The node table
/// carries both columns and the rel table has no edges, because YCSB has
/// no edges and an empty one costs a few blocks of schema.
///
/// The whole dataset is built in memory before the store, which is what
/// zu1's bulk path takes, so this is the one part of the benchmark that
/// wants the record count times the value size in RAM. `ZU2_ZU1=0` turns
/// it off for a machine that does not have it.
fn run_zu1(dir: &Path, records: u64) -> Option<Storage> {
    use zu_zu1::file::Zu1File;
    use zu_zu1::graph::bulk_load_as;
    use zu_zu1::props::{PropValues, store_props};

    let path = dir.join("zu1.zu");
    let mut db = Zu1File::create(&path).expect("create zu1");
    bulk_load_as(&mut db, "usertable", "links", records, &[]).expect("bulk load");
    let keys: Vec<Vec<u8>> = (0..records).map(|i| key(i).into_bytes()).collect();
    let mut buf = Vec::with_capacity(VALUE_BYTES);
    let values: Vec<Vec<u8>> = (0..records)
        .map(|i| {
            value(i, 0, &mut buf);
            buf.clone()
        })
        .collect();
    let key_refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
    let value_refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
    store_props(
        &mut db,
        "usertable",
        &[
            ("ykey", PropValues::Str(&key_refs)),
            ("yvalue", PropValues::Str(&value_refs)),
        ],
    )
    .expect("store props");
    drop(db);
    Some(Storage {
        label: "zu1".into(),
        loaded: file_bytes(&path),
        after: None,
    })
}

// ------------------------------------------------------------- report

fn line(run: &Run) {
    println!(
        "{:<26} {:>12.0} {:>8.2} {:>8.2} {:>12.0} {:>12.0}",
        run.label,
        run.read.rate(),
        run.read.p50,
        run.read.p99,
        run.update.rate(),
        run.mixed.rate()
    );
}

/// zu2's best against the best sqlite, among the configurations that
/// give the same answer to "does a commit wait for the device".
fn ratios(runs: &[Run], waits: Waits) {
    let best = |mine: bool, pick: fn(&Run) -> f64| {
        runs.iter()
            .filter(|r| r.waits == waits && r.mine == mine)
            .map(pick)
            .fold(0.0_f64, f64::max)
    };
    let ratio = |pick: fn(&Run) -> f64| {
        let rival = best(false, pick);
        if rival <= 0.0 {
            0.0
        } else {
            best(true, pick) / rival
        }
    };
    let what = match waits {
        Waits::No => "commit does not wait for the device",
        Waits::Device => "commit waits for the device",
    };
    println!(
        "  {:<38} read {:>6.1}x  update {:>6.1}x  mixed {:>6.1}x",
        what,
        ratio(|r| r.read.rate()),
        ratio(|r| r.update.rate()),
        ratio(|r| r.mixed.rate())
    );
}

/// The storage table. Every column is bytes the device is holding for the
/// same records, so the rows are comparable straight across.
fn storage_table(rows: &[Storage], records: u64) {
    // What the records are, before any engine has an opinion about them:
    // the key and the value, nothing else.
    let logical = records * (key(0).len() as u64 + VALUE_BYTES as u64);
    println!(
        "\nstorage, {:.1} MiB of keys and values, in bytes the device is holding",
        logical as f64 / (1 << 20) as f64
    );
    println!(
        "{:<26} {:>12} {:>10} {:>12} {:>10} {:>8}",
        "engine", "after load", "b/record", "after phases", "b/record", "over raw"
    );
    for row in rows {
        // zu1 has no per record update path, so there is nothing honest
        // to put in the second pair of columns for it, and its ratio is
        // against the load.
        let (after, per, standing) = match row.after {
            Some(after) => (
                mib(after),
                format!("{:.0}", Storage::per_record(after, records)),
                after,
            ),
            None => ("-".into(), "-".into(), row.loaded),
        };
        println!(
            "{:<26} {:>12} {:>10.0} {:>12} {:>10} {:>7.2}x",
            row.label,
            mib(row.loaded),
            Storage::per_record(row.loaded, records),
            after,
            per,
            standing as f64 / logical as f64
        );
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

fn main() {
    let budget = Budget {
        seconds: env("ZU2_BUDGET", 3.0_f64),
        records: env("ZU2_RECORDS", 250_000_u64),
    };
    // 4 MiB a page. The default keeps the whole load resident and leaves
    // room for the records the update phases append, and it is settable
    // because a machine with less memory than the load needs would
    // otherwise swap and measure the swap.
    let memory_pages: usize = env("ZU2_MEMORY_PAGES", 1024_usize);
    // The zu1 storage row builds the whole dataset in memory, which a
    // small machine may not have. Off is a missing row, not a wrong one.
    let zu1: bool = env("ZU2_ZU1", 1_u8) != 0;
    let threads: Vec<usize> = std::env::var("ZU2_THREADS")
        .unwrap_or_else(|_| "1,8".into())
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    println!(
        "ycsb: {} records of {} bytes, uniform keys, one transaction per operation, {:.1}s a measured phase",
        budget.records, VALUE_BYTES, budget.seconds
    );
    println!(
        "host: {} {}, zu2 resident cap {} MiB",
        std::env::consts::OS,
        std::env::consts::ARCH,
        memory_pages * 4
    );

    {
        let dir = tempfile::tempdir().expect("tempdir");
        let floor = device_floor(dir.path());
        if cfg!(target_os = "macos") {
            println!(
                "device: {floor:.0} durable writes/s, measured with F_FULLFSYNC, which is the\n        only macos call that reaches the drive. Neither engine uses it, so the\n        waiting rows below sit above this line and do not mean what they say"
            );
        } else {
            println!(
                "device: {floor:.0} durable writes/s on this filesystem, so a row that\n        waits for the device cannot honestly be faster than this"
            );
        }
    }

    for &t in &threads {
        let dir = tempfile::tempdir().expect("tempdir");
        println!("\n--- {t} thread{} ---", if t == 1 { "" } else { "s" });

        let (zu2_load, mut runs, mut storage) = run_zu2(dir.path(), t, budget, memory_pages);
        let (rowid_load, rowid_runs, rowid_storage) = run_sqlite(dir.path(), true, t, budget);
        let (norowid_load, norowid_runs, norowid_storage) =
            run_sqlite(dir.path(), false, t, budget);
        storage.push(rowid_storage);
        storage.push(norowid_storage);
        if zu1 {
            storage.extend(run_zu1(dir.path(), budget.records));
        }

        println!("\nload, one transaction per record, at each engine's fastest setting");
        for (what, load) in [
            ("zu2 async", &zu2_load),
            ("sqlite wal/off rowid", &rowid_load),
            ("sqlite wal/off norowid", &norowid_load),
        ] {
            println!(
                "  {:<24} {:>12.0} op/s   {:>7.1}s",
                what,
                load.rate(),
                load.seconds
            );
        }

        runs.extend(rowid_runs);
        runs.extend(norowid_runs);
        println!(
            "\n{:<26} {:>12} {:>8} {:>8} {:>12} {:>12}",
            "config", "read op/s", "p50 us", "p99 us", "update op/s", "mixed op/s"
        );
        for run in &runs {
            line(run);
        }

        println!("\nbest zu2 against best sqlite, at the same durability:");
        ratios(&runs, Waits::No);
        ratios(&runs, Waits::Device);
        if cfg!(target_os = "macos") {
            println!(
                "  the waiting row means nothing on macos: sqlite takes F_BARRIERFSYNC for\n  synchronous=FULL and zu2 takes fsync, and neither reaches the drive"
            );
        }
        storage_table(&storage, budget.records);
    }
}
