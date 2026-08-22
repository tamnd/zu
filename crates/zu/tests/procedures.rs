//! What a named procedure may be handed: a graph to read (GP15) and a
//! binding table to read arguments out of (GP14), ISO 13.1.
//!
//! Both are the same idea from two sides. A procedure takes values,
//! and the two things GQL has that are not values, a graph and a
//! table of rows, are the two things it takes here. What makes them
//! worth a test of their own is when they are settled: which graph a
//! call reads and which rel table it walks are settled while the
//! statement is being bound, so a graph has to arrive as a reference,
//! while a binding table is read when the call runs and may be
//! anything a query answered.

use zu::query::Value;
use zu::session::Session;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A session on a file whose home graph holds five people in a ring of
/// `knows`, and a second graph `twin` holding the same ring with one
/// person taken out of it. The count is what tells the two apart, and
/// it is all any of these queries asks.
fn opened(name: &str) -> (tempfile::TempDir, Session) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    // A property column, because an INSERT adds a row to the columns
    // the table has and a table with none has nowhere to put one.
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
        .run(
            "USE twin MATCH (p:person) WHERE p.id = 0 DETACH DELETE p",
            &[],
        )
        .expect("one person fewer in the copy");
    (dir, session)
}

fn count(session: &mut Session, source: &str) -> i64 {
    let result = session
        .run(source, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    match &result.rows[0][0] {
        Value::Int(n) => *n,
        other => panic!("expected a count, got {other:?}"),
    }
}

fn refused(session: &mut Session, source: &str) -> String {
    let err = session.run(source, &[]).expect_err("this one does not run");
    match err.diagnostic() {
        Some(record) => record.detail.clone(),
        None => err.to_string(),
    }
}

const HOME: &str = "CALL wcc('knows') YIELD node, component RETURN count(*) AS n";
const TWIN: &str = "CALL wcc(/twin, 'knows') YIELD node, component RETURN count(*) AS n";

/// The whole of GP15: a graph written in front of the arguments is the
/// graph the procedure reads, and the rel table named after it is that
/// graph's table rather than one of the statement's.
#[test]
fn a_procedure_reads_the_graph_it_was_handed() {
    let (_dir, mut session) = opened("proc-graph.zu1");
    assert_eq!(count(&mut session, HOME), 5);
    assert_eq!(count(&mut session, TWIN), 4);
    // Both orders on the one session, since the first call is the one
    // that compiles and a cache holding the plan under the text alone
    // would answer the second out of the wrong graph.
    assert_eq!(count(&mut session, TWIN), 4);
    assert_eq!(count(&mut session, HOME), 5);
}

/// The graph the statement is already running against, named. It is
/// the call that named no graph, said out loud, and it answers what
/// that call answers.
#[test]
fn naming_the_graph_the_statement_is_in_is_the_call_that_named_none() {
    let (_dir, mut session) = opened("proc-current.zu1");
    let named = "CALL wcc(CURRENT_PROPERTY_GRAPH, 'knows') YIELD node, component \
                 RETURN count(*) AS n";
    assert_eq!(count(&mut session, named), 5);
    assert_eq!(count(&mut session, HOME), 5);
}

/// A graph variable is a name for a graph, so a procedure takes one
/// where it takes a reference, which is GP12 and GP15 written
/// together.
#[test]
fn a_procedure_takes_a_graph_a_name_stands_for() {
    let (_dir, mut session) = opened("proc-graph-var.zu1");
    let source = "GRAPH g = /twin \
                  CALL wcc(g, 'knows') YIELD node, component RETURN count(*) AS n";
    assert_eq!(count(&mut session, source), 4);
}

/// A graph named in front of the arguments answers what running the
/// same call inside that graph answers, row for row. That is the whole
/// claim the feature makes, and the rows say it where a count could be
/// right for the wrong reason.
#[test]
fn the_answer_is_the_one_the_graph_would_have_given_from_inside() {
    let (_dir, mut session) = opened("proc-graph-answer.zu1");
    let yields = "YIELD node, component RETURN node.id AS id, component ORDER BY id";
    let handed = session
        .run(&format!("CALL wcc(/twin, 'knows') {yields}"), &[])
        .expect("wcc over the graph it was handed");
    let inside = session
        .run(&format!("USE twin CALL wcc('knows') {yields}"), &[])
        .expect("wcc from inside that graph");
    assert_eq!(handed.rows.len(), 4, "the person taken out is not answered");
    assert_eq!(handed.rows.to_vec(), inside.rows.to_vec());
}

/// A graph a query answered is refused rather than read, because which
/// rel table the call walks is settled while the statement is bound
/// and a query has not run by then. The refusal says so.
///
/// The clause in the query is what makes it a run rather than a
/// reading: a body that is a bare `RETURN` of a graph reference says
/// which graph in its text and is read where a reference is read, so
/// it is the wrong query to write a refusal against.
#[test]
fn a_graph_only_a_run_would_know_is_refused_by_name() {
    let (_dir, mut session) = opened("proc-graph-late.zu1");
    let detail = refused(
        &mut session,
        "GRAPH g = { MATCH (p:person) RETURN CURRENT_PROPERTY_GRAPH AS g LIMIT 1 } \
         CALL wcc(g, 'knows') YIELD node, component RETURN count(*) AS n",
    );
    assert!(
        detail.contains("defined as a graph reference"),
        "the refusal says what to write instead: {detail}"
    );
}

/// A rel table the named graph does not hold is refused with the graph
/// named, not looked for in the graph the statement happens to be in.
#[test]
fn a_rel_table_is_looked_for_in_the_graph_that_was_named() {
    let (_dir, mut session) = opened("proc-graph-rel.zu1");
    let detail = refused(
        &mut session,
        "CALL wcc(/twin, 'nonsense') YIELD node, component RETURN count(*) AS n",
    );
    assert!(
        detail.contains("'twin' holds no rel table 'nonsense'"),
        "the refusal names the graph it looked in: {detail}"
    );
}

/// GP14. The sources of a centrality run are a sample, and a sample is
/// the sort of thing a query picks, so a binding table stands where
/// the list stands and answers the same numbers.
#[test]
fn a_binding_table_stands_where_a_list_of_arguments_stands() {
    let (_dir, mut session) = opened("proc-table-arg.zu1");
    let written = "CALL betweenness('knows', [0, 1]) YIELD node, centrality \
                   RETURN node.id AS id, centrality ORDER BY id";
    let queried = "BINDING TABLE seeds = { MATCH (p:person) WHERE p.id < 2 RETURN p.id AS id } \
                   CALL betweenness('knows', seeds) YIELD node, centrality \
                   RETURN node.id AS id, centrality ORDER BY id";
    let by_hand = session.run(written, &[]).expect("the written sample");
    let by_query = session.run(queried, &[]).expect("the queried sample");
    assert_eq!(by_hand.rows.len(), NODES as usize);
    assert_eq!(by_hand.rows.to_vec(), by_query.rows.to_vec());
}

/// A binding table of more than one column is refused. The sources are
/// node ids and nothing says which of two columns holds them, so
/// reading the first would be a sample nobody asked for.
#[test]
fn a_binding_table_of_the_wrong_shape_is_refused() {
    let (_dir, mut session) = opened("proc-table-shape.zu1");
    let detail = refused(
        &mut session,
        "BINDING TABLE seeds = { MATCH (p:person) RETURN p.id AS id, p.id AS again } \
         CALL betweenness('knows', seeds) YIELD node, centrality RETURN count(*) AS n",
    );
    assert!(
        detail.contains("one column of node ids"),
        "the refusal says what shape it wanted: {detail}"
    );
}
