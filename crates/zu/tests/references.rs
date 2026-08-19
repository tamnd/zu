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

/// GE01, the half of a graph reference that is not a parameter. The
/// four words that name a graph without naming it answer handles, and
/// the handles are the ones the session hands out, which is the whole
/// claim: a reference written in a statement and a reference taken
/// through the API are the same value.
#[test]
fn the_words_that_name_a_graph_answer_the_handle_the_session_hands_out() {
    let (_dir, mut session) = opened("graph-expr.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");

    for source in [
        "RETURN CURRENT_GRAPH AS g",
        "RETURN CURRENT_PROPERTY_GRAPH AS g",
        "RETURN HOME_GRAPH AS g",
        "RETURN HOME_PROPERTY_GRAPH AS g",
        "RETURN /home AS g",
    ] {
        let out = session
            .run(source, &[])
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(out.rows[0], vec![home.clone()], "{source}");
    }
}

/// A graph reference is a value of `GRAPH` wherever it was written,
/// and the type predicate is how a statement says so without the API
/// having handed anything in.
#[test]
fn a_graph_reference_written_in_a_statement_is_typed_a_graph() {
    let (_dir, mut session) = opened("graph-expr-typed.zu1");
    yes(&mut session, "CURRENT_PROPERTY_GRAPH IS TYPED GRAPH", &[]);
    yes(&mut session, "/home IS TYPED ANY GRAPH", &[]);
    no(&mut session, "CURRENT_GRAPH IS TYPED BINDING TABLE", &[]);
    yes(
        &mut session,
        "CURRENT_PROPERTY_GRAPH = HOME_PROPERTY_GRAPH",
        &[],
    );
}

/// The path is the whole of how a graph is named in an expression,
/// and this is why: a bare name is a variable, and a word with a path
/// behind it is a division as often as it is a graph. Both readings
/// are kept here, because both are what somebody wrote.
#[test]
fn graph_is_still_a_name_and_a_slash_after_one_is_still_a_division() {
    let (_dir, mut session) = opened("graph-is-a-name.zu1");
    let out = session
        .run("LET graph = 12 RETURN graph AS n", &[])
        .expect("a variable of that name");
    assert_eq!(out.rows[0], vec![Value::Int(12)]);

    let out = session
        .run("LET graph = 12, home = 4 RETURN graph / home AS n", &[])
        .expect("a division of two variables");
    assert_eq!(out.rows[0], vec![Value::Int(3)]);
}

/// A name the catalog does not hold is `42002`, the same condition a
/// `USE` of it raises and for the same reason: the reference resolves
/// to nothing.
#[test]
fn a_graph_reference_to_a_name_that_is_not_there_is_refused() {
    let (_dir, mut session) = opened("graph-expr-missing.zu1");
    let err = session
        .run("RETURN /nowhere AS g", &[])
        .expect_err("no such graph");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002");
}

/// GE01 and GE02, the reference a caller passed in. The word in front
/// of the parameter says which of the two types the caller means to
/// have passed and the value is the same value either way, which is
/// what `USE GRAPH $g` beside `USE $g` already says.
#[test]
fn the_word_in_front_of_a_reference_parameter_says_nothing_the_value_does_not() {
    let (_dir, mut session) = opened("reference-params.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");
    let graph = [("g", home.clone())];
    for source in [
        "RETURN $g AS g",
        "RETURN GRAPH $g AS g",
        "RETURN PROPERTY GRAPH $g AS g",
    ] {
        let out = session
            .run(source, &graph)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(out.rows[0], vec![home.clone()], "{source}");
    }

    let rows = session_run(&mut session, "MATCH (p:person) RETURN p.id AS id");
    let table = session.binding_table(rows);
    let params = [("t", table.clone())];
    for source in [
        "RETURN $t AS t",
        "RETURN TABLE $t AS t",
        "RETURN BINDING TABLE $t AS t",
    ] {
        let out = session
            .run(source, &params)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(out.rows[0], vec![table.clone()], "{source}");
    }
}

