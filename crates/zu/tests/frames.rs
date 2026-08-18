//! Registering a caller's own columns as a table, and reading them
//! where they lie.
//!
//! The unit tests in `zu_query::frame` check what a frame does to one
//! column. What is checked here is the thing a caller actually does: a
//! statement names a registered frame, the rows come back, the strings
//! come back, a filter over one runs, the frame is gone when it is
//! unregistered, and a statement that tries to write one is told why it
//! cannot. What is not checked here is that the read points at the
//! caller's bytes, because an address is not visible from out here;
//! `an_eight_byte_column_is_scanned_where_it_lies` asserts it where the
//! vector is built.

use std::any::Any;
use std::ptr::NonNull;
use std::sync::Arc;

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Column, Database, FloatBits, Frame, IntBits, Layout, LogicalType};

/// The arrays a test registers, kept alive exactly as a caller's would
/// be: the frame holds this and the columns point into it.
struct Held {
    ns: Vec<i64>,
    scores: Vec<f64>,
    ages: Vec<i32>,
    names: Vec<u8>,
    ends: Vec<i32>,
}

impl Held {
    /// Five rows of four columns: an eight-byte integer, a double, a
    /// four-byte integer that has to widen, and strings.
    fn five() -> Held {
        let names = ["ada", "grace", "alan", "edsger", "barbara"];
        let mut bytes = Vec::new();
        let mut ends = vec![0i32];
        for name in names {
            bytes.extend_from_slice(name.as_bytes());
            ends.push(bytes.len() as i32);
        }
        Held {
            ns: vec![10, 20, 30, 40, 50],
            scores: vec![1.5, 2.5, 3.5, 4.5, 5.5],
            ages: vec![36, 45, 41, 54, 92],
            names: bytes,
            ends,
        }
    }
}

fn ptr<T>(v: &[T]) -> NonNull<u8> {
    NonNull::new(v.as_ptr() as *mut u8).expect("a real pointer")
}

/// The four columns of [`Held::five`] as a frame under `name`.
///
/// # Safety
///
/// The pointers are taken out of the `Arc` that is handed over as the
/// owner, so they stay valid for as long as the frame does, which is
/// the contract [`Frame::new`] asks for.
fn five(name: &str) -> Frame {
    let held = Arc::new(Held::five());
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
        Column {
            name: "age".into(),
            ty: LogicalType::Int {
                signed: true,
                bits: IntBits::B32,
                precision: None,
            },
            layout: Layout::Int {
                ptr: ptr(&held.ages),
                bits: IntBits::B32,
                signed: true,
                scale: 1,
            },
        },
        Column {
            name: "name".into(),
            ty: LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
            layout: Layout::Str {
                offsets: ptr(&held.ends),
                wide: false,
                data: ptr(&held.names),
                data_len: held.names.len(),
            },
        },
    ];
    let owner: Arc<dyn Any + Send + Sync> = held;
    unsafe { Frame::new(name, 5, columns, owner) }.expect("a frame")
}

/// A database with a table in it, so that a frame is registered beside
/// stored data rather than into an empty catalog.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let edges: Vec<(u32, u32)> = (0..8).map(|i| (i, (i + 1) % 8)).collect();
    bulk_load_as(&mut db, "person", "knows", 8, &edges).expect("load");
}

fn open(dir: &std::path::Path) -> zu::Connection {
    let path = dir.join("frames.zu1");
    seeded(&path);
    Database::open(&path)
        .expect("open")
        .connect()
        .expect("connect")
}

#[test]
fn a_registered_frame_is_a_table_of_the_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    conn.register(five("people")).expect("register");
    assert_eq!(conn.registered(), ["people"]);

    let rows = conn
        .query("MATCH (p:people) RETURN p.n AS n, p.name AS name ORDER BY n")
        .expect("query");
    let read: Vec<(i64, String)> = rows
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int(n), Value::Str(s)) => (*n, s.clone()),
            other => panic!("unexpected row {other:?}"),
        })
        .collect();
    assert_eq!(
        read,
        [
            (10, "ada".to_string()),
            (20, "grace".to_string()),
            (30, "alan".to_string()),
            (40, "edsger".to_string()),
            (50, "barbara".to_string()),
        ]
    );

    // The stored table is still there and still reads, which is the
    // whole point of the ids counting down from the top.
    let stored = conn
        .query("MATCH (p:person) RETURN count(p) AS n")
        .expect("query");
    assert_eq!(stored.rows[0][0], Value::Int(8));
}

