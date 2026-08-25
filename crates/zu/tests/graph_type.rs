//! What a graph type may say about a property, end to end from GQL.
//!
//! `zu-zu1` pins the frontier at the encoder, where a `LogicalType` is
//! either written or refused. This pins the same frontier where a user
//! meets it, which is a `CREATE GRAPH TYPE` and a spelling, and the two
//! are not the same question: a type the encoder would take is no use
//! if no spelling reaches it, and a spelling the parser takes is no use
//! if the catalog then refuses it.
//!
//! S1 made every one of those refusals a condition. What is left is the
//! frontier itself, which is S2's work, and it is pinned below so that
//! a type crossing it is a change somebody is measuring.

use zu::query::run;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("graph_type.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// What happened when a graph type named a property of this type.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// The declaration stood.
    Declared,
    /// The parser did not reach a type at all: 42001, invalid syntax.
    NotSpelled,
    /// The parser read a type and the catalog would not write it. The
    /// string is what the user was told.
    NotStored(String),
}

/// Declares one property of `ty`, under a name of its own so that a
/// file can be asked about many types in a row.
///
/// Both refusals carry a condition now, so the two are told apart by
/// which one: a type with no spelling never reaches the catalog and is
/// 42001, and a type the catalog will not write is 42000.
fn declare(db: &mut Zu1File, n: usize, ty: &str) -> Answer {
    let source = format!("CREATE GRAPH TYPE probe{n} {{ (:Probe {{v :: {ty}}}) }}");
    match run(&source, db, &[]) {
        Ok(_) => Answer::Declared,
        Err(e) => {
            let message = e.to_string();
            match e.gqlstatus().map(|s| s.to_string()).as_deref() {
                Some("42001") => Answer::NotSpelled,
                _ => Answer::NotStored(message),
            }
        }
    }
}

/// Every spelling the frontier turns on, and the answer it gets today.
///
/// The order is the order of `schema/02` section 2, so this table and
/// that one can be read side by side.
fn probe() -> Vec<(&'static str, Answer)> {
    let stored = |m: &str| Answer::NotStored(m.into());
    let cannot_write = |n: usize| {
        stored(&format!(
            "42000: element type 'probe{n}.Probe' declares 'v' with a type this file cannot write"
        ))
    };
    vec![
        // Storable, and every one of them has a spelling.
        ("BOOL", Answer::Declared),
        ("INT8", Answer::Declared),
        ("INT16", Answer::Declared),
        ("INT32", Answer::Declared),
        ("INT64", Answer::Declared),
        ("UINT8", Answer::Declared),
        ("UINT16", Answer::Declared),
        ("UINT32", Answer::Declared),
        ("UINT64", Answer::Declared),
        ("FLOAT32", Answer::Declared),
        ("FLOAT64", Answer::Declared),
        ("STRING", Answer::Declared),
        ("BYTES", Answer::Declared),
        ("DATE", Answer::Declared),
        ("LOCAL TIME", Answer::Declared),
        ("LOCAL DATETIME", Answer::Declared),
        ("DURATION", Answer::Declared),
        ("ZONED TIME", Answer::Declared),
        ("ZONED DATETIME", Answer::Declared),
        ("STRING(1,512)", Answer::Declared),
        ("CHAR(2)", Answer::Declared),
        ("BINARY(16)", Answer::Declared),
        ("LIST<FLOAT32>", Answer::Declared),
        // S2's first two rows. A bounded list and a nested one are both
        // declarable now: the catalog remembers the bound and the
        // element's nullability, which is what every later check about
        // an embedding is against. Neither has a column encoding yet,
        // so a declaration is still all they are.
        ("LIST<FLOAT32 NOT NULL>[768]", Answer::Declared),
        ("LIST<LIST<STRING>>", Answer::Declared),
        // A decimal a 64 bit lane holds. The column stores unscaled
        // units and the declared scale is what makes them a number, so
        // twelve digits of them fit a word and this is a column a
        // statement can fill.
        ("DECIMAL(12,2)", Answer::Declared),
        // Spelled and not stored. This is the list S2 works through.
        // The condition is 42000 for all of them, which is what S1
        // replaced the sentence about corruption with.
        //
        // The wide decimal is here for the same reason INT128 is: its
        // unscaled units want more than a lane word, and the lane is
        // sixty four bits. It is the one row on this list whose sibling
        // is on the list above.
        ("DECIMAL(38,2)", cannot_write(27)),
        ("INT128", cannot_write(28)),
        ("INT256", cannot_write(29)),
        ("UINT128", cannot_write(30)),
        ("FLOAT16", cannot_write(31)),
        ("FLOAT128", cannot_write(32)),
        ("FLOAT256", cannot_write(33)),
        ("ANY", cannot_write(34)),
        ("ANY PROPERTY VALUE", cannot_write(35)),
        ("PATH", cannot_write(36)),
        ("NODE", cannot_write(37)),
        ("EDGE", cannot_write(38)),
        ("GRAPH", cannot_write(39)),
        ("BINDING TABLE", cannot_write(40)),
        ("NULL", cannot_write(41)),
        ("NOTHING", cannot_write(42)),
        // Not spelled at all. The year month duration is the one type a
        // column already holds that no declaration can ask for.
        ("YEAR MONTH DURATION", Answer::NotSpelled),
    ]
}

