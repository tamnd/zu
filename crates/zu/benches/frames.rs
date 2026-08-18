//! What registering a frame costs, and what reading one costs.
//!
//! The claim the frame layer makes is that registering copies nothing.
//! That is not a claim about a constant, it is a claim about a slope,
//! so the bench measures it against the cheapest copy there is: a
//! `memcpy` of the same columns. Registering ten million rows has to
//! come out orders of magnitude under copying them, and it has to stay
//! flat as the row count grows, because what it walks is the columns
//! and not the rows.
//!
//! The read side is the other half. A statement over a frame goes
//! through the same vectorized executor a stored table goes through,
//! and the two eight-byte lanes are read where they lie, so a scan of a
//! frame should run at memory speed rather than at decode speed.
//!
//! Run: cargo bench -p zu --bench frames

use std::any::Any;
use std::hint::black_box;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Column, Database, FloatBits, Frame, IntBits, Layout, LogicalType};

const ROWS: usize = 10_000_000;

/// Two columns of `rows` rows: the ids and a score.
struct Held {
    ns: Vec<i64>,
    scores: Vec<f64>,
}

fn columns(rows: usize) -> Arc<Held> {
    Arc::new(Held {
        ns: (0..rows as i64).collect(),
        scores: (0..rows).map(|i| i as f64 * 0.5).collect(),
    })
}

fn ptr<T>(v: &[T]) -> NonNull<u8> {
    NonNull::new(v.as_ptr() as *mut u8).expect("a real pointer")
}

/// The two columns as a frame, pointing into `held`, which the frame
/// then holds.
fn frame(name: &str, held: Arc<Held>, rows: usize) -> Frame {
    let columns = vec![
        Column {
            name: "n".into(),
            ty: LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            },
            layout: Layout::Int {
                ptr: ptr(&held.ns),
                bits: IntBits::B64,
                signed: true,
                scale: 1,
            },
        },
        Column {
            name: "score".into(),
            ty: LogicalType::Float {
                bits: FloatBits::B64,
                precision: None,
            },
            layout: Layout::Float {
                ptr: ptr(&held.scores),
                bits: FloatBits::B64,
            },
        },
    ];
    let owner: Arc<dyn Any + Send + Sync> = held;
    // Safe by the contract: the pointers came out of the `Arc` that is
    // handed over as the owner, so the bytes outlive the frame.
    unsafe { Frame::new(name, rows as u64, columns, owner) }.expect("a frame")
}

fn ms(at: Instant) -> f64 {
    at.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("frames.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
    drop(db);
    let mut conn = Database::open(&path)
        .expect("open")
        .connect()
        .expect("connect");

    let bytes = ROWS * 16;
    println!("frame: {ROWS} rows, {} MiB of columns", bytes >> 20);

    // The baseline: what the same columns cost to copy once, which is
    // the floor any interop that copies has to pay per registration.
    let held = columns(ROWS);
    let at = Instant::now();
    let copied = (held.ns.clone(), held.scores.clone());
    let copy = ms(at);
    black_box(&copied);
    drop(copied);
    println!("copy: {copy:.2} ms, {:.1} GiB/s", gib(bytes, copy));

    // The registration, which walks the two columns and not the ten
    // million rows.
    let at = Instant::now();
    conn.register(frame("people", Arc::clone(&held), ROWS))
        .expect("register");
    let register = ms(at);
    println!(
        "register: {register:.3} ms, {:.0}x under the copy",
        copy / register.max(f64::MIN_POSITIVE)
    );

    // The same again at a hundredth of the rows. A registration that
    // copied would fall by a hundred; one that points stays where it is.
    let small = columns(ROWS / 100);
    let at = Instant::now();
    conn.register(frame("few", small, ROWS / 100))
        .expect("register");
    let hundredth = ms(at);
    println!("register 1/100th of the rows: {hundredth:.3} ms");

    // The read. Both columns are the lane they are read into, so the
    // scan is over the caller's own arrays.
    let sum = "MATCH (p:people) RETURN count(p) AS n, sum(p.score) AS total";
    let at = Instant::now();
    let rows = conn.query(sum).expect("query");
    let scan = ms(at);
    assert_eq!(rows.rows[0][0], Value::Int(ROWS as i64));
    println!(
        "scan: {scan:.2} ms, {:.1} M rows/s, {:.1} GiB/s",
        ROWS as f64 / scan / 1000.0,
        gib(bytes, scan)
    );

    let filter = "MATCH (p:people) WHERE p.n >= 9999990 RETURN count(p) AS n";
    let at = Instant::now();
    let rows = conn.query(filter).expect("query");
    let filtered = ms(at);
    assert_eq!(rows.rows[0][0], Value::Int(10));
    println!(
        "filtered scan: {filtered:.2} ms, {:.1} M rows/s",
        ROWS as f64 / filtered / 1000.0
    );

    // The gate is the slope and not the constant: registering has to
    // beat copying by a wide margin on any machine, because it does not
    // touch a row. Ten times is far under what it measures and far over
    // what a copy could reach.
    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        assert!(
            register * 10.0 < copy,
            "registering {register:.3} ms against a copy of {copy:.2} ms is not a zero copy"
        );
        println!("gate: registration does not copy");
    }
}

fn gib(bytes: usize, ms: f64) -> f64 {
    bytes as f64 / (ms / 1000.0) / (1024.0 * 1024.0 * 1024.0)
}
