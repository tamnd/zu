//! No refusal says corrupt unless something is corrupt.
//!
//! `ZuError::Corrupt` means the bytes on disk are not the bytes zu
//! wrote. It is the one error a user cannot act on, because the answer
//! to it is restore from a backup. Saying it about a healthy file when
//! the real problem is a statement zu will not perform sends a user to
//! look for damage that is not there, and it is the difference between
//! a database that refuses a request and a database that reports a
//! fault.
//!
//! Every statement below is legal GQL, run against a file that is
//! known good, and refused. None of them may use the word, and the
//! ones that do today are named in an allowlist that S1 empties. The
//! test is a lint over behaviour rather than over source because a
//! source lint counts call sites and a user counts sentences: the
//! catalog's `validate` is called both when a file is read, where
//! corrupt is the truth, and when a statement is performed, where it
//! is a lie, and the same line is right in one caller and wrong in the
//! other.

use zu::query::run;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A file this test wrote itself, so nothing in it is damaged and any
/// sentence about damage is about something else.
fn healthy(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("refusals.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// A statement that is legal GQL and that a healthy file refuses.
struct Refusal {
    /// What the statement asks that zu will not do.
    asks: &'static str,
    /// Statements to run first, so that the last one is refused for the
    /// reason named rather than for want of a catalog.
    setup: &'static [&'static str],
    /// The statement under test.
    source: &'static str,
    /// Whether the refusal says corrupt today. Every `true` here is a
    /// defect, and the count is asserted so one appearing is a visible
    /// diff. S1 emptied the list and it stays empty.
    says_corrupt: bool,
    /// The condition the refusal carries, or `None` where it still
    /// carries none. Every `None` here is work S1 did not reach.
    status: Option<&'static str>,
}

/// Every refusal this test can reach from a statement.
///
/// The first group is the catalog's `validate`, which is where a graph
/// type is checked and where every check reports damage. The second is
/// the rest of the catalog, which already answers properly, and is here
/// so that the allowlist is a list and not the whole table.
///
/// Not every check in `validate` is in the first group, because not
/// every one of them can be reached from a statement. A key label a
/// node type does not carry cannot: `(:P => :Q)` is performed, and the
/// key label set joins the label set rather than being checked against
/// it, so `Q` is carried by the time `validate` looks. Label bounds and
/// an edge type with no endpoint are the same, structures the parser
/// cannot build. Those checks only fire on a graph type read back out
/// of a file, which is the caller where the word is right.
fn refusals() -> Vec<Refusal> {
    vec![
        // GraphType::validate, reached from a declaration. Four checks a
        // user can trip, four sentences about a file that is fine.
        Refusal {
            asks: "a type no column holds",
            setup: &[],
            source: "CREATE GRAPH TYPE v1 { (:P {v :: DECIMAL(38,2)}) }",
            says_corrupt: false,
            status: Some("42000"),
        },
        Refusal {
            asks: "a union of two property types",
            setup: &[],
            source: "CREATE GRAPH TYPE v2 { (:P {v :: INT64 | STRING}) }",
            says_corrupt: false,
            status: Some("42000"),
        },
        Refusal {
            asks: "the same element type twice",
            setup: &[],
            source: "CREATE GRAPH TYPE v3 { NODE TYPE A (:P), NODE TYPE A (:Q) }",
            says_corrupt: false,
            status: Some("42000"),
        },
        Refusal {
            asks: "the same property twice",
            setup: &[],
            source: "CREATE GRAPH TYPE v4 { (:P {v :: INT64, v :: INT64}) }",
            says_corrupt: false,
            status: Some("42000"),
        },
        // The rest of the catalog, which answers a request it will not
        // perform by saying so.
        Refusal {
            asks: "a graph type name that is taken",
            setup: &["CREATE GRAPH TYPE taken { (:P) }"],
            source: "CREATE GRAPH TYPE taken { (:P) }",
            says_corrupt: false,
            status: None,
        },
        Refusal {
            asks: "a graph type that is not there",
            setup: &[],
            source: "CREATE GRAPH gg :: nosuch",
            says_corrupt: false,
            status: None,
        },
        Refusal {
            asks: "a graph to resemble that is not there",
            setup: &[],
            source: "CREATE GRAPH TYPE likeit LIKE nosuch",
            says_corrupt: false,
            status: None,
        },
        Refusal {
            asks: "a schema that is not there",
            setup: &[],
            source: "CREATE GRAPH /nosuch/g",
            says_corrupt: false,
            status: None,
        },
        Refusal {
            asks: "a schema name that is taken",
            setup: &["CREATE SCHEMA /s"],
            source: "CREATE SCHEMA /s",
            says_corrupt: false,
            status: None,
        },
    ]
}

/// The lint. Nothing here is damaged, so nothing here says damaged,
/// except what the allowlist admits.
#[test]
fn no_refusal_of_a_healthy_file_says_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = healthy(dir.path());
    let table = refusals();
    let admitted = table.iter().filter(|r| r.says_corrupt).count();
    for r in &table {
        for s in r.setup {
            run(s, &mut db, &[]).unwrap_or_else(|e| panic!("setup {s}: {e}"));
        }
        let err = run(r.source, &mut db, &[])
            .err()
            .unwrap_or_else(|| panic!("{}: {} was performed", r.asks, r.source));
        let said = err.to_string().contains("corrupt");
        assert_eq!(said, r.says_corrupt, "{}: {err}", r.asks);
        let got = err.gqlstatus().map(|s| s.to_string());
        assert_eq!(got.as_deref(), r.status, "{}: {err}", r.asks);
    }
    assert_eq!(
        admitted, 0,
        "S1 emptied the allowlist and nothing puts anything back on it"
    );
    let uncondemned = table.iter().filter(|r| r.status.is_none()).count();
    assert_eq!(
        uncondemned, 5,
        "the refusals that still carry no condition, which S1 did not reach"
    );
}

/// The other caller, where the word is right and stays.
///
/// A file whose bytes are not the bytes zu wrote is corruption, and
/// saying so is the error doing its job. This is why the lint above is
/// over behaviour and not over source: the very checks that are wrong
/// for a declaration are right when the same catalog is read back off
/// a disk that changed under it, so a lint that counted `corrupt` call
/// sites would have to call these wrong too, and they are not.
#[test]
fn a_file_that_is_damaged_is_told_it_is_damaged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.zu1");
    {
        let mut zu = Zu1File::create(&path).unwrap();
        bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
        run(
            "CREATE GRAPH TYPE shape { (:P {v :: INT64}) }",
            &mut zu,
            &[],
        )
        .unwrap();
        zu.checkpoint().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();

    // Too short to hold a header.
    std::fs::write(&path, &bytes[..512]).unwrap();
    let err = Zu1File::open(&path).expect_err("512 bytes is not a database");
    assert!(err.to_string().contains("corrupt file header"), "{err}");

    // Long enough, and every byte after the magic is wrong.
    let mut flipped = bytes.clone();
    for b in flipped.iter_mut().skip(8) {
        *b ^= 0xff;
    }
    std::fs::write(&path, &flipped).unwrap();
    let err = Zu1File::open(&path).expect_err("a flipped header is not a header");
    assert!(err.to_string().contains("crc mismatch"), "{err}");
}
