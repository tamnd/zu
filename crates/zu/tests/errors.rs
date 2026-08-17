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

    // A label the catalog does not hold is a condition the standard
    // names, raised after the text has been read and thrown away, so
    // it carries a code and a page but no place.
    let err = conn
        .query("MATCH (p:nothing) RETURN p.id AS id")
        .expect_err("no such label");
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
