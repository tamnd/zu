//! Differential parity for the pipeline executor: every shape it
//! claims runs on both engines over one real zu1 file and the rows
//! must match exactly, order included; shapes it does not claim must
//! come back `None` so the caller falls back. The old executor is the
//! oracle here on purpose, that is the migration contract.

use zu_query::binder::Schema;
use zu_query::exec::{self, Options, Value};

use crate::query::{self, Zu1Graph};
use crate::snapshot::Zu1Snapshot;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::graph::bulk_load_keyed;
use crate::zu1::props::{PropValues, store_props};

const N: u64 = 3000;

/// A person table spanning three scan chunks with int and string
/// props, and a knows graph dense enough that expands cross chunks in
/// both directions.
fn setup(path: &std::path::Path) -> (Zu1File, Catalog, Schema) {
    let mut db = Zu1File::create(path).unwrap();
    let n = N as u32;
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for i in 0..n {
        edges.push((i, (i * 7 + 3) % n));
        edges.push(((i * 13 + 5) % n, i));
        if i % 3 == 0 {
            edges.push((i, (i / 3) % n));
        }
    }
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", N, &edges, None).unwrap();
    let age: Vec<u64> = (0..N).map(|i| (i * 37) % 100).collect();
    let score: Vec<u64> = (0..N).map(|i| i * 3).collect();
    let names: Vec<Vec<u8>> = (0..N)
        .map(|i| format!("p{}", i % 50).into_bytes())
        .collect();
    let name_refs: Vec<&[u8]> = names.iter().map(|v| v.as_slice()).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&age)),
            ("score", PropValues::Int(&score)),
            ("name", PropValues::Str(&name_refs)),
        ],
    )
    .unwrap();
    drop(db);
    let mut db = Zu1File::open(path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    (db, catalog, schema)
}

fn run_both(
    db: &mut Zu1File,
    catalog: &Catalog,
    schema: &Schema,
    source: &str,
    threads: usize,
) -> (Option<exec::QueryResult>, exec::QueryResult) {
    let (query, plan) = query::compile(source, schema).unwrap();
    assert!(query.params.is_empty(), "parity queries take no params");
    let options = Options {
        threads,
        ..Options::default()
    };
    let new = {
        let mut snap = Zu1Snapshot::new(db, catalog.clone());
        zu_exec::try_execute(&plan, &query, schema, &mut snap, &[], &options).unwrap()
    };
    let old = {
        let mut graph = Zu1Graph::new(db, catalog.clone());
        exec::execute(&plan, &query, schema, &mut graph, &[], &options).unwrap()
    };
    (new, old)
}

/// The shape must compile on the new executor and match the old one
/// exactly, sequential and parallel.
fn covered(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) {
    for threads in [1, 0] {
        let (new, old) = run_both(db, catalog, schema, source, threads);
        let new = new.unwrap_or_else(|| panic!("exec2 should cover: {source}"));
        assert_eq!(new.columns, old.columns, "columns for {source}");
        assert_eq!(new.rows, old.rows, "rows for {source} at threads={threads}");
    }
}

/// The shape must decline so the caller falls back to the old engine.
fn falls_back(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) {
    let (new, _) = run_both(db, catalog, schema, source, 1);
    assert!(new.is_none(), "exec2 should fall back on: {source}");
}

