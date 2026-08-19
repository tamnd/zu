//! How a literal is written: the radix of an integer, the kind of a
//! number, and the text of a string.
//!
//! ISO 21.2 lets the same value be written more than one way, and the
//! spelling is the whole of what these cases measure. A query that says
//! `0xFF` means 255 and a query that says `1.5F` means 1.5, so what
//! matters is that the engine reads the words rather than stopping at
//! them.

use zu::query::{Value, run};
use zu_zu1::file::Zu1File;

fn db(dir: &std::path::Path) -> Zu1File {
    Zu1File::create(&dir.join("literals.zu1")).unwrap()
}

fn one(db: &mut Zu1File, source: &str) -> Value {
    let result = run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
    assert_eq!(result.rows.len(), 1, "{source} returned {:?}", result.rows);
    result.rows[0][0].clone()
}

fn code(db: &mut Zu1File, source: &str) -> String {
    let err = run(source, db, &[]).expect_err(source);
    err.gqlstatus()
        .unwrap_or_else(|| panic!("{source}: {err} carries no status"))
        .code()
        .to_string()
}

/// GL01, GL02, GL03. Four ways to write an integer, one integer.
#[test]
fn an_integer_may_be_written_in_hexadecimal_octal_or_binary() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(one(&mut db, "RETURN 0xFF AS v"), Value::Int(255));
    assert_eq!(one(&mut db, "RETURN 0o17 AS v"), Value::Int(15));
    assert_eq!(one(&mut db, "RETURN 0b1010 AS v"), Value::Int(10));
    assert_eq!(one(&mut db, "RETURN 0xff AS v"), Value::Int(255));

    // The radix is the number's own business, so the four spellings mean
    // the same value and arithmetic does not care which was used.
    assert_eq!(
        one(&mut db, "RETURN (0xFF = 255 AND 0o17 + 0b1 = 16) AS v"),
        Value::Bool(true)
    );
    assert_eq!(one(&mut db, "RETURN -0xFF AS v"), Value::Int(-255));

    // A digit the prefix did not ask for is not a second token: the word
    // is one number and it is a bad one.
    assert_eq!(code(&mut db, "RETURN 0b19 AS v"), "42001");
    assert_eq!(code(&mut db, "RETURN 0o8 AS v"), "42001");
}

/// GL04 to GL10. The suffix names the kind. An exact number with no
/// fraction is still an integer, which is the one case where the suffix
/// changes what comes back rather than only what was meant.
#[test]
fn a_number_may_say_whether_it_is_exact_or_approximate() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(one(&mut db, "RETURN 2.5 AS v"), Value::Float(2.5));
    assert_eq!(one(&mut db, "RETURN 1.25M AS v"), Value::Float(1.25));
    assert_eq!(one(&mut db, "RETURN 1.5E2M AS v"), Value::Float(150.0));
    assert_eq!(one(&mut db, "RETURN 2.5F AS v"), Value::Float(2.5));
    assert_eq!(one(&mut db, "RETURN 1.5E2F AS v"), Value::Float(150.0));
    assert_eq!(one(&mut db, "RETURN 1.5F AS v"), Value::Float(1.5));
    assert_eq!(one(&mut db, "RETURN 1.5D AS v"), Value::Float(1.5));

    // An exact number without a fraction has nothing to approximate, so
    // it stays the integer it was written as, while F and D ask for the
    // float even when the digits would have made an integer.
    assert_eq!(one(&mut db, "RETURN 7M AS v"), Value::Int(7));
    assert_eq!(one(&mut db, "RETURN 7F AS v"), Value::Float(7.0));
    assert_eq!(one(&mut db, "RETURN 7D AS v"), Value::Float(7.0));
    assert_eq!(
        one(
            &mut db,
            "RETURN (7M IS TYPED INT AND 7F IS TYPED FLOAT) AS v"
        ),
        Value::Bool(true)
    );

    // One letter is a suffix and two are a name, so a query that meant
    // to multiply by a variable still does.
    assert_eq!(
        one(&mut db, "UNWIND [2] AS Fx RETURN 3 * Fx AS v"),
        Value::Int(6)
    );
}

/// GL11. The `@` form is the text as written, which is the only way to
/// get a backslash into a string that means a backslash.
#[test]
fn an_at_sign_before_a_string_turns_off_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(
        one(&mut db, r"RETURN @'a\nb' AS v"),
        Value::Str(r"a\nb".into())
    );
    assert_eq!(
        one(&mut db, r"RETURN 'a\nb' AS v"),
        Value::Str("a\nb".into())
    );
    assert_eq!(
        one(&mut db, r"RETURN (@'a\nb' = 'a\\nb') AS v"),
        Value::Bool(true)
    );

    // With no escape to write a quote with, a doubled quote is the one,
    // and it is the only character in the form that is not itself.
    assert_eq!(
        one(&mut db, "RETURN @'it''s' AS v"),
        Value::Str("it's".into())
    );
    assert_eq!(
        one(&mut db, r#"RETURN @"c\td" AS v"#),
        Value::Str(r"c\td".into())
    );

    // An `@` that is not in front of a quote is not part of the language.
    assert_eq!(code(&mut db, "RETURN @x AS v"), "42001");
}

