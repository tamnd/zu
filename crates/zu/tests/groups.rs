//! Quantified path patterns and group variables: ISO 16.11 and 22.7,
//! features G035, GQ17 and GE09.
//!
//! A quantifier behind a parenthesized path pattern repeats the stretch
//! the brackets hold, so a pattern of one edge written `{2}` walks two.
//! What that does to the names inside the brackets is the rest of it:
//! every repetition binds them again, so a name there stands for one
//! element per repetition rather than for one element, and that is a
//! group variable. Reading one answers the list, reading a property of
//! one answers the list of the properties, and an aggregate around one
//! folds that row's group rather than the rows.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A chain of five, so a stretch repeated twice has somewhere to go and
/// a stretch repeated three times still has:
///
/// ```text
/// 0 -> 1 -> 2 -> 3 -> 4
/// ```
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("groups.zu1")).unwrap();
    let mut edges = vec![(0, 1), (1, 2), (2, 3), (3, 4)];
    edges.sort_unstable();
    bulk_load_as(&mut zu, "person", "knows", 5, &edges).unwrap();
    zu
}

/// A chain with a loop at the end of it, so a stretch repeated twice
/// has an edge it could take twice:
///
/// ```text
/// 0 -> 1 -> 2 -> 2
/// ```
fn looped(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("looped.zu1")).unwrap();
    let mut edges = vec![(0, 1), (1, 2), (2, 2)];
    edges.sort_unstable();
    bulk_load_as(&mut zu, "person", "knows", 3, &edges).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn count(db: &mut Zu1File, source: &str) -> i64 {
    match one(db, source) {
        Value::Int(n) => n,
        other => panic!("{source} answered {other:?}"),
    }
}

fn refusal(db: &mut Zu1File, source: &str) -> String {
    run(source, db, &[]).expect_err(source).to_string()
}

/// A quantifier behind the brackets repeats what they hold, so the
/// pattern walks as many edges as the count asks for and ends where a
/// pattern written out that many times would have ended.
#[test]
fn a_quantifier_repeats_the_stretch_the_brackets_hold() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN b.id AS n";
    assert_eq!(count(&mut db, source), 2);
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){3}(b:person) \
                  WHERE a.id = 0 RETURN b.id AS n";
    assert_eq!(count(&mut db, source), 3);
    // The same walk written out, which is what the repetition stands
    // for and what it has to answer.
    let source = "MATCH (a:person)-[:knows]->(:person)-[:knows]->(b:person) \
                  WHERE a.id = 0 RETURN b.id AS n";
    assert_eq!(count(&mut db, source), 2);
}

/// A name inside a repeated stretch stands for one element per
/// repetition, so reading it answers the list of them in the order the
/// walk took them.
#[test]
fn a_name_inside_a_repeated_stretch_stands_for_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN y.id AS steps";
    assert_eq!(
        one(&mut db, source),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    // The other end of each repetition, which is the same walk read one
    // node earlier.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN x.id AS steps";
    assert_eq!(
        one(&mut db, source),
        Value::List(vec![Value::Int(0), Value::Int(1)])
    );
    // The group is a list, so the functions that count a list count it.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){3}(b:person) \
                  WHERE a.id = 0 RETURN CARDINALITY(y) AS n";
    assert_eq!(count(&mut db, source), 3);
}

/// An edge pattern inside a repeated stretch names a group the same way
/// a node pattern does, and what it gathers is edges.
#[test]
fn an_edge_inside_a_repeated_stretch_names_a_group_of_edges() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[e:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN SIZE(e) AS n";
    assert_eq!(count(&mut db, source), 2);
}

/// An aggregate written around a group variable folds the group rather
/// than the rows, which is the horizontal aggregate: one row in and one
/// row out, and the answer is the fold of what that row bound.
#[test]
fn an_aggregate_over_a_group_folds_that_rows_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN SUM(y.id) AS total";
    assert_eq!(count(&mut db, source), 3);
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN MIN(y.id) AS low";
    assert_eq!(count(&mut db, source), 1);
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN MAX(y.id) AS high";
    assert_eq!(count(&mut db, source), 2);
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 RETURN COUNT(y.id) AS n";
    assert_eq!(count(&mut db, source), 2);
    // It is an expression and not an aggregate, so nothing groups for
    // it: the rows are the rows the match answered, one per walk.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 3);
    let result = run(
        "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) RETURN SUM(y.id) AS total",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(result.rows.len(), 3, "one row per walk, not one in all");
}