/// The frontier as a user meets it: forty three spellings, and which of
/// the three answers each one gets.
#[test]
fn a_graph_type_declares_the_types_the_frontier_says_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let mut declared = 0;
    for (n, (ty, want)) in probe().into_iter().enumerate() {
        let got = declare(&mut db, n + 1, ty);
        assert_eq!(got, want, "{ty}");
        if got == Answer::Declared {
            declared += 1;
        }
    }
    assert_eq!(declared, 26, "the declarable set changed");
}

/// The frontier has a far side, and one type is on it.
///
/// A column code for the year month duration has existed since the
/// first version of the property format, and `DURATION_BETWEEN(a, b)
/// YEAR TO MONTH` produces values of it, so the store holds them
/// happily. No declaration can ask for one. `zu-common`'s type name
/// table knows the spelling `YEAR MONTH DURATION` and the parser's
/// table does not, and the grammar's `type_name` is at most two
/// identifiers, so a three word name could not reach it either.
///
/// So the two duration kinds are not symmetric: one is declarable and
/// the other is only reachable by computing one. S1 owes an answer to
/// whether ISO gives this type a spelling that fits `type_name`, and if
/// it does, the parser owes the row.
#[test]
fn the_year_month_duration_is_stored_and_cannot_be_declared() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    // The day time duration declares.
    assert_eq!(declare(&mut db, 100, "DURATION"), Answer::Declared);
    // Its sibling does not, and the refusal is about the first word.
    let source = "CREATE GRAPH TYPE ym { (:Span {d :: YEAR MONTH DURATION}) }";
    let err = run(source, &mut db, &[]).expect_err("there is no such spelling");
    assert_eq!(err.gqlstatus().map(|s| s.to_string()), Some("42001".into()));
    assert!(err.to_string().contains("YEAR"), "{err}");
    // And the engine computes values of it either way.
    let counted = run(
        "RETURN DURATION_BETWEEN(DATE '2024-01-01', DATE '2026-03-01') YEAR TO MONTH AS d",
        &mut db,
        &[],
    )
    .expect("the year month duration is a value this engine has");
    assert_eq!(counted.rows.len(), 1);
}

/// Every refusal above carries a condition and none of them says
/// corrupt, which is what S1 changed.
///
/// A user who writes `DECIMAL(38,2)` has written a legal GQL statement
/// that this engine will not perform. Before S1 they were told their
/// file was damaged, with no condition to catch on. Now it is 42000,
/// syntax error or access rule violation, which is the class the
/// standard keeps for a statement the engine will not carry out. The
/// two data exception codes that say invalid value type, 22G03 and
/// 22G12, are class 22 and are about a value at run time; a type in a
/// declaration is not a value.
///
/// The example is the wide decimal rather than the narrow one because
/// the narrow one now stores. Thirty eight digits of unscaled units do
/// not fit a lane word, so this is where the same sentence still holds.
#[test]
fn a_type_the_catalog_will_not_write_is_refused_with_a_condition() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "CREATE GRAPH TYPE money { (:Purchase {total :: DECIMAL(38,2)}) }";
    let err = run(source, &mut db, &[]).expect_err("a wide decimal is not storable");
    assert_eq!(err.gqlstatus().map(|s| s.to_string()), Some("42000".into()));
    assert!(!err.to_string().contains("corrupt"), "{err}");
    assert!(
        err.to_string().contains("DECIMAL") || err.to_string().contains("total"),
        "{err}"
    );
}

/// A type the parser refuses is refused as text, with a syntax
/// condition and with the place the text went wrong.
///
/// S1 added the place. An unknown type name used to raise 42001 saying
/// only that the statement had a bad type, which in a declaration of
/// thirty properties is a search over the user's own statement. The
/// offset is where the type began rather than where the parser stopped,
/// because the word the reader has to change is the first one.
///
/// The excerpt comes with it, and is the whole line the position falls
/// on, so a caller holding nothing but the error can still underline
/// the token. `refusal_shape.rs` pins the rest of the envelope.
#[test]
fn a_type_with_no_spelling_is_a_syntax_error_that_says_where() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "CREATE GRAPH TYPE nope { (:Probe {v :: NOSUCHTYPE}) }";
    let err = run(source, &mut db, &[]).expect_err("there is no such type");
    assert_eq!(err.gqlstatus().map(|s| s.to_string()), Some("42001".into()));
    assert!(err.to_string().contains("NOSUCHTYPE"), "{err}");
    let at = err.position().expect("the type name is somewhere");
    assert_eq!((at.line, at.column), (1, 40), "{err}");
    assert_eq!(&source[at.offset as usize..], "NOSUCHTYPE}) }");
    assert_eq!(err.excerpt(), Some(source), "one line, quoted whole");
}