/// The separator ISO 21.3 allows between digits, which is mandatory
/// lexis rather than an optional feature. It changes nothing about the
/// value, which is the point: it is there so a person reading a
/// statement can see where the thousands are.
#[test]
fn the_digits_of_a_number_may_be_grouped() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(one(&mut db, "RETURN 1_000_000 AS v"), Value::Int(1_000_000));
    assert_eq!(one(&mut db, "RETURN 0xF_F AS v"), Value::Int(255));
    assert_eq!(one(&mut db, "RETURN 0b1_0 AS v"), Value::Int(2));
    assert_eq!(one(&mut db, "RETURN 0o1_7 AS v"), Value::Int(15));
    assert_eq!(one(&mut db, "RETURN 1_0.5 AS v"), Value::Float(10.5));
    assert_eq!(one(&mut db, "RETURN 1_000M AS v"), Value::Int(1000));
    assert_eq!(
        one(&mut db, "RETURN (1_000 = 1000) AS v"),
        Value::Bool(true)
    );

    // The separator stands between two digits and nowhere else, so a
    // number with a name behind it is a number and then a name, which
    // is two things where a statement wanted one.
    assert_eq!(code(&mut db, "RETURN 1_000_ AS v"), "42001");
    assert_eq!(code(&mut db, "RETURN 1__000 AS v"), "42001");
}

/// GB02 and GB03. A comment runs to the end of the line from either
/// introducer, and the bracketed form is mandatory.
#[test]
fn a_comment_may_open_with_two_solidi_or_two_minus_signs() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(one(&mut db, "RETURN 1 AS v -- what it is"), Value::Int(1));
    assert_eq!(one(&mut db, "RETURN 1 AS v // what it is"), Value::Int(1));
    assert_eq!(
        one(&mut db, "RETURN /* what it is */ 1 AS v"),
        Value::Int(1)
    );
    assert_eq!(one(&mut db, "RETURN -- what it is\n 1 AS v"), Value::Int(1));

    // Which costs the subtraction of a negative number written with no
    // space, since the standard reads those characters as a comment
    // wherever they stand. What comes back is one column of the value
    // that was in front of them, the name and all.
    assert_eq!(one(&mut db, "RETURN 1 - -2 AS v"), Value::Int(3));
    let result = run("RETURN 1--2 AS v", &mut db, &[]).expect("the statement runs");
    assert_eq!(result.columns, vec!["1".to_string()]);
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// A delimited identifier is a quoted sequence like a string is, so the
/// accent may be doubled to mean itself and the escapes are the same
/// escapes (ISO 21.3). Which is what makes every name writable: a
/// catalog holds the name it was given, not the name a lexer could
/// spell.
#[test]
fn a_delimited_name_holds_what_a_string_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    let result = run("RETURN 1 AS `odd``name`", &mut db, &[]).expect("the statement runs");
    assert_eq!(result.columns, vec!["odd`name".to_string()]);
    let result = run(r"RETURN 1 AS `a\tb`", &mut db, &[]).expect("the statement runs");
    assert_eq!(result.columns, vec!["a\tb".to_string()]);
    let result = run(r"RETURN 1 AS @`raw\name`", &mut db, &[]).expect("the statement runs");
    assert_eq!(result.columns, vec![r"raw\name".to_string()]);
    assert_eq!(code(&mut db, "RETURN 1 AS ``"), "42001");
}

/// The rest of ISO's escape set, which is the same set inside either
/// quote and inside an accent: a backspace, a form feed, an accent, and
/// a character named by the digits of its code point.
#[test]
fn a_string_takes_every_escape_the_standard_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = db(dir.path());
    assert_eq!(
        one(&mut db, r"RETURN 'a\bb' AS v"),
        Value::Str("a\u{8}b".into())
    );
    assert_eq!(
        one(&mut db, r"RETURN 'a\fb' AS v"),
        Value::Str("a\u{c}b".into())
    );
    assert_eq!(
        one(&mut db, r"RETURN 'a\`b' AS v"),
        Value::Str("a`b".into())
    );
    assert_eq!(one(&mut db, r"RETURN 'A' AS v"), Value::Str("A".into()));
    assert_eq!(
        one(&mut db, r"RETURN '\U01F600' AS v"),
        Value::Str("\u{1F600}".into())
    );

    // A doubled quote means the quote in the escaped form too, which is
    // the SQL spelling and is not only the `@` form's.
    assert_eq!(
        one(&mut db, "RETURN 'it''s' AS v"),
        Value::Str("it's".into())
    );
    assert_eq!(
        one(&mut db, r#"RETURN "say ""hi""" AS v"#),
        Value::Str("say \"hi\"".into())
    );

    // Digits that name no character, and digits that are not digits.
    assert_eq!(code(&mut db, r"RETURN '\UD800AA' AS v"), "42001");
    assert_eq!(code(&mut db, r"RETURN '\uZZZZ' AS v"), "42001");
}