/// A group projected under a name is a list from there on, so a clause
/// behind the projection reads a list and not a group.
#[test]
fn a_projected_group_carries_on_as_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 0 WITH y.id AS steps RETURN CARDINALITY(steps) AS n";
    assert_eq!(count(&mut db, source), 2);
}

/// A repeated stretch walks a trail, so the copies of one step do not
/// all take one edge, which is what a loop in the graph would otherwise
/// let them do. It is the same answer the standard's own repeated step
/// gives, and the mode the pattern walks under is what changes it.
#[test]
fn a_repeated_stretch_does_not_take_one_edge_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = looped(dir.path());
    // The loop at node 2, which two repetitions could take twice.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 2 RETURN count(*) AS n";
    assert_eq!(count(&mut db, source), 0);
    // Written out as the standard's repeated step, which is the answer
    // the stretch has to agree with.
    let source = "MATCH (a:person)-[:knows*2..2]->(b:person) WHERE a.id = 2 RETURN count(*) AS n";
    assert_eq!(count(&mut db, source), 0);
    // A walk repeats what it likes, whether the pattern says so or the
    // match mode does.
    let source = "MATCH WALK (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 2 RETURN count(*) AS n";
    assert_eq!(count(&mut db, source), 1);
    let source = "MATCH REPEATABLE ELEMENTS (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) \
                  WHERE a.id = 2 RETURN count(*) AS n";
    assert_eq!(count(&mut db, source), 1);
    // The walks the whole graph answers, which are 0 to 1 to 2 and the
    // two ways of reaching the loop and leaving it again.
    let source =
        "MATCH (a:person)((x:person)-[:knows]->(y:person)){2}(b:person) RETURN count(*) AS n";
    assert_eq!(count(&mut db, source), 2);
}

/// What a quantified stretch refuses. Two of them are the shapes that
/// wait on a union of patterns, and the others are the ones where a name
/// would have to stand for something this engine has no value for.
#[test]
fn what_a_quantified_stretch_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    // A range repeats the stretch a variable number of times, so the
    // pattern matches paths of several lengths.
    let source =
        "MATCH (a:person)((x:person)-[:knows]->(y:person)){1,3}(b:person) RETURN a.id AS n";
    let said = refusal(&mut db, source);
    assert!(said.contains("union of patterns"), "got: {said}");
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person)){0}(b:person) RETURN a.id AS n";
    let said = refusal(&mut db, source);
    assert!(said.contains("leaves nothing"), "got: {said}");
    // A name on the repeated stretch itself would be a name for as many
    // paths as the count asks for.
    let source =
        "MATCH (a:person)(p = (x:person)-[:knows]->(y:person)){2}(b:person) RETURN a.id AS n";
    let said = refusal(&mut db, source);
    assert!(said.contains("walked once per repetition"), "got: {said}");
    // A condition inside the brackets is asked per repetition, and what
    // its names stand for there is that repetition's element.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person) WHERE x.id < y.id){2}(b:person) \
                  RETURN a.id AS n";
    let said = refusal(&mut db, source);
    assert!(said.contains("once per repetition"), "got: {said}");
    // A name a repetition binds cannot also be one the pattern already
    // bound to one element.
    let source = "MATCH (y:person)((x:person)-[:knows]->(y:person)){2}(b:person) RETURN b.id AS n";
    let said = refusal(&mut db, source);
    assert!(
        said.contains("already stands for one element"),
        "got: {said}"
    );
    // An INSERT writes the elements rather than walking them.
    let source = "INSERT ((a:person)-[:knows]->(b:person)){2}";
    let said = refusal(&mut db, source);
    assert!(said.contains("writing the elements"), "got: {said}");
}
