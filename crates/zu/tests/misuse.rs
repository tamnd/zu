//! Deliberately wrong programs, and what each of them is told.
//!
//! DX2 asks for a misuse suite in both clients: no crash, no leak, and
//! a clear error for every program that is wrong on purpose. Clear is
//! the hard word of the three, so it is spelled out here as three
//! things a message has to do. It names the thing that was wrong, by
//! the name the caller used for it: the file they opened, the column
//! they asked for, the parameter they left out. It says what was
//! expected instead, when there is something to say. And it is the
//! engine's own sentence rather than a syscall's, because "failed to
//! fill whole buffer" is a true statement about a read and tells
//! nobody which file was not a database.
//!
//! No crash is the suite running at all: every case here returns an
//! error, and a panic anywhere in the table fails the run. No leak is
//! checked two ways, since a leak is invisible from inside one call.
//! The same failing open is repeated five hundred times and the
//! database still opens afterwards, which is what a descriptor left
//! behind by every failure would end. And every case is followed by a
//! query on the database it was aimed at, which is what a session or a
//! file handle left pinned by a failure would end.
//!
//! The last test is the other half of a misuse suite and the half that
//! is usually missing: the programs that look like misuse and are not.
//! A parameter nothing reads and a label nothing carries are both
//! legal, and a suite that does not say so is a suite somebody will
//! eventually "fix" the engine against.

use std::io::ErrorKind;
use std::path::Path;

use zu::query::Value;
use zu::{Config, Database, ZuError, params};

/// The statement every case is followed by, on the database it just
/// failed against.
const READ: &str = "MATCH (p:person) RETURN p.uid AS uid";

/// A database of two people, written by the statements a reader would
/// have written.
fn seeded(name: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    {
        let db = Database::create(&path).expect("create");
        let mut conn = db.connect().expect("connect");
        conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")
            .expect("ada");
        conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")
            .expect("grace");
    }
    let db = Database::open(&path).expect("open");
    (dir, db)
}

/// The directory a database is in, which is where the cases that need
/// a file that is not a database put one.
fn beside(db: &Database) -> &Path {
    db.path().parent().expect("a database is in a directory")
}

/// One deliberately wrong program.
struct Misuse {
    /// What it does wrong, as a person would say it.
    what: &'static str,
    /// Runs it against a database that is already there. Returning
    /// `Ok` fails the test: every program in the table is wrong.
    run: fn(&Database) -> zu::Result<()>,
    /// Every phrase the message has to carry. These are the engine's
    /// own words, not the operating system's, so they are the same on
    /// every platform; the three failures that are the operating
    /// system's are checked by the test below this one.
    says: &'static [&'static str],
}