/// GE08. The standard writes a reference parameter with two dollar
/// signs, `<substituted parameter reference>` in ISO 21.10, and a
/// value parameter with one. Both name the same parameter here,
/// because what a parameter holds is settled by the value that
/// arrives, which is the rule the word in front of it is read under
/// too.
#[test]
fn a_reference_parameter_may_be_written_the_way_the_standard_writes_one() {
    let (_dir, mut session) = opened("substituted-params.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");
    let graph = [("g", home.clone())];
    for source in [
        "RETURN $$g AS g",
        "RETURN GRAPH $$g AS g",
        "RETURN PROPERTY GRAPH $$g AS g",
    ] {
        let out = session
            .run(source, &graph)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(out.rows[0], vec![home.clone()], "{source}");
    }

    let out = session
        .run(
            "USE GRAPH $$g MATCH (p:person) RETURN count(p) AS n",
            &graph,
        )
        .expect("a graph parameter says which graph to run against");
    assert_eq!(out.columns, vec!["n".to_string()]);

    // One dollar sign in front of a value is the other spelling and
    // is untouched by any of this.
    let out = session
        .run("RETURN $n + 1 AS n", &[("n", Value::Int(1))])
        .expect("a value parameter");
    assert_eq!(out.rows[0], vec![Value::Int(2)]);
}

/// `graph` and `table` are names right up until a parameter or a path
/// follows them, which is the rule `PATH` and `DATE` are read under.
#[test]
fn table_is_still_a_name() {
    let (_dir, mut session) = opened("table-is-a-name.zu1");
    let out = session
        .run(
            "LET table = 7, binding = 8 RETURN table + binding AS n",
            &[],
        )
        .expect("variables of those names");
    assert_eq!(out.rows[0], vec![Value::Int(15)]);
}

/// Two references are equal when they name the same thing, and that is
/// the identity and not the contents: two graph handles taken at
/// different epochs name one graph, and two tables over the same rows
/// are two tables.
#[test]
fn references_compare_by_what_they_name() {
    let (_dir, mut session) = opened("reference-equality.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");
    let Value::Graph(handle) = home.clone() else {
        panic!("a graph handle, got {home:?}");
    };
    let older = Value::Graph(GraphHandle::new(
        handle.id,
        handle.schema.clone(),
        handle.name.clone(),
        handle.epoch.wrapping_sub(1),
    ));
    yes(
        &mut session,
        "$g = CURRENT_PROPERTY_GRAPH",
        &[("g", older.clone())],
    );
    yes(&mut session, "$g = $h", &[("g", older), ("h", home)]);

    let source = "MATCH (p:person) RETURN p.id AS id";
    let rows = session_run(&mut session, source);
    let first = session.binding_table(rows);
    let rows = session_run(&mut session, source);
    let second = session.binding_table(rows);
    yes(
        &mut session,
        "$a = $b",
        &[("a", first.clone()), ("b", first.clone())],
    );
    no(&mut session, "$a = $b", &[("a", first), ("b", second)]);
}

/// GQ23. A `FOR` runs over the rows of a binding table the way it runs
/// over the elements of a list, and a row arrives as a record over the
/// table's columns, so a field read is what gets at the value in it.
/// The counter of `WITH ORDINALITY` numbers the rows, because a table
/// is a sequence of rows the way a list is a sequence of elements and
/// nothing else about the statement changes.
#[test]
fn a_for_statement_runs_over_the_rows_of_a_binding_table() {
    let (_dir, mut session) = opened("for-over-a-table.zu1");
    let rows = session_run(&mut session, "MATCH (p:person) RETURN p.id AS id");
    let table = session.binding_table(rows);

    let out = session
        .run("FOR r IN $t RETURN r.id AS v", &[("t", table.clone())])
        .expect("a row for each row of the table");
    let mut got: Vec<Value> = out.rows.into_iter().map(|row| row[0].clone()).collect();
    got.sort_by_key(|v| match v {
        Value::Int(n) => *n,
        other => panic!("an integer, got {other:?}"),
    });
    assert_eq!(got, vec![Value::Int(0), Value::Int(1)]);

    let out = session
        .run(
            "FOR r IN $t WITH ORDINALITY i RETURN i AS n ORDER BY n",
            &[("t", table)],
        )
        .expect("the rows numbered from one");
    assert_eq!(out.rows[0], vec![Value::Int(1)]);
    assert_eq!(out.rows[1], vec![Value::Int(2)]);
}

/// What a `FOR` refuses is everything that is neither a list nor a
/// table, and it says both in the message, because a reader who wrote
/// one of the two meant the statement to work.
#[test]
fn a_for_statement_over_a_number_is_refused() {
    let (_dir, mut session) = opened("for-over-a-number.zu1");
    let err = session
        .run("FOR x IN 1 RETURN x AS v", &[])
        .expect_err("a number is not a sequence");
    let message = err.to_string();
    assert!(message.contains("list"), "{message}");
}
