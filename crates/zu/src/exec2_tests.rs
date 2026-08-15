//! Differential parity for the pipeline executor: every shape it
//! claims runs on both engines over one real zu1 file and the rows
//! must match exactly, order included; shapes it does not claim must
//! come back `None` so the caller falls back. The old executor is the
//! oracle here on purpose, that is the migration contract.

use zu_query::binder::Schema;
use zu_query::exec::{self, Options, Sip, Value};

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
    sip: Sip,
) -> (Option<exec::QueryResult>, exec::QueryResult) {
    let (query, plan, _) =
        query::compile_parsed(&zu_query::parser::parse(source).unwrap(), schema).unwrap();
    assert!(query.params.is_empty(), "parity queries take no params");
    let options = Options {
        threads,
        sip,
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
/// exactly, sequential and parallel, and with the sideways filter off
/// as well as on: a filter that changes an answer is a filter that
/// dropped a row the join would have matched.
fn covered(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) {
    for (threads, sip) in [(1, Sip::On), (0, Sip::On), (1, Sip::Off), (0, Sip::Off)] {
        let (new, old) = run_both(db, catalog, schema, source, threads, sip);
        let new = new.unwrap_or_else(|| panic!("exec2 should cover: {source}"));
        assert_eq!(new.columns, old.columns, "columns for {source}");
        assert_eq!(
            new.rows, old.rows,
            "rows for {source} at threads={threads} sip={sip:?}"
        );
    }
}

/// The shape must decline so the caller falls back to the old engine.
fn falls_back(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) {
    let (new, _) = run_both(db, catalog, schema, source, 1, Sip::On);
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
        // An integer column against a float constant. The two operands
        // have one static type each and neither is the other's, and the
        // compiler moves the bound into the column's type rather than
        // handing the query to an engine that reads a tag per value.
        // The fixture holds every age from 0 to 99, so a bound between
        // two of them and a bound on one of them are different rows and
        // the closed side of the operator is what decides.
        "MATCH (p:person) WHERE p.age > 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age >= 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age < 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age <= 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age > 30.0 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age <= 30.0 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age = 30.0 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age = 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age <> 30.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age > -0.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age < -0.5 RETURN count(p) AS n",
        // The constant on the left is the same predicate written the
        // other way round, and a fraction against a string column is
        // neither and stays with the old engine.
        "MATCH (p:person) WHERE 30.5 < p.age RETURN count(p) AS n",
        "MATCH (p:person) WHERE 30.5 >= p.age AND p.score > 600 RETURN count(p) AS n",
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
        // Aggregates that read nothing off the expanded level, so the
        // hop is a weight rather than a walk. The counted key, the same
        // one backward where a person nobody knows has to stay out of
        // the answer, a string key so the weight goes through the probe
        // instead of the counting slots, the sum and the average that
        // scale with the weight beside the min and the max that do not,
        // a filtered source so the weights line up with the survivors,
        // a hub whose weight is a degree product, a chained hop with
        // the key pinned two levels down, and the same grouped answer
        // sorted and cut.
        "MATCH (a:person)-[:knows]->(b) RETURN a.age AS age, count(b) AS n",
        "MATCH (a:person)<-[:knows]-(b) RETURN a.age AS age, count(*) AS n",
        "MATCH (a:person)-[:knows]-(b) RETURN a.age AS age, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) RETURN a.name AS name, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) RETURN a.age AS age, sum(a.score) AS s, \
         min(a.score) AS lo, max(a.score) AS hi, avg(a.score) AS m, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) RETURN sum(a.score) AS s, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age > 50 RETURN a.age AS age, count(*) AS n",
        "MATCH (b:person)<-[:knows]-(a) MATCH (b)-[:knows]->(c) RETURN b.age AS age, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN a.age AS age, count(c) AS n",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN sum(a.score) AS s",
        "MATCH (a:person)-[:knows]->(b) RETURN a.age AS age, count(*) AS n \
         ORDER BY n DESC, age LIMIT 5",
        // Second pattern branches: a hop off a level the pipeline has
        // already walked past, which is a cross product per source row
        // rather than a chain. Both far ends read so the weight fusion
        // cannot take it, the same under a filter on the branch, the
        // branch grouped by the shared variable, a branch off the head
        // of a two hop chain so the pinned level is two below the
        // newest, a predicate spanning the two branches, and the rows
        // themselves sorted and cut so the pairing order is checked.
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN b.age AS x, c.age AS y ORDER BY x, y LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) WHERE c.age > 50 \
         RETURN b.age AS x, c.age AS y ORDER BY x, y LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN a.age AS k, count(b) AS n ORDER BY k LIMIT 10",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) MATCH (a)-[:knows]->(d) \
         WHERE d.age > 50 RETURN a.age AS k, count(*) AS n ORDER BY k LIMIT 10",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) WHERE b.age > c.age \
         RETURN count(*) AS n",
        "MATCH (a:person {id: 1})-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN b.id AS b, c.id AS c ORDER BY b, c LIMIT 20",
        // Hubs whose first pattern nothing reads: the hop comes out of
        // the middle of the pipeline and its degree weighs the rows the
        // other pattern kept, so the answers below are the ones that
        // catch a weight applied to the wrong rows or dropped. Grouped
        // by the far end, grouped by both ends, counted bare, summed off
        // the shared variable, minned so the weight has to be ignored,
        // one where the unread hop runs backwards so the direction has
        // to reach the degree read, and one where a filter on the
        // shared variable runs before any of it.
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN c.age AS k, count(*) AS n ORDER BY k LIMIT 10",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN c.age AS k, a.age AS j, count(*) AS n ORDER BY k, j LIMIT 10",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN sum(a.age) AS s, count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN min(c.age) AS lo, max(c.age) AS hi, avg(c.age) AS mid",
        "MATCH (a:person)<-[:knows]-(b) MATCH (a)-[:knows]->(c) \
         RETURN c.age AS k, count(*) AS n ORDER BY k LIMIT 10",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) WHERE a.age > 50 \
         RETURN c.age AS k, count(*) AS n ORDER BY k LIMIT 10",
        // The unread hop last rather than first, with the pipeline
        // ending two levels above the one it hangs off, so the weight is
        // read off a pin and every row of the walk that is still there
        // carries the same one.
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) MATCH (a)-[:knows]->(d) \
         RETURN c.age AS k, count(*) AS n ORDER BY k LIMIT 10",
        "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
         RETURN b.age AS k, count(*) AS n ORDER BY k LIMIT 10",
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
        // EXISTS blocks, which are the same bracket asked a yes or no
        // question: the outer row comes out once on a match for a semi
        // bracket and once on a miss for an anti one, and the block's
        // own slots are never read above it. Once bare, once with the
        // block's WHERE deciding the match, once with an inline
        // property doing the same, once negated, once where the block
        // matches nothing at all and once where it matches everything,
        // once beside an ordinary predicate, once counted, once
        // grouped, once ordered and cut, and once off an expanded
        // level rather than the scan.
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b {age: 7}) } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 500 } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 500 } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 AND EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN a.id AS id ORDER BY id",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } RETURN count(a) AS n",
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(b) } RETURN count(a) AS n",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN a.age AS age, count(*) AS n",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN a.id AS a ORDER BY a LIMIT 4",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN DISTINCT a.age AS age ORDER BY age",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 AND EXISTS { MATCH (b)-[:knows]->(c) } \
         RETURN a.id AS a, b.id AS b ORDER BY b",
        // Two blocks stacked, the bare one first: it is a filter and
        // leaves the pipeline where it found it, so the bracketed one
        // compiles on top of it.
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         AND NOT EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 } RETURN count(a) AS n",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         AND NOT EXISTS { MATCH (a)-[:knows]->(c) } RETURN count(a) AS n",
        // The same two the other way round, the bracketed one first.
        // What follows a bracket runs inside it, where the newest level
        // is the null the group bound, and a bare block is the one
        // thing that does not mind: it reads the outer row off its pin
        // and answers off the degrees. Once counted, once returning the
        // rows, and once with the bare block negated.
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 } \
         AND EXISTS { MATCH (a)-[:knows]->(b) } RETURN count(a) AS n",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 } \
         AND EXISTS { MATCH (a)-[:knows]->(b) } RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 } \
         AND NOT EXISTS { MATCH (a)-[:knows]->(b) } RETURN count(a) AS n",
        // A block written on a level the pipeline has already walked
        // off: the question is about the row that level's pin holds and
        // every row in hand came off it, so one degree read decides for
        // the whole vector. Once as a semi, once negated, and once two
        // hops out where the level it names is two pins down.
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 AND EXISTS { MATCH (a)-[:knows]->(c) } \
         RETURN a.id AS a, b.id AS b ORDER BY b",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 \
         AND NOT EXISTS { MATCH (a)-[:knows]->(c) } RETURN a.id AS a, b.id AS b ORDER BY b",
        "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) WHERE a.age = 13 \
         AND EXISTS { MATCH (a)-[:knows]->(d) } RETURN count(*) AS n",
        // A block under an OR, which is the mark: the row survives
        // either way and carries whether the block found anything, and
        // the predicate around it reads that as a column. Once with
        // the other side of the OR keeping rows the block missed, once
        // negated, once where the block answers yes for everything,
        // once where it answers no for everything, once counted and
        // once grouped.
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE a.id < 20 OR NOT EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.age > 90 OR NOT EXISTS { MATCH (a)-[:knows]->(b) } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)<-[:knows]-(b) } \
         RETURN a.age AS age, count(*) AS n",
        // Two marks under one predicate, and a mark beside an ordinary
        // conjunct, which is the block lifted out of the AND standing
        // next to the one that stayed.
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
         OR NOT EXISTS { MATCH (a)<-[:knows]-(c) } RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.age = 13 AND (a.id < 20 \
         OR EXISTS { MATCH (a)-[:knows]->(b) }) RETURN a.id AS a ORDER BY a",
        // A mark on a level the pipeline has walked off: the answer is
        // the pinned row's and it enters the level the predicate runs
        // on as a constant column, the same way a correlated end does.
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 \
         AND (b.id < 100 OR EXISTS { MATCH (a)-[:knows]->(c) }) RETURN count(*) AS n",
        // And one on the level the pipeline is standing on, which is
        // a degree read over the whole vector.
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 \
         AND (b.id < 100 OR EXISTS { MATCH (b)-[:knows]->(c) }) RETURN count(*) AS n",
        // A block over a pattern with no variable in common with the
        // outer row, tied to it by an equality: the walk a hop would do
        // is a probe into a table built off the other side, and the
        // bracket is the same one either way. Once as a semi, once
        // negated, once with a predicate of the block's own beside the
        // tie, and once counted.
        "MATCH (a:person) WHERE EXISTS { MATCH (b:person) WHERE b.score = a.age } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (b:person) WHERE b.score = a.age } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (b:person) WHERE b.score = a.age AND b.age > 90 } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE EXISTS { MATCH (b:person) WHERE b.score = a.age } \
         RETURN count(a) AS n",
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
        // A leading CALL: the kernel runs once and level 0 is the node
        // domain it answered over, with the yielded value beside the
        // row. Once counted, once summed, once as rows in id order,
        // once deduplicated, once grouped by the value, once with a hop
        // off every yielded node, once with the value filtering above
        // that hop, and once yielding a value that is null wherever the
        // kernel reached nothing.
        "CALL wcc('knows') YIELD node, component RETURN count(node) AS n",
        "CALL wcc('knows') YIELD node, component RETURN count(node) AS n, sum(component) AS total",
        "CALL wcc('knows') YIELD node, component RETURN node.id AS id, component ORDER BY id \
         LIMIT 10",
        "CALL wcc('knows') YIELD node, component RETURN DISTINCT component AS c",
        "CALL wcc('knows') YIELD node, component RETURN component AS c, count(node) AS n \
         ORDER BY n DESC, c LIMIT 5",
        "CALL louvain('knows') YIELD node, community RETURN count(node) AS n",
        "CALL wcc('knows') YIELD node, component MATCH (node)-[:knows]->(f) RETURN count(f) AS n",
        "CALL wcc('knows') YIELD node, component MATCH (node)-[:knows]->(f) WHERE component > 0 \
         RETURN count(f) AS n",
        "CALL wcc('knows') YIELD node, component MATCH (node)-[:knows]->(f) \
         RETURN component AS c, f.id AS f ORDER BY f LIMIT 8",
        "CALL sssp('knows', 1) YIELD node, distance RETURN node.id AS id, distance ORDER BY id \
         LIMIT 20",
        "CALL sssp('knows', 1) YIELD node, distance MATCH (node)-[:knows]->(f) \
         WHERE distance = 1 RETURN count(f) AS n",
        // Two patterns sharing no variable, tied by an equality on a
        // property. The held one is read into a table once and the
        // pipeline probes it a row at a time, which is the same answer
        // the old engine's nested loop gives and in the same order,
        // since the table hands its rows back in build order. Counted,
        // aggregated, deduplicated, projected off both ends, with the
        // equality written each way round, and with a filter on the
        // held pattern that has to wait for the join to place it.
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND b.score = a.age RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score \
         RETURN sum(b.age) AS s, min(b.score) AS lo, max(b.score) AS hi",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score \
         RETURN DISTINCT b.age AS age ORDER BY age LIMIT 4",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score \
         RETURN a.id AS a, b.id AS b ORDER BY a, b LIMIT 6",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score \
         RETURN b.name AS name ORDER BY name LIMIT 4",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score AND a.score > 10 \
         RETURN count(*) AS n",
        // The dense id as the probe key, and a hop off the level the
        // join built.
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND b.score = a.id RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(c), (b:person) WHERE a.id < 50 AND c.age > 10 \
         AND a.age = b.score RETURN count(*) AS n",
        "MATCH (a:person), (b:person)-[:knows]->(c) WHERE a.id < 50 AND a.age = b.score \
         RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score \
         RETURN b.age AS age, count(*) AS n ORDER BY age LIMIT 3",
        // Two held patterns, both tied to the same probe. The second
        // join goes in once the first one has placed its level.
        "MATCH (a:person), (b:person), (c:person) WHERE a.id < 5 AND a.age = b.score \
         AND a.age = c.score RETURN count(*) AS n",
        // The left join: an OPTIONAL MATCH over a pattern that shares
        // no variable, tied by an equality. An outer row the probe
        // finds nothing for keeps going with the level bound to one
        // null row, so the row counts stay the outer ones. Once with
        // hits and misses mixed, once with a group predicate that turns
        // hits into misses, once reading a string off the joined level,
        // once counted, once with the equality the other way round, and
        // once ordered by the outer side.
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.score = a.age \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) \
         WHERE b.score = a.age AND b.age > 90 RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.score = a.age \
         RETURN a.id AS a, b.name AS name",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.score = a.age \
         RETURN count(*) AS n",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE a.age = b.score \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.score = a.age \
         RETURN a.id AS a, b.id AS b ORDER BY a DESC LIMIT 8",
        // The sideways filter, which is a join publishing its keys to
        // the level its probe reads. Every one of these runs twice with
        // it and twice without, so the answer is the same either way by
        // construction; what the shapes are here for is the paths it
        // takes. The scores are three apart, so their filter is a mask
        // with gaps in it and the operator goes in; the ages cover
        // nought to ninety nine solid, so theirs is a range the scan
        // takes as a pushdown and no operator at all. Then a probe on
        // the dense id, a walk between the filter and the join that
        // runs on what the filter left, a filter over a level a walk
        // made rather than the scan, one that rejects every row, and
        // one it rejects nothing for, which is the operator switching
        // itself off mid run.
        "MATCH (a:person), (b:person) WHERE a.score = b.score RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.age = b.age RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id = b.score RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(c), (b:person) WHERE a.score = b.score \
         RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.score = b.score AND b.age > 200 \
         RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.age = b.age RETURN a.id AS a, b.id AS b LIMIT 5",
        "MATCH (a:person), (b:person) WHERE a.score = b.score \
         RETURN b.age AS age, count(*) AS n ORDER BY age LIMIT 3",
        // A filter over the level a join built rather than over the
        // scan, which is the second join in a chain publishing to the
        // first one's rows.
        "MATCH (a:person), (b:person), (c:person) WHERE a.id < 5 AND a.age = b.score \
         AND b.age = c.score RETURN count(*) AS n",
        // The filter over a level a walk made rather than the scan,
        // which is a block tied by an equality: the block is a semi
        // join and a row of the walk that no build key can match is a
        // row the block was going to drop. On the node itself the
        // filter goes into the walk, so those rows are never built:
        // once counted, once with a property of the walked level read,
        // which is the gather they stop paying for, and once under a
        // limit. On a property of that level it stays an operator
        // after the walk, since the column it reads is only there once
        // the row is built.
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 \
         AND EXISTS { MATCH (c:person) WHERE c.score = b.id } RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 \
         AND EXISTS { MATCH (c:person) WHERE c.score = b.id } RETURN sum(b.age) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 \
         AND EXISTS { MATCH (c:person) WHERE c.score = b.id } RETURN a.id AS a, b.id AS b LIMIT 7",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 \
         AND EXISTS { MATCH (c:person) WHERE c.age = b.age } RETURN count(*) AS n",
        // The other two kinds of the same bracket publish nothing. A
        // row the build side cannot match is a match for the anti and
        // an outer row with a null bound to it for the left join, so
        // dropping it there would be the filter answering the query.
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 \
         AND NOT EXISTS { MATCH (c:person) WHERE c.score = b.id } RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 OPTIONAL MATCH (c:person) \
         WHERE c.score = b.id RETURN count(*) AS n",
        // The mark a join writes: the block shares no variable with the
        // pipeline and is tied to it by an equality, so what it asks is
        // whether the build side holds the row's key and the answer is
        // a column. Once under an OR, once negated, once with the key a
        // row id rather than a property, once on a level the pipeline
        // has walked off, where the answer is the pinned row's, and
        // once on the level it is standing on.
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.id < 20 OR EXISTS { MATCH (c:person) WHERE c.score = a.age }) \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.id < 20 OR NOT EXISTS { MATCH (c:person) WHERE c.score = a.age }) \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.age > 90 OR EXISTS { MATCH (c:person) WHERE c.score = a.id }) \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 5 \
         AND (b.id < 100 OR EXISTS { MATCH (c:person) WHERE c.score = a.age }) \
         RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 5 \
         AND (b.id < 100 OR EXISTS { MATCH (c:person) WHERE c.score = b.age }) \
         RETURN count(*) AS n",
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
        // A mark whose block is more than a degree read: the answer is
        // per outer row and the row survives either way, which is a
        // group the pipeline would have to run and keep both sides of.
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b)-[:knows]->(c) } \
         RETURN count(a) AS n",
        // A mark over a pattern that shares no variable is a probe, and
        // the block is answered as a column. A second predicate inside
        // it is not: it says which build rows count, which is a group
        // per outer row, and a mark may not drop the rows a group
        // finds nothing for.
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.id < 20 OR EXISTS { MATCH (c:person) WHERE c.score = a.age AND c.id > 3 }) \
         RETURN count(a) AS n",
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
        // pagerank yields a float and a compiled column carries an
        // integer or a string, so the call stays with the old engine.
        "CALL pagerank('knows') YIELD node, rank RETURN count(node) AS n",
        // A yielded value that is null where the kernel reached
        // nothing. It reads fine as a row and as a predicate, but
        // grouping on one, ordering by one and aggregating over one
        // follow rules the packed key and the accumulators do not
        // implement.
        "CALL sssp('knows', 1) YIELD node, distance RETURN distance AS d, count(node) AS n",
        "CALL sssp('knows', 1) YIELD node, distance RETURN sum(distance) AS s",
        "CALL sssp('knows', 1) YIELD node, distance RETURN count(distance) AS n",
        "CALL sssp('knows', 1) YIELD node, distance RETURN DISTINCT distance AS d",
        "CALL sssp('knows', 1) YIELD node, distance RETURN node.id AS id, distance \
         ORDER BY distance LIMIT 5",
        // A bound above 2^53 is where an integer stops converting to a
        // float without losing a digit, so the two domains stop
        // agreeing and the rewrite that moves the bound into the
        // column's type is refused rather than made approximate.
        "MATCH (p:person) WHERE p.age < 1e300 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age > 9007199254740993.0 RETURN count(p) AS n",
        // A string column against a number is not a pair of types any
        // kernel compares. The old engine answers it from the type
        // precedence, which is where an answer of that kind belongs.
        "MATCH (p:person) WHERE p.name < 1.5 RETURN count(p) AS n",
        // Two patterns nothing ties together is a cross product, and
        // this pipeline would have to read a whole table into a join
        // to answer what the old engine's nested loop already does.
        "MATCH (a:person), (b:person) WHERE a.id < 50 RETURN count(*) AS n",
        // A tie the join table has no key for. Strings would have to
        // carry their bytes into the table and hash them there, and a
        // build key that is computed rather than stored is not a column
        // to read, so both go back.
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.name = b.name RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age = b.score + 1 \
         RETURN count(*) AS n",
        // An inequality is not a join this table answers: it hands back
        // the rows for one key, not a range of them.
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND a.age > b.score RETURN count(*) AS n",
        // A predicate over the held pattern alone, with nothing tying it
        // to the pipeline. It is still a cross product, just a smaller
        // one.
        "MATCH (a:person), (b:person) WHERE a.id < 50 AND b.age = 13 RETURN count(*) AS n",
        // An OPTIONAL MATCH over an untied pattern keeps every outer
        // row against the whole of the other side, which is a cross
        // product with a bracket around it, and a string tie is the
        // same key the table has no room for.
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.age > 90 \
         RETURN count(*) AS n",
        "MATCH (a:person) WHERE a.id < 20 OPTIONAL MATCH (b:person) WHERE b.name = a.name \
         RETURN count(*) AS n",
        // Two blocks stacked with the bracketed one written first and
        // the second one a group as well: it would run inside the
        // first, walking off the null level that one left as the
        // newest.
        "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 } \
         AND EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } RETURN count(a) AS n",
        // A block over an untied pattern is a cross product against the
        // whole of the other side, which the probe has no key for.
        "MATCH (a:person) WHERE EXISTS { MATCH (b:person) WHERE b.age > 90 } \
         RETURN count(a) AS n",
    ];
    for q in fallback_queries {
        falls_back(&mut db, &catalog, &schema, q);
    }
}

