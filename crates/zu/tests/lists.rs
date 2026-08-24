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
    assert!(yes(&mut db, "[1, 2] IS TYPED LIST<INT>[2]"));
    assert!(!yes(&mut db, "[1, 2, 3] IS TYPED LIST<INT>[2]"));
    assert!(yes(&mut db, "[1, 2] IS TYPED INT LIST[9]"));
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
        code(&mut db, "RETURN CAST([1, 2, 3] AS LIST<INT>[2]) AS v"),
        "22G0B"
    );
    assert_eq!(
        code(&mut db, "RETURN CAST(['a'] AS LIST<INT>) AS v"),
        "22G0C"
    );
    // A scalar does not become a list of one. A cast that invented the
    // brackets would turn a mistyped query into a quiet success. The
    // condition is the one for a value of the wrong type and not the
    // one for a string that spells its type badly, because an integer
    // is not a list however it is written.
    assert_eq!(code(&mut db, "RETURN CAST(1 AS LIST<INT>) AS v"), "22G03");
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

/// The two list value expressions of ISO 20.16: the join, which is the
/// same operator two strings are joined with, and the trim, which takes
/// a count rather than a character because a list has no character to
/// take off.
#[test]
fn two_lists_are_joined_and_one_is_trimmed_to_a_count() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN [1, 2] || [3] AS v"),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        one(&mut db, "RETURN [] || [1] AS v"),
        Value::List(vec![Value::Int(1)])
    );
    assert_eq!(one(&mut db, "RETURN [1] || NULL AS v"), Value::Null);
    // The element type of the join is the one both sides agree on, and
    // a list of strings joined to a list of numbers is still a list, so
    // this is a question about the values and not about the binder.
    assert_eq!(
        one(&mut db, "RETURN ['a'] || [1] AS v"),
        Value::List(vec![Value::Str("a".to_owned()), Value::Int(1)])
    );
    assert_eq!(
        one(&mut db, "RETURN TRIM([1, 2, 3], 2) AS v"),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    // A count past the end takes the whole list rather than failing,
    // which is what a trim does to a string that is already short.
    assert_eq!(
        one(&mut db, "RETURN TRIM([1], 5) AS v"),
        Value::List(vec![Value::Int(1)])
    );
    assert_eq!(
        one(&mut db, "RETURN TRIM([1, 2], 0) AS v"),
        Value::List(vec![])
    );
    assert_eq!(one(&mut db, "RETURN TRIM(NULL, 2) AS v"), Value::Null);
    // A list has to be told how many to keep, and the count has to be a
    // whole number that is not negative.
    assert_eq!(code(&mut db, "RETURN TRIM([1, 2]) AS v"), "22G03");
    assert_eq!(code(&mut db, "RETURN TRIM([1, 2], -1) AS v"), "22011");
    assert_eq!(code(&mut db, "RETURN [1] || 'a' AS v"), "22G03");
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

/// A type name is read as a type where a type is what is being asked
/// for, and nowhere else, so an ordinary name in the same slot binds
/// and reads the way any name does. `LIST` and `ARRAY` are themselves
/// reserved words (ISO 21.3) and so are not among the names a query may
/// pick, which is the other half of the same rule.
#[test]
fn a_type_name_is_read_as_a_type_only_where_a_type_belongs() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "UNWIND [1, 2] AS items RETURN sum(items) AS v";
    assert_eq!(one(&mut db, source), Value::Int(3));
    let source = "UNWIND [[1, 2], [3]] AS rows RETURN CARDINALITY(rows) AS v ORDER BY v DESC \
                  LIMIT 1";
    assert_eq!(one(&mut db, source), Value::Int(2));

    for source in [
        "UNWIND [1, 2] AS list RETURN sum(list) AS v",
        "UNWIND [1, 2] AS array RETURN sum(array) AS v",
    ] {
        let err = run(source, &mut db, &[]).expect_err(source);
        assert!(err.to_string().contains("reserved word"), "{source}: {err}");
    }
}

/// GF10 and GF12. `size(collect(n))` is two things that run at two
/// times: the list is accumulated over the rows of the group and the
/// count is taken of what came out. The binder splits the clause into
/// the grouping that accumulates and the projection that reads it, so
/// what a reader writes as one item is one column of the answer and two
/// operators underneath.
#[test]
fn a_scalar_function_over_a_set_function_reads_what_the_grouping_answered() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for (source, want) in [
        ("UNWIND [1, 2] AS n RETURN size(collect(n)) AS v", 2),
        (
            "UNWIND [[1], [2]] AS n RETURN cardinality(collect(n)) AS v",
            2,
        ),
        // The set function written twice is accumulated once, which is
        // what the hoist reusing an item's slot buys, and there is no
        // way to see that from here beyond the answer being right.
        ("UNWIND [1, 2, 3] AS n RETURN sum(n) + count(n) AS v", 9),
        // A grouping with keys, where the read runs once per group.
        (
            "UNWIND [1, 2, 3, 4] AS n RETURN count(n) * 10 AS v, n % 2 AS k ORDER BY k LIMIT 1",
            20,
        ),
    ] {
        assert_eq!(one(&mut db, source), Value::Int(want), "{source}");
    }
}

/// A sort key holding a set function beside an item that reads one is
/// the shape the split leaves out: the key would be a value the
/// grouping answers and the projection behind it sorts by, and neither
/// half is where it stands. An unimplemented shape is an error and
/// never a panic, because a client on the other side of the socket
/// cannot tell a crash from a hang.
#[test]
fn a_set_function_in_a_sort_key_beside_one_that_is_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "UNWIND [1, 2] AS n RETURN size(collect(n)) AS v ORDER BY count(*)";
    let err = run(source, &mut db, &[]).expect_err(source);
    let text = err.to_string();
    assert!(text.contains("not implemented yet"), "{source}: {text}");
}
