//! List types: the spellings, what belongs to one, and the cast.
//!
//! The list *value* has been in the executor since the first path was
//! returned. What is new is the list *type*, and a type is only worth
//! having if the two questions asked of it, membership and conversion,
//! answer the same way about the same value. That is what is checked
//! here, end to end through the parser rather than against the lattice
//! directly, because the spellings are half of GV50.

use zu::query::{Value, run};
use zu_common::{IntBits, LogicalType};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;
use zu_zu1::props::{ListElement, PropValues, store_props};

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("lists.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn yes(db: &mut Zu1File, predicate: &str) -> bool {
    match one(db, &format!("RETURN ({predicate}) AS v")) {
        Value::Bool(b) => b,
        other => panic!("{predicate} answered {other:?}"),
    }
}

fn code(db: &mut Zu1File, source: &str) -> String {
    let err = run(source, db, &[]).expect_err(source);
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source}: {err} carries no status"))
        .code()
        .to_string()
}

/// ISO spells one type four ways. `LIST` and `ARRAY` are the same name,
/// the element type may be written inside the brackets or in front, and
/// `GROUP` in front of the name is the aggregation spelling of it.
#[test]
fn every_spelling_of_a_list_type_names_the_same_type() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for spelling in [
        "LIST<INT>",
        "ARRAY<INT>",
        "INT LIST",
        "INT ARRAY",
        "GROUP LIST<INT>",
    ] {
        assert!(
            yes(&mut db, &format!("[1, 2] IS TYPED {spelling}")),
            "{spelling}"
        );
        assert!(
            !yes(&mut db, &format!("['a'] IS TYPED {spelling}")),
            "{spelling} admitted a string"
        );
    }
    // A list type with no element type at all admits any list, and a
    // list is still not a scalar however it is spelled.
    assert!(yes(&mut db, "['a', 1] IS TYPED LIST"));
    assert!(!yes(&mut db, "1 IS TYPED LIST<INT>"));
}

/// The element type is a value type, so it is nullable unless it says
/// otherwise, and it may itself be a list.
#[test]
fn an_element_type_is_a_value_type_and_carries_a_value_type_s_rules() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert!(yes(&mut db, "[1, null] IS TYPED LIST<INT>"));
    assert!(!yes(&mut db, "[1, null] IS TYPED LIST<INT NOT NULL>"));
    assert!(yes(&mut db, "[[1], [2, 3]] IS TYPED LIST<LIST<INT>>"));
    assert!(!yes(&mut db, "[[1], ['a']] IS TYPED LIST<LIST<INT>>"));
    assert!(yes(&mut db, "[1, 'a'] IS TYPED LIST<INT | STRING>"));
    // The empty list belongs to every list type, which is what makes
    // the element type a promise about elements rather than about the
    // list, and the declared width still bounds the elements there are.
    assert!(yes(&mut db, "[] IS TYPED LIST<INT NOT NULL>"));
    assert!(!yes(&mut db, "[1000] IS TYPED LIST<INT8>"));
}

#[test]
fn a_maximum_length_bounds_the_list_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert!(yes(&mut db, "[1, 2] IS TYPED LIST<INT>(2)"));
    assert!(!yes(&mut db, "[1, 2, 3] IS TYPED LIST<INT>(2)"));
    assert!(yes(&mut db, "[1, 2] IS TYPED INT LIST(9)"));
}

/// A cast to a list type casts every element, and the two ways it fails
/// are two conditions because they are two different faults.
#[test]
fn casting_to_a_list_casts_the_elements_and_names_the_element_that_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CAST([1, 2] AS LIST<STRING>) AS v"),
        Value::List(vec![Value::Str("1".into()), Value::Str("2".into())])
    );
    assert_eq!(
        one(&mut db, "RETURN CAST(['1', '2'] AS LIST<INT>) AS v"),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        one(&mut db, "RETURN CAST([] AS LIST<INT>) AS v"),
        Value::List(Vec::new())
    );
    assert_eq!(
        code(&mut db, "RETURN CAST([1, 2, 3] AS LIST<INT>(2)) AS v"),
        "22G0B"
    );
    assert_eq!(
        code(&mut db, "RETURN CAST(['a'] AS LIST<INT>) AS v"),
        "22G0C"
    );
    // A scalar does not become a list of one. A cast that invented the
    // brackets would turn a mistyped query into a quiet success.
    assert_eq!(code(&mut db, "RETURN CAST(1 AS LIST<INT>) AS v"), "22018");
}

