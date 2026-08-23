//! The select statement of ISO 14.12.
//!
//! GQL spells a query two ways and this is the second of them, the
//! items first and the rows they come from second. It is not an
//! optional feature: no code in `features.xml` gates it and it hangs
//! off `<focused linear query statement>` beside the match and return
//! form, so an engine that reads only that form is missing mandatory
//! GQL.
//!
//! What is checked here is mostly one thing said several ways: a select
//! statement answers what the linear statement written out of the same
//! parts answers. That is the standard's own account of it, the General
//! Rules of 14.12 being written in terms of the clauses, and it is the
//! thing an engine that read the words and did something else with them
//! would fail. The rest is the two places where the two forms are not
//! written alike, the graph expression that sits inside the body rather
//! than in front of the statement, and the having clause, which the
//! other form has no word for at all.

use zu::Database;
use zu::query::Value;

/// Five people in three cities, and one company, which is the second
/// label a graph match list needs to have two matches to list.
fn people(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("select.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for (name, city, age) in [
            ("Alice", "Berlin", 30),
            ("Bob", "Paris", 25),
            ("Carol", "Paris", 41),
            ("Dave", "Lisbon", 20),
            ("Erin", "Berlin", 55),
        ] {
            conn.execute(&format!(
                "INSERT (p:Person {{name: '{name}', city: '{city}', age: {age}}})"
            ))
            .expect("a person");
        }
        conn.execute("INSERT (c:Company {name: 'Acme'})")
            .expect("a company");
    }
    db
}

/// The rows as strings, in the order the statement answered them, one
/// string a row so a test can say what it expects without a type per
/// column.
fn rows(db: &Database, source: &str) -> Vec<String> {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    rows.iter()
        .map(|row| {
            (0..row.len())
                .map(|i| match row.value(i).expect("a value") {
                    Value::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// The same rows, sorted, for the statements that say nothing about
/// order.
fn sorted(db: &Database, source: &str) -> Vec<String> {
    let mut rows = rows(db, source);
    rows.sort();
    rows
}

fn columns(db: &Database, source: &str) -> Vec<String> {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    rows.columns.clone()
}

fn refused(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .expect_err(&format!("{source} should have been refused"))
        .to_string()
}

/// The two forms over the same parts, which is what 14.12 says a select
/// statement is: the body binds the rows, the clauses behind it narrow
/// and group what the body bound, and the items say what a row of the
/// answer holds.
#[test]
fn a_select_statement_answers_what_the_linear_statement_answers() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    for (select, linear) in [
        (
            "SELECT p.name AS name, p.age AS age \
             FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) WHERE p.age > 40",
            "MATCH (p:Person) FILTER p.age > 40 RETURN p.name AS name, p.age AS age",
        ),
        (
            "SELECT DISTINCT p.city AS city FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person)",
            "MATCH (p:Person) RETURN DISTINCT p.city AS city",
        ),
        (
            "SELECT p.city AS city, COUNT(*) AS n \
             FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) GROUP BY p.city ORDER BY city",
            "MATCH (p:Person) RETURN p.city AS city, COUNT(*) AS n GROUP BY p.city ORDER BY city",
        ),
        (
            "SELECT p.name AS name FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
             ORDER BY p.age DESC OFFSET 1 LIMIT 2",
            "MATCH (p:Person) RETURN p.name AS name ORDER BY p.age DESC OFFSET 1 LIMIT 2",
        ),
        (
            // The where clause sits behind the whole body rather than
            // inside one match of it, so it narrows the pairs and not
            // the people.
            "SELECT p.name AS person, c.name AS company \
             FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person), CURRENT_PROPERTY_GRAPH MATCH (c:Company) \
             WHERE p.name = 'Alice'",
            "MATCH (p:Person) MATCH (c:Company) FILTER p.name = 'Alice' \
             RETURN p.name AS person, c.name AS company",
        ),
    ] {
        assert_eq!(sorted(&db, select), sorted(&db, linear), "{select}");
        assert_eq!(columns(&db, select), columns(&db, linear), "{select}");
    }
}

/// A select item list is written in the order it is answered, so the
/// page and the sort are the ones the reader asked for rather than
/// whatever the body happened to bind.
#[test]
fn the_clauses_behind_the_body_run_in_the_order_iso_writes_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let source = "SELECT p.name AS name FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  WHERE p.city = 'Berlin' ORDER BY p.age";
    assert_eq!(rows(&db, source), ["Alice", "Erin"]);
    let source = "SELECT p.name AS name FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  ORDER BY p.age LIMIT 2";
    assert_eq!(rows(&db, source), ["Dave", "Bob"]);
}

/// A having clause tests a group where a where clause tests a row, so
/// what it reads is a value only the projection can work out and it
/// runs after that projection and before the order and the page. Those
/// three sentences are the whole of the difference between the two
/// clauses, and each of them is a case here.
#[test]
fn a_having_clause_tests_the_group_the_projection_made() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    // Three cities, two of them with two people in them.
    let source = "SELECT p.city AS city, COUNT(*) AS n \
                  FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  GROUP BY p.city HAVING COUNT(*) > 1 ORDER BY city";
    assert_eq!(rows(&db, source), ["Berlin|Int(2)", "Paris|Int(2)"]);

    // No group by clause, so the whole binding table is the one group
    // and the clause has one row to keep or to drop.
    let source = "SELECT COUNT(*) AS n FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  HAVING COUNT(*) > 3";
    assert_eq!(rows(&db, source), ["Int(5)"]);
    let source = "SELECT COUNT(*) AS n FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  HAVING COUNT(*) > 30";
    assert!(rows(&db, source).is_empty());

    // The limit is taken from what the having clause kept and not from
    // what the projection made, which is the one thing an engine that
    // ran the two the other way round would get wrong.
    let source = "SELECT p.city AS city, COUNT(*) AS n \
                  FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  GROUP BY p.city HAVING COUNT(*) = 1 ORDER BY city LIMIT 2";
    assert_eq!(rows(&db, source), ["Lisbon|Int(1)"]);

    // The column the clause travels in is not one of the answer's, and
    // an item that named no alias keeps the name it would have had
    // without the clause.
    let source = "SELECT p.city, COUNT(*) AS n FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) \
                  GROUP BY p.city HAVING COUNT(*) > 1";
    assert_eq!(columns(&db, source), ["p.city", "n"]);
}

/// A body may be a nested query rather than a graph match, which is the
/// second of the two forms ISO gives it. The asterisk keeps every
/// column the nested query answered, so a case written this way
/// measures the body and not a projection over it.
#[test]
fn a_body_may_be_a_query_in_braces() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let source = "SELECT * FROM { MATCH (p:Person) WHERE p.city = 'Lisbon' RETURN p.name AS name }";
    assert_eq!(rows(&db, source), ["Dave"]);
    assert_eq!(columns(&db, source), ["name"]);

    // And the clauses behind the body read what it answered, the same
    // way they read what a graph match bound.
    let source = "SELECT n AS name FROM { MATCH (p:Person) RETURN p.name AS n, p.age AS a } \
                  WHERE a > 40 ORDER BY a";
    assert_eq!(rows(&db, source), ["Carol", "Erin"]);

    // A query in braces has to answer a table, so one that finishes
    // without returning is refused rather than answering the rows the
    // statement started from.
    let says = refused(&db, "SELECT * FROM { MATCH (p:Person) FINISH }");
    assert!(says.contains("has to end with RETURN"), "{says}");
}

/// The body and everything behind it is one optional group in ISO's
/// rule, so a select statement written with no body at all is a whole
/// statement: the items are read over the one row a statement starts
/// from.
#[test]
fn a_select_statement_with_no_body_answers_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(rows(&db, "SELECT 1 AS n"), ["Int(1)"]);
    assert_eq!(rows(&db, "SELECT 'a' AS a, 'b' AS b"), ["a|b"]);
    // An unaliased item is named the way the same item is named in a
    // return statement, there being one rule for it and not two.
    assert_eq!(columns(&db, "SELECT 1 + 1"), ["1 + 1"]);
    assert_eq!(columns(&db, "RETURN 1 + 1"), ["1 + 1"]);
}