/// Chunks the scan never decoded, which is what the zone map bought
/// this query.
fn zone_skipped(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) -> u64 {
    let (query, plan, _) =
        query::compile_parsed(&zu_query::parser::parse(source).unwrap(), schema).unwrap();
    let options = Options {
        threads: 1,
        ..Options::default()
    };
    let mut snap = Zu1Snapshot::new(db, catalog.clone());
    let run =
        zu_exec::try_execute_profiled(&plan, &query, schema, &mut snap, &[], &options).unwrap();
    run.unwrap_or_else(|| panic!("the pipeline should run: {source}"))
        .1
        .zone_skipped
}

/// The score column climbs with the row, so a lower bound on it rules
/// out whole chunks before anything is decoded. A bound written as a
/// float rules out the same ones: it narrows to an integer bound in
/// the compiler, and the zone map is asked the question the narrowed
/// bound asks. Without that, spelling a bound `6000.5` instead of
/// `6000` would quietly read the whole table.
#[test]
fn a_float_bound_skips_the_chunks_its_integer_bound_skips() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("zone.zu1"));
    let want = zone_skipped(
        &mut db,
        &catalog,
        &schema,
        "MATCH (p:person) WHERE p.score > 6000 RETURN count(p) AS n",
    );
    assert!(want > 0, "the bound rules out chunks of an ordered column");
    for source in [
        "MATCH (p:person) WHERE p.score > 6000.0 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.score > 6000.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.score >= 6000.5 RETURN count(p) AS n",
        "MATCH (p:person) WHERE 6000.5 < p.score RETURN count(p) AS n",
    ] {
        assert_eq!(
            zone_skipped(&mut db, &catalog, &schema, source),
            want,
            "{source}"
        );
    }
}

