//! The function registry of ISO 20: what a builtin is called, what it
//! takes, what it answers, and when it is answered.
//!
//! One table holds all four, so what these tests ask is that the table
//! is the only place the answers come from. A name resolves to a
//! signature and every spelling of that name resolves to the same one,
//! a call that does not fit the signature is refused by the signature's
//! own words, and a deterministic call over what the statement wrote is
//! answered while the statement is bound rather than once per row it
//! would have reached.

use zu::Database;
use zu::query::Value;
use zu_common::temporal::NANOS_PER_DAY;
use zu_common::{DurationKind, Temporal};

fn opened(dir: &std::path::Path) -> Database {
    let db = Database::create(dir.join("functions.zu1")).expect("create");
    {
        let mut conn = db.connect().expect("connect");
        for (name, height) in [("ana", -2.5), ("bo", 6.25)] {
            conn.execute(&format!(
                "INSERT (p:person {{name: '{name}', height: {height}}})"
            ))
            .expect("a person");
        }
    }
    db
}

fn one(db: &Database, source: &str) -> Value {
    let mut conn = db.connect().expect("connect");
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let rows: Vec<_> = rows.iter().collect();
    assert_eq!(rows.len(), 1, "{source} answered {} rows", rows.len());
    rows[0].value(0).expect("a value").clone()
}

fn refused(db: &Database, source: &str) -> String {
    let mut conn = db.connect().expect("connect");
    conn.query(source)
        .expect_err(&format!("{source} should have been refused"))
        .to_string()
}

/// A name is matched against the table without regard to case, and the
/// long spelling of a function is the same function as the short one,
/// so the two answer the same value and the plan names them alike.
#[test]
fn every_spelling_of_a_name_reaches_the_same_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    for source in [
        "MATCH (p:person) RETURN CHAR_LENGTH(p.name) AS n ORDER BY n",
        "MATCH (p:person) RETURN character_length(p.name) AS n ORDER BY n",
        "MATCH (p:person) RETURN Char_Length(p.name) AS n ORDER BY n",
    ] {
        let rows = conn
            .query(source)
            .unwrap_or_else(|e| panic!("{source}: {e}"));
        let lengths: Vec<i64> = rows
            .iter()
            .map(|row| row.get_by_name::<i64>("n").expect("n"))
            .collect();
        assert_eq!(lengths, [2, 3], "{source}");
        let plan = conn.explain(source).expect("explain");
        assert!(plan.contains("char_length("), "{source}: {plan}");
    }
}

/// A deterministic function over values the statement wrote answers the
/// same thing on every row, so it is answered once while binding and
/// the plan carries the answer instead of the call.
#[test]
fn a_call_over_what_the_statement_wrote_is_answered_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let plan = conn
        .explain("MATCH (p:person) RETURN UPPER('ab') AS u")
        .expect("explain");
    assert!(plan.contains("'AB'"), "{plan}");
    assert!(!plan.contains("upper("), "{plan}");

    // And the answer is the answer, folded or not.
    assert_eq!(one(&db, "RETURN UPPER('ab') AS u"), Value::Str("AB".into()));
    assert_eq!(one(&db, "RETURN CHAR_LENGTH('日本') AS n"), Value::Int(2));

    // A call over a column is a call: the plan keeps it, because what
    // it answers is one thing per row.
    let plan = conn
        .explain("MATCH (p:person) RETURN UPPER(p.name) AS u")
        .expect("explain");
    assert!(plan.contains("upper("), "{plan}");
}

/// GF01 to GF03, the numeric library over a column: what the kernels
/// answer per row, that an exact argument stays exact through the
/// roundings, and that a function with no answer for the value one row
/// holds raises the condition the standard names rather than handing
/// back a NaN that every comparison below would read as false.
#[test]
fn the_numeric_library_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person) RETURN ABS(p.height) AS a, FLOOR(p.height) AS f, SQRT(ABS(p.height)) AS s ORDER BY a";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let read = |rows: &zu::query::QueryResult, name: &str| -> Vec<f64> {
        rows.iter()
            .map(|row| row.get_by_name::<f64>(name).expect(name))
            .collect()
    };
    assert_eq!(read(&rows, "a"), [2.5, 6.25]);
    assert_eq!(read(&rows, "f"), [-3.0, 6.0]);
    assert_eq!(read(&rows, "s"), [2.5_f64.sqrt(), 2.5]);

    // The call over a column stays in the plan, one literal argument
    // and all is answered while binding, and the two agree.
    let plan = conn
        .explain("MATCH (p:person) RETURN ABS(p.height) AS a")
        .expect("explain");
    assert!(plan.contains("abs("), "{plan}");
    let plan = conn.explain("RETURN ABS(-3) AS a").expect("explain");
    assert!(!plan.contains("abs("), "{plan}");
    assert_eq!(one(&db, "RETURN ABS(-3) AS a"), Value::Int(3));
    assert_eq!(one(&db, "RETURN MOD(7, 3) AS m"), Value::Int(1));
    assert_eq!(one(&db, "RETURN POWER(2, 3) AS p"), Value::Float(8.0));

    // A condition the values raise is raised where the values are, so
    // the same statement over a column that held no negative height
    // would have answered.
    let says = refused(&db, "MATCH (p:person) RETURN LN(p.height) AS l");
    assert!(says.contains("ln()"), "{says}");
    let says = refused(&db, "RETURN MOD(1, 0) AS m");
    assert!(says.contains("division by zero"), "{says}");
}

