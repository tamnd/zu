//! Parenthesized path patterns: ISO 16.11, feature G038.
//!
//! Brackets around part of a pattern match nothing of their own. What
//! they do is let three things be said about a stretch of a walk rather
//! than about the whole of it: a name for the stretch, a path mode for
//! it, and a condition that has to hold of it. The condition may read a
//! variable bound outside the brackets, which is what makes it a non
//! local predicate.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A chain with a way back from the middle:
///
/// ```text
/// 0 -> 1        the chain
/// 1 -> 2
/// 2 -> 3
/// 1 -> 0        the way back, so a walk may go round and a trail may not
/// ```
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("subpaths.zu1")).unwrap();
    let mut edges = vec![(0, 1), (1, 2), (2, 3), (1, 0)];
    edges.sort_unstable();
    bulk_load_as(&mut zu, "person", "knows", 4, &edges).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

/// One number the statement answered, which is how most of these read:
/// the same graph twice over, and more or fewer ways to fill a pattern.
fn count(db: &mut Zu1File, source: &str) -> i64 {
    match one(db, source) {
        Value::Int(n) => n,
        other => panic!("{source} answered {other:?}"),
    }
}

/// What the engine said about a statement it refused.
fn refusal(db: &mut Zu1File, source: &str) -> String {
    run(source, db, &[]).expect_err(source).to_string()
}

/// Brackets around a whole pattern say nothing, so the pattern answers
/// what it answered without them. It is worth a test of its own because
/// everything else here rests on it: the brackets are where the reading
/// of a pattern changes and not what it walks.
#[test]
fn brackets_around_a_pattern_leave_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let plain = "MATCH (a:person)-[:knows]->(b:person)-[:knows]->(c:person) RETURN COUNT(*) AS n";
    let bracketed =
        "MATCH ((a:person)-[:knows]->(b:person)-[:knows]->(c:person)) RETURN COUNT(*) AS n";
    let walked = count(&mut db, plain);
    assert!(walked > 0);
    assert_eq!(count(&mut db, bracketed), walked);
    // Brackets around a stretch of it, and brackets inside those.
    let nested = "MATCH (a:person)-[:knows]->((b:person)-[:knows]->(c:person)) \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, nested), walked);
}

/// A subpath variable binds a path over the stretch its brackets hold,
/// which is what makes it different from the path variable in front of
/// the whole pattern: the two are bound to the same walk, measured over
/// different parts of it.
#[test]
fn a_subpath_variable_binds_the_stretch_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH p = (a:person)-[:knows]->(b:person) (q = (b)-[:knows]->(c:person)) \
                  WHERE a.id = 0 AND c.id = 2 RETURN PATH_LENGTH(p) AS n";
    assert_eq!(count(&mut db, source), 2);
    let source = "MATCH p = (a:person)-[:knows]->(b:person) (q = (b)-[:knows]->(c:person)) \
                  WHERE a.id = 0 AND c.id = 2 RETURN PATH_LENGTH(q) AS n";
    assert_eq!(count(&mut db, source), 1);
    // The value is a path and not the list of the same elements, the
    // same as the one a path variable binds.
    let source = "MATCH (a:person)-[:knows]->(b:person) (q = (b)-[:knows]->(c:person)) \
                  WHERE a.id = 0 AND c.id = 2 RETURN (q IS TYPED PATH) AS v";
    assert_eq!(one(&mut db, source), Value::Bool(true));
    // Brackets around one node hold a stretch of no edges, and a path of
    // no edges is a path.
    let source = "MATCH (p = (a:person)) WHERE a.id = 0 RETURN PATH_LENGTH(p) AS n";
    assert_eq!(count(&mut db, source), 0);
}