const MISUSES: &[Misuse] = &[
    Misuse {
        what: "opens a file too small to be a database",
        run: |db| {
            let path = beside(db).join("junk.zu1");
            std::fs::write(&path, b"not a database at all")?;
            Database::open(&path).map(|_| ())
        },
        says: &["junk.zu1", "21 bytes", "too short to be a zu1 database"],
    },
    Misuse {
        what: "opens a file the right size and the wrong kind",
        run: |db| {
            let path = beside(db).join("long.zu1");
            std::fs::write(&path, vec![b'x'; 40 * 1024])?;
            Database::open(&path).map(|_| ())
        },
        says: &["long.zu1", "not a zu1 file"],
    },
    Misuse {
        what: "writes through a connection it opened read-only",
        run: |db| {
            let ro = Database::open_with(db.path(), Config::new().read_only(true))?;
            let mut conn = ro.connect()?;
            conn.execute("INSERT (p:person {uid: 3, name: 'kay'})")
                .map(|_| ())
        },
        says: &["misuse.zu1", "read-only"],
    },
    Misuse {
        what: "opens an appender on a connection it opened read-only",
        run: |db| {
            let ro = Database::open_with(db.path(), Config::new().read_only(true))?;
            let mut conn = ro.connect()?;
            conn.appender("person").map(|_| ())
        },
        says: &["appender writes", "read-only"],
    },
    Misuse {
        what: "runs text that will not parse",
        run: |db| {
            let mut conn = db.connect()?;
            conn.query("MATCH (p:person)\nRETURN p.uid AS uid ORDR BY p.uid")
                .map(|_| ())
        },
        says: &["42001", "line 2, column 21", "ORDR"],
    },
    Misuse {
        what: "runs nothing at all",
        run: |db| {
            let mut conn = db.connect()?;
            conn.query("").map(|_| ())
        },
        says: &["42001", "empty query"],
    },
    Misuse {
        what: "leaves out a parameter the statement reads",
        run: |db| {
            let mut conn = db.connect()?;
            conn.query("MATCH (p:person) WHERE p.uid = $uid RETURN p.uid AS uid")
                .map(|_| ())
        },
        says: &["42002", "missing parameter $uid"],
    },
    Misuse {
        what: "names a graph the catalog does not hold",
        run: |db| {
            let mut conn = db.connect()?;
            conn.query("USE nowhere MATCH (p) RETURN p").map(|_| ())
        },
        says: &["42002", "nowhere"],
    },
    Misuse {
        what: "reads a column by a name the result does not have",
        run: |db| {
            let mut conn = db.connect()?;
            let rows = conn.query(READ)?;
            let row = rows.iter().next().expect("a row");
            row.get_by_name::<i64>("nope").map(|_| ())
        },
        says: &["no column 'nope'", "it has uid"],
    },
    Misuse {
        what: "reads a column past the end of the row",
        run: |db| {
            let mut conn = db.connect()?;
            let rows = conn.query(READ)?;
            let row = rows.iter().next().expect("a row");
            row.get_at::<i64>(7).map(|_| ())
        },
        says: &["1 columns", "no column 7"],
    },
    Misuse {
        what: "reads a one-column row into a pair",
        run: |db| {
            let mut conn = db.connect()?;
            let rows = conn.query(READ)?;
            let row = rows.iter().next().expect("a row");
            row.get::<(i64, i64)>().map(|_| ())
        },
        says: &["the row has 1 columns", "asked for 2"],
    },
    Misuse {
        what: "reads a string column as an integer",
        run: |db| {
            let mut conn = db.connect()?;
            let rows = conn.query("MATCH (p:person) RETURN p.name AS name")?;
            let row = rows.iter().next().expect("a row");
            row.get_at::<i64>(0).map(|_| ())
        },
        says: &["22G03", "column 'name'", "expected INT64", "STRING"],
    },
    Misuse {
        what: "runs a prepared statement it never prepared",
        run: |db| {
            let mut conn = db.connect()?;
            conn.execute_prepared(999, &[]).map(|_| ())
        },
        says: &["no prepared statement 999"],
    },
    Misuse {
        what: "runs a prepared statement it already closed",
        run: |db| {
            let mut conn = db.connect()?;
            let (stmt, _) = conn.prepare(READ)?;
            assert!(conn.close_prepared(stmt), "the first close");
            conn.execute_prepared(stmt, &[]).map(|_| ())
        },
        says: &["no prepared statement"],
    },
    Misuse {
        what: "opens an appender on a table that is not there",
        run: |db| {
            let mut conn = db.connect()?;
            conn.appender("nope").map(|_| ())
        },
        says: &["no node table or rel table 'nope'"],
    },
    Misuse {
        what: "appends a row shorter than the table",
        run: |db| {
            let mut conn = db.connect()?;
            let mut rows = conn.appender("person")?;
            rows.append_row((7i64,))
        },
        says: &["row carries 1 values", "the table takes 2"],
    },
    Misuse {
        what: "appends a row with its two values the wrong way round",
        run: |db| {
            let mut conn = db.connect()?;
            let mut rows = conn.appender("person")?;
            rows.append_row(("seven", 7i64))
        },
        says: &["value 0 of the row", "column 0", "integer"],
    },
];

