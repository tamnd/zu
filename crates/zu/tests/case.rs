//! The case expression and its two abbreviations: ISO 20.7, mandatory
//! feature GE01.
//!
//! `CASE` is written in two forms, one that asks a condition per branch
//! and one that names a value and compares each branch with it, and it
//! is the one expression here that decides which of its parts to
//! evaluate: the branches are asked in the order they were written and
//! the walk stops at the first that says yes. `COALESCE` and `NULLIF`
//! are the abbreviations ISO writes for two shapes that come up often
//! enough to have a spelling of their own.

use zu::Database;

/// Four people of three ages, and a pet for two of them, which is where
/// the nulls come from: an OPTIONAL MATCH that found nothing leaves the
/// pet's columns null, and that is what a case is asked about.
fn people(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("case.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for (name, age) in [("ana", 15), ("bo", 30), ("cai", 70), ("dee", 80)] {
            conn.execute(&format!("INSERT (p:person {{name: '{name}', age: {age}}})"))
                .expect("a person");
        }
        for (owner, weight) in [("ana", 4), ("bo", 20)] {
            conn.execute(&format!(
                "INSERT (t:pet {{owner: '{owner}', weight: {weight}}})"
            ))
            .expect("a pet");
            conn.execute(&format!(
                "MATCH (p:person), (t:pet) WHERE p.name = '{owner}' AND t.owner = '{owner}' \
                 INSERT (p)-[:has]->(t)"
            ))
            .expect("an owner");
        }
    }
    db
}

fn strings(db: &Database, source: &str) -> Vec<String> {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    rows.iter()
        .map(|row| match row.get_by_name::<String>("n") {
            Ok(s) => s,
            Err(_) => "null".to_string(),
        })
        .collect()
}

fn numbers(db: &Database, source: &str) -> Vec<i64> {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    rows.iter()
        .map(|row| row.get_by_name::<i64>("n").unwrap_or(-1))
        .collect()
}

/// The searched form: a condition per branch, asked in the order they
/// were written, and the first that says yes is the answer.
#[test]
fn the_searched_form_answers_the_first_branch_that_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let mut said = strings(
        &db,
        "MATCH (p:person) RETURN CASE WHEN p.age < 18 THEN 'child' \
         WHEN p.age < 65 THEN 'adult' ELSE 'senior' END AS n",
    );
    said.sort();
    assert_eq!(said, ["adult", "child", "senior", "senior"]);
    // The order the branches were written in is the order they are
    // asked in, so a row two branches hold answers the first of them.
    assert_eq!(
        strings(
            &db,
            "MATCH (p:person) WHERE p.name = 'ana' \
             RETURN CASE WHEN p.age < 18 THEN 'child' WHEN p.age < 65 THEN 'adult' END AS n"
        ),
        ["child"]
    );
}

/// A case that answers no branch and wrote no ELSE is null, and so is a
/// branch whose condition is null, since a condition that answers null
/// did not hold the way a WHERE that answers null keeps no row.
#[test]
fn a_case_that_answers_no_branch_is_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let said = strings(
        &db,
        "MATCH (p:person) WHERE p.name = 'dee' \
         RETURN CASE WHEN p.age < 18 THEN 'child' END AS n",
    );
    assert_eq!(said, ["null"]);
    let mut unmatched = strings(
        &db,
        "MATCH (p:person) OPTIONAL MATCH (p)-[:has]->(t:pet) \
         RETURN CASE WHEN t.weight < 5 THEN 'small' ELSE 'big' END AS n",
    );
    unmatched.sort();
    assert_eq!(unmatched, ["big", "big", "big", "small"]);
}

/// The simple form names a value once and compares each branch with it,
/// which is the searched form with the equality written for the reader.
#[test]
fn the_simple_form_compares_each_branch_with_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let mut said = numbers(
        &db,
        "MATCH (p:person) RETURN CASE p.name WHEN 'ana' THEN 1 WHEN 'bo' THEN 2 ELSE 0 END AS n",
    );
    said.sort();
    assert_eq!(said, [0, 0, 1, 2]);
    // A branch is compared and not tested, so a null subject matches no
    // branch at all, the way `=` answers null rather than true.
    let mut unmatched = numbers(
        &db,
        "MATCH (p:person) OPTIONAL MATCH (p)-[:has]->(t:pet) \
         RETURN CASE t.weight WHEN 4 THEN 1 ELSE 0 END AS n",
    );
    unmatched.sort();
    assert_eq!(unmatched, [0, 0, 0, 1]);
}

/// Only the branch that holds is evaluated, which is what lets a case
/// stand in front of an expression the other rows cannot answer: the
/// division by zero is in a branch the walk never reaches.
#[test]
fn only_the_branch_that_holds_is_evaluated() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let mut said = numbers(
        &db,
        "MATCH (p:person) RETURN CASE WHEN p.age = 0 THEN 100 / p.age ELSE p.age END AS n",
    );
    said.sort();
    assert_eq!(said, [15, 30, 70, 80]);
}

/// COALESCE answers the first argument that is not null and stops
/// there, and it is null only where every one of them is.
#[test]
fn coalesce_answers_the_first_argument_that_is_not_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let mut said = numbers(
        &db,
        "MATCH (p:person) OPTIONAL MATCH (p)-[:has]->(t:pet) RETURN COALESCE(t.weight, 0) AS n",
    );
    said.sort();
    assert_eq!(said, [0, 0, 4, 20]);
    assert_eq!(numbers(&db, "RETURN COALESCE(NULL, NULL, 7) AS n"), [7]);
    assert_eq!(strings(&db, "RETURN COALESCE(NULL, NULL) AS n"), ["null"]);
}

/// NULLIF is null where the two are equal and the first of them
/// otherwise, which is the shape that turns a placeholder into a null.
#[test]
fn nullif_is_null_where_the_two_are_equal() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(strings(&db, "RETURN NULLIF(1, 1) AS n"), ["null"]);
    assert_eq!(numbers(&db, "RETURN NULLIF(2, 1) AS n"), [2]);
    // The two together are the pair the abbreviations are written for:
    // a placeholder becomes a null and the null becomes a default.
    assert_eq!(numbers(&db, "RETURN COALESCE(NULLIF(0, 0), 9) AS n"), [9]);
}

/// A case reads whatever an expression reads, so it stands where a
/// value stands: in a condition, in a sort key, and inside another one.
#[test]
fn a_case_stands_where_a_value_stands() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    assert_eq!(
        numbers(
            &db,
            "MATCH (p:person) WHERE CASE WHEN p.age < 18 THEN true ELSE false END \
             RETURN p.age AS n"
        ),
        [15]
    );
    assert_eq!(
        numbers(
            &db,
            "MATCH (p:person) RETURN p.age AS n \
             ORDER BY CASE WHEN p.age < 18 THEN 1 ELSE 0 END, p.age"
        ),
        [30, 70, 80, 15]
    );
    assert_eq!(
        strings(
            &db,
            "RETURN CASE WHEN false THEN 'no' ELSE CASE WHEN true THEN 'yes' END END AS n"
        ),
        ["yes"]
    );
}

/// A WHEN of the searched form is a condition, and a query that wrote a
/// number there is refused rather than asked to make a truth of it.
#[test]
fn a_searched_branch_that_is_not_a_condition_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = people(dir.path());
    let mut conn = db.connect().expect("connect");
    let said = conn
        .query("RETURN CASE WHEN 1 THEN 'one' END AS n")
        .map(|_| ())
        .expect_err("a number where a condition goes")
        .to_string();
    assert!(said.contains("condition"), "the refusal said: {said}");
}
