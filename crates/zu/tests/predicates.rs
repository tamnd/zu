//! The pattern predicates: ISO 19.8 through 19.13, features G110 to
//! G115.
//!
//! Six questions about an element the query already bound, rather than
//! about the rows a pattern is walking: whether an edge has a
//! direction, whether an element carries a label, whether a node is one
//! end of an edge, whether several elements are all different or all
//! the same, and whether an element carries a property. Every one of
//! them is answerable from what a matched row already holds, which is
//! why none of them reads storage twice: an edge value carries both of
//! its ends, a node carries its table, and which properties a table has
//! is a question about the table.

use zu::Database;
use zu::query::{Value, run};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{bulk_load_as, bulk_load_undirected_as};

/// One directed edge, the graph where the direction predicate has
/// something to say yes about.
fn directed(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("directed.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// The same pair joined by an edge with no direction (GH02), which is
/// the graph where it has something to say no about.
fn undirected(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("undirected.zu1")).unwrap();
    bulk_load_undirected_as(&mut zu, "peer", "friend", 2, &[(0, 1)]).unwrap();
    zu
}

/// A chain with a loop at the end of it:
///
/// ```text
/// 0 -> 1 -> 2 -> 2
/// ```
///
/// A two hop pattern over it ends on three different nodes once and on
/// a node it already stood on once, which is what tells ALL_DIFFERENT
/// and SAME apart from a predicate that answers the same for both.
fn looped(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("looped.zu1")).unwrap();
    let mut edges = vec![(0, 1), (1, 2), (2, 2)];
    edges.sort_unstable();
    bulk_load_as(&mut zu, "person", "knows", 3, &edges).unwrap();
    zu
}

/// Two people, a company and one edge, written by the statements a
/// reader would have written, so the tables carry real property
/// columns and there is more than one label in the graph.
fn seeded(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("predicates.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")
            .expect("ada");
        conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")
            .expect("grace");
        conn.execute("INSERT (c:company {uid: 1, founded: 1889})")
            .expect("acme");
        conn.execute(
            "MATCH (a:person), (b:person) WHERE a.uid = 1 AND b.uid = 2 \
             INSERT (a)-[:knows {since: 2020}]->(b)",
        )
        .expect("the edge");
    }
    db
}

fn count(db: &mut Zu1File, source: &str) -> i64 {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    match result.rows[0][0] {
        Value::Int(n) => n,
        ref other => panic!("{source} answered {other:?}"),
    }
}

fn refusal(db: &mut Zu1File, source: &str) -> String {
    run(source, db, &[]).expect_err(source).to_string()
}

/// The same count, against a database rather than a file, which is what
/// the cases that need property columns run on.
fn seeded_count(db: &Database, source: &str) -> i64 {
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

fn seeded_refusal(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .map(|_| ())
        .expect_err(source)
        .to_string()
}

/// G110. Every edge of a table has a direction or none of them does, so
/// the predicate is a question about the table the edge came from, and
/// the two graphs answer it the two ways.
#[test]
fn is_directed_asks_whether_an_edge_has_a_direction() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = directed(dir.path());
    let source = "MATCH (a:person)-[k:knows]->(b:person) WHERE k IS DIRECTED \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 1);
    let source = "MATCH (a:person)-[k:knows]->(b:person) WHERE k IS NOT DIRECTED \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 0);

    let mut db = undirected(dir.path());
    let source = "MATCH (a:peer)~[k:friend]~(b:peer) WHERE k IS DIRECTED \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 0);
    // Both ends answer, because the edge is one edge and the question
    // is about the edge rather than about the way the pattern read it.
    let source = "MATCH (a:peer)~[k:friend]~(b:peer) WHERE k IS NOT DIRECTED \
                  RETURN COUNT(*) AS n";
    assert_eq!(count(&mut db, source), 2);
}

