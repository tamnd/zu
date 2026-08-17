//! Reading a plan and reading a profile, the structured halves of
//! EXPLAIN and EXPLAIN ANALYZE that `dx/04` §4 asks the Rust SDK for.
//!
//! What is checked here is that a caller never has to parse a listing:
//! the tree carries the operators, the columns and the parameters, the
//! profile carries the counters per operator per stage, and the two
//! renderings are those structures printed rather than a second
//! description of them.

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Config, Database};

const NODES: u32 = 200;

fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| [(i, (i + 1) % NODES), (i, (i + 3) % NODES)])
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

fn opened(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    seeded(&path);
    let db = Database::open(&path).expect("open");
    (dir, db)
}

#[test]
fn a_plan_reads_as_operators_and_prints_as_the_listing() {
    let (_dir, db) = opened("plan.zu1");
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person)-[r:knows]->(f) WHERE p.id > $floor RETURN f.id AS friend";
    let plan = conn.explain_plan(source).expect("plan");
    assert_eq!(plan.columns, ["friend"]);
    assert_eq!(plan.params, ["floor"]);
    assert_eq!(plan.render(), conn.explain(source).expect("explain"));

    let root = plan.root.as_ref().expect("a statement with operators");
    assert_eq!(root.op, "Project");
    assert_eq!(root.binds, ["friend"]);

    // Every operator of the tree, top down, which is the order the
    // listing prints them in and the order a viewer draws them in.
    let mut ops = Vec::new();
    root.walk(&mut |node, depth| ops.push((depth, node.op)));
    assert_eq!(
        ops,
        [
            (0, "Project"),
            (1, "Expand"),
            (2, "Filter"),
            (3, "ScanNodes"),
        ],
        "got {ops:?}"
    );
    assert_eq!(root.count(), 4);

    // The tables an operator touches are a list rather than a piece of
    // its printed line, which is the difference this API exists for.
    let mut tables: Vec<String> = Vec::new();
    root.walk(&mut |node, _| tables.extend(node.tables.iter().cloned()));
    assert_eq!(tables, ["knows", "person"]);
}

/// The three things a plan can be asked for that the listing answers
/// only by being read: a statement with no operators, a statement whose
/// columns are not its variables, and a write, which explains as the
/// whole statement even though it runs as parts.
#[test]
fn the_edges_of_a_plan_are_still_a_plan() {
    let (_dir, db) = opened("edges.zu1");
    let mut conn = db.connect().expect("connect");

    let plan = conn.explain_plan("RETURN 1 AS one").expect("plan");
    let root = plan.root.expect("a projection is an operator");
    assert_eq!(root.op, "Project");
    assert!(root.children.is_empty(), "the starting row is no operator");
    assert_eq!(plan.columns, ["one"]);

    let plan = conn
        .explain_plan("MATCH (p:person) RETURN count(p) AS n")
        .expect("plan");
    let root = plan.root.expect("a statement with operators");
    assert_eq!(root.op, "Aggregate");
    assert_eq!(root.binds, ["n"]);

    let plan = conn
        .explain_plan("MATCH (p:person) WHERE p.id = 1 INSERT (p)-[:knows]->(q:person)")
        .expect("plan");
    let mut ops = Vec::new();
    plan.root
        .expect("a statement with operators")
        .walk(&mut |node, _| ops.push(node.op));
    assert!(ops.contains(&"Insert"), "got {ops:?}");
}

#[test]
fn a_profile_counts_what_each_operator_did() {
    let (_dir, db) = opened("profile.zu1");
    let mut conn = db.connect().expect("connect");

    let profile = conn
        .profile(
            "MATCH (p:person) WHERE p.id > $floor RETURN p.id AS id",
            &[("floor", Value::Int(i64::from(NODES) - 11))],
        )
        .expect("profile");
    assert_eq!(profile.stages.len(), 1);
    let stage = &profile.stages[0];
    assert_eq!(stage.out_rows, 10);

    // The operator kind is a field, so grouping by it needs no parsing,
    // and the detail beside it is what the scan is reading.
    let scan = stage
        .ops
        .iter()
        .find(|op| op.kind == "Scan")
        .expect("a scan");
    assert_eq!(scan.detail, "p: person");
    assert_eq!(scan.name(), "Scan p: person");
    assert_eq!(scan.flat, u64::from(NODES));
    assert!(scan.pulls > 0);

    let filter = stage
        .ops
        .iter()
        .find(|op| op.kind == "Filter")
        .expect("a filter");
    assert_eq!(filter.flat, 10);
    // An estimate that is there is comparable with what happened, which
    // is the number a caller is profiling to see.
    if let Some(q) = filter.qerror() {
        assert!(q >= 1.0, "a q-error is a ratio at or above one, got {q}");
    }
    assert!(
        !stage.ops.iter().any(|op| op.bound_violation()),
        "a ceiling the optimizer set was passed"
    );

    // The rendering is the counters printed, so what it says is what
    // the fields say.
    let text = profile.render();
    assert!(text.contains("Scan p: person"), "got:\n{text}");
    assert!(text.contains("10 rows"), "got:\n{text}");
}

#[test]
fn a_write_cannot_be_profiled_and_a_read_only_connection_says_so_first() {
    let (_dir, db) = opened("refused.zu1");
    let mut conn = db.connect().expect("connect");

    // A write runs as the parts it was split at its write into, and
    // profiling it would apply the write besides.
    let err = conn
        .profile("INSERT (p:person {id: 1})", &[])
        .expect_err("a write has no one plan to profile");
    assert!(err.to_string().contains("profiling a statement"), "{err}");

    let path = db.path().to_path_buf();
    let ro = Database::open_with(&path, Config::new().read_only(true)).expect("open");
    let mut ro = ro.connect().expect("connect");
    let err = ro
        .profile("CREATE PROPERTY GRAPH second ANY", &[])
        .expect_err("read-only refuses a statement that writes");
    assert!(err.to_string().contains("read-only"), "{err}");

    // Reading a plan is not running it, and profiling a read is a read,
    // so both are what a connection that cannot write is for.
    let plan = ro
        .explain_plan("MATCH (p:person) RETURN p.id AS id")
        .expect("plan");
    assert_eq!(plan.columns, ["id"]);
    let profile = ro
        .profile("MATCH (p:person) RETURN p.id AS id", &[])
        .expect("profile");
    assert_eq!(profile.stages[0].out_rows, u64::from(NODES));
}