/// The graph expression is where this form writes what the other form
/// writes as a `USE` clause. This engine runs a statement against the
/// one graph it names, so a body naming two of them is declined rather
/// than misread: the grammar is fine and the answer says which rule of
/// the engine's own is being applied.
#[test]
fn a_body_that_names_two_graphs_is_declined() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let source =
        "SELECT p.name AS n FROM $g MATCH (p:Person), HOME_PROPERTY_GRAPH MATCH (q:Person)";
    let says = refused(&db, source);
    assert!(says.contains("25G04"), "{says}");
    let source = "USE $g SELECT p.name AS n FROM HOME_PROPERTY_GRAPH MATCH (p:Person)";
    let says = refused(&db, source);
    assert!(says.contains("25G04"), "{says}");

    // The same graph twice is one graph, and the graph the statement is
    // already running against is what CURRENT_PROPERTY_GRAPH names, so
    // a body may write it as often as it has matches.
    let source = "SELECT p.name AS a, q.name AS b \
                  FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person), \
                       CURRENT_PROPERTY_GRAPH MATCH (q:Company) \
                  WHERE p.name = 'Alice'";
    assert_eq!(rows(&db, source), ["Alice|Acme"]);
}

/// The comma of a graph match list and the comma of a path pattern list
/// are the same character in ISO's grammar, so what tells them apart is
/// what stands after it. A pattern never begins with a name that a
/// MATCH follows, which is what makes the reading an answer rather than
/// a guess.
#[test]
fn a_comma_in_a_body_joins_patterns_or_matches_by_what_follows_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let two_patterns = "SELECT p.name AS a, q.name AS b \
                        FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person), (q:Company) \
                        WHERE p.name = 'Alice'";
    let two_matches = "SELECT p.name AS a, q.name AS b \
                       FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person), \
                            CURRENT_PROPERTY_GRAPH MATCH (q:Company) \
                       WHERE p.name = 'Alice'";
    assert_eq!(rows(&db, two_patterns), ["Alice|Acme"]);
    assert_eq!(rows(&db, two_matches), rows(&db, two_patterns));
}

/// An asterisk says the columns are whatever the body bound, and a
/// having clause needs to know what a group is, so the two together say
/// nothing an engine could act on.
#[test]
fn an_asterisk_with_a_having_clause_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let source = "SELECT * FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person) HAVING COUNT(*) > 1";
    let says = refused(&db, source);
    assert!(says.contains("write the items out"), "{says}");
}

/// `ALL` is the default written down, so it says what writing neither
/// word says and not something else.
#[test]
fn a_set_quantifier_says_which_rows_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let plain = "SELECT p.city AS city FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person)";
    let all = "SELECT ALL p.city AS city FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person)";
    let distinct = "SELECT DISTINCT p.city AS city FROM CURRENT_PROPERTY_GRAPH MATCH (p:Person)";
    assert_eq!(sorted(&db, plain).len(), 5);
    assert_eq!(sorted(&db, all), sorted(&db, plain));
    assert_eq!(sorted(&db, distinct), ["Berlin", "Lisbon", "Paris"]);
}
