//! The error model the Rust SDK hands a caller, which dx/03 §5 fixes
//! and dx/04 §5 asks for as fields rather than as a sentence.
//!
//! What is checked here is that nothing a caller needs has to be read
//! back out of the message: the condition, the severity, where in the
//! statement it happened counted both ways, the line that place is on,
//! where the condition is written up, and whether running the same
//! statement again could get anywhere.

use zu::gqlstatus::Severity;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Database, ZuError};

const NODES: u32 = 8;

fn opened(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    drop(db);
    let db = Database::open(&path).expect("open");
    (dir, db)
}

#[test]
fn a_refused_statement_says_what_where_and_whether_to_try_again() {
    let (_dir, db) = opened("refused.zu1");
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person)\nRETURN p.id AS id ORDR BY p.id";
    let err = conn.query(source).expect_err("ORDR is not a clause");
    let record = err.diagnostic().expect("a condition, not an engine fault");

    assert_eq!(record.status.code(), "42001");
    assert_eq!(record.status.class(), "42", "the class a binding raises on");
    assert_eq!(record.severity(), Severity::Exception);
    assert_eq!(
        record.status.standard_text(),
        "syntax error or access rule violation, invalid syntax"
    );
    assert_eq!(record.doc_url(), "https://zu.dev/docs/errors/42001");
    assert!(!record.retryable(), "text that will not parse will not");

    // The place, said both ways: the pair a caller prints a caret
    // under, and the offset a caller slices the statement at.
    let at = record.position.expect("a syntax error happens somewhere");
    assert_eq!((at.line, at.column), (2, 19));
    assert!(source[at.offset as usize..].starts_with("ORDR"));

    // The line that place is on, which is the half a caller no longer
    // holding the statement cannot reconstruct. The column indexes
    // into it, so a caret built from the pair lands on the word.
    let excerpt = record.excerpt.as_deref().expect("a line to quote");
    assert_eq!(excerpt, "RETURN p.id AS id ORDR BY p.id");
    assert!(excerpt[at.column as usize - 1..].starts_with("ORDR"));

    // And the message is still the whole report, so a caller that
    // prints one line prints a complete one.
    assert!(
        err.to_string().starts_with("42001: line 2, column 19: "),
        "{err}"
    );
}

/// The failures that carry no place: one raised while the statement
/// ran, one raised against the catalog rather than against the text,
/// and one worth running again.
#[test]
fn a_failure_with_no_place_has_none_rather_than_a_guessed_one() {
    let (_dir, db) = opened("runtime.zu1");
    let mut conn = db.connect().expect("connect");

    // Dividing by zero happens at no token, so there is nothing to
    // point at and nothing to quote.
    let err = conn.query("RETURN 1 / 0").expect_err("division by zero");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "22012");
    assert_eq!(record.status.class(), "22");
    assert!(record.position.is_none());
    assert!(record.excerpt.is_none());
    assert!(!err.retryable());
    // It is still written up, because the condition is what a reader
    // wants the page for.
    assert_eq!(record.doc_url(), "https://zu.dev/docs/errors/22012");

    // A graph the catalog does not hold is a condition the standard
    // names, raised after the text has been read and thrown away, so
    // it carries a code and a page but no place. A label the catalog
    // does not hold is not one of these: no element carries it, so the
    // pattern matches nothing and the statement answers with no rows.
    let err = conn
        .query("USE nowhere MATCH (p) RETURN p")
        .expect_err("no such graph");
    let record = err.diagnostic().expect("a condition");
    assert_eq!(record.status.code(), "42002");
    assert_eq!(record.doc_url(), "https://zu.dev/docs/errors/42002");
    assert_eq!(err.position(), None);
    assert_eq!(err.excerpt(), None);
    assert!(!err.retryable());

    // A write that lost to a concurrent one is the failure a retry
    // loop exists for, and it says so without a condition to say it
    // with.
    assert!(ZuError::Conflict("the swap was lost".into()).retryable());
}

