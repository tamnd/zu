//! The session statements end to end (ISO 7.1 and 7.2, GS01 through
//! GS16).
//!
//! A GQL session is a named mutable environment and not a transport
//! detail: it holds parameters, the schema and the graph names are
//! resolved in, and the time zone a clock is read in. These check what
//! a statement can move in one, and that what it moved is there for the
//! statement after it and gone after a reset.
//!
//! End to end rather than at the parser, because every one of these is
//! a statement that answers no rows: the only way to see that it did
//! anything is to run a second statement and read what it says.

use zu::query::Value;
use zu::session::{Session, Stale};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu_common::Temporal;
use zu_common::gqlstatus::codes;

/// The same four people the binding variable tests use, so a number
/// here can be checked against a number there.
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

fn run(session: &mut Session, source: &str) {
    session
        .run(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"));
}

fn rows(session: &mut Session, source: &str) -> Vec<Vec<Value>> {
    session
        .run(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .into_vec()
}

fn one(session: &mut Session, source: &str) -> Value {
    let mut rows = rows(session, source);
    assert_eq!(rows.len(), 1, "{source}");
    rows.swap_remove(0).swap_remove(0)
}

fn failure(session: &mut Session, source: &str) -> String {
    match session.run(source, &[]) {
        Ok(_) => panic!("{source}: expected a refusal"),
        Err(e) => e.to_string(),
    }
}

/// GS03 and GS14. A value parameter set from an expression, read by
/// the statement after it under the name it was set with.
#[test]
fn a_value_parameter_outlives_the_statement_that_set_it() {
    let (_dir, mut session) = opened("value.zu1");
    run(&mut session, "SESSION SET VALUE $cutoff = 35");
    assert_eq!(one(&mut session, "RETURN $cutoff AS c"), Value::Int(35));
    assert_eq!(
        rows(
            &mut session,
            "MATCH (p:person) WHERE p.age > $cutoff RETURN count(*) AS n"
        ),
        vec![vec![Value::Int(3)]]
    );
    assert_eq!(
        session.session_params(),
        vec![("cutoff", "VALUE", &Value::Int(35))]
    );
}

/// GS03 with a type written between the name and the `=`, which is the
/// same type a binding variable takes and is checked the same way.
#[test]
fn a_value_parameter_takes_the_type_it_was_written_with() {
    let (_dir, mut session) = opened("typed.zu1");
    run(&mut session, "SESSION SET VALUE $n :: INTEGER = 7");
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(7));
    let refused = failure(&mut session, "SESSION SET VALUE $s :: INTEGER = 'no'");
    assert!(refused.contains("answered STRING"), "{refused}");
}

/// GS11. A value parameter set from a query in braces, which is worked
/// out once where it is written and not once per reader.
#[test]
fn a_value_parameter_can_be_set_from_a_query() {
    let (_dir, mut session) = opened("valuequery.zu1");
    run(
        &mut session,
        "SESSION SET VALUE $oldest = { MATCH (p:person) RETURN max(p.age) AS a }",
    );
    assert_eq!(one(&mut session, "RETURN $oldest AS a"), Value::Int(50));
    assert_eq!(
        rows(
            &mut session,
            "MATCH (p:person) WHERE p.age = $oldest RETURN p.name AS n"
        ),
        vec![vec![Value::Str("cy".into())]]
    );
}

/// GS02 and GS10. A binding table parameter holding the rows a query
/// answered, counted and run over, which are the two things a table is
/// for.
#[test]
fn a_binding_table_parameter_holds_the_rows_of_its_query() {
    let (_dir, mut session) = opened("table.zu1");
    run(
        &mut session,
        "SESSION SET BINDING TABLE $t = { MATCH (p:person) WHERE p.age > 35 RETURN p.name AS name }",
    );
    assert_eq!(
        one(&mut session, "RETURN cardinality($t) AS n"),
        Value::Int(3)
    );
    assert_eq!(
        rows(&mut session, "FOR r IN $t RETURN count(*) AS n"),
        vec![vec![Value::Int(3)]]
    );
    assert_eq!(session.session_params()[0].1, "BINDING TABLE");
}

/// GS13. A binding table parameter set from a reference the caller
/// already holds, which is the second of the two value sources.
#[test]
fn a_binding_table_parameter_can_be_set_from_a_reference() {
    let (_dir, mut session) = opened("tableref.zu1");
    let read = session
        .run("MATCH (p:person) RETURN p.id AS id", &[])
        .expect("read");
    let table = session.binding_table(read);
    session
        .run("SESSION SET BINDING TABLE $t = $held", &[("held", table)])
        .expect("set");
    assert_eq!(
        one(&mut session, "RETURN cardinality($t) AS n"),
        Value::Int(4)
    );
}

/// GS01 and GS12. A graph parameter, which holds a reference and not
/// the graph, and is what a `USE` may name.
#[test]
fn a_graph_parameter_is_a_reference_a_use_can_name() {
    let (_dir, mut session) = opened("graph.zu1");
    run(
        &mut session,
        "SESSION SET PROPERTY GRAPH $g = HOME_PROPERTY_GRAPH",
    );
    assert_eq!(
        one(&mut session, "RETURN $g AS g"),
        session.home_graph_ref().expect("home")
    );
    assert_eq!(
        rows(&mut session, "USE $g MATCH (p:person) RETURN count(*) AS n"),
        vec![vec![Value::Int(4)]]
    );
}

/// The dollar is what tells the two `SESSION SET GRAPH` statements
/// apart: with one it defines a parameter and without one it moves the
/// graph the session works in (GS06).
#[test]
fn setting_the_graph_moves_the_session_and_setting_a_parameter_does_not() {
    let (_dir, mut session) = opened("working.zu1");
    let home = session.working_graph();
    run(&mut session, "CREATE PROPERTY GRAPH later");
    run(&mut session, "SESSION SET PROPERTY GRAPH $g = later");
    assert_eq!(session.working_graph(), home);
    run(&mut session, "SESSION SET PROPERTY GRAPH later");
    assert_ne!(session.working_graph(), home);
    // The second graph holds none of the first graph's tables, so the
    // statement that counted four people now counts none: it is the
    // same text against a different graph.
    assert_eq!(
        rows(&mut session, "MATCH (p:person) RETURN count(*) AS n"),
        vec![vec![Value::Int(0)]]
    );
    run(&mut session, "SESSION RESET PROPERTY GRAPH");
    assert_eq!(session.working_graph(), home);
    assert_eq!(
        rows(&mut session, "MATCH (p:person) RETURN count(*) AS n"),
        vec![vec![Value::Int(4)]]
    );
}

/// GS05. The schema a name written with no path in front of it is
/// looked up in, moved and moved back.
#[test]
fn setting_the_schema_moves_where_an_unqualified_name_is_looked_up() {
    let (_dir, mut session) = opened("schema.zu1");
    assert_eq!(session.session_schema(), "/");
    run(&mut session, "CREATE SCHEMA /app");
    run(&mut session, "CREATE PROPERTY GRAPH /app/orders");
    // From the root the name is nowhere, and from the schema it is
    // there, which is the whole of what the statement does.
    assert!(
        session
            .run("USE orders MATCH (n) RETURN count(*) AS n", &[])
            .is_err()
    );
    run(&mut session, "SESSION SET SCHEMA /app");
    assert_eq!(session.session_schema(), "/app");
    run(&mut session, "USE orders MATCH (n) RETURN count(*) AS n");
    run(&mut session, "SESSION RESET SCHEMA");
    assert_eq!(session.session_schema(), "/");
    let refused = failure(&mut session, "SESSION SET SCHEMA /nowhere");
    assert!(refused.contains("no schema"), "{refused}");
}

/// GS15 and GS07. The displacement the datetime value functions answer
/// in, which moves the zone and not the instant.
#[test]
fn setting_the_time_zone_moves_the_zone_and_not_the_instant() {
    let (_dir, mut session) = opened("zone.zu1");
    assert_eq!(session.time_zone(), 0);
    let stamped = |session: &mut Session| match one(session, "RETURN CURRENT_TIMESTAMP AS t") {
        Value::Temporal(Temporal::ZonedDatetime { nanos, offset }) => (nanos, offset),
        other => panic!("current_timestamp answered {other:?}"),
    };
    let (utc, zero) = stamped(&mut session);
    assert_eq!(zero, 0, "a session opens in UTC");

    run(&mut session, "SESSION SET TIME ZONE '+07:00'");
    assert_eq!(session.time_zone(), 420);
    let (east, offset) = stamped(&mut session);
    assert_eq!(offset, 420);
    // The instant is the instant either way: what a displacement moves
    // is the zone the statement reads it in, so the second reading is
    // later than the first by however long the two runs took and not
    // by seven hours.
    assert!(
        east >= utc && east - utc < 60 * 1_000_000_000,
        "{utc} {east}"
    );

    run(&mut session, "SESSION RESET TIME ZONE");
    assert_eq!(session.time_zone(), 0);
    assert_eq!(stamped(&mut session).1, 0);
    let refused = failure(&mut session, "SESSION SET TIME ZONE 'Europe/Dublin'");
    assert!(refused.contains("zone name"), "{refused}");
}

/// GS16, GS08 and GS04. The three widths of reset: one parameter, all
/// of them, and the session as it opened.
#[test]
fn a_reset_puts_back_what_it_names_and_no_more() {
    let (_dir, mut session) = opened("reset.zu1");
    run(&mut session, "SESSION SET VALUE $a = 1");
    run(&mut session, "SESSION SET VALUE $b = 2");
    run(&mut session, "SESSION SET TIME ZONE '-05:30'");
    assert_eq!(session.session_params().len(), 2);

    run(&mut session, "SESSION RESET PARAMETER $a");
    assert_eq!(session.session_params().len(), 1);
    assert_eq!(one(&mut session, "RETURN $b AS b"), Value::Int(2));
    // A name the session is not holding is not a refusal: afterwards it
    // holds nothing under that name either way.
    run(&mut session, "SESSION RESET PARAMETER $a");

    run(&mut session, "SESSION RESET ALL PARAMETERS");
    assert!(session.session_params().is_empty());
    // The zone is not a parameter, so resetting the parameters left it
    // where it was.
    assert_eq!(session.time_zone(), -330);

    run(&mut session, "SESSION SET VALUE $c = 3");
    run(&mut session, "SESSION RESET");
    assert!(session.session_params().is_empty());
    assert_eq!(session.time_zone(), 0);
    assert_eq!(session.session_schema(), "/");
}

/// A parameter the caller passes stands in front of one the session
/// holds, the way a definition at the head of a statement stands in
/// front of a graph the catalog holds under that name.
#[test]
fn a_passed_parameter_wins_over_a_held_one() {
    let (_dir, mut session) = opened("shadow.zu1");
    run(&mut session, "SESSION SET VALUE $n = 1");
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(1));
    let passed = session
        .run("RETURN $n AS n", &[("n", Value::Int(9))])
        .expect("run");
    assert_eq!(passed.rows.into_vec(), vec![vec![Value::Int(9)]]);
    // And the session still holds what it held: a statement reading
    // past a parameter does not take it away.
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(1));
}

