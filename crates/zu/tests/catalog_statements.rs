//! The six catalog statements, in every form their productions allow.
//!
//! ISO/IEC 39075:2024 has no `ALTER`. The whole catalog modifying
//! surface of the standard is `CREATE SCHEMA`, `DROP SCHEMA`, `CREATE
//! GRAPH`, `DROP GRAPH`, `CREATE GRAPH TYPE` and `DROP GRAPH TYPE`, and
//! everything the schema program does has to be said with those six or
//! not said at all. So this is the standing gate under it: every
//! optional word of every one of the six productions in
//! `docs/grammar.ebnf`, written out, and each one has to parse.
//!
//! A syntax error here is this engine saying the standard's grammar is
//! not a grammar it reads, and that is the one claim the project makes
//! about itself. So the gate is over what the engine did rather than
//! only over whether it parsed: forty three forms of the six, and every
//! one performed. Asking only whether a form parsed would pass just as
//! well if all forty three died at the same wall one step later.
//!
//! The negative half is the load bearing half. There is no `ALTER` in
//! the standard, so there is none here, and the schema program's whole
//! argument is that `CREATE OR REPLACE GRAPH TYPE` is the `ALTER`. That
//! argument is worth nothing if an `ALTER` quietly appears one release
//! later, so a list of the things zu is not is pinned at the bottom.

use zu::query::run;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("catalog.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// What the engine did with a statement.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    /// Parsed and performed.
    Performed,
    /// Parsed, and refused for what it meant.
    Refused,
    /// Not parsed. The one answer none of the forms below may get.
    Syntax,
}

fn ask(db: &mut Zu1File, source: &str) -> Got {
    match run(source, db, &[]) {
        Ok(_) => Got::Performed,
        Err(e) if e.gqlstatus().map(|s| s.to_string()) == Some("42001".into()) => Got::Syntax,
        Err(_) => Got::Refused,
    }
}