#[test]
fn a_filter_and_a_widening_column_read_through_a_frame() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    conn.register(five("people")).expect("register");

    let rows = conn
        .query("MATCH (p:people) WHERE p.age > 44 RETURN p.name AS name ORDER BY p.age")
        .expect("query");
    let read: Vec<String> = rows
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Str(s) => s.clone(),
            other => panic!("unexpected value {other:?}"),
        })
        .collect();
    assert_eq!(read, ["grace", "edsger", "barbara"]);

    let sum = conn
        .query("MATCH (p:people) RETURN sum(p.score) AS total")
        .expect("query");
    assert_eq!(sum.rows[0][0], Value::Float(17.5));
}

#[test]
fn a_frame_replaces_one_of_its_own_name_and_goes_when_it_is_unregistered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    conn.register(five("people")).expect("register");
    conn.register(five("others")).expect("register");
    assert_eq!(conn.registered(), ["others", "people"]);

    // The same name again is the same table with different rows behind
    // it, so a statement over it answers from the new arrays.
    conn.register(five("people")).expect("register");
    assert_eq!(conn.registered(), ["others", "people"]);
    let rows = conn
        .query("MATCH (p:people) RETURN count(p) AS n")
        .expect("query");
    assert_eq!(rows.rows[0][0], Value::Int(5));

    assert!(conn.unregister("people").expect("unregister"));
    assert!(!conn.unregister("people").expect("unregister"));
    assert_eq!(conn.registered(), ["others"]);
    // The name is not a table any more, so it answers whatever an
    // unknown label answers, which is what a name that was never
    // registered answers too.
    let gone = conn
        .query("MATCH (p:people) RETURN count(p) AS n")
        .expect("query");
    let never = conn
        .query("MATCH (p:nobody) RETURN count(p) AS n")
        .expect("query");
    assert_eq!(gone.rows, never.rows);
    assert_eq!(gone.rows[0][0], Value::Int(0));
}

#[test]
fn a_name_a_stored_table_holds_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    let err = conn.register(five("person")).expect_err("refused");
    assert!(
        err.to_string().contains("already a table"),
        "unexpected error {err}"
    );
    assert!(conn.registered().is_empty());
}

#[test]
fn a_statement_that_writes_a_frame_is_told_why_it_cannot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    conn.register(five("people")).expect("register");
    let err = conn
        .query("MATCH (p:people) WHERE p.n = 10 SET p.n = 11")
        .expect_err("refused");
    assert!(
        err.to_string().contains("registered frame"),
        "unexpected error {err}"
    );
}

#[test]
fn a_frame_longer_than_a_vector_reads_every_row_of_itself() {
    // Past 1024 rows a scan is more than one chunk, and the answer has
    // to be the whole frame rather than the first chunk of it.
    const ROWS: i64 = 5000;
    let ns: Vec<i64> = (0..ROWS).collect();
    let held = Arc::new(ns);
    let column = Column {
        name: "n".into(),
        ty: LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        },
        layout: Layout::Int {
            ptr: ptr(&held),
            bits: IntBits::B64,
            signed: true,
            scale: 1,
        },
    };
    let owner: Arc<dyn Any + Send + Sync> = held;
    let frame = unsafe { Frame::new("wide", ROWS as u64, vec![column], owner) }.expect("a frame");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut conn = open(dir.path());
    conn.register(frame).expect("register");
    let rows = conn
        .query("MATCH (p:wide) RETURN count(p) AS n, sum(p.n) AS total")
        .expect("query");
    assert_eq!(rows.rows[0][0], Value::Int(ROWS));
    assert_eq!(rows.rows[0][1], Value::Int(ROWS * (ROWS - 1) / 2));

    let filtered = conn
        .query("MATCH (p:wide) WHERE p.n >= 4990 RETURN count(p) AS n")
        .expect("query");
    assert_eq!(filtered.rows[0][0], Value::Int(10));
}

#[test]
fn a_frame_id_and_a_catalog_id_share_one_space() {
    // The two count from opposite ends of the same field, which only
    // works while they agree on where the ends are. If the catalog ever
    // widens its id, this is what says the frame side has to widen too.
    assert_eq!(
        zu_query::frame::TOP_TABLE_ID,
        zu::zu1::catalog::MAX_TABLE_ID
    );
}
