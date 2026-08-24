//! The P8 gate on the warm read hot path (perf/09 section 5 P8): how
//! many times a warm query asks the allocator for memory.
//!
//! Two zero-allocation gates already exist further down: warm block
//! pins in zu-zu1/tests/alloc.rs and the steady-state morsel loop in
//! zu-vector/tests/alloc.rs. Both hold at zero. Neither of them covers
//! the path a caller actually rides, which is text in and rows out
//! through a resident session, and that path is where an allocation
//! per row hides. A time can be argued with on a loaded box and a
//! count cannot: the numbers here are the same on any machine at any
//! load, which is why the gate is a count and not a latency.
//!
//! Three shapes, warm in every case, allocations divided by the runs
//! that caused them:
//!
//! hot_plan_hit_allocs is Session::warm on cached text, the lookup
//! that decides a statement is already compiled. Nothing about
//! recognising known text needs the heap, so this one is gated at
//! zero and is expected to stay there.
//! hot_point_allocs is the full warm point read: bind, key lookup,
//! one-hop count, one row out. It cannot be zero, because the answer
//! itself is heap and the caller owns it after the call returns.
//! What it can be is flat, and flat is what the gate holds it to.
//! hot_hop_allocs is the same read against a hub node with a wide
//! adjacency list, which is the shape that catches an allocation per
//! edge rather than per call. If the count matches the point read the
//! expand is not allocating per neighbour.
//!
//! The counter is off except during the counted runs, so nothing here
//! is measuring the warmup or the graph build.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench alloc

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

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

/// Calls handed out while the counter was on, bytes with them, and
/// whether it is on. A realloc counts as one call and as what it grew
/// by, so a vector that doubles its way to a size counts the size once
/// and not several multiples of it.
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            if new_size > layout.size() {
                BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
            }
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `body` `runs` times with the counter on and returns the calls
/// and the bytes, each per run.
fn counted(runs: u64, mut body: impl FnMut()) -> (f64, f64) {
    CALLS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    for _ in 0..runs {
        body();
    }
    COUNTING.store(false, Ordering::Relaxed);
    let calls = CALLS.load(Ordering::Relaxed) as f64 / runs as f64;
    let bytes = BYTES.load(Ordering::Relaxed) as f64 / runs as f64;
    (calls, bytes)
}

fn xorshift(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

const NODES: u32 = 10_000;
/// The hub is node 0 and every fourth node points at it, so the
/// backward walk from it is thousands of edges wide.
const HUB_DEGREE: u32 = NODES / 4;
const POINT_Q: &str = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";
const HUB_Q: &str = "MATCH (a:person {id: $src})<-[:follows]-(b) RETURN count(b) AS n";

/// Builds the same synthetic graph the session bench builds, plus the
/// hub edges, and returns out-degree by node.
fn build(path: &std::path::Path) -> Vec<i64> {
    let mut rng = 0x2064u64;
    let mut edges: Vec<(u32, u32)> = (0..NODES * 4)
        .map(|_| {
            let src = (xorshift(&mut rng) % u64::from(NODES)) as u32;
            let dst = (xorshift(&mut rng) % u64::from(NODES)) as u32;
            (src, dst)
        })
        .collect();
    edges.extend((0..HUB_DEGREE).map(|i| (i * 4 + 1, 0u32)));
    edges.sort_unstable();
    edges.dedup();
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "follows", u64::from(NODES), &edges).expect("load");
    let mut degree = vec![0i64; NODES as usize];
    for (src, _) in &edges {
        degree[*src as usize] += 1;
    }
    degree
}

fn count_of(r: &zu::query::QueryResult) -> i64 {
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one count, got {other:?}"),
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alloc.zu1");
    let degree = build(&path);
    let edge_count: i64 = degree.iter().sum();
    println!("alloc bench graph: {NODES} nodes, {edge_count} edges");

    let mut session = Session::open(&path).expect("open");
    assert!(!session.warm(POINT_Q).expect("compile"), "first sight");
    let (plan_hit, plan_hit_bytes) = counted(10_000, || {
        assert!(session.warm(POINT_Q).expect("warm"), "text stayed cached");
    });
    println!("plan cache hit: {plan_hit:.3} allocations, {plan_hit_bytes:.1} bytes a hit");

    let mut rng = 0xbeefu64;
    for _ in 0..100 {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        session
            .run(POINT_Q, &[("src", Value::Int(src))])
            .expect("warmup");
    }
    let (point, point_bytes) = counted(10_000, || {
        let src = (xorshift(&mut rng) % u64::from(NODES)) as i64;
        let r = session
            .run(POINT_Q, &[("src", Value::Int(src))])
            .expect("point");
        assert_eq!(count_of(&r), degree[src as usize], "src {src}");
    });
    println!("warm point read: {point:.3} allocations, {point_bytes:.1} bytes a read");

    // The hub read is checked against the edges built for it rather
    // than the degree table, which counts the forward direction.
    let hub = session.warm(HUB_Q).expect("compile hub");
    assert!(!hub, "first sight");
    for _ in 0..100 {
        session
            .run(HUB_Q, &[("src", Value::Int(0))])
            .expect("warmup");
    }
    let (hop, hop_bytes) = counted(1_000, || {
        let r = session
            .run(HUB_Q, &[("src", Value::Int(0))])
            .expect("hub read");
        assert!(
            count_of(&r) >= i64::from(HUB_DEGREE),
            "the hub read walks a wide list"
        );
    });
    println!(
        "warm hub read ({HUB_DEGREE} edges in): {hop:.3} allocations, {hop_bytes:.1} bytes a read"
    );
    println!(
        "the hub read allocates {over:+.3} times a read over the point read, \
         on {HUB_DEGREE} more edges walked",
        over = hop - point
    );

    let mut failed = false;
    let mut check = |what: &str, key: &str, got: f64| {
        if let Some(ceiling) = budget(key)
            && got > ceiling
        {
            println!("GATE FAIL {what}: {got:.3} allocations a run > ceiling {ceiling}");
            failed = true;
        }
    };
    check("plan cache hit", "hot_plan_hit_allocs", plan_hit);
    check("warm point read", "hot_point_allocs", point);
    check("warm hub read", "hot_hop_allocs", hop);
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