/// Every optional word of every one of the six, written out.
///
/// Grouped by production and in the order `docs/grammar.ebnf` writes
/// them, so a reader can hold the two side by side and see that each
/// branch is here. Creates come before drops within that ordering, so
/// that every form finds the catalog it needs and is performed rather
/// than refused.
fn every_form() -> Vec<(&'static str, &'static str)> {
    vec![
        // create_schema = "CREATE", "SCHEMA", ["IF","NOT","EXISTS"],
        //                 schema_path, [";"]
        ("create_schema", "CREATE SCHEMA /app"),
        ("create_schema", "CREATE SCHEMA IF NOT EXISTS /app"),
        ("create_schema", "CREATE SCHEMA /app/inner"),
        ("create_schema", "CREATE SCHEMA IF NOT EXISTS /"),
        ("create_schema", "CREATE SCHEMA /other;"),
        // create_graph_type = "CREATE", ["OR","REPLACE"], ["PROPERTY"],
        //                     "GRAPH", "TYPE", ["IF","NOT","EXISTS"],
        //                     name, ["AS"], graph_type_source, [";"]
        ("create_graph_type", "CREATE GRAPH TYPE shape { (:P) }"),
        (
            "create_graph_type",
            "CREATE PROPERTY GRAPH TYPE t2 { (:P) }",
        ),
        (
            "create_graph_type",
            "CREATE OR REPLACE GRAPH TYPE t3 { (:P) }",
        ),
        (
            "create_graph_type",
            "CREATE OR REPLACE PROPERTY GRAPH TYPE t4 { (:P) }",
        ),
        (
            "create_graph_type",
            "CREATE GRAPH TYPE IF NOT EXISTS t5 { (:P) }",
        ),
        ("create_graph_type", "CREATE GRAPH TYPE t6 AS { (:P) }"),
        ("create_graph_type", "CREATE GRAPH TYPE t9 { }"),
        ("create_graph_type", "CREATE GRAPH TYPE t10 { (:P) };"),
        // create_graph = "CREATE", ["OR","REPLACE"], ["PROPERTY"],
        //                "GRAPH", ["IF","NOT","EXISTS"], graph_name,
        //                graph_type_ref, ["AS","COPY","OF", graph_ref],
        //                [";"]
        ("create_graph", "CREATE GRAPH g1"),
        ("create_graph", "CREATE PROPERTY GRAPH g2"),
        ("create_graph", "CREATE OR REPLACE GRAPH g3"),
        ("create_graph", "CREATE OR REPLACE PROPERTY GRAPH g4"),
        ("create_graph", "CREATE GRAPH IF NOT EXISTS g5"),
        ("create_graph", "CREATE GRAPH /app/g6"),
        ("create_graph", "CREATE GRAPH g15;"),
        // graph_type_ref, all four branches, and AS COPY OF
        ("graph_type_ref", "CREATE GRAPH g7 ANY"),
        ("graph_type_ref", "CREATE GRAPH g8 ANY PROPERTY GRAPH"),
        ("graph_type_ref", "CREATE GRAPH g9 ANY GRAPH"),
        ("graph_type_ref", "CREATE GRAPH g10 :: shape"),
        ("graph_type_ref", "CREATE GRAPH g11 TYPED shape"),
        ("graph_type_ref", "CREATE GRAPH g12 { (:P) }"),
        ("graph_type_ref", "CREATE GRAPH g13 LIKE g1"),
        ("create_graph", "CREATE GRAPH g14 AS COPY OF g1"),
        // graph_type_source's LIKE branch, which needs a graph to
        // resemble and so comes after the graphs.
        ("create_graph_type", "CREATE GRAPH TYPE t7 LIKE g1"),
        ("create_graph_type", "CREATE GRAPH TYPE t8 AS LIKE g1"),
        // drop_graph = "DROP", ["PROPERTY"], "GRAPH", ["IF","EXISTS"],
        //              graph_name, [";"]
        ("drop_graph", "DROP GRAPH g1"),
        ("drop_graph", "DROP PROPERTY GRAPH g2"),
        ("drop_graph", "DROP GRAPH /app/g6"),
        ("drop_graph", "DROP GRAPH IF EXISTS gone"),
        ("drop_graph", "DROP GRAPH IF EXISTS gone;"),
        // drop_graph_type = "DROP", ["PROPERTY"], "GRAPH", "TYPE",
        //                   ["IF","EXISTS"], name, [";"]
        ("drop_graph_type", "DROP GRAPH TYPE t9"),
        ("drop_graph_type", "DROP PROPERTY GRAPH TYPE t2"),
        ("drop_graph_type", "DROP GRAPH TYPE IF EXISTS gone"),
        ("drop_graph_type", "DROP GRAPH TYPE IF EXISTS gone;"),
        // drop_schema = "DROP", "SCHEMA", ["IF","EXISTS"], schema_path,
        //               [";"]
        ("drop_schema", "DROP SCHEMA /app/inner"),
        ("drop_schema", "DROP SCHEMA /other"),
        ("drop_schema", "DROP SCHEMA IF EXISTS /gone"),
        ("drop_schema", "DROP SCHEMA IF EXISTS /gone;"),
    ]
}

/// Every element type an element type list may hold, which is the whole
/// of what a graph type can say and therefore the whole of what the
/// schema program has to work with.
///
/// All of these parse. All but one are performed, and the one that is
/// not is the frontier of `graph_type.rs` showing up again from a
/// different direction: a union type is spelled and has no column.
fn every_element_type() -> Vec<(&'static str, &'static str, Got)> {
    vec![
        // element_type = [("NODE"|"EDGE"|"RELATIONSHIP"), "TYPE", name],
        //                node_type_pattern, [arc, node_type_pattern]
        ("bare node", "(:P)", Got::Performed),
        ("named node", "NODE TYPE Person (:P)", Got::Performed),
        ("edge", "(:P)-[:KNOWS]->(:P)", Got::Performed),
        (
            "named edge",
            "EDGE TYPE Knows (:P)-[:KNOWS]->(:P)",
            Got::Performed,
        ),
        (
            "relationship",
            "RELATIONSHIP TYPE Knows (:P)-[:KNOWS]->(:P)",
            Got::Performed,
        ),
        // arc, all three branches
        ("arc right", "(:P)-[:R]->(:P)", Got::Performed),
        ("arc left", "(:P)<-[:R]-(:P)", Got::Performed),
        ("arc undirected tilde", "(:P)~[:R]~(:P)", Got::Performed),
        ("arc undirected dash", "(:P)-[:R]-(:P)", Got::Performed),
        // type_body = [name], [":", label_set], ["=>", ":", label_set],
        //             [property_types]
        ("empty body", "()", Got::Performed),
        ("body name only", "(Person)", Got::Performed),
        ("label set", "(:P&Q)", Got::Performed),
        ("key label set", "(:P => :Q)", Got::Performed),
        (
            "key and rest",
            "(k :P => :Q&R {v :: INT64})",
            Got::Performed,
        ),
        // property_types, both branches
        ("no properties", "(:P NO PROPERTIES)", Got::Performed),
        ("bare property map", "(:P {v :: INT64})", Got::Performed),
        (
            "PROPERTIES keyword",
            "(:P PROPERTIES {v :: INT64})",
            Got::Performed,
        ),
        ("empty property map", "(:P {})", Got::Performed),
        // property_type = name, ("::"|"TYPED"), value_type
        ("TYPED spelling", "(:P {v TYPED INT64})", Got::Performed),
        ("NOT NULL", "(:P {v :: INT64 NOT NULL})", Got::Performed),
        ("union type", "(:P {v :: INT64 | STRING})", Got::Refused),
    ]
}