/// A session parameter is worked out once, where it is set, so a write
/// after it does not change what it holds. That is the difference
/// between a session parameter and a view.
#[test]
fn a_parameter_holds_what_it_was_worked_out_to_and_not_the_query() {
    let (_dir, mut session) = opened("frozen.zu1");
    run(
        &mut session,
        "SESSION SET VALUE $n = { MATCH (p:person) RETURN count(*) AS n }",
    );
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(4));
    run(&mut session, "INSERT (:person {name: 'ed', age: 20})");
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(4));
    assert_eq!(
        rows(&mut session, "MATCH (p:person) RETURN count(*) AS n"),
        vec![vec![Value::Int(5)]]
    );
}

/// `IF NOT EXISTS` leaves a parameter the session already holds alone,
/// which is what the words say and is the one form that does not set
/// what it names.
#[test]
fn if_not_exists_leaves_a_parameter_the_session_already_holds() {
    let (_dir, mut session) = opened("ifnot.zu1");
    run(&mut session, "SESSION SET VALUE $n = 1");
    run(&mut session, "SESSION SET VALUE IF NOT EXISTS $n = 2");
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(1));
    run(&mut session, "SESSION SET VALUE $n = 2");
    assert_eq!(one(&mut session, "RETURN $n AS n"), Value::Int(2));
}

