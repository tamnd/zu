//! Graph references and binding table references end to end (GV60,
//! GV61).
//!
//! Neither type has a literal. A graph lives in the catalog and a
//! binding table is the result of something that already ran, so the
//! only place a reference can come from is the engine, and the only
//! way one reaches a statement is as a parameter. That is the shape
//! this file checks: a caller asks a session for a handle, hands it
//! back in, and the statement sees a value of the right type. The
//! other half is lifetime, because a handle outlives the thing it
//! names and the two ways it can do that are not the same one.

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu_query::refs::{BindingTable, GraphHandle};

/// Two people with a name, one edge, so that a row can hold an
/// element and a later statement has a column to write into.
fn opened(name: &str) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
    let names: Vec<&[u8]> = vec![b"ada", b"kay"];
    zu::zu1::props::store_props(
        &mut db,
        "person",
        &[("name", zu::zu1::props::PropValues::Str(&names))],
    )
    .expect("props");
    drop(db);
    let session = Session::open(&path).expect("open");
    (dir, session)
}

fn yes(session: &mut Session, predicate: &str, params: &[(&str, Value)]) {
    let source = format!("RETURN ({predicate}) AS v");
    let result = session
        .run(&source, params)
        .unwrap_or_else(|e| panic!("{predicate}: {e}"));
    assert_eq!(result.rows[0], vec![Value::Bool(true)], "{predicate}");
}

fn no(session: &mut Session, predicate: &str, params: &[(&str, Value)]) {
    let source = format!("RETURN ({predicate}) AS v");
    let result = session
        .run(&source, params)
        .unwrap_or_else(|e| panic!("{predicate}: {e}"));
    assert_eq!(result.rows[0], vec![Value::Bool(false)], "{predicate}");
}

/// The whole of what a graph reference is: the catalog answers a name
/// with a handle, the handle rides in as a parameter, and the type
/// predicate says what it is. It is not a property value and it is not
/// a binding table, which are the two things a value at that position
/// could otherwise have been.
#[test]
fn a_graph_reference_comes_from_the_catalog_and_reads_as_a_graph() {
    let (_dir, mut session) = opened("graph-ref.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");
    assert_eq!(home, session.working_graph_ref().expect("the same graph"));

    let params = [("g", home.clone())];
    yes(&mut session, "$g IS TYPED GRAPH", &params);
    yes(&mut session, "$g IS TYPED PROPERTY GRAPH", &params);
    yes(&mut session, "$g IS TYPED ANY GRAPH", &params);
    no(&mut session, "$g IS TYPED BINDING TABLE", &params);
    no(&mut session, "$g IS TYPED ANY PROPERTY VALUE", &params);

    let out = session.run("RETURN $g AS g", &params).expect("a handle");
    assert_eq!(out.rows[0], vec![home]);
}

/// A name the catalog does not hold is `42002`, at the point the
/// handle is asked for rather than at the point one is used, because
/// there is nothing to hand back.
#[test]
fn a_graph_that_is_not_there_has_no_reference() {
    let (_dir, mut session) = opened("no-graph.zu1");
    let err = session
        .graph_ref("/", "nowhere")
        .expect_err("no such graph");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002");
}

/// The other lifetime: a handle can name a graph that was there when
/// it was taken and is gone by the time it is used, and the statement
/// it is passed to is refused rather than run against nothing.
#[test]
fn a_handle_to_a_graph_that_is_gone_is_refused() {
    let (_dir, mut session) = opened("dropped-graph.zu1");
    let ghost = Value::Graph(GraphHandle::new(4242, "/", "ghost", 0));
    let err = session
        .run("RETURN $g AS g", &[("g", ghost)])
        .expect_err("a handle to nothing");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002");
    assert!(
        record.detail.contains("dropped"),
        "the message says what happened: {}",
        record.detail
    );
}

