//! The graph a statement runs against, written as a reference (GQ01,
//! ISO 16.2).
//!
//! `USE` has always taken a name, and a name is a thing the catalog
//! looks up while the statement is being read. A reference is not: the
//! caller holds a graph and hands it in, so which graph the statement
//! is against is settled when the parameter arrives. That is what is
//! checked here, together with the two words a session has for the
//! graph it is working in and the graph it started in.
//!
//! The half that is easy to get wrong is the plan cache. A plan is
//! against one graph's tables, and two calls of one text can name two
//! graphs, so both orders are run below on the same session.

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu_query::refs::GraphHandle;

const NODES: u32 = 5;

/// A session on a file whose home graph holds five people in a ring,
/// and a second graph `twin` holding those five and one more. The
/// count tells the two apart, which is all any of these queries asks.
fn opened(name: &str) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    // A property column, because a row an INSERT adds grows the
    // columns the table has and a table with none has nowhere to put
    // one.
    let ids: Vec<u64> = (0..NODES.into()).collect();
    zu::zu1::props::store_props(
        &mut db,
        "person",
        &[("id", zu::zu1::props::PropValues::Int(&ids))],
    )
    .expect("props");
    drop(db);
    let mut session = Session::open(&path).expect("open");
    session
        .run(
            "CREATE GRAPH twin ANY AS COPY OF CURRENT_PROPERTY_GRAPH",
            &[],
        )
        .expect("the copy");
    session
        .run("USE twin INSERT (x:person {id: 99})", &[])
        .expect("one more person in the copy");
    (dir, session)
}

fn count(session: &mut Session, source: &str, params: &[(&str, Value)]) -> i64 {
    let result = session
        .run(source, params)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    match &result.rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected a count, got {other:?}"),
    }
}

fn refused(session: &mut Session, source: &str, params: &[(&str, Value)]) -> String {
    let err = session
        .run(source, params)
        .expect_err("this one does not run");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002", "{}", record.detail);
    record.detail.clone()
}

const COUNT: &str = "USE $g MATCH (p:person) RETURN count(*) AS n";

/// The whole of the form: the caller holds two graphs, and one text
/// runs against whichever of them the parameter named.
#[test]
fn a_use_takes_the_graph_the_caller_passed_in() {
    let (_dir, mut session) = opened("use-param.zu1");
    let home = session.graph_ref("/", "home").expect("the home graph");
    let twin = session.graph_ref("/", "twin").expect("the copy");

    assert_eq!(count(&mut session, COUNT, &[("g", home.clone())]), 5);
    assert_eq!(count(&mut session, COUNT, &[("g", twin.clone())]), 6);
    // The other order, on the same session, because the first call is
    // the one that compiles and the second is the one a cache holding
    // the plan under the text alone would answer wrongly.
    assert_eq!(count(&mut session, COUNT, &[("g", home)]), 5);
    assert_eq!(count(&mut session, COUNT, &[("g", twin)]), 6);
}

/// A statement that writes names its graph the same way, and writes to
/// the one it named.
#[test]
fn a_write_reaches_the_graph_the_parameter_named() {
    let (_dir, mut session) = opened("use-param-write.zu1");
    let twin = session.graph_ref("/", "twin").expect("the copy");
    session
        .run("USE $g INSERT (x:person {id: 100})", &[("g", twin.clone())])
        .expect("the write");
    assert_eq!(count(&mut session, COUNT, &[("g", twin)]), 7);
    assert_eq!(
        count(&mut session, "MATCH (p:person) RETURN count(*) AS n", &[]),
        5,
        "the graph the session is working in is untouched"
    );
}

/// The two words for the graph a session is in. Nothing moves the
/// working graph yet, so they name the same graph here; they are
/// separate because one of them is the graph the session started in
/// and the other is the graph it is in now.
#[test]
fn the_words_for_the_current_and_the_home_graph_both_name_it() {
    let (_dir, mut session) = opened("use-home.zu1");
    let both = [
        "USE HOME_PROPERTY_GRAPH MATCH (p:person) RETURN count(*) AS n",
        "USE HOME_GRAPH MATCH (p:person) RETURN count(*) AS n",
    ];
    for source in both {
        assert_eq!(count(&mut session, source, &[]), 5, "{source}");
    }
}

/// A graph is what the parameter has to hold. A string that happens to
/// be a graph's name is not a reference to it, and the message says
/// where a reference comes from.
#[test]
fn a_use_parameter_that_is_not_a_graph_is_refused() {
    let (_dir, mut session) = opened("use-wrong-type.zu1");
    let detail = refused(&mut session, COUNT, &[("g", Value::Str("twin".into()))]);
    assert!(detail.contains("has to be a graph reference"), "{detail}");
    assert!(
        detail.contains("graph_ref"),
        "{detail}, want where one comes from"
    );
}

/// The graph is named by a parameter, so a call that passed none named
/// no graph, and that is a reference resolving to nothing rather than a
/// statement with something wrong with it.
#[test]
fn a_use_parameter_that_was_not_given_is_refused() {
    let (_dir, mut session) = opened("use-missing.zu1");
    let detail = refused(&mut session, COUNT, &[]);
    assert!(detail.contains("$g"), "{detail}, want the name");
}

/// A handle outlives the graph it names. Using one afterwards is
/// refused rather than run against whatever holds that id now, which
/// is why the handle is checked against the catalog and not merely
/// read for its id.
#[test]
fn a_handle_to_a_graph_that_is_gone_is_refused() {
    let (_dir, mut session) = opened("use-dropped.zu1");
    let twin = session.graph_ref("/", "twin").expect("the copy");
    session.run("DROP GRAPH twin", &[]).expect("drop it");
    let detail = refused(&mut session, COUNT, &[("g", twin)]);
    assert!(detail.contains("GRAPH /twin"), "{detail}, want which graph");

    let ghost = Value::Graph(GraphHandle::new(4242, "/", "twin", 0));
    refused(&mut session, COUNT, &[("g", ghost)]);
}

/// The graph a copy starts from is written the same way a `USE` writes
/// the graph to read, so it takes the same reference.
#[test]
fn a_copy_starts_from_the_graph_a_parameter_names() {
    let (_dir, mut session) = opened("use-copy-of.zu1");
    let twin = session.graph_ref("/", "twin").expect("the copy");
    session
        .run("CREATE GRAPH third ANY AS COPY OF $g", &[("g", twin)])
        .expect("a copy of the copy");
    let third = session.graph_ref("/", "third").expect("the second copy");
    assert_eq!(count(&mut session, COUNT, &[("g", third)]), 6);
}