/// The graph a session works in is a reference and not a name, so it
/// survives an epoch, and what it does not survive is the graph being
/// dropped (plan/06 §2).
#[test]
fn the_working_graph_survives_an_epoch_and_a_dropped_one_is_refused() {
    let (_dir, mut session) = opened("droppedgraph.zu1");
    run(&mut session, "CREATE PROPERTY GRAPH scratch");
    run(&mut session, "SESSION SET PROPERTY GRAPH scratch");
    let moved = session.working_graph();
    // A catalog statement publishes a new epoch, and the session is
    // still working where it was put. Resolving the graph by name again
    // is what must not happen here: it would put a session that had
    // moved back home for no reason it could see.
    run(&mut session, "CREATE PROPERTY GRAPH other");
    assert_eq!(session.working_graph(), moved);

    run(&mut session, "DROP PROPERTY GRAPH scratch");
    let refused = failure(&mut session, "MATCH (p:person) RETURN count(*) AS n");
    assert!(refused.contains("42002"), "{refused}");
    assert!(refused.contains("has been dropped"), "{refused}");
    // A statement that names its own graph is unaffected, since it is
    // not asking the session where to run.
    run(&mut session, "USE other MATCH (n) RETURN count(*) AS n");
    // And the session is not stuck: the reset is the way back.
    run(&mut session, "SESSION RESET PROPERTY GRAPH");
    assert_eq!(
        rows(&mut session, "MATCH (p:person) RETURN count(*) AS n"),
        vec![vec![Value::Int(4)]]
    );
}