/// A binding table is a result already read, held behind a handle. The
/// rows are the rows: what the table is checked against is the row
/// type, so a table of one integer column is of that record type and
/// not of another.
#[test]
fn a_result_becomes_a_binding_table_of_the_row_type_it_has() {
    let (_dir, mut session) = opened("binding-table.zu1");
    let result = session
        .run("MATCH (p:person) RETURN p.id AS id", &[])
        .expect("two rows");
    assert_eq!(result.rows.len(), 2);
    let table = session.binding_table(result);

    let params = [("t", table)];
    yes(&mut session, "$t IS TYPED BINDING TABLE", &params);
    yes(&mut session, "$t IS TYPED TABLE", &params);
    yes(&mut session, "$t IS TYPED ANY BINDING TABLE", &params);
    no(&mut session, "$t IS TYPED GRAPH", &params);
    no(&mut session, "$t IS TYPED ANY PROPERTY VALUE", &params);
}

/// Two tables over the same rows are two tables. Content equality
/// would make them one, and they are not: a reference is an identity,
/// and the identity is what a later statement is talking about when it
/// names one.
#[test]
fn two_tables_over_the_same_rows_are_two_references() {
    let (_dir, mut session) = opened("table-identity.zu1");
    let source = "MATCH (p:person) RETURN p.id AS id";
    let rows = session_run(&mut session, source);
    let first = session.binding_table(rows);
    let rows = session_run(&mut session, source);
    let second = session.binding_table(rows);
    assert_ne!(first, second);
    assert_eq!(first, first.clone());
}

fn session_run(session: &mut Session, source: &str) -> zu::query::QueryResult {
    session.run(source, &[]).expect("rows")
}

/// A table of scalars is answerable at any epoch and a table holding
/// elements is not. A node is a row of the snapshot it was read from,
/// so once a write has moved the session on, the values in such a
/// table name rows that may now belong to something else, and the
/// statement is refused rather than told a plausible wrong answer.
///
/// Not every write moves it. An appended row is handed to the readers
/// on a patch and the rows that were already there stay where they
/// were, so a table taken before one still names what it named. A
/// label set rewrites the column it is on and folds, and that is the
/// write this checks.
#[test]
fn a_table_holding_elements_does_not_survive_a_fold() {
    let (_dir, mut session) = opened("table-epoch.zu1");
    let rows = session_run(&mut session, "MATCH (p:person) RETURN p.id AS id");
    let scalars = session.binding_table(rows);
    let rows = session_run(&mut session, "MATCH (p:person) RETURN p AS p");
    let elements = session.binding_table(rows);

    session
        .run("INSERT (p:person {name: 'zoe'})", &[])
        .expect("a write the readers are handed on a patch");
    let params = [("t", elements.clone())];
    yes(&mut session, "$t IS TYPED BINDING TABLE", &params);

    session
        .run("MATCH (p:person) WHERE p.id = 0 SET p:bot", &[])
        .expect("a write, which moves the epoch on");

    yes(&mut session, "$t IS TYPED BINDING TABLE", &[("t", scalars)]);
    let err = session
        .run("RETURN 1 AS n", &[("t", elements)])
        .expect_err("the table names rows of an older snapshot");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002");
}

/// The epoch check is on what the table holds and not on how deep it
/// is held: a node inside a list inside a row is still a node, and a
/// table that is only numbers stays good however it is nested.
#[test]
fn the_epoch_check_reaches_a_nested_element() {
    let (_dir, mut session) = opened("nested.zu1");
    let numbers = BindingTable::new(
        vec!["xs".into()],
        vec![vec![Value::List(vec![Value::Int(1), Value::Int(2)])]],
        0,
    );
    let result = session
        .run("MATCH (p:person) RETURN [p] AS ps", &[])
        .expect("a list of nodes");
    let Value::List(_) = &result.rows[0][0] else {
        panic!("a list, got {:?}", result.rows[0][0]);
    };
    let nested = BindingTable::new(result.columns, result.rows, 0);

    assert!(!numbers.holds_elements());
    assert!(nested.holds_elements());
}
