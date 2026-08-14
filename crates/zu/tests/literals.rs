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