#[test]
fn every_wrong_program_is_told_what_is_wrong_and_leaves_the_database_working() {
    let (dir, db) = seeded("misuse.zu1");
    for case in MISUSES {
        let Err(error) = (case.run)(&db) else {
            panic!("{}: this is supposed to fail and it did not", case.what);
        };
        let message = error.to_string();
        for phrase in case.says {
            assert!(
                message.contains(phrase),
                "{}: the message is missing '{phrase}': {message}",
                case.what
            );
        }
        // The engine's sentence, not the read that noticed.
        assert!(
            !message.contains("failed to fill whole buffer"),
            "{}: {message}",
            case.what
        );

        // And the database it was aimed at is still a database: the
        // failure took nothing with it, so the next caller does not
        // pay for the last one's mistake.
        let mut conn = db.connect().expect("connect after a failure");
        let rows = conn.query(READ).expect("read after a failure");
        assert_eq!(rows.iter().count(), 2, "after: {}", case.what);
    }
    drop(db);
    dir.close().expect("the temporary directory closes");
}

/// The three failures that are the operating system's rather than the
/// engine's. They keep the kind, because code that branches on
/// [`ErrorKind::NotFound`] is code that has something better to do than
/// read English, and they gain the path, because the operating system
/// says "No such file or directory" about a file it declines to name.
#[test]
fn a_failure_the_operating_system_raised_names_the_file_and_keeps_its_kind() {
    let (dir, db) = seeded("kinds.zu1");
    let missing = beside(&db).join("nope.zu1");

    let cases: [(&str, ZuError, ErrorKind, &str); 3] = [
        (
            "a database that is not there",
            Database::open(&missing).expect_err("no such database"),
            ErrorKind::NotFound,
            "nope.zu1",
        ),
        (
            "a database that is not there, read-only",
            Database::open_with(&missing, Config::new().read_only(true))
                .expect_err("no such database"),
            ErrorKind::NotFound,
            "nope.zu1",
        ),
        (
            "a create over a database that is there",
            Database::create(db.path()).expect_err("it is already there"),
            ErrorKind::AlreadyExists,
            "kinds.zu1",
        ),
    ];

    for (what, error, kind, named) in cases {
        let ZuError::Io(io) = &error else {
            panic!("{what}: this is the operating system's failure: {error}");
        };
        assert_eq!(io.kind(), kind, "{what}");
        assert!(io.to_string().contains(named), "{what}: {io}");
        // The system's own words are still in there, since they are
        // the half a reader recognizes.
        assert!(io.to_string().len() > named.len() + 2, "{what}: {io}");
    }

    drop(db);
    dir.close().expect("the temporary directory closes");
}

/// A failure that keeps the file open is a failure a program can only
/// make a few hundred times, and the few hundredth is where it is
/// found, in production, as a limit nobody thought was near. Five
/// hundred is above the default descriptor limit on macOS, so a leak
/// of one per failure ends this test rather than passing it.
#[test]
fn five_hundred_failed_opens_leave_nothing_open() {
    let (dir, db) = seeded("repeat.zu1");
    let missing = beside(&db).join("nope.zu1");
    let junk = beside(&db).join("junk.zu1");
    std::fs::write(&junk, b"not a database at all").expect("junk");

    for _ in 0..500 {
        Database::open(&missing).expect_err("no such database");
        Database::open(&junk).expect_err("not a database");
        Database::create(db.path()).expect_err("it is already there");
    }

    // A descriptor per failure would have run out long ago, and the
    // database this ran beside is still readable and still writable.
    let mut conn = db.connect().expect("connect");
    assert_eq!(conn.query(READ).expect("read").iter().count(), 2);
    conn.execute("INSERT (p:person {uid: 3, name: 'kay'})")
        .expect("write");
    assert_eq!(conn.query(READ).expect("read").iter().count(), 3);

    drop(conn);
    drop(db);
    dir.close().expect("the temporary directory closes");
}

