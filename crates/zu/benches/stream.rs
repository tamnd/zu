//! What streaming a result costs and what it saves (`dx/04` §4).
//!
//! Streaming is not a way to read rows faster, it is a way to read them
//! without holding them, so two of the three numbers here are about
//! memory and stopping and only one is about time. The time one is the
//! obligation: handing rows to a callback in batches has to cost about
//! what collecting them costs, because a streaming API that is slower
//! per row is one callers avoid until they are already out of memory.
//!
//! Three numbers. stream_x gates the wall clock of a streamed scan
//! against the same scan buffered, over the same connection and the
//! same warm caches. stream_bytes_x gates what the two hold: the live
//! heap above where it stood when the statement started, at its highest
//! during the run, which for the buffered read is every row and for the
//! streamed read is one batch and the chunk feeding it. stream_stop_x
//! gates a caller that reads the first batch and stops, against reading
//! the whole thing, which is the case that turns a full scan into a
//! bounded one and should measure near zero.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench stream

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Instant;

use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Config, Connection, Database, Flow};

/// Live bytes above the last reset, and the highest that ever got.
///
/// A counting allocator is the only way to ask what a call was holding
/// rather than what the process has: peak resident set includes every
/// page the caches touched, which both sides of this bench touch
/// equally and which would drown the difference being measured.
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
/// Whether the counters are running. Two atomic read-modify-writes per
/// allocation is a real cost and the buffered read allocates once per
/// row, so a timed pass would be timing the counters as much as the
/// query. The measured passes are counted or timed, never both.
static COUNTING: AtomicBool = AtomicBool::new(false);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() && COUNTING.load(Ordering::Relaxed) {
            let live =
                LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed) + layout.size() as isize;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Starts the count over and turns it on. Memory allocated before this
/// and freed after it drives the live count negative, which is
/// harmless: the peak is growth above this point and that is the number
/// wanted.
fn count_from_here() {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
}

/// The highest the live heap got, and the counters off again.
fn peak() -> f64 {
    COUNTING.store(false, Ordering::Relaxed);
    PEAK.load(Ordering::Relaxed).max(0) as f64
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

const NODES: u32 = 200_000;
const SCAN: &str = "MATCH (p:person) RETURN p.id AS id";

fn build(path: &std::path::Path) {
    let mut edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    edges.sort_unstable();
    let mut file = Zu1File::create(path).expect("create");
    bulk_load_as(&mut file, "person", "knows", u64::from(NODES), &edges).expect("load");
}

/// One measured run: nanoseconds, and the sum the rows carried, which
/// is what proves the run read the answer and not a piece of it.
struct Run {
    ns: f64,
    sum: i64,
}

/// The baseline: the whole result in memory, read the ordinary way.
fn buffered(conn: &mut Connection) -> Run {
    let start = Instant::now();
    let result = conn.query(SCAN).expect("query");
    let mut sum = 0i64;
    for row in result.iter() {
        sum += row.get_at::<i64>(0).expect("an id");
    }
    let ns = start.elapsed().as_nanos() as f64;
    drop(result);
    Run { ns, sum }
}

/// The same rows through a sink, which is the same work minus the
/// holding.
fn streamed(conn: &mut Connection) -> Run {
    let mut sum = 0i64;
    let start = Instant::now();
    let out = conn
        .query_stream(SCAN, &[], |batch| {
            for row in batch.iter() {
                sum += row.get_at::<i64>(0).expect("an id");
            }
            Ok(Flow::More)
        })
        .expect("stream");
    let ns = start.elapsed().as_nanos() as f64;
    assert_eq!(out.rows, u64::from(NODES));
    assert!(out.streamed, "a plain scan must stream, not buffer");
    Run { ns, sum }
}

/// A caller that has seen enough after one batch. The rows it did not
/// read are rows nothing decoded.
fn stopped(conn: &mut Connection) -> Run {
    let mut sum = 0i64;
    let start = Instant::now();
    let out = conn
        .query_stream(SCAN, &[], |batch| {
            for row in batch.iter() {
                sum += row.get_at::<i64>(0).expect("an id");
            }
            Ok(Flow::Stop)
        })
        .expect("stream");
    let ns = start.elapsed().as_nanos() as f64;
    assert!(out.stopped);
    assert!(out.rows < u64::from(NODES) / 4, "{} rows read", out.rows);
    Run { ns, sum }
}

/// The cheapest of five timed passes and the bytes of a sixth counted
/// one. The minimum is the pass nothing else perturbed, and the counted
/// pass is separate because a counted pass is not a timed one.
fn best(f: impl Fn(&mut Connection) -> Run, conn: &mut Connection) -> (Run, f64) {
    let mut ns = f64::MAX;
    let mut sum = 0i64;
    for _ in 0..5 {
        let run = f(conn);
        ns = ns.min(run.ns);
        sum = run.sum;
    }
    count_from_here();
    let run = f(conn);
    let bytes = peak();
    assert_eq!(run.sum, sum, "the counted pass read the same rows");
    (Run { ns, sum }, bytes)
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stream.zu1");
    build(&path);
    let db = Database::open_with(&path, Config::new().threads(1)).expect("open");
    let mut conn = db.connect().expect("connect");
    // A warm run before the measured ones, so the block cache and the
    // plan cache are the same on both sides and the first pass is not
    // paying for the file.
    let _ = buffered(&mut conn);
    println!("stream bench: {NODES} rows of one column");

    let (whole, whole_bytes) = best(buffered, &mut conn);
    let (piece, piece_bytes) = best(streamed, &mut conn);
    let (early, _) = best(stopped, &mut conn);
    let want: i64 = (0..i64::from(NODES)).sum();
    assert_eq!(whole.sum, want, "the buffered read must agree");
    assert_eq!(piece.sum, want, "the streamed read must agree");

    let time_x = piece.ns / whole.ns.max(1.0);
    let bytes_x = piece_bytes / whole_bytes.max(1.0);
    let stop_x = early.ns / whole.ns.max(1.0);
    let ms = |ns: f64| ns / 1_000_000.0;
    let kib = |b: f64| b / 1024.0;

    println!(
        "buffered: {:.2} ms, {:.0} KiB held",
        ms(whole.ns),
        kib(whole_bytes)
    );
    println!(
        "streamed: {:.2} ms, {:.0} KiB held, {time_x:.3}x the time, {bytes_x:.4}x the bytes",
        ms(piece.ns),
        kib(piece_bytes)
    );
    println!(
        "stopped after one batch: {:.2} ms, {stop_x:.4}x the whole scan",
        ms(early.ns)
    );

    let mut failed = false;
    for (key, measured, what) in [
        ("stream_x", time_x, "streamed time"),
        ("stream_bytes_x", bytes_x, "streamed bytes"),
        ("stream_stop_x", stop_x, "stopping early"),
    ] {
        if let Some(ceiling) = budget(key)
            && measured > ceiling
        {
            println!("GATE FAIL {what}: {measured:.4}x > ceiling {ceiling}");
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