/// Chunks the scan decoded and then took rows out of.
fn zone_thinned(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) -> u64 {
    let (query, plan, _) =
        query::compile_parsed(&zu_query::parser::parse(source).unwrap(), schema).unwrap();
    let options = Options {
        threads: 1,
        ..Options::default()
    };
    let mut snap = Zu1Snapshot::new(db, catalog.clone());
    let run =
        zu_exec::try_execute_profiled(&plan, &query, schema, &mut snap, &[], &options).unwrap();
    run.unwrap_or_else(|| panic!("the pipeline should run: {source}"))
        .1
        .zone_thinned
}

/// A chunk the zone map could not rule out is handed on whole unless
/// the bound takes most of it away. Everything above a thinned chunk
/// reads its rows through a selection instead of straight down the
/// vector, and the predicate is still in the program either way, so
/// thinning a chunk that keeps nearly all of its rows costs a pass and
/// saves nothing.
#[test]
fn a_chunk_is_thinned_only_when_the_bound_takes_most_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("thin.zu1"));
    // Ages cycle through a hundred values in every chunk, so no chunk
    // is ruled out and the bound decides on its own.
    assert_eq!(
        zone_thinned(
            &mut db,
            &catalog,
            &schema,
            "MATCH (p:person) WHERE p.age > 10 RETURN count(p) AS n",
        ),
        0,
        "a bound nine rows in ten pass is not worth a selection"
    );
    assert!(
        zone_thinned(
            &mut db,
            &catalog,
            &schema,
            "MATCH (p:person) WHERE p.age > 90 RETURN count(p) AS n",
        ) > 0,
        "a bound one row in ten passes is"
    );
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
    // A CALL routes the same way, and a source key with no row behind
    // it falls back rather than being answered here: what a walk with
    // no start does is the old engine's contract, and it still counts
    // every node the kernel ran over.
    let r = query::run(
        "CALL wcc('knows') YIELD node, component RETURN count(node) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(N as i64)]]);
    let r = query::run(
        "CALL sssp('knows', 999999) YIELD node, distance RETURN count(node) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(N as i64)]]);
}

/// An edge property is read off a rel variable, and the pipeline
/// executor binds slots for node variables only, so the shape falls
/// back and the row engine answers it. The point of the test is that
/// the fallback happens rather than the new engine answering null, and
/// that the answer the public entry point gives is the right one.
#[test]
fn an_edge_property_falls_back_and_still_answers() {
    use crate::zu1::props::store_rel_props;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edgeprops.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    let n = N as u32;
    let mut edges: Vec<(u32, u32)> = (0..n).map(|i| (i, (i * 7 + 3) % n)).collect();
    edges.extend((0..n).map(|i| ((i * 13 + 5) % n, i)));
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", N, &edges, None).unwrap();
    let age: Vec<u64> = (0..N).map(|i| (i * 37) % 100).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).unwrap();
    let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2000 + i % 25).collect();
    store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).unwrap();
    drop(db);

    let mut db = Zu1File::open(&path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    falls_back(
        &mut db,
        &catalog,
        &schema,
        "MATCH (a:person)-[e:knows]->(b) WHERE e.since > 2020 RETURN count(b) AS n",
    );
    falls_back(
        &mut db,
        &catalog,
        &schema,
        "MATCH (a:person)-[e:knows]->(b) RETURN e.since AS since ORDER BY since LIMIT 3",
    );

    let late = since.iter().filter(|&&v| v > 2020).count() as i64;
    assert!(late > 0, "the bound has to keep some edges");
    let r = query::run(
        "MATCH (a:person)-[e:knows]->(b) WHERE e.since > 2020 RETURN count(b) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(late)]]);
    // The same count reached backward: an edge is the edge its
    // endpoints name, whichever side the walk started from.
    let r = query::run(
        "MATCH (b:person)<-[e:knows]-(a) WHERE e.since > 2020 RETURN count(a) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(late)]]);
}

/// A label bit is not a column, so the pipeline compiler has nothing
/// to read it with and hands the query back. The row engine answers
/// it, and a pattern whose labels the narrowing settled stays on the
/// pipeline because it plants no predicate at all.
#[test]
fn a_secondary_label_falls_back_and_still_answers() {
    use crate::zu1::props::store_labels;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("labels.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    let n = N as u32;
    let mut edges: Vec<(u32, u32)> = (0..n).map(|i| (i, (i * 7 + 3) % n)).collect();
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", N, &edges, None).unwrap();
    let age: Vec<u64> = (0..N).map(|i| (i * 37) % 100).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).unwrap();
    let rows: Vec<Vec<&str>> = (0..N)
        .map(|i| match i % 3 {
            0 => vec!["Employee"],
            1 => vec!["Employee", "Manager"],
            _ => vec![],
        })
        .collect();
    store_labels(&mut db, "person", &rows).unwrap();
    drop(db);

    let mut db = Zu1File::open(&path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    falls_back(
        &mut db,
        &catalog,
        &schema,
        "MATCH (p:Employee) RETURN count(p) AS n",
    );
    let covered = "MATCH (p:person) WHERE p.age > 50 RETURN count(p) AS n";
    let (new, old) = run_both(&mut db, &catalog, &schema, covered, 1, Sip::On);
    assert!(new.is_some(), "a table label plants no predicate");
    assert_eq!(new.unwrap().rows, old.rows);

    let employees = rows.iter().filter(|r| r.contains(&"Employee")).count() as i64;
    let managers = rows.iter().filter(|r| r.contains(&"Manager")).count() as i64;
    assert!(managers > 0 && employees > managers);
    let r = query::run("MATCH (p:Employee) RETURN count(p) AS n", &mut db, &[]).unwrap();
    assert_eq!(r.rows, [[Value::Int(employees)]]);
    let r = query::run(
        "MATCH (p:Employee:Manager) RETURN count(p) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(managers)]]);
}

#[test]
#[ignore = "scratch"]
fn dump_plans() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("dump.zu1"));
    for q in [
        "MATCH (a:person), (b:person) WHERE b.id < 100 AND a.score = b.score RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.score = b.age RETURN count(*) AS n",
        "MATCH (b:person), (a:person) WHERE b.id < 100 AND a.score = b.score RETURN count(*) AS n",
    ] {
        eprintln!("--- {q}");
        let _ = run_both(&mut db, &catalog, &schema, q, 1, Sip::On);
    }
}