/// A statement that fails partway is the case where "no crash" is not
/// enough: the connection has to be where it was, and so does the
/// data. Both halves are checked, because a write that half happened
/// is worse than one that did not happen at all.
#[test]
fn a_statement_that_failed_wrote_nothing_and_left_the_session_alone() {
    let (dir, db) = seeded("partial.zu1");
    let mut conn = db.connect().expect("connect");

    // The value is computed while the row is being written, so this
    // fails after the statement started and before it committed.
    let error = conn
        .execute("INSERT (p:person {uid: 1 / 0, name: 'kay'})")
        .expect_err("division by zero");
    assert_eq!(
        error.diagnostic().expect("a condition").status.code(),
        "22012"
    );
    assert_eq!(conn.query(READ).expect("read").iter().count(), 2, "no row");

    // The same for an appender: a value the column does not take is
    // refused by the call that appended it, and the rows already
    // buffered are still there to flush.
    let mut rows = conn.appender("person").expect("appender");
    rows.append_row((3i64, "kay")).expect("a row that fits");
    rows.append_row(("four", 4i64))
        .expect_err("one that does not");
    rows.flush().expect("flush");
    drop(rows);
    assert_eq!(
        conn.query(READ).expect("read").iter().count(),
        3,
        "the row that fitted went in and the one that did not stayed out"
    );

    // And the connection is the same connection: a statement prepared
    // before the failures still runs on it.
    let (stmt, names) = conn.prepare(READ).expect("prepare");
    assert!(names.is_empty(), "this one takes no parameters");
    assert_eq!(
        conn.execute_prepared(stmt, &[])
            .expect("run")
            .iter()
            .count(),
        3
    );
    assert!(conn.close_prepared(stmt));

    drop(conn);
    drop(db);
    dir.close().expect("the temporary directory closes");
}

/// The other half of a misuse suite: the programs that look wrong and
/// are not. Each of these is a decision, and a decision nobody wrote
/// down is a decision somebody will reverse by accident.
#[test]
fn the_programs_that_look_like_misuse_and_are_not() {
    let (dir, db) = seeded("allowed.zu1");
    let mut conn = db.connect().expect("connect");

    // A parameter the statement does not read is not an error. A
    // caller that passes one map of parameters to several statements
    // is doing something reasonable, and refusing it would make the
    // map the union of what every statement wants.
    let rows = conn
        .query_with(READ, &params! { "unread" => 1 })
        .expect("an unread parameter is allowed");
    assert_eq!(rows.iter().count(), 2);

    // A label nothing carries matches nothing. It is not a condition,
    // because a pattern that matches no rows is the ordinary answer to
    // a question about a graph, and the alternative is a query that
    // fails on the day the last row of a label is deleted.
    let rows = conn
        .query("MATCH (p:nobody) RETURN p.uid AS uid")
        .expect("a label nothing carries is not an error");
    assert_eq!(rows.iter().count(), 0);

    // Closing a prepared statement twice answers false rather than
    // raising. The question the call answers is whether the handle was
    // open, which a caller cleaning up in a loop is allowed to ask.
    let (stmt, _) = conn.prepare(READ).expect("prepare");
    assert!(conn.close_prepared(stmt), "it was open");
    assert!(!conn.close_prepared(stmt), "and now it is not");

    // Stopping a connection with nothing running is not an error
    // either. The ask is made from another thread, which cannot know
    // whether the statement it meant to stop has already finished, so
    // it is kept and spent on the next statement instead of being
    // dropped, and the caller who asked is the one who takes it back.
    // A handle that cleared itself would race the statement that is
    // still winding down.
    conn.interrupt().stop();
    assert!(matches!(
        conn.query(READ).expect_err("the ask is spent here"),
        ZuError::Interrupted
    ));
    conn.interrupt().clear();
    assert_eq!(conn.query(READ).expect("read").iter().count(), 2);

    // A parameter is a value, and a value of the wrong type against a
    // column is a comparison that is false rather than a failure. GQL
    // compares across types by saying no.
    let rows = conn
        .query_with(
            "MATCH (p:person) WHERE p.uid = $uid RETURN p.uid AS uid",
            &[("uid", Value::Str("ada".into()))],
        )
        .expect("a string against an integer column");
    assert_eq!(rows.iter().count(), 0);

    drop(conn);
    drop(db);
    dir.close().expect("the temporary directory closes");
}