/// Three refusals the standard gives a code of its own, where the
/// obvious code is the wrong one.
///
/// A value written where a value belongs and out of range is not a
/// syntax error, and a name that resolves to something already taken is
/// not an argument the caller got wrong, so each of these carries the
/// condition ISO names for it rather than the class the refusal was
/// easiest to raise from.
#[test]
fn a_condition_the_standard_names_is_the_one_raised() {
    let (_dir, db) = opened("named.zu1");
    let mut conn = db.connect().expect("connect");

    // 22G0F invalid number of paths or groups. The statement parses and
    // the count sits where a count belongs, so what is wrong is the
    // number and not the text.
    let err = conn
        .query("MATCH SHORTEST 0 (a:person)-[:knows]->{1,3}(b:person) RETURN COUNT(*) AS n")
        .expect_err("a selector keeping no path");
    assert_eq!(
        err.diagnostic().expect("a condition").status.code(),
        "22G0F"
    );

    // 22G0H invalid duration format, which is the duration's own code
    // where the rest of the temporals raise 22007.
    let err = conn
        .query("RETURN DURATION 'not a duration' AS v")
        .expect_err("a duration nobody can read");
    assert_eq!(
        err.diagnostic().expect("a condition").status.code(),
        "22G0H"
    );
    let err = conn
        .query("RETURN DATE 'not a date' AS v")
        .expect_err("a date nobody can read");
    assert_eq!(
        err.diagnostic().expect("a condition").status.code(),
        "22007"
    );

    // 42002 invalid reference: the name resolves, and resolves to
    // something already bound as an edge.
    let err = conn
        .query("MATCH (p:person)-[k:knows]->(q:person), (k:person) RETURN p.id AS id")
        .expect_err("a name bound twice, two ways");
    assert_eq!(
        err.diagnostic().expect("a condition").status.code(),
        "42002"
    );
}

/// Every condition about a thing the statement named says which thing,
/// as a field and not as a sentence.
///
/// This is the G9 exit criterion: a record for an element-related code
/// carries a subject, so a client underlines the offending name in an
/// editor instead of reading it back out of English. The kind and the
/// name are apart, because asking "is this about a label" should be one
/// string against one word.
#[test]
fn a_condition_about_a_name_says_which_name() {
    let (_dir, db) = opened("subject.zu1");
    let mut conn = db.connect().expect("connect");
    // A table with a property on it, since a property nobody declared
    // is only a condition where the table declares some.
    conn.query("INSERT (:thing {a: 1})").expect("insert");

    // Statement, code, kind of thing, and the name itself. Each of
    // these is a condition ISO raises about something the statement
    // wrote down, which is the whole set the criterion is about.
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "MATCH (p:person) RETURN q.id AS id",
            "42002",
            "variable",
            "q",
        ),
        ("RETURN nosuchfn(1) AS v", "42002", "function", "nosuchfn"),
        (
            "USE nowhere MATCH (p) RETURN p",
            "42002",
            "graph",
            "nowhere",
        ),
        (
            "MATCH (t:thing) SET t.nosuchprop = 1",
            "22G0S",
            "property",
            "nosuchprop",
        ),
        ("MATCH (t:thing) SET t:thing", "22G03", "label", "thing"),
        ("MATCH (t:thing) REMOVE t:thing", "22G0N", "label", "thing"),
    ];

    for (source, code, kind, name) in cases {
        let err = conn.query(source).expect_err(source);
        let record = err
            .diagnostic()
            .unwrap_or_else(|| panic!("{source}: {err}"));
        assert_eq!(record.status.code(), *code, "{source}");
        let subject = record
            .subject
            .as_ref()
            .unwrap_or_else(|| panic!("{source} raised {code} about nothing"));
        assert_eq!(subject.kind(), *kind, "{source}");
        assert_eq!(subject.name(), *name, "{source}");
        // And the record says where the statement was running, which
        // is what ISO 23.2 asks for and what no raise site holds.
        assert_eq!(record.graph.as_deref(), Some("home"), "{source}");
        assert!(record.schema.is_some(), "{source}");
    }
}