/// GF05 and GF06, the trim family: the explicit form over a column,
/// which is the one spelling that is not written like a call, and the
/// three multi-character functions beside it.
#[test]
fn the_trim_family_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    // The names in the fixture have no spaces on them, so what is
    // trimmed here is a letter, which is also the only way to see that
    // the end named is the end trimmed.
    let source = "MATCH (p:person) RETURN TRIM(LEADING 'a' FROM p.name) AS l ORDER BY l";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.get_by_name::<String>("l").expect("l"))
        .collect();
    assert_eq!(names, ["bo", "na"]);

    // The call over a column stays in the plan and is printed under the
    // name of the row that answers it, and the same form over what the
    // statement wrote is answered while binding.
    let plan = conn.explain(source).expect("explain");
    assert!(plan.contains("trim_leading("), "{plan}");
    let plan = conn
        .explain("RETURN TRIM(LEADING 'x' FROM 'xxay') AS v")
        .expect("explain");
    assert!(!plan.contains("trim"), "{plan}");

    assert_eq!(
        one(&db, "RETURN TRIM(TRAILING 'y' FROM 'xxay') AS v"),
        Value::Str("xxa".into())
    );
    assert_eq!(
        one(&db, "RETURN TRIM('  a  ') AS v"),
        Value::Str("a".into())
    );
    assert_eq!(
        one(&db, "RETURN BTRIM('xyaxy', 'xy') AS v"),
        Value::Str("a".into())
    );

    // The three words are ordinary names everywhere else, which is what
    // keeps a query that bound one of them readable.
    assert_eq!(
        one(&db, "LET leading = 'x' RETURN TRIM(leading) AS v"),
        Value::Str("x".into())
    );

    // One character, and the condition the standard names when it is
    // handed more, which is the whole reason the three above exist.
    let says = refused(&db, "RETURN TRIM(BOTH 'ab' FROM 'abx') AS v");
    assert!(says.contains("trims one character"), "{says}");
}

/// ISO 20.24, the substring function: LEFT and RIGHT over a column,
/// which in GQL are the whole of it, since SUBSTRING is a word the
/// standard has reserved and given no meaning to.
#[test]
fn the_substring_function_answers_over_a_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person) RETURN LEFT(p.name, 2) AS l ORDER BY l";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let names: Vec<String> = rows
        .iter()
        .map(|row| row.get_by_name::<String>("l").expect("l"))
        .collect();
    assert_eq!(names, ["an", "bo"]);

    let plan = conn.explain(source).expect("explain");
    assert!(plan.contains("left("), "{plan}");
    let plan = conn.explain("RETURN LEFT('abc', 2) AS l").expect("explain");
    assert!(!plan.contains("left("), "{plan}");

    assert_eq!(
        one(&db, "RETURN RIGHT('abc', 2) AS v"),
        Value::Str("bc".into())
    );
    // The middle of a string is one written inside the other, there
    // being no third function for it.
    assert_eq!(
        one(&db, "RETURN LEFT(RIGHT('abcde', 4), 2) AS v"),
        Value::Str("bc".into())
    );

    // A count is a number and a string is a string, both settled while
    // binding, and a count no string has is the standard's condition.
    let says = refused(&db, "RETURN LEFT('abc', 'two') AS v");
    assert!(
        says.contains("left() needs a string and a count of characters"),
        "{says}"
    );
    let says = refused(&db, "MATCH (p:person) RETURN RIGHT(p.name, -1) AS v");
    assert!(says.contains("negative number"), "{says}");
}