/// GF12 and GF13 are one count and two names, and the names are not
/// interchangeable: ISO gives `CARDINALITY` lists and groups, and a
/// string has a length rather than a cardinality.
#[test]
fn cardinality_counts_a_list_and_refuses_what_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN CARDINALITY([1, 2, 3]) AS v"),
        Value::Int(3)
    );
    assert_eq!(one(&mut db, "RETURN cardinality([]) AS v"), Value::Int(0));
    assert_eq!(one(&mut db, "RETURN SIZE([1, 2, 3]) AS v"), Value::Int(3));
    assert_eq!(one(&mut db, "RETURN CARDINALITY(null) AS v"), Value::Null);
    assert_eq!(code(&mut db, "RETURN CARDINALITY('abc') AS v"), "22G03");
    assert_eq!(one(&mut db, "RETURN SIZE('abc') AS v"), Value::Int(3));
}

/// A list in a property column is the same value a list literal is,
/// which is the point of storing one: the count, the membership test
/// and the elements all answer the way they answer for a literal.
#[test]
fn a_stored_list_is_the_same_value_a_written_one_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stored.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).unwrap();
    let first: Vec<ListElement> = [1u64, 2, 3].iter().map(|&w| ListElement::Word(w)).collect();
    let second: Vec<ListElement> = vec![ListElement::Word(7)];
    let rows: Vec<&[ListElement]> = vec![&first, &second];
    let names: Vec<&[ListElement]> =
        vec![&[ListElement::Blob(b"ay"), ListElement::Blob(b"bee")], &[]];
    let int = LogicalType::Int {
        signed: true,
        bits: IntBits::B64,
        precision: None,
    };
    let text = LogicalType::Str {
        min: None,
        max: None,
        fixed: false,
    };
    store_props(
        &mut db,
        "person",
        &[
            (
                "xs",
                PropValues::List {
                    elem: &int,
                    rows: &rows,
                },
            ),
            (
                "tags",
                PropValues::List {
                    elem: &text,
                    rows: &names,
                },
            ),
        ],
    )
    .unwrap();

    let source = "MATCH (p:person) RETURN CARDINALITY(p.xs) AS n ORDER BY n DESC";
    let result = run(source, &mut db, &[]).unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(3)], vec![Value::Int(1)]]);
    let source = "MATCH (p:person) WHERE p.id = 0 RETURN p.xs AS v";
    assert_eq!(
        one(&mut db, source),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
    let source = "MATCH (p:person) WHERE p.id = 0 RETURN p.tags AS v";
    assert_eq!(
        one(&mut db, source),
        Value::List(vec![Value::Str("ay".into()), Value::Str("bee".into())])
    );
    let source = "MATCH (p:person) WHERE p.id = 1 RETURN SIZE(p.tags) AS v";
    assert_eq!(one(&mut db, source), Value::Int(0));
    let source = "MATCH (p:person) WHERE p.id = 0 RETURN (p.xs IS TYPED LIST<INT>) AS v";
    assert_eq!(one(&mut db, source), Value::Bool(true));
    let source = "MATCH (p:person) WHERE p.id = 0 RETURN (p.xs IS TYPED LIST<STRING>) AS v";
    assert_eq!(one(&mut db, source), Value::Bool(false));
}

/// A type name is a name until a type is what is being asked for, so a
/// query may still call a variable `list` and a list a `list`.
#[test]
fn list_is_still_a_name_where_a_name_is_what_is_wanted() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "UNWIND [1, 2] AS list RETURN sum(list) AS v";
    assert_eq!(one(&mut db, source), Value::Int(3));
    let source = "UNWIND [[1, 2], [3]] AS array RETURN CARDINALITY(array) AS v ORDER BY v DESC \
                  LIMIT 1";
    assert_eq!(one(&mut db, source), Value::Int(2));
}