#[test]
fn covered_shapes_match_the_old_engine() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("parity.zu1"));
    let covered_queries = [
        // Scans and counts.
        "MATCH (p:person) RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age > 50 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age > 50 AND p.score > 600 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age = 999 RETURN count(p) AS n",
        // Row sinks, including strings, nodes, and dense ids.
        "MATCH (p:person) WHERE p.score = 300 RETURN p.name AS name, p.age AS age",
        "MATCH (p:person) WHERE p.age = 7 RETURN p AS node, p.id AS id",
        "MATCH (p:person) WHERE p.name = 'p13' RETURN p.score AS s",
        // Posts absorbed into the sink.
        "MATCH (p:person) RETURN p.id AS id LIMIT 10",
        "MATCH (p:person) RETURN p.age AS age SKIP 5 LIMIT 7",
        "MATCH (p:person) RETURN DISTINCT p.age AS age",
        // ORDER BY, on its own and fused with SKIP and LIMIT.
        "MATCH (p:person) RETURN p.age AS age ORDER BY age",
        "MATCH (p:person) RETURN p.age AS age ORDER BY age DESC LIMIT 10",
        "MATCH (p:person) RETURN p.id AS id, p.name AS name ORDER BY name DESC, id",
        "MATCH (p:person) WHERE p.age > 50 RETURN p.id AS id ORDER BY id DESC SKIP 3 LIMIT 5",
        "MATCH (p:person) RETURN DISTINCT p.age AS age ORDER BY age DESC",
        "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY age, name LIMIT 4",
        // The cyclic close, which fuses into one intersection.
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         RETURN a.id AS a, b.id AS b, c.id AS c",
        // The same close with the scan at the far side: a filter on the
        // closing node moves the scan there, and the intersection comes
        // back with the two lists in the other order.
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         WHERE c.age > 40 RETURN c.id AS c, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         WHERE c.age > 40 RETURN a.id AS a, b.id AS b, c.id AS c",
        // Aggregation, keyed and bare.
        "MATCH (p:person) RETURN p.age AS age, count(p) AS n",
        "MATCH (p:person) RETURN p.name AS name, count(p) AS n",
        "MATCH (p:person) RETURN p.age AS age, count(p) AS n ORDER BY n DESC, age LIMIT 5",
        "MATCH (p:person) WHERE p.age > 90 RETURN sum(p.score) AS s, min(p.age) AS lo, \
         max(p.age) AS hi, avg(p.score) AS m, count(p) AS n",
        "MATCH (p:person) WHERE p.age > 200 RETURN sum(p.score) AS s, min(p.age) AS lo",
        // Expands in every direction, plus the degree fusions.
        "MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
        "MATCH (a:person)<-[:knows]-(b) RETURN count(b) AS n",
        "MATCH (a:person)-[:knows]-(b) RETURN count(b) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS paths",
        "MATCH (a:person)-[:knows]->(b)<-[:knows]-(c) RETURN count(c) AS n",
        // Hub shapes: expands fanning out of one scanned level, the
        // plan the optimizer picks for an unseeded two-hop. The count
        // is a per-row degree product, no expand runs.
        "MATCH (b:person)<-[:knows]-(a) MATCH (b)-[:knows]->(c) RETURN count(c) AS n",
        "MATCH (b:person)<-[:knows]-(a) MATCH (b)-[:knows]->(c) MATCH (b)-[:knows]->(d) \
         RETURN count(d) AS n",
        "MATCH (b:person)<-[:knows]-(a) MATCH (b)-[:knows]->(c) WHERE b.age > 80 \
         RETURN count(c) AS n",
        "MATCH (a:person) WHERE a.age = 13 MATCH (a)-[:knows]->(b) RETURN b.name AS name",
        "MATCH (a:person)-[:knows]->(b) WHERE b.age < 10 RETURN a.id AS a, b.id AS b",
        "MATCH (a:person)-[:knows]->(b) RETURN b.age AS age, count(b) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.score = 300 RETURN sum(b.score) AS s",
    ];
    for q in covered_queries {
        covered(&mut db, &catalog, &schema, q);
    }
}

#[test]
fn unclaimed_shapes_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("fallback.zu1"));
    let fallback_queries = [
        // ORDER BY over something the projection does not return has
        // no output column to read, so it stays with the old engine.
        "MATCH (p:person) RETURN p.name AS name ORDER BY p.age",
        // A sort inside a WITH orders rows the pipeline is not the
        // last reader of, so the whole chain goes back.
        "MATCH (p:person) WITH p.age AS age ORDER BY age LIMIT 5 RETURN age AS age",
        "MATCH (p:person) RETURN p.age + 1 AS b",
        "MATCH (p:person) RETURN collect(p.age) AS ages",
        "MATCH (p:person) RETURN count(DISTINCT p.age) AS n",
        "MATCH (a:person)-[r:knows]->(b) RETURN count(r) AS n",
        "MATCH (a:person)-[:knows*1..2]->(b) RETURN count(b) AS n",
        "OPTIONAL MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
        "UNWIND [1, 2, 3] AS x RETURN x",
        // An id point predicate is the shape the old engine seeks
        // instead of scanning; the pipeline has no seek source, so
        // claiming these would read the table to find one row.
        "MATCH (p:person {id: 42}) RETURN p.name AS name",
        "MATCH (p:person) WHERE p.id = 42 RETURN count(p) AS n",
        "MATCH (p:person {id: 42})-[:knows]->(b) RETURN count(b) AS n",
    ];
    for q in fallback_queries {
        falls_back(&mut db, &catalog, &schema, q);
    }
}

#[test]
fn public_run_uses_the_pipeline_executor_transparently() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, _, _) = setup(&dir.path().join("public.zu1"));
    // Same answers through the public entry point, which routes through
    // try_execute and falls back on its own.
    let r = query::run("MATCH (p:person) RETURN count(p) AS n", &mut db, &[]).unwrap();
    assert_eq!(r.rows, [[Value::Int(N as i64)]]);
    let r = query::run(
        "MATCH (a:person {id: $src})-[:knows]->(b) RETURN b.id AS id ORDER BY id",
        &mut db,
        &[("src", Value::Int(3))],
    )
    .unwrap();
    assert!(!r.rows.is_empty(), "the fallback path still answers");
}