/// What a signature refuses, and in its own words: a name no builtin
/// has, a count of arguments the signature does not allow, and a type
/// the function has nothing to say about.
#[test]
fn what_the_registry_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());

    let says = refused(&db, "RETURN NOSUCHTHING(1) AS v");
    assert!(says.contains("unknown function"), "{says}");

    let says = refused(&db, "RETURN CHAR_LENGTH('a', 'b') AS v");
    assert!(says.contains("takes 1 argument(s), got 2"), "{says}");

    let says = refused(&db, "MATCH (p:person) RETURN SAME(p) AS v");
    assert!(says.contains("at least two"), "{says}");

    let says = refused(&db, "RETURN CHAR_LENGTH(1) AS v");
    assert!(says.contains("char_length() needs a string"), "{says}");

    let says = refused(&db, "RETURN CARDINALITY('ab') AS v");
    assert!(says.contains("cardinality() needs a list"), "{says}");

    let says = refused(&db, "MATCH (p:person) RETURN UPPER(*) AS v");
    assert!(says.contains("only count(*) takes *"), "{says}");

    let says = refused(&db, "RETURN ABS('a') AS v");
    assert!(says.contains("abs() needs a number"), "{says}");

    let says = refused(&db, "RETURN ROUND(1.5, 2, 3) AS v");
    assert!(says.contains("takes 1 or 2 argument(s), got 3"), "{says}");
}

/// The datetime value functions of ISO 20.27: what time the statement
/// is running at, cut five ways.
///
/// Two things are asked here that no other function asks. One is that
/// the five agree: a statement holding CURRENT_DATE beside
/// CURRENT_TIMESTAMP reads one instant, so the date it answers is the
/// date that instant fell on and not the date a second clock read a
/// moment later. The other is that the answer is not folded, since a
/// folded one would be written into the plan and every later statement
/// reading that plan would be told the time it was compiled at.
#[test]
fn the_datetime_value_functions_answer_one_instant() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());
    let mut conn = db.connect().expect("connect");

    let source = "RETURN CURRENT_DATE AS d, CURRENT_TIME AS t, CURRENT_TIMESTAMP AS ts, \
                  LOCAL_TIME AS lt, LOCAL_TIMESTAMP AS lts";
    let rows = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let rows: Vec<_> = rows.iter().collect();
    assert_eq!(rows.len(), 1);
    let read = |name: &str| match rows[0].value_by_name(name).expect(name) {
        Value::Temporal(t) => *t,
        other => panic!("{name} answered {other:?}"),
    };
    let (date, time, stamp) = (read("d"), read("t"), read("ts"));
    let (local_time, local_stamp) = (read("lt"), read("lts"));

    // The instant, and the four cuts of it, each stated as the
    // arithmetic that takes the whole to the part.
    let Temporal::ZonedDatetime { nanos, offset } = stamp else {
        panic!("current_timestamp answered {stamp:?}");
    };
    assert_eq!(offset, 0, "zu's session displacement is UTC");
    assert_eq!(date, Temporal::Date((nanos / NANOS_PER_DAY) as i32));
    assert_eq!(
        time,
        Temporal::ZonedTime {
            nanos: nanos % NANOS_PER_DAY,
            offset: 0
        }
    );
    assert_eq!(local_time, Temporal::LocalTime(nanos % NANOS_PER_DAY));
    assert_eq!(local_stamp, Temporal::LocalDatetime(nanos));

    // Every row of a scan reads the same instant, the clock having
    // been read once before the first of them.
    let rows = conn
        .query("MATCH (p:person) RETURN CURRENT_TIMESTAMP AS ts")
        .expect("a scan");
    let seen: Vec<Value> = rows
        .iter()
        .map(|row| row.value(0).expect("ts").clone())
        .collect();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);

    // Not folded, in the plan and in the answer alike. The plan names
    // the word rather than a time, and running the same statement
    // again reads the clock again.
    let plan = conn.explain(source).expect("explain");
    assert!(plan.contains("CURRENT_TIMESTAMP"), "{plan}");
    std::thread::sleep(std::time::Duration::from_millis(2));
    let again = conn
        .query(source)
        .unwrap_or_else(|e| panic!("{source}: {e}"));
    let again: Vec<_> = again.iter().collect();
    let later = match again[0].value_by_name("ts").expect("ts") {
        Value::Temporal(t) => *t,
        other => panic!("ts answered {other:?}"),
    };
    assert_ne!(later, stamp, "a cached plan handed back the old instant");

    // They are words and not calls, so the parentheses are a call to a
    // function nobody defined.
    let says = refused(&db, "RETURN CURRENT_DATE() AS d");
    assert!(says.contains("unknown function"), "{says}");
}