/// The gate. Every form of every one of the six parses, and every one
/// of them is performed.
#[test]
fn every_form_of_the_six_catalog_statements_is_performed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let forms = every_form();
    let total = forms.len();
    for (production, source) in forms {
        assert_eq!(
            ask(&mut db, source),
            Got::Performed,
            "{production}: {source}"
        );
    }
    assert_eq!(total, 43, "the form table changed");
}

/// The same gate one level down, over what a graph type may declare.
#[test]
fn every_element_type_a_graph_type_may_declare_is_performed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let elements = every_element_type();
    let total = elements.len();
    let mut performed = 0;
    for (n, (shape, element, want)) in elements.into_iter().enumerate() {
        let source = format!("CREATE GRAPH TYPE et{n} {{ {element} }}");
        let got = ask(&mut db, &source);
        assert_eq!(got, want, "{shape}: {source}");
        if got == Got::Performed {
            performed += 1;
        }
    }
    assert_eq!(total, 21, "the element type table changed");
    assert_eq!(performed, 20, "the one that is not performed is the union");
}

/// The one element type above that is not performed, and why.
///
/// A union of two property types is a `value_type` the grammar allows
/// and the parser reads, and there is no column that holds either an
/// `INT64` or a `STRING`, so the catalog will not write it. The refusal
/// is the same shape `graph_type.rs` pins for `DECIMAL(12,2)`: no
/// condition, and the word corrupt for a file that is not damaged. S1
/// gives it a condition and S2 decides whether a union gets a column.
#[test]
fn a_union_of_property_types_is_read_and_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let source = "CREATE GRAPH TYPE u { (:P {v :: INT64 | STRING}) }";
    let err = run(source, &mut db, &[]).expect_err("no column holds either");
    assert_eq!(err.gqlstatus(), None, "S1 gives this one a condition");
    assert!(err.to_string().starts_with("corrupt catalog:"), "{err}");
}

/// A statement saying both `OR REPLACE` and `IF NOT EXISTS` says
/// nothing, since one takes a taken name over and the other leaves it
/// alone, and the grammar's comment says it is refused rather than
/// given a reading. It is refused as text, which is the right kind of
/// refusal for two words that contradict each other.
#[test]
fn or_replace_and_if_not_exists_together_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "CREATE OR REPLACE GRAPH TYPE t IF NOT EXISTS { (:P) }",
        "CREATE OR REPLACE GRAPH g IF NOT EXISTS",
    ] {
        assert_eq!(ask(&mut db, source), Got::Syntax, "{source}");
    }
}

/// There is no `ALTER` in the standard, so there is none here, and a
/// user who reaches for one gets a syntax error rather than a meaning
/// zu invented. This is the negative half of the gate above and it is
/// the load bearing half: the schema program's whole argument is that
/// `CREATE OR REPLACE GRAPH TYPE` is the `ALTER`, and that argument is
/// worth nothing if an `ALTER` quietly appears one release later.
#[test]
fn there_is_no_alter_and_no_show() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "ALTER GRAPH TYPE t ADD (:Q)",
        "ALTER GRAPH g SET TYPE t",
        "ALTER CURRENT GRAPH TYPE SET (:Q)",
        "ALTER TABLE person ADD score FLOAT64",
        "SHOW GRAPH TYPES",
        "SHOW CONSTRAINTS",
        "SHOW INDEXES",
        "CREATE CONSTRAINT c FOR (p:person) REQUIRE p.name IS NOT NULL",
        "CREATE INDEX i FOR (p:person) ON (p.name)",
        "CREATE VECTOR INDEX v FOR (c:chunk) ON (c.embedding)",
        "DESCRIBE GRAPH TYPE t",
    ] {
        assert_eq!(
            ask(&mut db, source),
            Got::Syntax,
            "{source} is not GQL and parsed"
        );
    }
}
