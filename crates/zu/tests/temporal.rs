//! Temporal values end to end: literals, arithmetic, comparison, and
//! the columns a date and a duration are stored in.
//!
//! The unit tests in `zu-common` check the reading and writing of the
//! text. What is only checkable here is that the value survives the
//! whole way: written as a literal, stored in a lane, read back by the
//! executor, and compared against the literal it was written from.

use zu::convert::sqlite_to_zu1;
use zu::query::{Value, run};
use zu_common::{DurationKind, Temporal};
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;
use zu_zu1::props::{PropValues, store_props};

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("temporal.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// The graph the property cases read: one Event whose `on` is a date
/// and whose `takes` is a duration, which is the corpus fixture.
fn dated(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("dated.zu1")).unwrap();
    bulk_load_as(&mut zu, "Event", "ZU_EMPTY", 1, &[]).unwrap();
    let names: Vec<&[u8]> = vec![b"launch"];
    store_props(
        &mut zu,
        "Event",
        &[
            ("name", PropValues::Str(&names)),
            ("on", PropValues::Date(&[19737])),
            (
                "takes",
                PropValues::Duration(DurationKind::DayTime, &[2 * 86_400_000_000_000]),
            ),
        ],
    )
    .unwrap();
    zu
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

#[test]
fn a_temporal_literal_is_read_at_parse_time_and_written_back_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for text in [
        "DATE '2024-01-15'",
        "LOCAL TIME '10:30:00'",
        "LOCAL DATETIME '2024-01-15T10:30:00'",
        "ZONED DATETIME '2024-01-15T10:30:00+07:00'",
        "DURATION 'P2D'",
        "DURATION 'P1Y'",
    ] {
        let source = format!("RETURN {text} AS v");
        let Value::Temporal(_) = one(&mut db, &source) else {
            panic!("{text} did not read as a temporal value");
        };
    }
    assert_eq!(
        one(&mut db, "RETURN DATE '2024-01-15' AS v"),
        Value::Temporal(Temporal::Date(19737))
    );
}

/// A type name in front of a string reads the string as whatever the
/// name says, so a local time written with an offset is refused rather
/// than quietly losing it. A zoned time written without one is not the
/// same case: the standard fills that in from the default displacement,
/// which here is UTC. The type names themselves are reserved words
/// (ISO 21.3), so nothing else is competing for the position.
#[test]
fn a_type_name_in_front_of_a_string_says_how_to_read_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        code(&mut db, "UNWIND [1, 2] AS date RETURN sum(date) AS v"),
        "42001"
    );
    assert_eq!(
        code(&mut db, "RETURN LOCAL TIME '10:30:00+02:00' AS v"),
        "22007"
    );
    assert_eq!(code(&mut db, "RETURN DATE 'the fifteenth' AS v"), "22007");
}

#[test]
fn two_spellings_of_one_instant_are_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "RETURN (ZONED DATETIME '2024-01-15T10:00:00+02:00' \
                  = ZONED DATETIME '2024-01-15T08:00:00Z') AS v";
    assert_eq!(one(&mut db, source), Value::Bool(true));
    let source = "RETURN (DATE '2024-01-15' < DATE '2024-02-01') AS v";
    assert_eq!(one(&mut db, source), Value::Bool(true));
    // Two kinds are two types, and a date is not the time of day it
    // has none of, so the equality is false rather than an error.
    let source = "RETURN (DATE '2024-01-15' = LOCAL TIME '10:00:00') AS v";
    assert_eq!(one(&mut db, source), Value::Bool(false));
}

#[test]
fn a_duration_shifts_an_instant_and_two_instants_leave_a_duration() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        one(&mut db, "RETURN DATE '2024-01-15' + DURATION 'P1D' AS v"),
        Value::Temporal(Temporal::Date(19738))
    );
    assert_eq!(
        one(&mut db, "RETURN DATE '2024-01-31' + DURATION 'P1M' AS v"),
        Value::Temporal(Temporal::Date(19782)),
        "the last of January and a month is the last of February"
    );
    assert_eq!(
        one(&mut db, "RETURN DATE '2024-01-15' - DURATION 'P1D' AS v"),
        Value::Temporal(Temporal::Date(19736))
    );
    assert_eq!(
        one(&mut db, "RETURN DATE '2024-01-16' - DATE '2024-01-15' AS v"),
        Value::Temporal(Temporal::Duration(
            DurationKind::DayTime,
            86_400_000_000_000
        ))
    );
    assert_eq!(
        one(&mut db, "RETURN DURATION 'P1D' + DURATION 'P1D' AS v"),
        Value::Temporal(Temporal::Duration(
            DurationKind::DayTime,
            2 * 86_400_000_000_000
        ))
    );
    assert_eq!(
        one(&mut db, "RETURN DURATION 'P1D' * 2 AS v"),
        Value::Temporal(Temporal::Duration(
            DurationKind::DayTime,
            2 * 86_400_000_000_000
        ))
    );
}