/// A node has no direction, so asking is a mistake rather than a
/// question answered no: an engine that answered false here would let a
/// query that named the wrong variable pass.
#[test]
fn is_directed_refuses_a_node() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = directed(dir.path());
    let says = refusal(
        &mut db,
        "MATCH (a:person) WHERE a IS DIRECTED RETURN COUNT(*) AS n",
    );
    assert!(says.contains("IS DIRECTED"), "{says}");
    assert!(says.contains("edge"), "{says}");
}

/// G111. The label expression a pattern writes after a colon, asked of
/// an element the query already bound. A node answers with the labels
/// its row carries and an edge with the name of its table, which is the
/// label an edge has.
#[test]
fn is_labeled_tests_a_label_expression_against_a_bound_element() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(dir.path());
    let of = |predicate: &str| {
        seeded_count(
            &db,
            &format!("MATCH (x) WHERE {predicate} RETURN COUNT(*) AS n"),
        )
    };
    assert_eq!(of("x IS LABELED person"), 2);
    assert_eq!(of("x IS NOT LABELED person"), 1);
    assert_eq!(of("x IS LABELED company"), 1);
    assert_eq!(of("x IS LABELED person|company"), 3);
    assert_eq!(of("x IS LABELED !person"), 1);
    // `%` is the label expression every element satisfies, since an
    // element has a label.
    assert_eq!(of("x IS LABELED %"), 3);
    // A name no element carries is a question with the answer no rather
    // than a mistake, the same reading a pattern naming one gets.
    assert_eq!(of("x IS LABELED robot"), 0);

    // An edge carries the label its table's name is, and carries no
    // node label, so the two dictionaries never cross.
    let of = |predicate: &str| {
        seeded_count(
            &db,
            &format!(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE {predicate} RETURN COUNT(*) AS n"
            ),
        )
    };
    assert_eq!(of("k IS LABELED knows"), 1);
    assert_eq!(of("k IS LABELED person"), 0);
    assert_eq!(of("k IS NOT LABELED knows"), 0);
    assert_eq!(of("k IS LABELED %"), 1);
}

/// G111 again, written the short way. ISO 19.9 gives the predicate two
/// spellings, `IS LABELED` and a colon, so the colon answers what the
/// words answer and the whole label expression stands behind it.
#[test]
fn a_colon_is_the_other_spelling_of_the_labeled_predicate() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(dir.path());
    let of = |predicate: &str| {
        seeded_count(
            &db,
            &format!("MATCH (x) WHERE {predicate} RETURN COUNT(*) AS n"),
        )
    };
    assert_eq!(of("x:person"), of("x IS LABELED person"));
    assert_eq!(of("x:person|company"), 3);
    assert_eq!(of("x:!person"), 1);
    assert_eq!(of("x:robot"), 0);
    // It is a predicate like the others, so it reads inside a longer
    // one and the word NOT in front of it says what it always says.
    assert_eq!(of("NOT x:person"), 1);
    assert_eq!(of("x:person AND x.name IS NOT NULL"), 2);
}

/// G112. An edge value already holds the rows of both of its ends, so
/// the predicate is a comparison and never a second read of storage.
#[test]
fn is_source_of_and_is_destination_of_relate_a_node_to_an_edge() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = directed(dir.path());
    let of = |db: &mut Zu1File, predicate: &str| {
        count(
            db,
            &format!(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE {predicate} RETURN COUNT(*) AS n"
            ),
        )
    };
    assert_eq!(of(&mut db, "a IS SOURCE OF k"), 1);
    assert_eq!(of(&mut db, "b IS SOURCE OF k"), 0);
    assert_eq!(of(&mut db, "b IS DESTINATION OF k"), 1);
    assert_eq!(of(&mut db, "a IS DESTINATION OF k"), 0);
    assert_eq!(of(&mut db, "a IS NOT DESTINATION OF k"), 1);

    // A loop is its own source and its own destination, which is the
    // one row where both answers are yes.
    let mut db = looped(dir.path());
    assert_eq!(
        count(
            &mut db,
            "MATCH (a:person)-[k:knows]->(b:person) \
             WHERE a IS SOURCE OF k AND a IS DESTINATION OF k RETURN COUNT(*) AS n"
        ),
        1
    );
}

