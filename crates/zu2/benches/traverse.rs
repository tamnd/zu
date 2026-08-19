//! Graph traversal: zu2 against an indexed sqlite, in process, same
//! algorithm, same host, same graph.
//!
//! The claim this exists to test is the one from the design: a hop costs
//! one array load and a sequential read of a sorted slice, where an
//! indexed relational store pays a B-tree descent per hop and a row
//! decode per neighbour. The exponent is the same for both, so what is
//! being measured is the constant, and the only way to measure a constant
//! honestly is to run the identical algorithm over both.
//!
//! That is why the traversals here are generic over one trait with one
//! method. `expand`, `bfs` and `triangles` are written once, and zu2 and
//! sqlite differ in nothing but what happens inside `neighbours`. A
//! benchmark that let each engine use its own traversal would be
//! comparing query planners, which is a different question.
//!
//! sqlite gets the adjacency shape it should have: a `WITHOUT ROWID`
//! table keyed on `(src, dst)`, so the primary key is the table, a
//! neighbour scan is one descent and then a sequential leaf walk, and the
//! rows come back in dst order without a sorter. That is the same access
//! pattern zu2 has, which is the point: it is the fastest sqlite can be
//! at this, not a strawman. It also gets a 256 MiB page cache and mmap
//! window, so the graph is in memory for both engines.
//!
//! One row is not the same algorithm and says so: `sqlite cte` runs
//! the k hop and the BFS as a single recursive CTE, which is how a person
//! would actually write it and which lets sqlite keep the whole traversal
//! inside its own loop. It is there so the comparison is against sqlite's
//! best rather than against a client that talks to it badly.
//!
//! Answers are cross checked before anything is timed. Every engine
//! answers the same hundred seeds and the neighbour lists have to match
//! exactly, because a fast wrong traversal is easy to write.
//!
//! Run: cargo bench -p zu2 --bench traverse
//! Bigger: ZU2_VERTICES=1000000 ZU2_DEGREE=16 cargo bench -p zu2 --bench traverse
//! Threads: ZU2_THREADS=1,8 cargo bench -p zu2 --bench traverse

use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;
use zu2::{Db, Direction, Durability, Options};

/// Operations per latency sample, as in the ycsb bench: reading the clock
/// costs more than a degree probe does, so the clock is read once a batch.
const SAMPLE: u64 = 32;

/// The most vertices a k hop or a BFS is allowed to touch before it stops.
///
/// A BFS over a connected graph of a million vertices visits a million
/// vertices, which measures memory bandwidth rather than the per hop cost,
/// and a six hop from a hub reaches the whole graph. Both engines stop at
/// the same number, so the comparison holds and a probe stays a probe.
const VISIT_CAP: usize = 20_000;

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

fn seed(t: usize) -> u64 {
    0x9E3779B97F4A7C15 ^ (t as u64 + 1).wrapping_mul(0x100000001B3)
}

fn key(v: u32) -> String {
    format!("v{v:09}")
}

// ------------------------------------------------------------ the graph

/// The edge list every engine loads.
///
/// Power law by construction rather than by name: one target in sixteen
/// goes to the top thousandth of the ids, which gives a few thousand hubs
/// with degrees in the thousands and leaves everything else near the
/// average. That shape is what makes the neighbourhood structure earn its
/// two forms, and it is what a uniform random graph would hide.
fn build_edges(vertices: u32, degree: u32) -> Vec<(u32, u32)> {
    let mut rng = Rng(0x2545F4914F6CDD1D);
    let hubs = (vertices / 1000).max(1);
    let mut edges = Vec::with_capacity(vertices as usize * degree as usize);
    for src in 0..vertices {
        for _ in 0..degree {
            let r = rng.next();
            let dst = if r.is_multiple_of(16) {
                (r >> 8) as u32 % hubs
            } else {
                (r >> 8) as u32 % vertices
            };
            edges.push((src, dst));
        }
    }
    edges
}

// -------------------------------------------------------- the traversals

/// What a traversal needs from an engine, and nothing else.
trait Adjacency {
    /// Hands the out neighbours of `vertex` to `visit`, sorted.
    fn neighbours(&mut self, vertex: u32, visit: &mut dyn FnMut(&[u32]));
    /// The out degree, which every engine can answer without materialising
    /// the neighbours.
    fn degree(&mut self, vertex: u32) -> u64;
}