/// The three conditions the corpus asks for by name. Each is a
/// different reason an answer does not exist, and an engine that
/// answered anyway would be answering a question nobody asked.
#[test]
fn temporal_arithmetic_refuses_what_it_cannot_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        code(&mut db, "RETURN DATE '9999-12-31' + DURATION 'P1D' AS v"),
        "22008"
    );
    assert_eq!(
        code(&mut db, "RETURN DATE '2024-01-01' + DURATION 'PT1H' AS v"),
        "22G14"
    );
    assert_eq!(
        code(&mut db, "RETURN DURATION 'P1D' + DURATION 'P1M' AS v"),
        "22G14"
    );
    assert_eq!(
        code(&mut db, "RETURN DURATION 'P1Y' * 1000000000000000000 AS v"),
        "22015"
    );
    assert_eq!(code(&mut db, "RETURN DATE '2024-01-01' * 2 AS v"), "22G03");
}

#[test]
fn a_temporal_value_is_of_its_own_type_and_of_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for (predicate, want) in [
        ("DATE '2024-01-15' IS TYPED DATE", true),
        ("DATE '2024-01-15' IS TYPED LOCAL DATETIME", false),
        ("LOCAL TIME '10:00:00' IS TYPED LOCAL TIME", true),
        ("DURATION 'P1D' IS TYPED DURATION", true),
        ("DURATION 'P1Y' IS TYPED DURATION", false),
        ("DATE '2024-01-15' IS TYPED ANY PROPERTY VALUE", true),
    ] {
        let source = format!("RETURN ({predicate}) AS v");
        assert_eq!(one(&mut db, &source), Value::Bool(want), "{predicate}");
    }
}

#[test]
fn a_stored_date_and_a_stored_duration_compare_against_their_literals() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = dated(dir.path());
    let source = "MATCH (e:Event) WHERE e.on = DATE '2024-01-15' RETURN e.name AS name";
    assert_eq!(one(&mut db, source), Value::Str("launch".into()));
    let source = "MATCH (e:Event) WHERE e.takes = DURATION 'P2D' RETURN e.name AS name";
    assert_eq!(one(&mut db, source), Value::Str("launch".into()));
    let source = "MATCH (e:Event) WHERE e.on > DATE '2024-01-01' RETURN e.name AS name";
    assert_eq!(one(&mut db, source), Value::Str("launch".into()));
    let source = "MATCH (e:Event) RETURN e.on AS on";
    assert_eq!(one(&mut db, source), Value::Temporal(Temporal::Date(19737)));
}

/// The staging route the conformance harness uses: a sqlite file with
/// declared temporal columns, converted and then queried. The
/// declaration is the only thing that says a count of days is a date,
/// so this is what checks it survives the conversion.
#[test]
fn a_staged_date_column_converts_into_a_date_column() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("staged.db");
    let zu1_path = dir.path().join("staged.zu1");
    {
        let mut sq = SqliteStore::open(&db_path).unwrap();
        sq.create_node_table(
            "Event",
            &[
                ("name", ColumnType::Text),
                ("on", ColumnType::Date),
                ("takes", ColumnType::Duration),
            ],
        )
        .unwrap();
        sq.create_rel_table("ZU_EMPTY", "Event", "Event", &[])
            .unwrap();
        sq.insert_node_at(
            "Event",
            0,
            &[
                SqlValue::Text("launch".into()),
                SqlValue::Int(19737),
                SqlValue::Int(2 * 86_400_000_000_000),
            ],
        )
        .unwrap();
    }
    sqlite_to_zu1(&db_path, &zu1_path).unwrap();
    let mut db = Zu1File::open(&zu1_path).unwrap();
    let source = "MATCH (e:Event) WHERE e.on = DATE '2024-01-15' RETURN e.name AS name";
    assert_eq!(one(&mut db, source), Value::Str("launch".into()));
    let source = "MATCH (e:Event) WHERE e.takes = DURATION 'P2D' RETURN e.name AS name";
    assert_eq!(one(&mut db, source), Value::Str("launch".into()));
}