/// Session state is not transactional (ISO 7.1). A `SESSION SET` inside
/// a transaction that rolls back keeps what it set, because what it set
/// is not in the database.
#[test]
fn a_session_set_survives_a_rollback() {
    let (_dir, mut session) = opened("rollback.zu1");
    run(&mut session, "START TRANSACTION");
    run(&mut session, "SESSION SET VALUE $cut = 35");
    run(&mut session, "SESSION SET TIME ZONE '+02:00'");
    run(&mut session, "INSERT (:person {name: 'ed', age: 20})");
    run(&mut session, "ROLLBACK");
    // The insert went with the transaction and the two settings did
    // not, which is the whole of the rule.
    assert_eq!(
        one(&mut session, "MATCH (p:person) RETURN count(*) AS n"),
        Value::Int(4)
    );
    assert_eq!(one(&mut session, "RETURN $cut AS c"), Value::Int(35));
    assert_eq!(session.time_zone(), 120);
}

/// A held graph parameter whose graph has been dropped is a warning on
/// the next statement and a refusal on the statement that reads it,
/// which is the difference between being told and being stopped.
#[test]
fn a_held_graph_parameter_says_so_once_its_graph_is_dropped() {
    let (_dir, mut session) = opened("pinnedgraph.zu1");
    run(&mut session, "CREATE PROPERTY GRAPH scratch");
    run(&mut session, "SESSION SET PROPERTY GRAPH $g = scratch");
    let pins = session.pins();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].name, "g");
    assert_eq!(pins[0].kind, "GRAPH");
    assert_eq!(pins[0].stale, Stale::Fresh);
    assert_eq!(session.pinned_epoch(), Some(pins[0].epoch));

    run(&mut session, "DROP PROPERTY GRAPH scratch");
    assert_eq!(session.pins()[0].stale, Stale::Gone);
    // A statement that never mentions it still runs, and comes back
    // with the warning rather than with a refusal.
    let answered = session.run("RETURN 1 AS n", &[]).expect("run");
    assert_eq!(answered.rows.into_vec(), vec![vec![Value::Int(1)]]);
    assert_eq!(answered.notices.len(), 1);
    assert_eq!(answered.notices[0].status, codes::C01G03);
    assert!(answered.notices[0].severity().is_success());
    // The statement that reads it is refused.
    let refused = failure(&mut session, "USE $g MATCH (n) RETURN count(*) AS n");
    assert!(refused.contains("42002"), "{refused}");
    // And the session can put itself right.
    run(&mut session, "SESSION RESET PARAMETER $g");
    assert!(session.pins().is_empty());
    assert_eq!(session.pinned_epoch(), None);
}