/// A path mode written inside the brackets speaks about the stretch
/// they hold, so an outer walk may hold an inner trail. The graph has a
/// pair of nodes with an edge each way, which is what a walk may go
/// round twice and a trail may not.
#[test]
fn a_path_mode_on_a_subpath_speaks_about_that_stretch() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let walk = "MATCH WALK (a:person)-[e:knows*1..4]->(b:person) RETURN COUNT(*) AS n";
    let trail = "MATCH TRAIL (a:person)-[e:knows*1..4]->(b:person) RETURN COUNT(*) AS n";
    let inner = "MATCH WALK (TRAIL (a:person)-[e:knows*1..4]->(b:person)) RETURN COUNT(*) AS n";
    let (walked, trailed) = (count(&mut db, walk), count(&mut db, trail));
    assert!(trailed < walked, "the way back gives a walk laps to take");
    assert_eq!(count(&mut db, inner), trailed);
    // The tightest brackets around a step are the ones that speak about
    // it, so the mode on the inner stretch stands where the outer one
    // said something else.
    let source = "MATCH TRAIL ((a:person)-[e:knows*1..4]->(b:person)) RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), trailed);
}

/// A `WHERE` inside the brackets is a condition on the stretch, and it
/// decides the match rather than filtering behind it. For a match that
/// is not optional those are the same rows, which is what this checks:
/// the condition written inside and the condition written after answer
/// alike.
#[test]
fn a_condition_inside_the_brackets_holds_of_the_stretch() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let inside = "MATCH ((a:person)-[:knows]->(b:person) WHERE a.id < b.id) RETURN COUNT(*) AS n";
    let after = "MATCH (a:person)-[:knows]->(b:person) WHERE a.id < b.id RETURN COUNT(*) AS n";
    let held = count(&mut db, after);
    assert!(held > 0);
    assert_eq!(count(&mut db, inside), held);
    // Two brackets, each with a condition of its own, and the clause
    // writing a third: the three fold together with AND.
    let source = "MATCH ((a:person)-[:knows]->(b:person) WHERE a.id < b.id) \
                  ((b)-[:knows]->(c:person) WHERE b.id < c.id) \
                  WHERE c.id = 3 RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 1);
}

/// A condition inside the brackets may read a variable bound outside
/// them, which is the non local predicate: what it says about the
/// stretch is said in terms of the walk that reached it.
#[test]
fn a_condition_inside_the_brackets_may_read_what_is_outside_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)-[:knows]->(b:person) ((b)-[:knows]->(c:person) WHERE c.id > a.id) \
                  RETURN COUNT(*) AS n";
    let after = "MATCH (a:person)-[:knows]->(b:person)-[:knows]->(c:person) WHERE c.id > a.id \
                 RETURN COUNT(*) AS n";
    let held = count(&mut db, after);
    assert!(held > 0);
    assert_eq!(count(&mut db, source), held);
}

/// Two names written where two stretches meet are two names for one
/// node, which is the shape the standard's own example of the feature is
/// written in. Both of them stand for that node from there on, and what
/// each stretch asked of it holds of it.
#[test]
fn two_stretches_meeting_name_one_node_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person))(b:person) \
                  WHERE a.id = 0 RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 1);
    // The names meet at a node each, so `a` and `x` are one node and `y`
    // and `b` are the other, and either name reads it.
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person))(b:person) \
                  WHERE a.id = 0 RETURN x.id + b.id AS n";
    assert_eq!(count(&mut db, source), 1);
    let source = "MATCH (a:person)((x:person)-[:knows]->(y:person))(b:person) \
                  WHERE x.id = 0 AND y.id = b.id RETURN a.id AS n";
    assert_eq!(count(&mut db, source), 0);
}

/// The two things the brackets may not hold. A path selector says how
/// many of the paths a pattern matches to keep, which is a question
/// about the answer rather than about a stretch of the walk. And a name
/// that already stands for something else cannot also stand for the node
/// two stretches meet at.
#[test]
fn what_the_brackets_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "MATCH (p = ANY SHORTEST (a:person)-[e:knows*1..4]->(b:person)) \
                  RETURN COUNT(*) AS n";
    let said = refusal(&mut db, source);
    assert!(said.contains("in front of the pattern"), "got: {said}");
    let source = "MATCH (z:person)-[:knows]->(a:person), (b:person)((a:person)-[:knows]->(c:person)) \
                  RETURN COUNT(*) AS n";
    let said = refusal(&mut db, source);
    assert!(
        said.contains("already stands for something else"),
        "got: {said}"
    );
    // An INSERT writes the elements rather than walking them, so a
    // stretch of a walk is nothing it can name.
    let source = "INSERT ((a:person)-[:knows]->(b:person))";
    let said = refusal(&mut db, source);
    assert!(said.contains("writing the elements"), "got: {said}");
}
