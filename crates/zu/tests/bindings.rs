//! The binding variable definition block end to end (ISO 13.3, GP05
//! through GP13 and GP17).
//!
//! A definition is a name for something worked out once, before the
//! statement runs, and that is what these check: that the three kinds
//! read back as the three kinds, that a definition can be read as many
//! times as the writer likes without being worked out again, and that
//! the ones a query defines are gone by the end of the statement.
//!
//! The end to end half matters more here than usual. What a binding
//! variable compiles to is a parameter position the engine fills, so
//! the only way to be sure the filling happened, in the right order
//! and before the first row, is to run a statement and read the rows.

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

/// Four people with a name and an age, three edges, which is the
/// fixture the conformance suite uses so that a number here can be
/// checked against a case there.
fn opened(name: &str) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (3, 3)]).expect("load");
    let names: Vec<&[u8]> = vec![b"ann", b"bo", b"cy", b"di"];
    zu::zu1::props::store_props(
        &mut db,
        "person",
        &[
            ("name", zu::zu1::props::PropValues::Str(&names)),
            ("age", zu::zu1::props::PropValues::Int(&[30, 40, 50, 40])),
        ],
    )
    .expect("props");
    drop(db);
    let session = Session::open(&path).expect("open");
    (dir, session)
}

fn rows(session: &mut Session, source: &str) -> Vec<Vec<Value>> {
    session
        .run(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .into_vec()
}

fn failure(session: &mut Session, source: &str) -> String {
    match session.run(source, &[]) {
        Ok(_) => panic!("{source}: expected a refusal"),
        Err(e) => e.to_string(),
    }
}

/// The simplest definition there is: a name for a value, read where a
/// value goes. It is one row because nothing was matched, which is
/// also what says the definition is not a source of rows.
#[test]
fn a_value_variable_is_a_name_for_something_worked_out_once() {
    let (_dir, mut session) = opened("value.zu1");
    assert_eq!(
        rows(&mut session, "VALUE n = 20 + 2 RETURN n AS a"),
        vec![vec![Value::Int(22)]]
    );
    // Read twice in one statement and read against the graph, which is
    // where a name that stood for a fresh value each time would show.
    assert_eq!(
        rows(
            &mut session,
            "VALUE cut = 35 MATCH (p:person) WHERE p.age > cut RETURN count(*) AS a, cut + cut AS b"
        ),
        vec![vec![Value::Int(3), Value::Int(70)]]
    );
}

/// A definition may read the definitions in front of it, which is what
/// makes the block a block rather than a list of unrelated names. It
/// may not read itself, and the message says which name it was.
#[test]
fn a_definition_reads_the_ones_in_front_of_it_and_not_itself() {
    let (_dir, mut session) = opened("order.zu1");
    assert_eq!(
        rows(&mut session, "VALUE a = 7 VALUE b = a * 6 RETURN b AS x"),
        vec![vec![Value::Int(42)]]
    );
    let itself = failure(&mut session, "VALUE a = a + 1 RETURN a AS x");
    assert!(itself.contains("'a'"), "{itself}");
    let twice = failure(&mut session, "VALUE a = 1 VALUE a = 2 RETURN a AS x");
    assert!(twice.contains("defined twice"), "{twice}");
}

/// A value variable defined out of a query (GP07). What stands after
/// the equals is a whole query, so it may match, aggregate and page,
/// and it is worked out once however many rows read it.
#[test]
fn a_value_variable_may_be_defined_out_of_a_query() {
    let (_dir, mut session) = opened("query.zu1");
    assert_eq!(
        rows(
            &mut session,
            "VALUE oldest = { MATCH (p:person) RETURN max(p.age) AS a } \
             MATCH (p:person) WHERE p.age = oldest RETURN p.name AS x"
        ),
        vec![vec![Value::Str("cy".into())]]
    );
}

/// A binding table variable (GP08 and GP10): the rows of a result held
/// as one value, counted with `CARDINALITY` and run over with `FOR`,
/// which are the two things a table is for.
#[test]
fn a_binding_table_variable_holds_the_rows_of_a_query() {
    let (_dir, mut session) = opened("table.zu1");
    assert_eq!(
        rows(
            &mut session,
            "BINDING TABLE t = { MATCH (p:person) RETURN p.name AS name } \
             RETURN cardinality(t) AS a"
        ),
        vec![vec![Value::Int(4)]]
    );
    assert_eq!(
        rows(
            &mut session,
            "TABLE t = { MATCH (p:person) WHERE p.age > 35 RETURN p.name AS name } \
             FOR r IN t RETURN count(*) AS a"
        ),
        vec![vec![Value::Int(3)]]
    );
}

/// A graph variable (GP11): the graph the session is working in, named,
/// and read back as a graph. It is a reference and not the graph, which
/// is what the type predicate says.
#[test]
fn a_graph_variable_is_a_reference_and_reads_as_one() {
    let (_dir, mut session) = opened("graph.zu1");
    assert_eq!(
        rows(
            &mut session,
            "GRAPH g = CURRENT_PROPERTY_GRAPH RETURN (g IS NOT NULL) AS a"
        ),
        vec![vec![Value::Bool(true)]]
    );
}

/// The kinds are checked against what the definition answered, because
/// the name is going to be read as one of the three and a reader that
/// found another thing there would have no way to say so.
#[test]
fn what_a_definition_answers_has_to_be_what_it_was_written_as() {
    let (_dir, mut session) = opened("kinds.zu1");
    let wide = failure(
        &mut session,
        "VALUE v = { MATCH (p:person) RETURN p.name AS a, p.age AS b } RETURN v AS x",
    );
    assert!(wide.contains("one column"), "{wide}");
    let many = failure(
        &mut session,
        "VALUE v = { MATCH (p:person) RETURN p.name AS a } RETURN v AS x",
    );
    assert!(many.contains("one value"), "{many}");
}

/// A definition may be written with a type, which is a statement about
/// what the query is going to answer and is checked against the answer.
/// It is the same decision `IS TYPED` makes, so a type means one thing
/// wherever it is written.
#[test]
fn a_definition_may_be_written_with_the_type_it_answers() {
    let (_dir, mut session) = opened("typed.zu1");
    assert_eq!(
        rows(
            &mut session,
            "VALUE oldest INT = { MATCH (p:person) RETURN max(p.age) AS a } RETURN oldest AS x"
        ),
        vec![vec![Value::Int(50)]]
    );
    let wrong = failure(&mut session, "VALUE n STRING = 1 + 1 RETURN n AS x");
    assert!(wrong.contains("was defined as"), "{wrong}");
}

/// A block defines names of its own and they are gone after it, which
/// is the lexical half of GP17. What is not gone is the value: it was
/// worked out with every other definition, before the first row.
#[test]
fn a_block_defines_names_of_its_own_and_they_end_with_it() {
    let (_dir, mut session) = opened("block.zu1");
    assert_eq!(
        rows(
            &mut session,
            "MATCH (p:person) CALL (p) { VALUE cut = 35 MATCH (q:person) \
             WHERE q.age > cut AND q.age > p.age RETURN q.name AS f } \
             RETURN count(*) AS a"
        ),
        vec![vec![Value::Int(5)]]
    );
    let gone = failure(
        &mut session,
        "MATCH (p:person) CALL (p) { VALUE cut = 35 MATCH (q:person) WHERE q.age > cut \
         RETURN q.name AS f } RETURN cut AS x",
    );
    assert!(gone.contains("'cut'"), "{gone}");
}

/// A definition cannot read a row, because there is no row when it is
/// worked out. The message says so by name rather than by leaving the
/// reference unresolved somewhere further in.
#[test]
fn a_definition_cannot_read_a_row() {
    let (_dir, mut session) = opened("row.zu1");
    let reads = failure(
        &mut session,
        "MATCH (p:person) CALL (p) { VALUE mine = p.age RETURN mine AS a } RETURN count(*) AS n",
    );
    assert!(reads.contains("'p'"), "{reads}");
}

/// Neither can it write, because a statement taken apart at its write
/// runs in parts and each part fills the positions it reads. Reading a
/// graph twice answers the same thing and writing to it twice does not.
#[test]
fn a_definition_cannot_write() {
    let (_dir, mut session) = opened("write.zu1");
    let wrote = failure(
        &mut session,
        "VALUE n = { INSERT (x:person) RETURN 1 AS a } RETURN n AS x",
    );
    assert!(wrote.contains("writes to the graph"), "{wrote}");
}