/// The scratch a multi hop probe reuses, so the number printed is the
/// traversal and not the allocator.
struct Scratch {
    seen: Vec<u64>,
    frontier: Vec<u32>,
    next: Vec<u32>,
    left: Vec<u32>,
    right: Vec<u32>,
}

impl Scratch {
    fn new(vertices: u32) -> Self {
        Self {
            seen: vec![0; (vertices as usize).div_ceil(64).max(1)],
            frontier: Vec::new(),
            next: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    #[inline]
    fn mark(&mut self, vertex: u32) -> bool {
        let word = vertex as usize / 64;
        let bit = 1u64 << (vertex % 64);
        if self.seen[word] & bit != 0 {
            return false;
        }
        self.seen[word] |= bit;
        true
    }

    /// Clears only the bits a probe set, so the cost of resetting is the
    /// size of the answer rather than the size of the graph.
    fn clear(&mut self, touched: &[u32]) {
        for &v in touched {
            self.seen[v as usize / 64] &= !(1u64 << (v % 64));
        }
    }
}

/// Expands `hops` levels from `seed`, counting distinct vertices reached.
fn expand<A: Adjacency>(a: &mut A, seed: u32, hops: u32, s: &mut Scratch) -> u64 {
    let mut touched = Vec::new();
    s.frontier.clear();
    s.frontier.push(seed);
    s.mark(seed);
    touched.push(seed);
    let mut reached = 0u64;
    for _ in 0..hops {
        s.next.clear();
        let mut frontier = std::mem::take(&mut s.frontier);
        for &v in &frontier {
            let mut collect = |slice: &[u32]| {
                for &n in slice {
                    let word = n as usize / 64;
                    let bit = 1u64 << (n % 64);
                    if s.seen[word] & bit == 0 {
                        s.seen[word] |= bit;
                        s.next.push(n);
                    }
                }
            };
            a.neighbours(v, &mut collect);
            if s.next.len() >= VISIT_CAP {
                break;
            }
        }
        touched.extend_from_slice(&s.next);
        reached += s.next.len() as u64;
        frontier.clear();
        frontier.extend_from_slice(&s.next);
        s.frontier = frontier;
        if s.frontier.is_empty() {
            break;
        }
    }
    s.clear(&touched);
    reached
}

/// A breadth first search from `seed`, stopped at [`VISIT_CAP`].
fn bfs<A: Adjacency>(a: &mut A, seed: u32, s: &mut Scratch) -> u64 {
    let mut touched = Vec::new();
    s.frontier.clear();
    s.frontier.push(seed);
    s.mark(seed);
    touched.push(seed);
    let mut visited = 1u64;
    while !s.frontier.is_empty() && visited < VISIT_CAP as u64 {
        s.next.clear();
        let mut frontier = std::mem::take(&mut s.frontier);
        for &v in &frontier {
            let mut collect = |slice: &[u32]| {
                for &n in slice {
                    let word = n as usize / 64;
                    let bit = 1u64 << (n % 64);
                    if s.seen[word] & bit == 0 {
                        s.seen[word] |= bit;
                        s.next.push(n);
                    }
                }
            };
            a.neighbours(v, &mut collect);
            if visited + s.next.len() as u64 >= VISIT_CAP as u64 {
                break;
            }
        }
        visited += s.next.len() as u64;
        touched.extend_from_slice(&s.next);
        frontier.clear();
        frontier.extend_from_slice(&s.next);
        s.frontier = frontier;
    }
    s.clear(&touched);
    visited
}

/// Triangles through `seed`: for each neighbour, the size of the sorted
/// intersection of the two neighbourhoods. This is the workload the
/// neighbour lists are kept sorted for.
fn triangles<A: Adjacency>(a: &mut A, seed: u32, s: &mut Scratch) -> u64 {
    let mut left = std::mem::take(&mut s.left);
    left.clear();
    a.neighbours(seed, &mut |slice| left.extend_from_slice(slice));
    let mut found = 0u64;
    let mut right = std::mem::take(&mut s.right);
    // A bound on the fan out, so one hub does not turn a probe into a
    // phase. Both engines take the same bound.
    for &n in left.iter().take(32) {
        right.clear();
        a.neighbours(n, &mut |slice| right.extend_from_slice(slice));
        let (mut i, mut j) = (0, 0);
        while i < left.len() && j < right.len() {
            match left[i].cmp(&right[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    found += 1;
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    s.left = left;
    s.right = right;
    found
}

// ---------------------------------------------------------------- zu2

struct Zu2Adj<'a> {
    session: zu2::Session<'a>,
}

impl Adjacency for Zu2Adj<'_> {
    fn neighbours(&mut self, vertex: u32, visit: &mut dyn FnMut(&[u32])) {
        self.session.neighbours(Direction::Out, vertex, |slice| {
            visit(slice);
        });
    }

    fn degree(&mut self, vertex: u32) -> u64 {
        u64::from(self.session.degree(Direction::Out, vertex))
    }
}

fn zu2_options(vertices: u32) -> Options {
    Options {
        durability: Durability::Async,
        // One record per vertex and none per edge, so the index is sized
        // for the vertex keys alone.
        index_buckets: (vertices as usize / 4).next_power_of_two().max(1 << 12),
        max_pages: 1 << 17,
        // Log pages held in memory, four megabytes each. The default
        // never evicts, which is what a laptop with room wants and what
        // a small server does not: the adjacency lives in memory either
        // way, so this only decides how much of the edge log stays with
        // it.
        memory_pages: env("ZU2_MEMORY_PAGES", usize::MAX),
        max_vertices: vertices as usize + 1,
        sessions: 256,
        // Every edge record the load writes is live, so there is nothing
        // for a compactor to take and leaving it on would only spend a
        // scan proving that.
        compact_below: 0,
        ..Options::default()
    }
}

fn load_zu2(path: &Path, vertices: u32, edges: &[(u32, u32)]) -> Db {
    let db = Db::create(path, zu2_options(vertices)).expect("create");
    {
        let mut s = db.session();
        for v in 0..vertices {
            let id = s.add_vertex(key(v).as_bytes()).expect("vertex");
            assert_eq!(id, v, "ids are handed out in creation order");
        }
        for &(src, dst) in edges {
            s.add_edge(src, dst).expect("edge");
        }
    }
    db.sync().expect("sync");
    db
}

// ------------------------------------------------------------- sqlite

/// The adjacency table shape, and why it is this one.
///
/// `WITHOUT ROWID` with the primary key on `(src, dst)` makes the primary
/// key the table: the neighbours of a source are one descent and then a
/// leaf walk in dst order, with no second lookup to fetch a row and no
/// sorter to put the neighbours in order. It also makes the edge set a
/// set, the way zu2's adjacency is, so a repeated edge is one edge in
/// both engines.
const SCHEMA: &str = "CREATE TABLE edges (src INTEGER NOT NULL, dst INTEGER NOT NULL,
                      PRIMARY KEY (src, dst)) WITHOUT ROWID;";

fn connect(path: &Path, synchronous: &str) -> Connection {
    let conn = Connection::open(path).expect("open sqlite");
    // Busy timeout before anything else. A pragma is a statement and a
    // statement takes a lock, so opening a connection while the other
    // workers are writing hard enough to hold one is a `database is
    // locked` on the very first line of setup rather than a wait.
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

struct SqlAdj {
    conn: Connection,
    buf: Vec<u32>,
}

impl SqlAdj {
    fn new(path: &Path) -> Self {
        Self {
            conn: connect(path, "NORMAL"),
            buf: Vec::new(),
        }
    }
}

impl Adjacency for SqlAdj {
    fn neighbours(&mut self, vertex: u32, visit: &mut dyn FnMut(&[u32])) {
        // Taken out and put back so the statement cache can borrow the
        // connection while the buffer is being filled.
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        {
            let mut stmt = self
                .conn
                // No ORDER BY: the primary key index is already in dst
                // order for a fixed src, and asking for the order would
                // only give sqlite a sorter to prove it does not need.
                .prepare_cached("SELECT dst FROM edges WHERE src = ?1")
                .expect("prepare neighbours");
            let mut rows = stmt.query([vertex]).expect("query");
            while let Some(row) = rows.next().expect("row") {
                buf.push(row.get_unwrap::<_, i64>(0) as u32);
            }
        }
        visit(&buf);
        self.buf = buf;
    }

    fn degree(&mut self, vertex: u32) -> u64 {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT count(*) FROM edges WHERE src = ?1")
            .expect("prepare degree");
        stmt.query_row([vertex], |r| r.get::<_, i64>(0))
            .expect("degree") as u64
    }
}

fn load_sqlite(path: &Path, edges: &[(u32, u32)]) {
    let conn = connect(path, "OFF");
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .expect("journal");
    conn.execute_batch(SCHEMA).expect("schema");
    // Batched, because the load is not what this benchmark measures and a
    // transaction per edge would spend an hour proving something the ycsb
    // bench already prints. The edge insert phase below is per statement,
    // which is where the write comparison happens.
    conn.execute_batch("BEGIN;").expect("begin");
    {
        let mut stmt = conn
            .prepare("INSERT OR IGNORE INTO edges (src, dst) VALUES (?1, ?2)")
            .expect("prepare insert");
        for (n, &(src, dst)) in edges.iter().enumerate() {
            stmt.execute(rusqlite::params![src, dst]).expect("insert");
            if n % 200_000 == 199_999 {
                drop(stmt);
                conn.execute_batch("COMMIT; BEGIN;").expect("chunk");
                stmt = conn
                    .prepare("INSERT OR IGNORE INTO edges (src, dst) VALUES (?1, ?2)")
                    .expect("prepare insert");
            }
        }
    }
    conn.execute_batch("COMMIT;").expect("commit");
    conn.execute_batch("ANALYZE;").expect("analyze");
}

/// sqlite doing the traversal itself, in one statement.
///
/// This is not the same algorithm as the rows above and is not meant to
/// be. It is what a person would write, and it keeps the whole walk
/// inside sqlite's loop with no round trip per hop, which is the fairest
/// thing to put next to an in process engine.
fn recursive_hops(conn: &Connection, seed: u32, hops: u32) -> u64 {
    let sql = format!(
        "WITH RECURSIVE reach(v, d) AS (
             SELECT ?1, 0
             UNION
             SELECT e.dst, r.d + 1 FROM edges e JOIN reach r ON e.src = r.v
             WHERE r.d < {hops}
         )
         SELECT count(*) FROM (SELECT v FROM reach LIMIT {VISIT_CAP})"
    );
    let mut stmt = conn.prepare_cached(&sql).expect("prepare recursive");
    stmt.query_row([seed], |r| r.get::<_, i64>(0))
        .expect("recursive") as u64
}

// -------------------------------------------------------------- timing

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

struct Work {
    ops: u64,
    visited: u64,
    samples: Vec<f64>,
}

struct Phase {
    ops: u64,
    visited: u64,
    seconds: f64,
    p50: f64,
    p99: f64,
}

impl Phase {
    fn pool(work: Vec<Work>, seconds: f64) -> Self {
        let ops = work.iter().map(|w| w.ops).sum();
        let visited = work.iter().map(|w| w.visited).sum();
        let mut all: Vec<f64> = work.into_iter().flat_map(|w| w.samples).collect();
        all.sort_unstable_by(f64::total_cmp);
        let at = |q: f64| match all.len() {
            0 => 0.0,
            n => all[((n as f64 - 1.0) * q).round() as usize] * 1e6,
        };
        Self {
            ops,
            visited,
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

    fn visits(&self) -> f64 {
        if self.seconds <= 0.0 {
            0.0
        } else {
            self.visited as f64 / self.seconds
        }
    }
}

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

// ------------------------------------------------------------ workloads

#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    Degree,
    Hop(u32),
    Bfs,
    Triangles,
}

impl Probe {
    fn label(self) -> String {
        match self {
            Probe::Degree => "degree".into(),
            Probe::Hop(k) => format!("{k} hop"),
            Probe::Bfs => "bfs".into(),
            Probe::Triangles => "triangles".into(),
        }
    }

    fn run<A: Adjacency>(self, a: &mut A, seed: u32, s: &mut Scratch) -> u64 {
        match self {
            Probe::Degree => a.degree(seed),
            Probe::Hop(k) => expand(a, seed, k, s),
            Probe::Bfs => bfs(a, seed, s),
            Probe::Triangles => triangles(a, seed, s),
        }
    }
}

/// One measured row.
struct Row {
    probe: String,
    engine: &'static str,
    phase: Phase,
}

fn measure<A: Adjacency>(
    probe: Probe,
    engine: &'static str,
    threads: usize,
    vertices: u32,
    seconds: f64,
    make: impl Fn(usize) -> A + Sync,
) -> Row {
    let phase = phase(threads, |t| {
        let mut a = make(t);
        let mut s = Scratch::new(vertices);
        let mut rng = Rng(seed(t));
        let mut lat = Lat::new();
        let started = Instant::now();
        let (mut ops, mut visited) = (0u64, 0u64);
        loop {
            let v = (rng.next() % u64::from(vertices)) as u32;
            visited += probe.run(&mut a, v, &mut s);
            lat.tick();
            ops += 1;
            if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
                break;
            }
        }
        Work {
            ops,
            visited,
            samples: lat.samples,
        }
    });
    Row {
        probe: probe.label(),
        engine,
        phase,
    }
}

// --------------------------------------------------------------- report

fn line(row: &Row) {
    println!(
        "{:<12} {:<16} {:>12.0} {:>9.2} {:>9.2} {:>14.0}",
        row.probe,
        row.engine,
        row.phase.rate(),
        row.phase.p50,
        row.phase.p99,
        row.phase.visits()
    );
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

fn main() {
    let vertices: u32 = env("ZU2_VERTICES", 200_000_u32);
    let degree: u32 = env("ZU2_DEGREE", 16_u32);
    let seconds: f64 = env("ZU2_BUDGET", 2.0_f64);
    let threads: Vec<usize> = std::env::var("ZU2_THREADS")
        .unwrap_or_else(|_| "1,8".into())
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();

    let edges = build_edges(vertices, degree);
    println!(
        "traverse: {} vertices, {} edges before dedup, average out degree {}, {:.1}s a probe phase",
        vertices,
        edges.len(),
        degree,
        seconds
    );
    println!(
        "host: {} {}, visit cap {} per probe",
        std::env::consts::OS,
        std::env::consts::ARCH,
        VISIT_CAP
    );

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
    let zu2_path = dir.path().join("graph.zu2");
    let sql_path = dir.path().join("graph.sqlite");
    let started = Instant::now();
    let db = load_zu2(&zu2_path, vertices, &edges);
    let zu2_load = started.elapsed().as_secs_f64();
    let started = Instant::now();
    load_sqlite(&sql_path, &edges);
    let sql_load = started.elapsed().as_secs_f64();
    println!(
        "\nload: zu2 {:.1}s one edge per transaction, sqlite {:.1}s batched, zu2 file {:.1} MiB, sqlite file {:.1} MiB",
        zu2_load,
        sql_load,
        db.disk_bytes().expect("disk bytes") as f64 / (1 << 20) as f64,
        std::fs::metadata(&sql_path).map(|m| m.len()).unwrap_or(0) as f64 / (1 << 20) as f64
    );

    // Cross check before anything is timed: the two engines have to hold
    // the same graph, or the rest of this file is measuring nothing.
    {
        let mut zu2 = Zu2Adj {
            session: db.session(),
        };
        let mut sql = SqlAdj::new(&sql_path);
        let mut rng = Rng(0x243F6A8885A308D3);
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        for _ in 0..100 {
            let v = (rng.next() % u64::from(vertices)) as u32;
            mine.clear();
            theirs.clear();
            zu2.neighbours(v, &mut |s| mine.extend_from_slice(s));
            sql.neighbours(v, &mut |s| theirs.extend_from_slice(s));
            assert_eq!(mine, theirs, "the two engines disagree about vertex {v}");
            assert_eq!(
                zu2.degree(v),
                sql.degree(v),
                "the two engines disagree about the degree of {v}"
            );
        }
    }

    for &t in &threads {
        println!("\n--- {t} thread{} ---", if t == 1 { "" } else { "s" });
        println!(
            "{:<12} {:<16} {:>12} {:>9} {:>9} {:>14}",
            "probe", "engine", "probes/s", "p50 us", "p99 us", "visited/s"
        );
        for probe in [
            Probe::Degree,
            Probe::Hop(1),
            Probe::Hop(2),
            Probe::Hop(4),
            Probe::Hop(6),
            Probe::Bfs,
            Probe::Triangles,
        ] {
            let mine = measure(probe, "zu2", t, vertices, seconds, |_| Zu2Adj {
                session: db.session(),
            });
            let theirs = measure(probe, "sqlite", t, vertices, seconds, |_| {
                SqlAdj::new(&sql_path)
            });
            line(&mine);
            line(&theirs);
            let ratio = if theirs.phase.rate() > 0.0 {
                mine.phase.rate() / theirs.phase.rate()
            } else {
                0.0
            };
            println!("{:<12} {:<16} {:>12.1}x", "", "zu2 over sqlite", ratio);
        }

        // sqlite's own traversal, for the multi hop probes where a
        // recursive CTE is what a person would write.
        for hops in [2u32, 4, 6] {
            let row = phase(t, |worker| {
                let conn = connect(&sql_path, "NORMAL");
                let mut rng = Rng(seed(worker));
                let mut lat = Lat::new();
                let started = Instant::now();
                let (mut ops, mut visited) = (0u64, 0u64);
                loop {
                    let v = (rng.next() % u64::from(vertices)) as u32;
                    visited += recursive_hops(&conn, v, hops);
                    lat.tick();
                    ops += 1;
                    if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
                        break;
                    }
                }
                Work {
                    ops,
                    visited,
                    samples: lat.samples,
                }
            });
            line(&Row {
                probe: format!("{hops} hop"),
                engine: "sqlite cte",
                phase: row,
            });
        }
    }

    // The write phases run last and they run once, because they change
    // the graph. Everything above this point is two engines answering
    // for the same edges; below it each engine has a few hundred
    // thousand random edges of its own, which is fine for measuring what
    // an insert costs and would not be fine for measuring a traversal.
    println!("\nedge writes, one edge per transaction, at paired durability");
    println!(
        "{:<12} {:<16} {:>12} {:>9} {:>9} {:>14}",
        "phase", "engine", "edges/s", "p50 us", "p99 us", "visited/s"
    );
    let t = *threads.last().expect("a thread count");
    for (durability, synchronous, waits) in [
        (Durability::Async, "NORMAL", "commit does not wait"),
        (Durability::Durable, "FULL", "commit waits"),
    ] {
        // Whatever a previous phase left unflushed is that phase's bill,
        // not this one's. Without this the first waiting commit pays to
        // write and sync the whole backlog the async phase built, and
        // one operation carrying that is not what a waiting workload
        // costs. The same note is in ycsb.rs.
        db.sync().expect("sync");
        let mine = phase(t, |worker| {
            insert_zu2(&db, vertices, durability, worker, seconds)
        });
        let theirs = phase(t, |worker| {
            insert_sqlite(&sql_path, vertices, synchronous, worker, seconds)
        });
        line(&Row {
            probe: "insert".into(),
            engine: "zu2",
            phase: mine,
        });
        line(&Row {
            probe: "insert".into(),
            engine: "sqlite",
            phase: theirs,
        });
        println!("{:<12} {:<16} {}", "", "", waits);
    }

    // Ninety percent traversal, ten percent write, concurrent, which is
    // the shape a graph application actually has and the one where an
    // adjacency that locks per vertex has to prove it does not stall the
    // readers.
    let mine = phase(t, |worker| mixed_zu2(&db, vertices, worker, seconds));
    let theirs = phase(t, |worker| {
        mixed_sqlite(&sql_path, vertices, worker, seconds)
    });
    println!(
        "\nmixed, 90 percent one hop and 10 percent edge insert, {t} threads, commit does not wait"
    );
    println!(
        "{:<12} {:<16} {:>12} {:>9} {:>9} {:>14}",
        "phase", "engine", "ops/s", "p50 us", "p99 us", "visited/s"
    );
    line(&Row {
        probe: "mixed".into(),
        engine: "zu2",
        phase: mine,
    });
    line(&Row {
        probe: "mixed".into(),
        engine: "sqlite",
        phase: theirs,
    });
    if cfg!(target_os = "macos") {
        println!(
            "  the waiting row means nothing on macos: sqlite takes F_BARRIERFSYNC for\n  synchronous=FULL and zu2 takes fsync, and neither reaches the drive"
        );
    }
}

/// A random edge between existing vertices. Both engines are handed the
/// same generator, seeded per worker, so neither is writing an easier
/// edge than the other.
#[inline]
fn random_edge(rng: &mut Rng, vertices: u32) -> (u32, u32) {
    let a = (rng.next() % u64::from(vertices)) as u32;
    let b = (rng.next() % u64::from(vertices)) as u32;
    (a, b)
}

fn insert_zu2(db: &Db, vertices: u32, durability: Durability, worker: usize, seconds: f64) -> Work {
    let mut s = db.session();
    s.set_durability(durability);
    let mut rng = Rng(seed(worker));
    let mut lat = Lat::new();
    let started = Instant::now();
    let mut ops = 0u64;
    loop {
        let (src, dst) = random_edge(&mut rng, vertices);
        s.add_edge(src, dst).expect("add edge");
        lat.tick();
        ops += 1;
        if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    Work {
        ops,
        visited: 0,
        samples: lat.samples,
    }
}

fn insert_sqlite(
    path: &Path,
    vertices: u32,
    synchronous: &str,
    worker: usize,
    seconds: f64,
) -> Work {
    let conn = connect(path, synchronous);
    let mut stmt = conn
        .prepare("INSERT OR IGNORE INTO edges (src, dst) VALUES (?1, ?2)")
        .expect("prepare insert");
    let mut rng = Rng(seed(worker));
    let mut lat = Lat::new();
    let started = Instant::now();
    let mut ops = 0u64;
    loop {
        let (src, dst) = random_edge(&mut rng, vertices);
        stmt.execute(rusqlite::params![src, dst]).expect("insert");
        lat.tick();
        ops += 1;
        if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    Work {
        ops,
        visited: 0,
        samples: lat.samples,
    }
}

fn mixed_zu2(db: &Db, vertices: u32, worker: usize, seconds: f64) -> Work {
    let mut a = Zu2Adj {
        session: db.session(),
    };
    a.session.set_durability(Durability::Async);
    let mut s = Scratch::new(vertices);
    let mut rng = Rng(seed(worker));
    let mut lat = Lat::new();
    let started = Instant::now();
    let (mut ops, mut visited) = (0u64, 0u64);
    loop {
        let v = (rng.next() % u64::from(vertices)) as u32;
        if rng.next().is_multiple_of(10) {
            let (src, dst) = random_edge(&mut rng, vertices);
            a.session.add_edge(src, dst).expect("add edge");
        } else {
            visited += expand(&mut a, v, 1, &mut s);
        }
        lat.tick();
        ops += 1;
        if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    Work {
        ops,
        visited,
        samples: lat.samples,
    }
}

fn mixed_sqlite(path: &Path, vertices: u32, worker: usize, seconds: f64) -> Work {
    let mut a = SqlAdj::new(path);
    let mut s = Scratch::new(vertices);
    let mut rng = Rng(seed(worker));
    let mut lat = Lat::new();
    let started = Instant::now();
    let (mut ops, mut visited) = (0u64, 0u64);
    loop {
        let v = (rng.next() % u64::from(vertices)) as u32;
        if rng.next().is_multiple_of(10) {
            let (src, dst) = random_edge(&mut rng, vertices);
            let mut stmt = a
                .conn
                .prepare_cached("INSERT OR IGNORE INTO edges (src, dst) VALUES (?1, ?2)")
                .expect("prepare insert");
            stmt.execute(rusqlite::params![src, dst]).expect("insert");
        } else {
            visited += expand(&mut a, v, 1, &mut s);
        }
        lat.tick();
        ops += 1;
        if ops.is_multiple_of(SAMPLE) && started.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    Work {
        ops,
        visited,
        samples: lat.samples,
    }
}
