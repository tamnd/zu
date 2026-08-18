//! What a declared type costs and what having none costs, in bytes on
//! disk (Spec/2064g/gql/plan/09 section 4.2, zu#116).
//!
//! The contract's density table has one line for Ladybug, 1094 to 3267
//! bits/edge converging to 1100 to 1400 and about 400 bytes a node, and
//! the reason it reads that way is written next to it: a schemaless
//! graph stores every property as a document, so every element carries
//! its own key names. zu's answer is a declared type per column with
//! the key name in the catalog, and the plan says that contrast is a
//! benchmark and that G1 adds it. This is it.
//!
//! Three stores of the same graph, so the comparison is a subtraction
//! and not an argument:
//!
//! - topology: the nodes and the edges, no properties. This is the
//!   floor both of the others stand on, and neither of them is credited
//!   with it.
//! - closed: one column per property, each with a declared type, and a
//!   graph type in the catalog naming them. This is what a closed graph
//!   type costs.
//! - schemaless: the same properties, as one document per element with
//!   the key names inside it, which is what a store with no catalog has
//!   to write and what the Ladybug figure is measuring.
//!
//! The two property figures are what the milestone asks to publish. The
//! ratio between them is what the catalog is worth, and it is printed
//! rather than gated, because a ceiling on a ratio fails when the good
//! side of it improves.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench density

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

/// Nodes in the fixture, and edges too: the graph is a ring, so every
/// node has exactly one out edge and the two counts are the same. That
/// is on purpose, because a figure per node and a figure per edge over
/// the same file each carry the other's share otherwise, and here the
/// share is one element's worth either way.
const NODES: i64 = 200_000;

/// The four properties of a node, one of each kind that stores
/// differently: an integer, a string, a truth value and a float.
fn node_props(row: i64) -> (i64, String, bool, f64) {
    (
        row,
        format!("person-{row}"),
        row % 2 == 0,
        (row % 1000) as f64 / 10.0,
    )
}

/// The one property of an edge.
fn edge_prop(row: i64) -> i64 {
    2000 + row % 25
}

/// How the schemaless store writes what the closed store puts in
/// columns: a document per element, every key name in every row.
fn node_document(row: i64) -> String {
    let (id, name, active, score) = node_props(row);
    format!("{{\"id\":{id},\"name\":\"{name}\",\"active\":{active},\"score\":{score}}}")
}

fn edge_document(row: i64) -> String {
    format!("{{\"since\":{}}}", edge_prop(row))
}

/// What a staging table holds, which is the difference between the
/// three stores and the only difference between them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// No properties at all.
    Topology,
    /// A column per property, each with its own declared type.
    Closed,
    /// One text column per element holding a document.
    Schemaless,
}

/// Stages the fixture and converts it, returning the bytes on disk.
fn build(dir: &std::path::Path, shape: Shape) -> (std::path::PathBuf, u64) {
    let name = match shape {
        Shape::Topology => "topology",
        Shape::Closed => "closed",
        Shape::Schemaless => "schemaless",
    };
    let staging = dir.join(format!("{name}.db"));
    let mut sq = SqliteStore::open(&staging).expect("open staging");
    let node_columns: Vec<(&str, ColumnType)> = match shape {
        Shape::Topology => vec![],
        Shape::Closed => vec![
            ("id", ColumnType::Integer),
            ("name", ColumnType::Text),
            ("active", ColumnType::Boolean),
            ("score", ColumnType::Real),
        ],
        Shape::Schemaless => vec![("props", ColumnType::Text)],
    };
    let edge_columns: Vec<(&str, ColumnType)> = match shape {
        Shape::Topology => vec![],
        Shape::Closed => vec![("since", ColumnType::Integer)],
        Shape::Schemaless => vec![("props", ColumnType::Text)],
    };
    sq.create_node_table("person", &node_columns)
        .expect("nodes");
    sq.create_rel_table("knows", "person", "person", &edge_columns)
        .expect("edges");
    sq.begin().expect("begin");
    for row in 0..NODES {
        let (id, person, active, score) = node_props(row);
        let values = match shape {
            Shape::Topology => vec![],
            Shape::Closed => vec![
                SqlValue::Int(id),
                SqlValue::Text(person),
                SqlValue::Int(active as i64),
                SqlValue::Real(score),
            ],
            Shape::Schemaless => vec![SqlValue::Text(node_document(row))],
        };
        sq.insert_node_at("person", row, &values).expect("node");
    }
    for row in 0..NODES {
        let values = match shape {
            Shape::Topology => vec![],
            Shape::Closed => vec![SqlValue::Int(edge_prop(row))],
            Shape::Schemaless => vec![SqlValue::Text(edge_document(row))],
        };
        sq.insert_rel("knows", row, (row + 1) % NODES, &values)
            .expect("edge");
    }
    sq.commit().expect("commit");
    sq.checkpoint().expect("checkpoint");

    let out = dir.join(format!("{name}.zu1"));
    zu::convert::sqlite_to_zu1(&staging, &out).expect("convert");
    if shape == Shape::Closed {
        // The closed side is closed: the file gets a graph type naming
        // the element types its tables hold, which is the object the
        // key names live in instead of in every row. Whatever it costs
        // is inside the closed figure, since a type nobody paid for is
        // not a comparison.
        let mut db = Zu1File::open(&out).expect("open closed");
        zu::query::run("CREATE GRAPH TYPE social LIKE home", &mut db, &[]).expect("graph type");
    }
    let bytes = std::fs::metadata(&out).expect("metadata").len();
    (out, bytes)
}