/// ISO 20.28, the datetime subtraction. The qualifier behind the
/// brackets picks the kind of the answer, the answer runs from the
/// first argument to the second, and the months are counted against
/// zu's own month addition rather than against a comparison of day
/// numbers.
#[test]
fn duration_between_counts_from_the_first_datetime_to_the_second() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = opened(dir.path());

    let duration = |source: &str| match one(&db, source) {
        Value::Temporal(Temporal::Duration(kind, count)) => (kind, count),
        other => panic!("{source} answered {other:?}"),
    };

    // No qualifier is DAY TO SECOND, and DAY TO SECOND written out is
    // the same call.
    let january = "DATE '2024-01-31'";
    let march = "DATE '2024-03-01'";
    let thirty = 30 * NANOS_PER_DAY;
    assert_eq!(
        duration(&format!("RETURN DURATION_BETWEEN({january}, {march})")),
        (DurationKind::DayTime, thirty)
    );
    assert_eq!(
        duration(&format!(
            "RETURN DURATION_BETWEEN({january}, {march}) DAY TO SECOND"
        )),
        (DurationKind::DayTime, thirty)
    );

    // Backwards is the same length with the other sign, and the minus
    // operator is the same call with the arguments the other way round.
    assert_eq!(
        duration(&format!("RETURN DURATION_BETWEEN({march}, {january})")),
        (DurationKind::DayTime, -thirty)
    );
    assert_eq!(
        duration(&format!("RETURN {march} - {january}")),
        (DurationKind::DayTime, thirty)
    );

    // A month is whole or it is not counted, and what counts as whole
    // is stated by the addition: 31 January to 1 March is one month
    // and a day, and 31 March to 30 April is one month exactly,
    // because 31 March plus one month is 30 April.
    assert_eq!(
        duration(&format!(
            "RETURN DURATION_BETWEEN({january}, {march}) YEAR TO MONTH"
        )),
        (DurationKind::YearMonth, 1)
    );
    assert_eq!(
        duration("RETURN DURATION_BETWEEN(DATE '2024-03-31', DATE '2024-04-30') YEAR TO MONTH"),
        (DurationKind::YearMonth, 1)
    );
    assert_eq!(
        duration("RETURN DURATION_BETWEEN(DATE '2020-01-15', DATE '2024-07-14') YEAR TO MONTH"),
        (DurationKind::YearMonth, 53)
    );
    assert_eq!(
        duration(&format!(
            "RETURN DURATION_BETWEEN({march}, {january}) YEAR TO MONTH"
        )),
        (DurationKind::YearMonth, -1)
    );

    // The time of day is part of the answer, and a zoned value is read
    // on its own wall clock for the months and on the instant for the
    // nanoseconds.
    assert_eq!(
        duration(
            "RETURN DURATION_BETWEEN(LOCAL DATETIME '2024-01-01T23:00:00', \
             LOCAL DATETIME '2024-01-02T01:30:00')"
        ),
        (
            DurationKind::DayTime,
            2 * 3_600_000_000_000 + 1_800_000_000_000
        )
    );
    assert_eq!(
        duration(
            "RETURN DURATION_BETWEEN(ZONED DATETIME '2024-01-01T00:00:00+07:00', \
             ZONED DATETIME '2024-01-01T00:00:00Z')"
        ),
        (DurationKind::DayTime, 7 * 3_600_000_000_000)
    );

    // A null on either side answers null, the way every other kernel
    // does.
    assert_eq!(
        one(&db, &format!("RETURN DURATION_BETWEEN({january}, NULL)")),
        Value::Null
    );

    // Two values of different shapes have no length between them, and
    // neither has a value that is not a datetime at all.
    let says = refused(
        &db,
        "RETURN DURATION_BETWEEN(DATE '2024-01-31', LOCAL TIME '10:00:00')",
    );
    assert!(says.contains("no DAY TIME DURATION"), "{says}");
    let says = refused(&db, "RETURN DURATION_BETWEEN(1, 2)");
    assert!(says.contains("expects a datetime"), "{says}");

    // The arity is the row's, so a short call is refused the way every
    // short call is.
    let says = refused(&db, &format!("RETURN DURATION_BETWEEN({january})"));
    assert!(says.contains("takes 2 argument(s)"), "{says}");

    // Only the two runs the standard names are a qualifier.
    let says = refused(
        &db,
        &format!("RETURN DURATION_BETWEEN({january}, {march}) YEAR TO DAY"),
    );
    assert!(says.contains("not a duration qualifier"), "{says}");

    // Everything above is answered while binding, both arguments being
    // literals, so the same call is run once over arguments that are
    // not, which is the kernel the rows go through.
    assert_eq!(
        duration("RETURN DURATION_BETWEEN(CURRENT_DATE, CURRENT_DATE)"),
        (DurationKind::DayTime, 0)
    );
    assert_eq!(
        duration("RETURN DURATION_BETWEEN(CURRENT_DATE, CURRENT_DATE) YEAR TO MONTH"),
        (DurationKind::YearMonth, 0)
    );

    // The plan prints the qualifier behind the brackets, where a query
    // writes it, and prints it even where the query left it out.
    let mut conn = db.connect().expect("connect");
    let plan = conn
        .explain("RETURN DURATION_BETWEEN(CURRENT_DATE, CURRENT_DATE)")
        .expect("explain");
    assert!(plan.contains("DURATION_BETWEEN"), "{plan}");
    assert!(plan.contains("DAY TO SECOND"), "{plan}");
}
