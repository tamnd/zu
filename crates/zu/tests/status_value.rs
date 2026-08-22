//! GA08, the GQL-status object read as a value (plan/07 §4).
//!
//! ISO/IEC 39075:2024 has no statement for reading a status back. There
//! is no `GET DIAGNOSTICS` in it, and subclause 23 is about producing
//! the record and exposing it rather than about asking for one, so the
//! name here is zu's own and the shape it answers is the standard's:
//! the five characters, the words for them, and the diagnostic records
//! under that.

use zu::Database;
use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 4;

fn opened(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    drop(db);
    let db = Database::open(&path).expect("open");
    (dir, db)
}

/// The one field out of a record, or a panic naming what was there.
fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Record(fields) = value else {
        panic!("not a record: {value:?}");
    };
    fields
        .iter()
        .find(|(had, _)| had == name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no field '{name}' in {fields:?}"))
}

fn text(value: &Value, name: &str) -> String {
    match field(value, name) {
        Value::Str(s) => s.clone(),
        other => panic!("'{name}' is {other:?}"),
    }
}

fn status(conn: &mut zu::Connection) -> Value {
    let result = conn
        .query("RETURN current_status() AS s")
        .expect("the status of the statement before this one");
    result.rows[0][0].clone()
}

#[test]
fn a_session_that_has_run_nothing_has_no_status() {
    let (_dir, db) = opened("first.zu1");
    let mut conn = db.connect().expect("connect");
    // Null and not a record of nulls: there is no statement before the
    // first one, and a record saying nothing would read as one that
    // said so.
    assert_eq!(status(&mut conn), Value::Null);
}

#[test]
fn a_statement_that_succeeded_says_so_and_diagnoses_nothing() {
    let (_dir, db) = opened("ok.zu1");
    let mut conn = db.connect().expect("connect");
    conn.query("MATCH (p:person) RETURN p.id AS id")
        .expect("a query that runs");

    let s = status(&mut conn);
    assert_eq!(text(&s, "gqlstatus"), "00000");
    assert_eq!(text(&s, "condition"), "successful completion");
    assert_eq!(text(&s, "severity"), "S");
    assert_eq!(field(&s, "message"), &Value::Null);
    // An empty list rather than a record of nulls, because nothing was
    // diagnosed is not the same answer as something blank was.
    assert_eq!(field(&s, "diagnostics"), &Value::List(Vec::new()));
}

#[test]
fn a_statement_that_was_refused_carries_its_whole_record() {
    let (_dir, db) = opened("refused.zu1");
    let mut conn = db.connect().expect("connect");
    conn.query("MATCH (p:person) RETURN q.id AS id")
        .expect_err("q is not defined");

    let s = status(&mut conn);
    assert_eq!(text(&s, "gqlstatus"), "42002");
    assert_eq!(text(&s, "severity"), "X");
    assert!(text(&s, "message").contains('q'), "{s:?}");

    let Value::List(records) = field(&s, "diagnostics") else {
        panic!("diagnostics is not a list: {s:?}");
    };
    assert_eq!(records.len(), 1, "one condition, one record");
    let record = &records[0];
    assert_eq!(text(record, "gqlstatus"), "42002");
    assert_eq!(text(record, "subject"), "q");
    assert_eq!(text(record, "subject_kind"), "variable");
    assert_eq!(text(record, "current_graph"), "home");
    assert_eq!(text(record, "current_schema"), "/");
}

/// The status is of the statement before this one and of no other, so
/// asking twice in a row answers the successful read the first time.
#[test]
fn the_status_is_of_the_statement_before_and_moves_with_it() {
    let (_dir, db) = opened("moves.zu1");
    let mut conn = db.connect().expect("connect");
    conn.query("RETURN 1 / 0 AS v").expect_err("by zero");
    assert_eq!(text(&status(&mut conn), "gqlstatus"), "22012");
    assert_eq!(text(&status(&mut conn), "gqlstatus"), "00000");
}

/// It takes nothing, and a call that writes an argument is told so
/// rather than being read as a function nobody defined.
#[test]
fn it_takes_no_arguments() {
    let (_dir, db) = opened("arity.zu1");
    let mut conn = db.connect().expect("connect");
    let err = conn
        .query("RETURN current_status(1) AS s")
        .expect_err("it takes nothing");
    assert!(err.to_string().contains("takes nothing"), "{err}");
}