/// G113 and G114. Two questions about element identity, which is the
/// table and the row for a node and the table, the ends and the ordinal
/// for an edge.
#[test]
fn all_different_and_same_compare_elements() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = looped(dir.path());
    let of = |db: &mut Zu1File, predicate: &str| {
        count(
            db,
            &format!(
                "MATCH (a:person)-[:knows]->(b:person)-[:knows]->(c:person) \
                 WHERE {predicate} RETURN COUNT(*) AS n"
            ),
        )
    };
    // The two hop walks are 0,1,2 and 1,2,2 and 2,2,2, so one of them
    // names three different nodes, one names three copies of one node,
    // and the third names neither.
    assert_eq!(of(&mut db, "ALL_DIFFERENT(a, b, c)"), 1);
    assert_eq!(of(&mut db, "SAME(a, b, c)"), 1);
    assert_eq!(of(&mut db, "SAME(b, c)"), 2);
    assert_eq!(of(&mut db, "ALL_DIFFERENT(a, b)"), 2);

    // Both are asked of at least two elements, since one element is the
    // same as itself and different from nothing.
    let says = refusal(
        &mut db,
        "MATCH (a:person) WHERE SAME(a) RETURN COUNT(*) AS n",
    );
    assert!(says.contains("at least two"), "{says}");
    // And of elements: a property is a value, and two equal values are
    // not one element.
    let says = refusal(
        &mut db,
        "MATCH (a:person), (b:person) WHERE ALL_DIFFERENT(a.id, b.id) RETURN COUNT(*) AS n",
    );
    assert!(says.contains("nodes and edges"), "{says}");
}

/// G115. Whether the element carries a property, which is a question
/// about its table and not about the value stored in the row, so a
/// property that is there and null is there.
#[test]
fn property_exists_asks_the_table_and_not_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(dir.path());
    let of = |pattern: &str, predicate: &str| {
        seeded_count(
            &db,
            &format!("MATCH {pattern} WHERE {predicate} RETURN COUNT(*) AS n"),
        )
    };
    assert_eq!(of("(p:person)", "PROPERTY_EXISTS(p, name)"), 2);
    assert_eq!(of("(p:person)", "PROPERTY_EXISTS(p, nickname)"), 0);
    // The other table carries other properties, and the question is
    // asked of the element's own table.
    assert_eq!(of("(c:company)", "PROPERTY_EXISTS(c, founded)"), 1);
    assert_eq!(of("(c:company)", "PROPERTY_EXISTS(c, name)"), 0);
    assert_eq!(of("(p:person)", "NOT PROPERTY_EXISTS(p, founded)"), 2);

    // An edge's properties are its own table's columns too.
    let edge = "(a:person)-[k:knows]->(b:person)";
    assert_eq!(of(edge, "PROPERTY_EXISTS(k, since)"), 1);
    assert_eq!(of(edge, "PROPERTY_EXISTS(k, weight)"), 0);

    // Not a value, and nothing else has properties.
    let says = seeded_refusal(
        &db,
        "MATCH (p:person) WHERE PROPERTY_EXISTS(p.uid, name) RETURN COUNT(*) AS n",
    );
    assert!(says.contains("PROPERTY_EXISTS"), "{says}");
}

/// A row that bound nothing has nothing to answer about, so a
/// predicate over it answers null rather than false, which is the
/// three valued reading every other predicate gets and the one that
/// keeps an unmatched optional row where a NOT stands over it.
#[test]
fn a_predicate_over_an_unmatched_optional_row_answers_null() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = directed(dir.path());
    let source = "MATCH (a:person) OPTIONAL MATCH (a)-[k:knows]->(b:person) \
                  RETURN k IS DIRECTED AS v ORDER BY a.id";
    let result = run(source, &mut db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    let answers: Vec<Value> = result.rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(answers, [Value::Bool(true), Value::Null]);
}