/// A held binding table pins the epoch it was read at, and a table
/// holding element references stops meaning what it meant once the
/// snapshot under it moves.
///
/// The write here is a label set, because that is a write that folds.
/// An appended row is handed to the readers on a patch and leaves the
/// rows that were already there where they were, so a table taken
/// before one still names what it named.
#[test]
fn a_held_binding_table_pins_the_epoch_it_was_read_at() {
    let (_dir, mut session) = opened("pinnedtable.zu1");
    run(
        &mut session,
        "SESSION SET BINDING TABLE $t = { MATCH (p:person) RETURN p AS p }",
    );
    // A value parameter beside it, to show that a value pins nothing:
    // a number is a number at every epoch.
    run(&mut session, "SESSION SET VALUE $n = 1");
    let pins = session.pins();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].name, "t");
    assert_eq!(pins[0].kind, "BINDING TABLE");
    assert_eq!(pins[0].epoch, session.epoch());
    assert_eq!(pins[0].stale, Stale::Fresh);

    run(&mut session, "MATCH (p:person) WHERE p.age = 30 SET p:bot");
    assert_eq!(session.pins()[0].stale, Stale::Gone);
    // A statement that never reads it still runs, and comes back with
    // the warning rather than with a refusal.
    let answered = session.run("RETURN 1 AS n", &[]).expect("run");
    assert_eq!(answered.rows.into_vec(), vec![vec![Value::Int(1)]]);
    assert_eq!(answered.notices.len(), 1);
    assert_eq!(answered.notices[0].status, codes::C01000);
    assert!(answered.notices[0].severity().is_success());
    // The statement that reads it is refused.
    let refused = failure(&mut session, "FOR r IN $t RETURN count(*) AS n");
    assert!(refused.contains("older snapshot"), "{refused}");
}

/// A held table of scalars carries forward across an epoch, since
/// there is nothing in it that names a row, and the session mentions
/// it only once it has fallen behind the bound.
#[test]
fn a_held_table_of_scalars_is_only_mentioned_once_it_is_old() {
    let (_dir, mut session) = opened("oldtable.zu1");
    run(
        &mut session,
        "SESSION SET BINDING TABLE $t = { MATCH (p:person) RETURN p.name AS name }",
    );
    session.set_stale_bound(1);
    // Any statement that publishes moves the epoch, and a catalog one
    // moves it without touching a row, which is what makes it the
    // clean way to say "time has passed" in a test.
    run(&mut session, "CREATE PROPERTY GRAPH one");
    // One epoch behind is inside the bound, and nothing is said.
    let answered = session.run("RETURN 1 AS n", &[]).expect("run");
    assert!(answered.notices.is_empty());
    assert_eq!(
        one(&mut session, "FOR r IN $t RETURN count(*) AS n"),
        Value::Int(4)
    );

    run(&mut session, "CREATE PROPERTY GRAPH two");
    run(&mut session, "CREATE PROPERTY GRAPH three");
    assert_eq!(session.pins()[0].stale, Stale::Old);
    let answered = session.run("RETURN 1 AS n", &[]).expect("run");
    assert_eq!(answered.notices.len(), 1);
    assert_eq!(answered.notices[0].status, codes::C01000);
    // Old is not gone: the rows say what they said, and reading them
    // is no error.
    assert_eq!(
        one(&mut session, "FOR r IN $t RETURN count(*) AS n"),
        Value::Int(4)
    );
}