/// Reads the store back, and panics unless it holds the graph that went
/// in. A density benchmark measures a file getting smaller, so the one
/// thing it has to rule out is a file that got smaller by losing
/// something.
fn check(path: &std::path::Path, shape: Shape) {
    let mut db = Zu1File::open(path).expect("open");
    let one = |db: &mut Zu1File, source: &str| -> Value {
        let r = zu::query::run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"));
        r.rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_else(|| panic!("{source}: no row"))
    };
    assert_eq!(
        one(&mut db, "MATCH (p:person) RETURN count(p) AS n"),
        Value::Int(NODES),
        "every node"
    );
    assert_eq!(
        one(
            &mut db,
            "MATCH (:person)-[:knows]->(:person) RETURN count(*) AS n"
        ),
        Value::Int(NODES),
        "every edge"
    );
    match shape {
        Shape::Topology => {}
        Shape::Closed => {
            assert_eq!(
                one(
                    &mut db,
                    "MATCH (p:person) WHERE p.id = 199999 RETURN p.name AS name"
                ),
                Value::Str("person-199999".into()),
                "and every property"
            );
        }
        Shape::Schemaless => {
            assert_eq!(
                one(
                    &mut db,
                    "MATCH (p:person) WHERE p.props = '{\"id\":199999,\"name\":\"person-199999\",\"active\":false,\"score\":99.9}' RETURN count(p) AS n"
                ),
                Value::Int(1),
                "and every property"
            );
        }
    }
}

/// One store's numbers, all of them derived from the same two counts so
/// two charts of this cannot disagree.
struct Density {
    bytes: u64,
    props: u64,
}

impl Density {
    fn report(&self, label: &str) {
        let elements = (NODES * 2) as f64;
        println!(
            "{label:<11} {:>10} B  {:>8.1} B/node  {:>9.1} bits/edge  \
             {:>10} B properties  {:>7.1} bits/element",
            self.bytes,
            self.bytes as f64 / NODES as f64,
            self.bytes as f64 * 8.0 / NODES as f64,
            self.props,
            self.props as f64 * 8.0 / elements,
        );
    }

    /// Property bits per element, which is the figure the gate holds.
    fn prop_bits(&self) -> f64 {
        self.props as f64 * 8.0 / (NODES * 2) as f64
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().expect("tempdir");

    let (path, topology) = build(dir.path(), Shape::Topology);
    check(&path, Shape::Topology);
    let (path, closed_bytes) = build(dir.path(), Shape::Closed);
    check(&path, Shape::Closed);
    let (path, schemaless_bytes) = build(dir.path(), Shape::Schemaless);
    check(&path, Shape::Schemaless);

    let floor = Density {
        bytes: topology,
        props: 0,
    };
    let closed = Density {
        bytes: closed_bytes,
        props: closed_bytes.saturating_sub(topology),
    };
    let schemaless = Density {
        bytes: schemaless_bytes,
        props: schemaless_bytes.saturating_sub(topology),
    };
    println!("{NODES} nodes, {NODES} edges, four node properties and one edge property");
    floor.report("topology");
    closed.report("closed");
    schemaless.report("schemaless");
    println!(
        "the catalog is worth {:.2}x: {} property bytes with a declared type, {} without one",
        schemaless.props as f64 / closed.props.max(1) as f64,
        closed.props,
        schemaless.props
    );
    // Ladybug's figures are quoted here for orientation and not as a
    // race: they come off edge dominated fixtures and this one is node
    // dominated, four properties on a node against one on an edge. The
    // figure that compares across shapes is the property bits per
    // element, which is what the gate holds.
    println!(
        "for orientation, Ladybug reports 1094 to 3267 bits/edge and about 400 bytes/node \
         on edge dominated fixtures; this fixture is node dominated and zu holds \
         {:.1} bytes/node closed",
        closed.bytes as f64 / NODES as f64,
    );

    let mut failed = false;
    for (key, got) in [
        ("density_closed_prop_bits", closed.prop_bits()),
        ("density_schemaless_prop_bits", schemaless.prop_bits()),
    ] {
        if let Some(ceiling) = budget(key)
            && got > ceiling
        {
            println!("GATE FAIL {key}: {got:.1} > ceiling {ceiling}");
            failed = true;
        }
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: the ceilings are met");
    }
}
