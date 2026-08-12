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
/// both directions. Person 1 is a hub with half the table as friends,
/// so a seeded two-hop out of it is over the threshold where the
/// executor splits the frontier across workers.
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
        if i < 256 {
            // A dense cluster, and person 1 knows all of it: the seed's
            // two-hop frontier is then big enough that the executor
            // splits it across workers.
            for j in 0..32 {
                edges.push((i, (i * 32 + j) % n));
            }
            edges.push((1, i));
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
    let (query, plan, _) = query::compile(source, schema).unwrap();
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
        // The closes the intersection cannot take. An undirected close
        // reads a list per side, so the optimizer leaves it a binary
        // probe, and the pipeline answers it as a membership test
        // against the end that is already pinned.
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]-(c) \
         RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]-(c) \
         RETURN a.id AS a, b.id AS b, c.id AS c",
        "MATCH (a:person)-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
         RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]-(c) \
         WHERE c.age > 40 RETURN c.id AS c, count(*) AS n",
        // The seeded shapes: the key index answers level 0, so these
        // never scan. A key past the end of the table is no rows, not
        // an error, and the pipeline still has to hand the sink an
        // empty batch so the columns come back.
        "MATCH (p:person {id: 42}) RETURN p.name AS name",
        "MATCH (p:person) WHERE p.id = 42 RETURN count(p) AS n",
        "MATCH (p:person) WHERE id(p) = 42 RETURN p.age AS age",
        "MATCH (p:person {id: 999999}) RETURN p.name AS name",
        "MATCH (p:person {id: 999999}) RETURN count(p) AS n",
        "MATCH (p:person {id: 42})-[:knows]->(b) RETURN count(b) AS n",
        "MATCH (p:person {id: 42})-[:knows]->(b) RETURN b.id AS id ORDER BY id",
        "MATCH (p:person {id: 42})-[:knows]->(b)-[:knows]->(c) \
         WHERE c.age > 40 RETURN DISTINCT c.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person {id: 42})-[:knows]->(b) RETURN b.age AS age, count(b) AS n",
        // The hub, whose frontier is split across workers. Row order
        // has to come back the same as one worker walking the list, so
        // these run unordered as well as ordered.
        "MATCH (p:person {id: 1})-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS n",
        "MATCH (p:person {id: 1})-[:knows]->(b)-[:knows]->(c) RETURN c.id AS id",
        "MATCH (p:person {id: 1})-[:knows]->(b)-[:knows]->(c) \
         WHERE c.id <> 1 RETURN DISTINCT c.id AS id, c.name AS name ORDER BY id LIMIT 20",
        "MATCH (p:person {id: 1})-[:knows]->(b)-[:knows]->(c) RETURN c.id AS id LIMIT 25",
        "MATCH (p:person {id: 1})-[:knows]->(b)-[:knows]->(c) \
         RETURN c.age AS age, count(c) AS n ORDER BY n DESC, age",
        // An id compared against another column is not a seek, it is
        // an ordinary filter over the scan.
        "MATCH (p:person) WHERE p.id = p.age RETURN count(p) AS n",
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
        // Undirected closes, the shape that keeps the binary probe and
        // so runs the semijoin folded into the expand above it. Once
        // over a scan, once under a key seek, and once with the closed
        // node's properties read, so the fold is checked where the
        // rows it drops would have been materialized.
        "MATCH (a:person)-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) RETURN count(*) AS n",
        "MATCH (a:person {id: 1})-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
         RETURN count(c) AS n",
        "MATCH (a:person {id: 1})-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
         RETURN b.id AS b, c.id AS c ORDER BY b, c LIMIT 20",
        // Correlated filters, the predicate that names a level and one
        // below it. The lower end is pinned while the filter runs, so
        // it joins the chunk as a constant column and the compare is
        // the ordinary two-vector kernel. Once over a plain hop, once
        // on a property instead of the row id, once reaching two levels
        // down, and once as the pair of predicates that turns the
        // triangle into one triangle per orientation.
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < b.id RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < b.id RETURN a.id AS a, b.id AS b",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age < b.age RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE b.age > a.age AND b.score > 100 \
         RETURN a.id AS a, b.id AS b ORDER BY a, b LIMIT 20",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) WHERE a.id <> c.id RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         WHERE a.id < b.id AND b.id < c.id RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
         WHERE a.id < b.id AND b.id < c.id RETURN a.id AS a, b.id AS b, c.id AS c",
        "MATCH (a:person)-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
         WHERE a.id < b.id AND b.id < c.id RETURN count(*) AS n",
        "MATCH (a:person {id: 1})-[:knows]->(b)-[:knows]->(c) WHERE b.id < c.id \
         RETURN c.id AS id ORDER BY id LIMIT 20",
        // Computed projections, which run as a column of the level
        // their properties come from instead of a per-row evaluation
        // over the sink. Once bare, once sorted on the computed value,
        // once grouped by it, once aggregated over it, and once on an
        // expanded level.
        "MATCH (p:person) RETURN p.age + 1 AS b",
        "MATCH (p:person) RETURN p.age * 2 - p.score AS b, p.id AS id ORDER BY b, id LIMIT 20",
        "MATCH (p:person) RETURN p.age * 3 AS b, count(p) AS n ORDER BY b",
        "MATCH (p:person) RETURN sum(p.age + p.score) AS s, max(p.score - p.age) AS hi",
        "MATCH (p:person) RETURN DISTINCT p.age + p.age AS b ORDER BY b",
        "MATCH (a:person {id: 1})-[:knows]->(b) RETURN b.age + 1 AS b ORDER BY b",
        "MATCH (a:person)-[:knows]->(b) RETURN sum(b.score + 1) AS s",
        // count(DISTINCT ...), which groups on its own argument and
        // answers with how many groups came out. Once on a column,
        // once on a node, once on a computed value, once on a string,
        // once over an expand where the argument sits on the level the
        // expand walks off, and once on the tuple that counts each
        // unordered triple exactly once.
        "MATCH (p:person) RETURN count(DISTINCT p.age) AS n",
        "MATCH (p:person) RETURN count(DISTINCT p) AS n",
        "MATCH (p:person) RETURN count(DISTINCT p.age + 1) AS n",
        "MATCH (p:person) RETURN count(DISTINCT p.name) AS n",
        "MATCH (p:person) WHERE p.age > 500 RETURN count(DISTINCT p.age) AS n",
        "MATCH (a:person)-[:knows]->(b) RETURN count(DISTINCT a.id) AS n",
        "MATCH (a:person)-[:knows]->(b) RETURN count(DISTINCT [a.id, b.id]) AS n",
        "MATCH (a:person)-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
         WHERE a.id < b.id AND b.id < c.id RETURN count(DISTINCT [a.id, b.id, c.id]) AS n",
        // OPTIONAL MATCH: the group runs per outer row and a row it
        // matched nothing for still comes out, with the group's slots
        // bound null. The setup graph has isolated people, so every
        // one of these has real misses in it. Once bare, once with
        // the far node itself projected, once with a string off it,
        // once under a filter that leaves one outer row, once with
        // the group's own WHERE deciding what counts as a match, once
        // with an inline property doing the same, once where the
        // group matches nothing at all, once off an expanded level
        // rather than the scan, once under a key seek, and once
        // counted, where a miss still counts as one row.
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN a.id AS a, b AS node",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) \
         RETURN a.name AS a, b.name AS b, b.age AS age",
        "MATCH (a:person) WHERE a.age = 13 OPTIONAL MATCH (a)-[:knows]->(b) \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) WHERE b.age > 90 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b {age: 7}) \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) WHERE b.age > 500 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 OPTIONAL MATCH (b)-[:knows]->(c) \
         RETURN a.id AS a, b.id AS b, c.id AS c",
        "MATCH (a:person {id: 42}) OPTIONAL MATCH (a)-[:knows]->(b) \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN count(*) AS n",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) WHERE b.age > 500 \
         RETURN count(*) AS n",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN a.age AS age, count(*) AS n",
        "MATCH (a:person) WHERE a.age = 13 OPTIONAL MATCH (a)-[:knows]->(b) \
         RETURN a.id AS a, b.id AS b LIMIT 4",
        // A leading UNWIND whose values are what the scan seeks on:
        // the list is the batch of point reads and the keys drive the
        // plan. Once bare, once with the key itself returned beside
        // the row it found, once with a key that finds nothing in the
        // middle of the list, once with a key written twice, once
        // spelled id(p), once with a hop off every row, once counted
        // per key, once counted two hops out where the count fuses into
        // degrees, once with the group's WHERE above the hop, and once
        // ordered under a limit.
        "UNWIND [7, 11, 13] AS id MATCH (p:person {id: id}) RETURN p.age AS age",
        "UNWIND [7, 11, 13] AS id MATCH (p:person {id: id}) RETURN id AS i, p.name AS name",
        "UNWIND [7, 999999, 13] AS id MATCH (p:person {id: id}) RETURN id AS i, p.age AS age",
        "UNWIND [7, 7, 13] AS id MATCH (p:person {id: id}) RETURN id AS i, p.age AS age",
        "UNWIND [7, 11] AS id MATCH (p:person) WHERE id(p) = id RETURN id AS i, p.age AS age",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f) \
         RETURN id AS i, f.id AS f",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f) RETURN count(f) AS n",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f) \
         RETURN id AS i, count(f) AS n",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f)-[:knows]->(g) \
         RETURN count(g) AS n",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f) WHERE f.age > 50 \
         RETURN id AS i, f.age AS age",
        "UNWIND [1, 7, 11] AS id MATCH (p:person {id: id})-[:knows]->(f) \
         RETURN f.id AS f ORDER BY f LIMIT 5",
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
        // Division and modulo in a projection: the old engine raises
        // on a zero divisor and the kernel returns null instead, so
        // the shape stays where the error is, divisor constant or not.
        "MATCH (p:person) RETURN p.score / 3 AS b",
        "MATCH (p:person) RETURN p.age % 10 AS b, p.id AS id",
        // A correlated end inside arithmetic. The compare kernel ands
        // the broadcast column's validity into its answer, so a null
        // end compares false there, but the arith kernels do not carry
        // validity yet and would turn a null into a number.
        "MATCH (a:person)-[:knows]->(b) WHERE b.age > a.age + 1 RETURN count(*) AS n",
        // A correlated string end would have to carry its buffers into
        // the broadcast.
        "MATCH (a:person)-[:knows]->(b) WHERE a.name < b.name RETURN count(*) AS n",
        "MATCH (p:person) RETURN collect(p.age) AS ages",
        // A distinct count beside a grouping key needs a set per group
        // rather than one table over the whole input.
        "MATCH (p:person) RETURN p.age AS age, count(DISTINCT p.score) AS n",
        // Two of them would need two tables, and the sink holds one.
        "MATCH (p:person) RETURN count(DISTINCT p.age) AS a, count(DISTINCT p.score) AS b",
        "MATCH (a:person)-[r:knows]->(b) RETURN count(r) AS n",
        "MATCH (a:person)-[:knows*1..2]->(b) RETURN count(b) AS n",
        // An OPTIONAL MATCH with no required match under it has no
        // driving scan the bracket can hang off.
        "OPTIONAL MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
        // The bracket ends the pipeline, so a second one, or anything
        // required above the first, goes back whole.
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) OPTIONAL MATCH (a)-[:knows]->(c) \
         RETURN a.id AS a, b.id AS b, c.id AS c",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) MATCH (b)-[:knows]->(c) \
         RETURN count(*) AS n",
        // Reading the group's level in a way a null has an answer for
        // that the sink does not implement: count(x) is zero over a
        // miss rather than one, and sorting and deduplicating have to
        // decide where a null sorts and whether two of them are one
        // row.
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN count(b) AS n",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN sum(b.score) AS s",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN b.age AS age, count(*) AS n",
        "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) RETURN DISTINCT b.age AS age",
        "MATCH (a:person) WHERE a.age = 13 OPTIONAL MATCH (a)-[:knows]->(b) \
         RETURN a.id AS a, b.id AS b ORDER BY b",
        "UNWIND [1, 2, 3] AS x RETURN x",
        // An UNWIND the scan does not seek on. The list would have to
        // join against the table some other way, and the pipeline has
        // no shape for that, so the query goes back rather than
        // scanning once per element.
        "UNWIND [1, 2] AS x MATCH (p:person) WHERE p.age = x RETURN count(p) AS n",
        "UNWIND [1, 2] AS x MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
        // An UNWIND above the scan is not a source at all.
        "MATCH (p:person) UNWIND [1, 2] AS x RETURN count(p) AS n",
        // A list the compiler cannot read off as keys before the query
        // runs: a negative element names no row, and one element is
        // enough to send the whole list back.
        "UNWIND [-1, 7] AS id MATCH (p:person {id: id}) RETURN p.age AS age",
        "UNWIND [1, 2] AS id MATCH (p:person {id: id + 0}) RETURN p.age AS age",
        // A seek key that is not an integer constant has no row to
        // seek, and a scan to find one row costs more than the old
        // engine's seek, so the shape goes back.
        "MATCH (p:person {id: 'x'}) RETURN count(p) AS n",
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
    assert!(!r.rows.is_empty(), "a seeded expand still answers");
}
