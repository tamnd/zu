//! The element pattern predicate: ISO 16.6, feature G041.
//!
//! A `WHERE` written inside an element pattern is asked of the one
//! element the pattern is standing on, where the pattern reaches it,
//! rather than of the row a whole pattern built. Everything the pattern
//! bound to its left is in scope inside it, which is what makes it the
//! non local predicate: the node just reached can be compared with the
//! node the walk came from.

use zu::Database;

/// A chain of four steps carrying the numbers that let a predicate
/// compare one node with another:
///
/// ```text
/// 0 -> 1 -> 2 -> 3
/// ```
///
/// The `kind` column is there so a pattern can write a property map and
/// a condition at once and both be asked of the one node.
fn chain(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("element_predicates.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for step in 0..4 {
            let kind = if step % 2 == 0 { "even" } else { "odd" };
            conn.execute(&format!("INSERT (s:step {{step: {step}, kind: '{kind}'}})"))
                .expect("a step");
        }
        for step in 0..3 {
            conn.execute(&format!(
                "MATCH (a:step), (b:step) WHERE a.step = {step} AND b.step = {} \
                 INSERT (a)-[:link]->(b)",
                step + 1
            ))
            .expect("a link");
        }
    }
    db
}

fn count(db: &Database, source: &str) -> i64 {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let counted: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("n").expect("an integer column"))
        .collect();
    assert_eq!(counted.len(), 1, "{source} returned {counted:?}");
    counted[0]
}

fn refusal(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .map(|_| ())
        .expect_err(source)
        .to_string()
}

/// The plainest form: a condition on the node the pattern names, which
/// asks what the same text behind the pattern would ask and answers the
/// same rows.
#[test]
fn a_condition_inside_a_node_pattern_selects_that_node() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    assert_eq!(
        count(&db, "MATCH (s:step WHERE s.step > 1) RETURN COUNT(*) AS n"),
        2
    );
    assert_eq!(
        count(&db, "MATCH (s:step) WHERE s.step > 1 RETURN COUNT(*) AS n"),
        2
    );
    // A condition nothing satisfies is an empty answer rather than an
    // error, the way a label nothing carries is.
    assert_eq!(
        count(&db, "MATCH (s:step WHERE s.step > 9) RETURN COUNT(*) AS n"),
        0
    );
}

/// What makes it the non local predicate: the names the pattern bound
/// to the left of it are in scope, so the node the walk has reached can
/// be compared with the node it came from.
#[test]
fn a_condition_reads_what_the_pattern_bound_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    assert_eq!(
        count(
            &db,
            "MATCH (a:step)-[:link]->(b:step WHERE b.step > a.step) RETURN COUNT(*) AS n"
        ),
        3
    );
    assert_eq!(
        count(
            &db,
            "MATCH (a:step)-[:link]->(b:step WHERE b.step < a.step) RETURN COUNT(*) AS n"
        ),
        0
    );
    // Both ends may carry one, and each is asked where its own node is
    // reached.
    assert_eq!(
        count(
            &db,
            "MATCH (a:step WHERE a.step = 0)-[:link]->(b:step WHERE b.step = 1) \
             RETURN COUNT(*) AS n"
        ),
        1
    );
}

/// A property map and a condition describe the one node, so both are
/// asked of it.
#[test]
fn a_condition_stands_beside_a_property_map() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    assert_eq!(
        count(
            &db,
            "MATCH (s:step {kind: 'even'} WHERE s.step > 0) RETURN COUNT(*) AS n"
        ),
        1
    );
    assert_eq!(
        count(&db, "MATCH (s:step {kind: 'even'}) RETURN COUNT(*) AS n"),
        2
    );
}

/// Inside an OPTIONAL MATCH the condition is part of the pattern, so a
/// node it refuses is a match that did not happen rather than a row to
/// drop, and the left side keeps its row with nulls in it.
#[test]
fn a_condition_inside_an_optional_match_keeps_the_unmatched_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    let inside = "MATCH (a:step) OPTIONAL MATCH (a)-[:link]->(b:step WHERE b.step > 2) \
                  RETURN COUNT(*) AS n";
    let behind = "MATCH (a:step) OPTIONAL MATCH (a)-[:link]->(b:step) WHERE b.step > 2 \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&db, inside), 4, "every left row is still there");
    assert_eq!(
        count(&db, behind),
        4,
        "and the WHERE of the optional agrees"
    );
    // The one row that matched is the only one with a node in it, which
    // is what tells the null rows from the matches.
    let matched = "MATCH (a:step) OPTIONAL MATCH (a)-[:link]->(b:step WHERE b.step > 2) \
                   RETURN COUNT(b) AS n";
    assert_eq!(count(&db, matched), 1);
}

/// An INSERT describes an element to make, and a condition picks
/// elements that are already there, so the two do not go together and
/// the statement is refused rather than quietly ignoring the condition.
#[test]
fn a_condition_inside_an_inserted_pattern_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    let said = refusal(
        &db,
        "INSERT (s:step {step: 9} WHERE s.step > 0) RETURN 1 AS n",
    );
    assert!(
        said.contains("an element to make"),
        "the refusal said: {said}"
    );
}

/// Inside a repeated stretch the condition would be asked once per
/// repetition, of an element the name inside the brackets no longer
/// stands for, so it is refused by name until the group machinery can
/// answer it.
#[test]
fn a_condition_inside_a_repeated_stretch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = chain(dir.path());
    let said = refusal(
        &db,
        "MATCH (a:step)((x:step WHERE x.step > 0)-[:link]->(y:step)){2} RETURN COUNT(*) AS n",
    );
    assert!(
        said.contains("once per repetition"),
        "the refusal said: {said}"
    );
}
