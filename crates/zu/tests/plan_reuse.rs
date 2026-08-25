//! A statement sent twice runs the plan it compiled the first time.
//!
//! The pipeline executor keeps the physical plan a statement compiled
//! to and writes the next set of parameters into it rather than
//! compiling again, which is a fifth of the executor's time and near
//! half of its allocations on a warm point read. What is checked here
//! is the part of that which can go wrong: the second run has to
//! answer what a first run of the same statement would have answered,
//! whatever the parameters are, and whatever happened to the store in
//! between.
//!
//! Every test runs the same text more than once on one connection,
//! because a plan is only reused on the second run and a connection is
//! what holds it. The oracle is a connection that has not seen the
//! text before, which compiles it from nothing.

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Connection, Database};

const NODES: u32 = 200;

fn opened(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    // Degrees vary, or a plan left holding the last key would answer
    // the same count as the key it was asked about and no assertion
    // here could tell the two apart.
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| {
            let mut out = vec![(i, (i + 1) % NODES)];
            if i % 3 == 0 {
                out.push((i, (i + 3) % NODES));
            }
            if i % 7 == 0 {
                out.push((i, (i + 5) % NODES));
            }
            out
        })
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    drop(db);
    let db = Database::open(&path).expect("open");
    (dir, db)
}

/// The rows of `source` under `args`, as text, so that two runs can be
/// compared without caring what shape the values are.
fn rows(conn: &mut Connection, source: &str, args: &[(&str, Value)]) -> Vec<String> {
    conn.query_with(source, args)
        .expect("run")
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

const POINT: &str = "MATCH (p:person {id: $id})-[:knows]->(f) RETURN p.id AS pid, count(f) AS n";

#[test]
fn the_same_point_read_on_a_new_key_answers_that_key() {
    let (_dir, db) = opened("keys.zu1");
    let mut conn = db.connect().expect("connect");
    // The key a seek folds into is the one thing a point read carries
    // from run to run, so a plan reused with the old key in it would
    // answer the same row every time and this is what would catch it.
    for id in [7i64, 3, 199, 0, 42, 7] {
        let got = rows(&mut conn, POINT, &[("id", Value::Int(id))]);
        let mut fresh = db.connect().expect("connect");
        assert_eq!(
            got,
            rows(&mut fresh, POINT, &[("id", Value::Int(id))]),
            "id {id}"
        );
    }
}

#[test]
fn a_key_that_is_not_an_integer_is_refused_the_same_way_as_the_first_time() {
    let (_dir, db) = opened("types.zu1");
    let mut conn = db.connect().expect("connect");
    // A held plan says where a value went, not that any value goes
    // there: a key of another type compiles to no seek at all, so the
    // run behind an integer one has to notice and compile rather than
    // stamp a string into a slot that holds a row id.
    rows(&mut conn, POINT, &[("id", Value::Int(11))]);
    let got = rows(&mut conn, POINT, &[("id", Value::Str("11".into()))]);
    let mut fresh = db.connect().expect("connect");
    assert_eq!(
        got,
        rows(&mut fresh, POINT, &[("id", Value::Str("11".into()))])
    );
    // And the integer after it is still answered as an integer.
    let after = rows(&mut conn, POINT, &[("id", Value::Int(11))]);
    assert_eq!(after, rows(&mut fresh, POINT, &[("id", Value::Int(11))]));
}

#[test]
fn a_write_between_two_reads_is_seen_by_the_second() {
    let (_dir, db) = opened("write.zu1");
    let mut conn = db.connect().expect("connect");
    let before = rows(&mut conn, POINT, &[("id", Value::Int(5))]);
    conn.execute("MATCH (p:person {id: 5}) INSERT (p)-[:knows]->(p)")
        .expect("insert");
    let after = rows(&mut conn, POINT, &[("id", Value::Int(5))]);
    assert_ne!(before, after, "the edge the write made is one more");
    let mut fresh = db.connect().expect("connect");
    assert_eq!(after, rows(&mut fresh, POINT, &[("id", Value::Int(5))]));
}

#[test]
fn a_parameter_that_did_not_become_a_key_is_read_every_run() {
    let (_dir, db) = opened("floor.zu1");
    let mut conn = db.connect().expect("connect");
    // The safety property, from the other side. This parameter is read
    // by the compiler and lands in a filter rather than in a hole the
    // plan can name, so the plan is not offered for reuse at all and
    // each floor compiles its own. A plan handed back with the first
    // floor still in it would answer the first floor's rows.
    let source = "MATCH (p:person) WHERE p.id > $floor RETURN count(p) AS n";
    for floor in [190i64, 10, 198, 0] {
        let got = rows(&mut conn, source, &[("floor", Value::Int(floor))]);
        let mut fresh = db.connect().expect("connect");
        assert_eq!(
            got,
            rows(&mut fresh, source, &[("floor", Value::Int(floor))]),
            "floor {floor}"
        );
    }
}

#[test]
fn a_paged_read_pages_where_this_run_asked_for() {
    let (_dir, db) = opened("page.zu1");
    let mut conn = db.connect().expect("connect");
    // A SKIP count is a parameter that steers the plan rather than
    // riding in it, which is the other shape that must not be reused.
    let source = "MATCH (p:person) RETURN p.id AS id ORDER BY id SKIP $at LIMIT 3";
    for at in [0i64, 17, 100, 17] {
        let got = rows(&mut conn, source, &[("at", Value::Int(at))]);
        let mut fresh = db.connect().expect("connect");
        assert_eq!(
            got,
            rows(&mut fresh, source, &[("at", Value::Int(at))]),
            "at {at}"
        );
    }
}

#[test]
fn two_statements_taking_turns_each_keep_their_own_answer() {
    let (_dir, db) = opened("turns.zu1");
    let mut conn = db.connect().expect("connect");
    // One plan is held, not one per statement, so alternating between
    // two texts is a miss every time. A miss has to compile, and this
    // is what says it does rather than handing back whatever was held.
    let other = "MATCH (p:person {id: $id})<-[:knows]-(f) RETURN p.id AS pid, count(f) AS n";
    let mut fresh = db.connect().expect("connect");
    for id in [4i64, 88, 4, 88] {
        let a = rows(&mut conn, POINT, &[("id", Value::Int(id))]);
        let b = rows(&mut conn, other, &[("id", Value::Int(id))]);
        assert_eq!(a, rows(&mut fresh, POINT, &[("id", Value::Int(id))]));
        assert_eq!(b, rows(&mut fresh, other, &[("id", Value::Int(id))]));
    }
}
