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
    // A second string column, and the one that is not plain. Names are
    // ASCII, which every normal form leaves alone, so a column that
    // asks the normalization to do something has to be here for the
    // parity queries to be worth running: the same letter written
    // composed and decomposed, a ligature that only the compatibility
    // forms take apart, and plain rows in among them.
    let tags: Vec<Vec<u8>> = (0..N)
        .map(|i| {
            match i % 4 {
                0 => "he\u{301}llo".to_string(),
                1 => "h\u{e9}llo".to_string(),
                2 => "o\u{fb01}ce".to_string(),
                _ => format!("t{}", i % 50),
            }
            .into_bytes()
        })
        .collect();
    let tag_refs: Vec<&[u8]> = tags.iter().map(|v| v.as_slice()).collect();
    // The temporal lanes: a date and a day-time duration, which are the
    // two shapes of the four that a query is most likely to ask about.
    // The dates run either side of the epoch so that the sign of the
    // count is exercised rather than assumed away, and the durations
    // repeat so that grouping on one has groups to make.
    let born: Vec<i32> = (0..N).map(|i| (i as i32 * 97) - 100_000).collect();
    let shift: Vec<i64> = (0..N)
        .map(|i| ((i % 7) as i64) * 3_600_000_000_000)
        .collect();
    // An instant, an hour apart per row, starting at 2020-09-13. A
    // datetime is nanoseconds since the epoch and a date is days, so
    // the two lanes are different widths of the same idea and the
    // arithmetic over them is not: a length of time between two of
    // these is a subtraction, and between two of the dates above it is
    // a multiplication first, which is where the answers stop fitting.
    let seen: Vec<i64> = (0..N)
        .map(|i| 1_600_000_000_000_000_000 + (i as i64) * 3_600_000_000_000)
        .collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&age)),
            ("score", PropValues::Int(&score)),
            ("name", PropValues::Str(&name_refs)),
            ("tag", PropValues::Str(&tag_refs)),
            ("born", PropValues::Date(&born)),
            (
                "shift",
                PropValues::Duration(zu_common::DurationKind::DayTime, &shift),
            ),
            ("seen", PropValues::LocalDatetime(&seen)),
        ],
    )
    .unwrap();
    drop(db);
    let mut db = Zu1File::open(path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    (db, catalog, schema)
}

/// What the pipeline answers, or `None` where it declines the shape.
fn run_new(
    db: &mut Zu1File,
    catalog: &Catalog,
    schema: &Schema,
    source: &str,
    threads: usize,
    sip: Sip,
) -> Option<exec::QueryResult> {
    let (query, plan, _) =
        query::compile_parsed(&zu_query::parser::parse(source).unwrap(), schema).unwrap();
    assert!(query.params.is_empty(), "parity queries take no params");
    let options = Options {
        threads,
        sip,
        ..Options::default()
    };
    let mut snap = Zu1Snapshot::new(db, catalog.clone());
    zu_exec::try_execute(&plan, &query, schema, &mut snap, &[], &options).unwrap()
}

/// What the oracle answers. Sequential with the sideways filter on,
/// because the old engine's own options are not what this file is
/// about and its answer does not move with them.
fn run_old(
    db: &mut Zu1File,
    catalog: &Catalog,
    schema: &Schema,
    source: &str,
) -> exec::QueryResult {
    let (query, plan, _) =
        query::compile_parsed(&zu_query::parser::parse(source).unwrap(), schema).unwrap();
    let options = Options {
        threads: 1,
        sip: Sip::On,
        ..Options::default()
    };
    let mut graph = Zu1Graph::new(db, catalog.clone());
    exec::execute(&plan, &query, schema, &mut graph, &[], &options).unwrap()
}

fn run_both(
    db: &mut Zu1File,
    catalog: &Catalog,
    schema: &Schema,
    source: &str,
    threads: usize,
    sip: Sip,
) -> (Option<exec::QueryResult>, exec::QueryResult) {
    let new = run_new(db, catalog, schema, source, threads, sip);
    let old = run_old(db, catalog, schema, source);
    (new, old)
}

/// The shape must compile on the new executor and match the old one
/// exactly, sequential and parallel, and with the sideways filter off
/// as well as on: a filter that changes an answer is a filter that
/// dropped a row the join would have matched.
///
/// The oracle runs once and the four options run against that one
/// answer. The old engine is where almost all the time in this file
/// goes, a cross product being a nested loop over the whole table
/// there, and asking it the same question four times to get the same
/// answer four times costs about twenty minutes of every CI run. The
/// options belong to the executor under test.
fn covered(db: &mut Zu1File, catalog: &Catalog, schema: &Schema, source: &str) {
    let old = run_old(db, catalog, schema, source);
    for (threads, sip) in [(1, Sip::On), (0, Sip::On), (1, Sip::Off), (0, Sip::Off)] {
        let new = run_new(db, catalog, schema, source, threads, sip)
            .unwrap_or_else(|| panic!("exec2 should cover: {source}"));
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

/// The shapes the pipeline claims. Run in strides across the tests
/// below rather than in one.
///
/// This file is the slowest thing in the workspace by a wide margin
/// and almost all of it is the oracle: a couple of dozen of these are
/// a nested loop over three thousand rows on each side there, and
/// several of those cost a minute apiece. As one test that is one
/// thread doing half an hour of work while the rest of the machine
/// waits on it, so the harness gets eight tests to spread instead.
///
/// A stride and not a block, so the expensive family lands one query
/// per shard whatever order this list ends up in. Adding a shape here
/// needs nothing else changed.
fn covered_queries() -> &'static [&'static str] {
    &[
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
        // Clauses above a grouped aggregate: the WITH's own WHERE over
        // the groups, a RETURN that reads a property off a node the
        // query grouped by, and a RETURN that groups the groups again.
        // The property is not a key the query wrote, so the sink picks
        // it up as one of its own and the projection reads it there.
        "MATCH (a:person)-[:knows]->(b) WITH a, count(*) AS n WHERE n > 3 \
         RETURN a.id AS id, n ORDER BY id LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(*) AS n WHERE n > 3 \
         RETURN a.name AS name, a.age AS age, n ORDER BY name, age, n LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) WITH a.age AS age, count(*) AS n WHERE n > 100 \
         RETURN age, n ORDER BY age",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(*) AS n WHERE n > 3 \
         RETURN count(a) AS c",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(*) AS n WHERE n >= 4 AND n <= 8 \
         RETURN count(*) AS c",
        "MATCH (a:person)-[:knows]->(b) WITH a.age AS age, count(*) AS n \
         RETURN sum(n) AS s, count(*) AS c",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(*) AS n WHERE 3 < n \
         RETURN a.id AS id ORDER BY id LIMIT 20",
        // The grouped distinct count, whose argument joins the sink's
        // own key so the groups hold one row per distinct pair, and a
        // stage above counts the pairs each group got.
        "MATCH (a:person)-[:knows]->(b) WITH a, count(DISTINCT b.age) AS n \
         RETURN a.id AS id, n ORDER BY id LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(DISTINCT b.age) AS n WHERE n > 10 \
         RETURN a.id AS id, n ORDER BY id LIMIT 20",
        "MATCH (a:person)-[:knows]->(b) WITH a.age AS age, count(DISTINCT b.name) AS n \
         RETURN age, n ORDER BY age",
        "MATCH (a:person)-[:knows]->(b) WITH a, count(DISTINCT b.id) AS n WHERE n > 20 \
         RETURN count(*) AS c",
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
        // A cycle joined to further patterns that close as well, so a
        // close is running under a close: the pipeline the first one
        // holds its probe bitmap across is the pipeline the second one
        // builds a bitmap in. Once counted and once with the rows read,
        // and once with a third close under the second (tamnd/zu#304).
        "MATCH (a:person {id: 1})-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c), \
         (b)-[:knows]-(d), (c)-[:knows]-(d) RETURN count(*) AS n",
        "MATCH (a:person {id: 1})-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c), \
         (b)-[:knows]-(d), (c)-[:knows]-(d) RETURN b.id AS b, c.id AS c, d.id AS d \
         ORDER BY b, c, d LIMIT 20",
        "MATCH (a:person {id: 1})-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c), \
         (b)-[:knows]-(d), (c)-[:knows]-(d), (a)-[:knows]-(e), (d)-[:knows]-(e) \
         RETURN count(*) AS n",
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
        // A division whose divisor is written as a number that is not
        // nought, which is the one division shape that cannot raise, so
        // it is a computed column like any other.
        "MATCH (p:person) RETURN p.score / 3 AS b",
        "MATCH (p:person) RETURN p.age % 10 AS b, p.id AS id",
        // The numeric functions over a whole number, which are kernels
        // rather than a call per row. Once bare, once in a filter, once
        // grouped on, and once behind a guard the row engine reads in
        // the order it was written.
        "MATCH (p:person) RETURN floor(p.score / 2) AS b, p.id AS id ORDER BY b, id LIMIT 20",
        "MATCH (p:person) WHERE abs(p.age - 40) < 5 RETURN p.id AS id ORDER BY id",
        "MATCH (p:person) RETURN sign(p.age - 40) AS s, count(*) AS n ORDER BY s",
        "MATCH (p:person) WHERE p.age > 40 AND floor(p.score) > 2 RETURN count(*) AS n",
        // And the half that answers a float whatever arrived, where a
        // whole column comes back wider than it went in. A root in a
        // filter is claimed because the rows it is asked about are the
        // rows the old engine asked it about, and an angle stands in a
        // projection because it has an answer for every number there is.
        "MATCH (p:person) WHERE sqrt(p.score) > 10 RETURN count(*) AS n",
        "MATCH (p:person) RETURN sin(p.age) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN radians(p.age) * 2 AS b, p.id AS id ORDER BY id LIMIT 20",
        // And a root in a projection that nothing stands between and
        // the answer, which is the shape a column of roots is written
        // in. Every row the level carries is a row the answer is built
        // from, so the kernel measures what the old engine measures
        // and a value neither has an answer for raises on both.
        "MATCH (p:person) RETURN sqrt(p.score) AS b, p.id AS id",
        "MATCH (p:person) RETURN abs(p.age) AS b, p.id AS id",
        "MATCH (p:person) RETURN power(p.age, 2) AS b, p.id AS id",
        // The three that take two numbers. A remainder by a written
        // number that is not nought is a computed column like any
        // other, and a power and a logarithm stand in a filter, where
        // the rows they are asked about are the rows the old engine
        // asked them about.
        "MATCH (p:person) RETURN mod(p.age, 7) AS b, p.id AS id ORDER BY id LIMIT 20",
        // The two questions about a string whose answer is a number.
        // Every string has a length, so a count is a computed column
        // like a floor, and it stands in a filter and in a group key
        // as well.
        "MATCH (p:person) RETURN char_length(p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN octet_length(p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) WHERE char_length(p.name) > 3 RETURN count(*) AS n",
        "MATCH (p:person) RETURN char_length(p.name) AS w, count(*) AS n ORDER BY w",
        "MATCH (p:person) WHERE p.age > 40 OR octet_length(p.name) > 3 RETURN count(*) AS n",
        // The two folds, which are the first calls whose answer is a
        // string. One in a projection, where the vector the kernel
        // wrote has to be read back the way a stored column is, one in
        // a filter, one as a group key, and one either side of a
        // comparison so the folded bytes are what is compared.
        "MATCH (p:person) RETURN upper(p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN lower(p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) WHERE upper(p.name) = 'ANN' RETURN count(*) AS n",
        "MATCH (p:person) RETURN upper(p.name) AS w, count(*) AS n ORDER BY w",
        "MATCH (p:person) WHERE char_length(upper(p.name)) > 3 RETURN count(*) AS n",
        // A folded string against the column it was folded from, which
        // is two string vectors compared row by row with no constant
        // either side to translate against.
        "MATCH (p:person) WHERE upper(p.name) = p.name RETURN count(*) AS n",
        "MATCH (p:person) WHERE lower(p.name) = p.name RETURN count(*) AS n",
        // The trim family, which is six spellings of one loop and the
        // kernel that answers without writing a byte. One of each end,
        // one set of several characters, and the explicit form the
        // standard writes with LEADING in it.
        "MATCH (p:person) RETURN TRIM('p' FROM p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN ltrim(p.name, 'p') AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN rtrim(p.name, '0123456789') AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN btrim(p.name, 'p0123456789') AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN TRIM(LEADING 'p' FROM p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        // In a filter, as a group key, feeding a length, and against
        // the column it was trimmed from, which is the same four
        // places the folds are tried in.
        "MATCH (p:person) WHERE ltrim(p.name, 'p') = '7' RETURN count(*) AS n",
        "MATCH (p:person) RETURN rtrim(p.name, '0123456789') AS w, count(*) AS n ORDER BY w",
        "MATCH (p:person) WHERE char_length(ltrim(p.name, 'p')) > 1 RETURN count(*) AS n",
        "MATCH (p:person) WHERE TRIM('p' FROM p.name) = p.name RETURN count(*) AS n",
        // The four normal forms over the column that is not plain, and
        // the same forms over the column that is, which the kernel
        // answers without copying a byte. The default form is written
        // in one of them and left out of another, since the standard
        // supplies NFC where a statement writes nothing.
        "MATCH (p:person) RETURN NORMALIZE(p.tag) AS t, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN NORMALIZE(p.tag, NFC) AS t, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN NORMALIZE(p.tag, NFD) AS t, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN NORMALIZE(p.tag, NFKC) AS t, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN NORMALIZE(p.tag, NFKD) AS t, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN NORMALIZE(p.name, NFD) AS t, p.id AS id ORDER BY id LIMIT 20",
        // In a filter, as a group key, feeding a length, and against
        // the column it normalized, which is the four places every
        // string function is tried in.
        "MATCH (p:person) WHERE NORMALIZE(p.tag, NFD) = 'he\u{301}llo' RETURN count(*) AS n",
        "MATCH (p:person) RETURN NORMALIZE(p.tag, NFKC) AS t, count(*) AS n ORDER BY t",
        "MATCH (p:person) WHERE char_length(NORMALIZE(p.tag, NFD)) > 5 RETURN count(*) AS n",
        "MATCH (p:person) WHERE NORMALIZE(p.tag, NFC) = p.tag RETURN count(*) AS n",
        // The predicate, which is the one string function whose answer
        // is a truth value. Both spellings of the negation, a form
        // written and a form left out, the column that is plain and so
        // is in every form, and the predicate behind an AND and an OR.
        "MATCH (p:person) WHERE p.tag IS NORMALIZED NFC RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.tag IS NORMALIZED NFD RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.tag IS NOT NORMALIZED NFC RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.tag IS NORMALIZED RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.name IS NORMALIZED NFKD RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.age > 50 AND p.tag IS NORMALIZED NFC RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.age > 50 OR p.tag IS NOT NORMALIZED NFC RETURN count(*) AS n",
        // The substring function, which in GQL is these two. A count
        // the statement wrote cannot be negative, so these are the
        // string calls that stand in a projection, and one written
        // inside the other is how a query asks for the middle.
        "MATCH (p:person) RETURN LEFT(p.name, 2) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN RIGHT(p.name, 1) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN LEFT(p.tag, 3) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN RIGHT(p.tag, 2) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN LEFT(p.name, 0) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN RIGHT(p.name, 40) AS s, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) RETURN LEFT(RIGHT(p.name, 2), 1) AS s, p.id AS id ORDER BY id LIMIT 20",
        // In a filter, as a group key, feeding a length, and against
        // the column it was cut from.
        "MATCH (p:person) WHERE LEFT(p.name, 1) = 'p' RETURN count(*) AS n",
        "MATCH (p:person) RETURN RIGHT(p.name, 1) AS d, count(*) AS n ORDER BY d",
        "MATCH (p:person) WHERE char_length(LEFT(p.tag, 3)) > 2 RETURN count(*) AS n",
        "MATCH (p:person) WHERE RIGHT(p.name, 40) = p.name RETURN count(*) AS n",
        // A count that is a column, which the fixture's ages supply and
        // none of them is negative. A filter is where a count nobody
        // wrote is allowed to stand, since both engines evaluate it for
        // every row the filter sees.
        "MATCH (p:person) WHERE LEFT(p.name, p.age) = p.name RETURN count(*) AS n",
        "MATCH (p:person) WHERE char_length(RIGHT(p.tag, p.age)) > 4 RETURN count(*) AS n",
        // The element family. ID of a node is the row the level
        // already carries, so it reads where a stored column reads and
        // the places a number can stand are the places it stands: a
        // projection, a filter, an order, a group key and the argument
        // of an aggregate.
        "MATCH (p:person) RETURN ID(p) AS i ORDER BY i LIMIT 20",
        "MATCH (p:person) WHERE ID(p) < 100 RETURN count(*) AS n",
        "MATCH (p:person) WHERE ID(p) = 7 RETURN p.name AS s",
        "MATCH (p:person) WHERE ID(p) > 2900 RETURN ID(p) AS i, p.age AS a ORDER BY i",
        "MATCH (p:person) RETURN max(ID(p)) AS m",
        "MATCH (p:person) RETURN min(ID(p)) AS m",
        "MATCH (p:person) WHERE p.age > 50 RETURN count(DISTINCT ID(p)) AS n",
        "MATCH (p:person) WHERE ID(p) < 40 RETURN ID(p) AS i, count(*) AS n ORDER BY i",
        // Two levels, where the number is what tells the ends of a hop
        // apart. A ring in the fixture makes both directions non-empty.
        "MATCH (a:person)-[:knows]->(b:person) WHERE ID(a) < ID(b) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person) WHERE ID(a) = ID(b) RETURN count(*) AS n",
        // SAME and ALL_DIFFERENT, which ask the same question the
        // comparison above asks and say so in one word. Two names for
        // one level and a hop's two ends are the two shapes, and the
        // three argument spelling walks the pairs.
        "MATCH (a:person)-[:knows]->(b:person) WHERE ALL_DIFFERENT(a, b) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person) WHERE SAME(a, b) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person) WHERE SAME(a, a) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person) WHERE ALL_DIFFERENT(a, a) RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person)-[:knows]->(c:person) \
         WHERE ALL_DIFFERENT(a, b, c) AND ID(a) < 20 RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person)-[:knows]->(c:person) \
         WHERE SAME(a, c) AND ID(a) < 20 RETURN count(*) AS n",
        "MATCH (a:person)-[:knows]->(b:person) \
         WHERE ALL_DIFFERENT(a, b) AND a.age > 50 RETURN count(*) AS n",
        // ELEMENT_ID, which is the same two numbers written as the
        // string the standard asks for. It stands where a string column
        // stands, so the shapes are the fold's shapes: a projection, a
        // comparison against something the statement wrote, an order, a
        // group key, and a string function reading it.
        "MATCH (p:person) RETURN ELEMENT_ID(p) AS e, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) WHERE ELEMENT_ID(p) = 'n:0:7' RETURN p.name AS s",
        "MATCH (p:person) WHERE ID(p) < 30 RETURN ELEMENT_ID(p) AS e ORDER BY e",
        "MATCH (p:person) WHERE ID(p) < 12 RETURN ELEMENT_ID(p) AS e, count(*) AS n ORDER BY e",
        "MATCH (p:person) RETURN count(DISTINCT ELEMENT_ID(p)) AS n",
        "MATCH (p:person) WHERE char_length(ELEMENT_ID(p)) > 6 RETURN count(*) AS n",
        "MATCH (p:person) WHERE ID(p) < 5 RETURN upper(ELEMENT_ID(p)) AS e ORDER BY e",
        "MATCH (a:person)-[:knows]->(b:person) \
         WHERE ID(a) < 20 RETURN ELEMENT_ID(a) AS x, ELEMENT_ID(b) AS y ORDER BY x, y",
        // SIZE over a string, which counts what CHAR_LENGTH counts and
        // runs the same kernel. A column, an order, a group key, a
        // condition, and one over a string the statement itself made,
        // so the kernel is reached through a register as well as
        // straight off a stored column.
        "MATCH (p:person) RETURN size(p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        "MATCH (p:person) WHERE size(p.name) > 3 RETURN count(*) AS n",
        "MATCH (p:person) RETURN size(p.name) AS w, count(*) AS n ORDER BY w",
        "MATCH (p:person) WHERE size(upper(p.name)) > 3 RETURN count(*) AS n",
        "MATCH (p:person) WHERE size(ELEMENT_ID(p)) > 6 RETURN count(*) AS n",
        "MATCH (p:person) WHERE power(p.age, 2) > 1600 RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.age > 0 AND log(2, p.age) < 6 RETURN count(*) AS n",
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
        // GP03. An OPTIONAL CALL over a block that lets out an element
        // it bound is the group above with a block around it: the name
        // is the slot the group left null, so there is nothing left of
        // the block after the binder and the executor sees the same
        // plan it sees for the match.
        "MATCH (a:person) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) RETURN b AS node } \
         RETURN a.id AS a, node.id AS b",
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
        // A correlated end inside arithmetic. The arith kernels do not
        // carry validity, so a null end would come back a number, but a
        // stored column that holds a null does not resolve into a plan
        // at all and this one is read straight out of storage.
        "MATCH (a:person)-[:knows]->(b) WHERE b.age > a.age + 1 RETURN count(*) AS n",
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
        // The null test over the yielded value. Dropping the nodes the
        // walk never reached is how a caller reads a traversal answer,
        // so it runs off the validity the column already carries rather
        // than sending the query back a row at a time.
        "CALL bfs('knows', 1) YIELD node, level WITH node, level \
         WHERE level IS NOT NULL RETURN count(node) AS n",
        "CALL bfs('knows', 1) YIELD node, level WITH node, level \
         WHERE level IS NOT NULL RETURN node.id AS id, level ORDER BY id LIMIT 20",
        "CALL bfs('knows', 1) YIELD node, level WITH node, level \
         WHERE level IS NULL RETURN count(node) AS n",
        "CALL bfs('knows', 1) YIELD node, level WITH node, level \
         WHERE level IS NOT NULL AND level > 1 RETURN count(node) AS n",
        // A WITH that only carries variables forward. It computes
        // nothing, so it leaves the pipeline alone and the filter above
        // it runs where the filter below it would have.
        "MATCH (p:person)-[:knows]->(f) WITH p, f WHERE f.age > 50 RETURN count(f) AS n",
        "MATCH (p:person) WITH p WHERE p.age > 50 RETURN p.id AS id ORDER BY id LIMIT 10",
        "MATCH (p:person)-[:knows]->(f) WITH p, f WHERE f.age > 50 \
         RETURN f.id AS id ORDER BY id LIMIT 10",
        // Two patterns sharing no variable and nothing tying them to
        // each other either, each picked out by a predicate of its own.
        // That is a cross product, and the held side is settled while
        // the plan compiles: by the key index where the predicate is on
        // the id, by one zone pushed scan where it is on an integer
        // column. It is the shape a statement writes to name the two
        // ends of an edge, so it is worth compiling even though neither
        // side narrows the other.
        //
        // The pin is read off the id and off a column, on a row in the
        // first scan chunk and on one past it, since a chunk the zones
        // rule out answers with nothing and the walk has to tell that
        // from the end of the table. Then a pin that matches nothing,
        // one on a key no row has, one matching many rows, one with a
        // second predicate over the held pattern that only becomes a
        // filter once the pin has placed the level, and the product
        // counted, ordered, cut short and read off both ends. Which of
        // the two patterns drives is the optimizer's call, so the last
        // pair names both by an equality and either way round is a pin.
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.id = 11 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.score = 900 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.score = 4500 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.score = 7 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.id = 999999 \
         RETURN a.id AS a, b.id AS b",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.age = 37 \
         RETURN count(*) AS n",
        "MATCH (a:person), (b:person) WHERE a.id = 7 AND b.age = 37 AND b.score > 4000 \
         RETURN b.id AS b ORDER BY b LIMIT 5",
        "MATCH (a:person), (b:person) WHERE a.age = 11 AND b.age = 37 \
         RETURN a.id AS a, b.name AS name LIMIT 9",
        "MATCH (a:person), (b:person) WHERE a.age = 11 AND b.age = 37 \
         RETURN sum(b.score) AS s, count(*) AS n",
        // The pinned level under a walk: the product is placed last, so
        // the hop is off the driving pattern and the pinned rows pair
        // with what it walked to.
        "MATCH (a:person)-[:knows]->(c), (b:person) WHERE a.id = 7 AND b.id = 11 \
         RETURN c.id AS c, b.id AS b ORDER BY c LIMIT 6",
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
        // A mark whose block asked more than a degree read or a
        // directory word. That one runs as a group, once per outer row,
        // and writes what it found into a column of the row it was
        // asked about rather than deciding it, so the vector carries on
        // whole. Once with the block's WHERE deciding the match, once
        // with an inline property doing the same, once negated, once
        // where the block matches nothing at all, once counted, once
        // grouped, once ordered and cut, and once off a walked level
        // rather than the scan.
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b {age: 7}) } \
         RETURN a.id AS a ORDER BY a",
        "MATCH (a:person) WHERE a.id < 20 \
         OR NOT EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 500 } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)<-[:knows]-(b) WHERE b.age > 90 } \
         RETURN a.age AS age, count(*) AS n",
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         RETURN a.id AS a ORDER BY a LIMIT 4",
        "MATCH (a:person)-[:knows]->(b) WHERE a.age = 13 \
         AND (b.id < 100 OR EXISTS { MATCH (b)-[:knows]->(c) WHERE c.age > 90 }) \
         RETURN count(*) AS n",
        // The same shape where the block is a probe rather than a walk:
        // a second predicate over the held pattern says which build
        // rows count, and the table alone cannot answer that.
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.id < 20 OR EXISTS { MATCH (c:person) WHERE c.score = a.age AND c.id > 3 }) \
         RETURN count(a) AS n",
        "MATCH (a:person) WHERE a.id < 40 \
         AND (a.id < 20 OR NOT EXISTS { MATCH (c:person) WHERE c.score = a.age AND c.id > 3 }) \
         RETURN count(a) AS n",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 5 \
         AND (b.id < 100 OR EXISTS { MATCH (c:person) WHERE c.score = b.age AND c.id > 3 }) \
         RETURN count(*) AS n",
        // The temporal lanes. A date is days and a duration is a count
        // of its own unit, and both ride the word an integer rides, so
        // what these check is that the meaning survives the trip: the
        // filter compares counts, the projection hands back values, and
        // the group keys off a word.
        "MATCH (p:person) WHERE p.born < DATE '1970-01-01' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.born >= DATE '1980-06-15' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.born = DATE '1970-01-02' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.age = 13 RETURN p.born AS born",
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, p.born AS born ORDER BY id",
        "MATCH (p:person) WHERE p.shift = DURATION 'PT3H' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.id < 30 RETURN p.shift AS shift, count(p) AS n ORDER BY shift",
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 20 AND b.born > DATE '1970-01-01' \
         RETURN count(*) AS n",
        "MATCH (p:person) WHERE p.id < 50 RETURN DISTINCT p.shift AS shift ORDER BY shift",
        "MATCH (p:person) WHERE p.id < 50 RETURN p.born AS born ORDER BY born DESC LIMIT 5",
        "MATCH (p:person) RETURN count(p.born) AS n",
        "MATCH (p:person) WHERE p.born < DATE '1970-01-01' RETURN p.born AS born ORDER BY born \
         LIMIT 3",
        // Two durations of one kind, which add and subtract the way the
        // counts under them do. The answer is a duration rather than a
        // number, so what these check is that the kind rides out of the
        // kernel as well as into it: the projection has to name one and
        // the grouping has to key off one.
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, p.shift + DURATION 'PT1H' AS s \
         ORDER BY id",
        "MATCH (p:person) WHERE p.shift - DURATION 'PT1H' = DURATION 'PT2H' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.id < 30 RETURN p.shift + p.shift AS s, count(p) AS n ORDER BY s",
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, \
         p.shift + DURATION 'PT1H' - DURATION 'PT30M' AS s ORDER BY id",
        // A length of time between two instants, which is what the
        // stored column and the literal have between them once both are
        // counts. The datetime pair counts in nanoseconds and is a
        // subtraction; the date pair counts in months and is a walk
        // over the calendar, and both are the same call with the same
        // two operands.
        "MATCH (p:person) \
         WHERE DURATION_BETWEEN(LOCAL DATETIME '2020-09-13T12:26:40', p.seen) \
         > DURATION 'PT1000H' RETURN count(p) AS n",
        "MATCH (p:person) WHERE DURATION_BETWEEN(p.born, DATE '2000-01-01') YEAR TO MONTH \
         > DURATION 'P30Y' RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.id < 40 \
         AND DURATION_BETWEEN(p.seen, p.seen) = DURATION 'PT0S' RETURN count(p) AS n",
    ]
}

/// The shapes the pipeline does not claim, which have to decline so
/// the caller falls back and then have to answer where it falls back
/// to. Sharded the same way and for the same reason.
fn fallback_queries() -> &'static [&'static str] {
    &[
        // An extreme or a total over a temporal column. The
        // accumulators hold words and answer integers, so a minimum
        // over a column of days would come back as a number of days
        // rather than as the date it stands for. Only the count, which
        // answers a number whatever it counted, is claimed.
        "MATCH (p:person) RETURN min(p.born) AS d",
        "MATCH (p:person) RETURN max(p.born) AS d",
        "MATCH (p:person) RETURN min(p.shift) AS d",
        // An instant shifted by a length of time. The kernel adds two
        // words and a date plus a duration is not that: a month has to
        // land on a day the month has, a day-time duration has to keep
        // the time of day it made, and the end of the calendar has to
        // be an error rather than a wrap. All three are conditions the
        // arithmetic kernel has no op for, so the shape declines.
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, p.born + DURATION 'P1D' AS d \
         ORDER BY id",
        // A length of time in a projection rather than behind a filter.
        // A date the calendar has is not always a number of
        // nanoseconds, so a pair of ordinary dates can have no day-time
        // duration between them, and a computed column is filled before
        // the filter that would have dropped the row it happened on.
        // The old engine reaches the call only for the rows the filter
        // kept, so the shape goes back to it rather than raising where
        // it never would.
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, \
         DURATION_BETWEEN(p.born, DATE '2000-01-01') YEAR TO MONTH AS d ORDER BY id",
        // A duration scaled by a number. The kernel would multiply the
        // words happily and the result is a duration, but the operands
        // are a duration and a number, and a register pair that does
        // not agree on a lane declines rather than guessing which one
        // the answer takes.
        "MATCH (p:person) WHERE p.id < 20 RETURN p.id AS id, p.shift * 2 AS d ORDER BY id",
        // A byte string, which the vector layer has no lane for: the
        // ten physical types are the ten a kernel computes over, and a
        // sequence of octets is a blob the way a string is without
        // being the string lane, since a kernel over the string lane
        // folds and normalizes and a byte string does neither. Both the
        // value and the trim over it go back to the row engine, which
        // is where the whole of GF07 lives.
        "MATCH (p:person) WHERE p.id < 5 RETURN p.id AS id, X'00AB00' AS b ORDER BY id",
        "MATCH (p:person) WHERE p.id < 5 RETURN p.id AS id, \
         OCTET_LENGTH(TRIM(BOTH X'00' FROM X'00AB00')) AS n ORDER BY id",
        // ORDER BY over something the projection does not return has
        // no output column to read, so it stays with the old engine.
        "MATCH (p:person) RETURN p.name AS name ORDER BY p.age",
        // A sort inside a WITH orders rows the pipeline is not the
        // last reader of, so the whole chain goes back.
        "MATCH (p:person) WITH p.age AS age ORDER BY age LIMIT 5 RETURN age AS age",
        // A division by a column in a projection, with something
        // between the level and the answer. A computed column is
        // filled where the level is built, so a filter or a slice
        // after it means the kernel is asked about a row the old
        // engine never reached, and the shape stays where the question
        // of raising the condition belongs. Without one of those the
        // same projection compiles, which is the pair of these in
        // `covered_queries`.
        "MATCH (p:person) WHERE p.id > 0 RETURN p.score / p.id AS b",
        // Two conditions in the one program, which the two engines
        // reach in different orders: the program divides the whole
        // chunk before the sum that makes the divisor has seen a
        // second row, and the old engine finishes each row before it
        // starts the next. Either could be the one that raises, so the
        // plan goes back rather than raising whichever this one would.
        "MATCH (p:person) RETURN p.score / (p.id + 1) AS b",
        "MATCH (p:person) RETURN p.age % (p.id + 1) AS b, p.id AS id",
        "MATCH (p:person) RETURN mod(p.age, p.id + 1) AS b, p.id AS id",
        // The same rule over a numeric function that has a condition
        // behind it: a root has no answer below nought, so a
        // projection holding one declines behind a guard where a floor
        // would not.
        "MATCH (p:person) WHERE p.score > 0 RETURN sqrt(p.score) AS b, p.id AS id",
        "MATCH (p:person) RETURN abs(p.age) AS b, p.id AS id LIMIT 5",
        // And behind an OR, where the old engine reads the halves in
        // the order they were written and never asks the second one
        // about a row the first said yes to. An AND is not the same
        // shape, the planner having split it into a filter apiece, and
        // the second filter sees the rows the first one kept.
        "MATCH (p:person) WHERE p.age > 40 OR abs(p.age - 40) > 1 RETURN count(*) AS n",
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
        // A mark whose block is a group of more than one operator. The
        // group runs off the level the block was written on, and there
        // is one walk in it, so a second hop is a level the group would
        // have to hold across outer rows.
        "MATCH (a:person) WHERE a.id < 20 OR EXISTS { MATCH (a)-[:knows]->(b)-[:knows]->(c) } \
         RETURN count(a) AS n",
        // A group mark about a level the pipeline has walked off. The
        // cheap marks answer that one from the pin, a degree read or a
        // directory word for the whole vector, but a group walks the
        // level it stands on and this block is about another.
        "MATCH (a:person)-[:knows]->(b) WHERE a.id < 5 \
         AND (b.id < 100 OR EXISTS { MATCH (a)-[:knows]->(c) WHERE c.age > 90 }) \
         RETURN count(*) AS n",
        // Two group marks in one predicate: the second one wants a
        // level, and the number it would take is the one the first
        // group's level is holding.
        "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 90 } \
         OR EXISTS { MATCH (a)<-[:knows]-(c) WHERE c.age > 90 } RETURN count(a) AS n",
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
        // A null test reads a column's validity, and an expression has
        // none to read: what it computes from a null is a rule the old
        // engine owns.
        "CALL sssp('knows', 1) YIELD node, distance WITH node, distance \
         WHERE distance + 1 IS NOT NULL RETURN count(node) AS n",
        // A WITH that computes is a projection in earnest: the rows
        // above it are not the rows below it, so it stays in the plan
        // and the plan goes back.
        "MATCH (p:person) WITH p.age AS a WHERE a > 50 RETURN count(a) AS n",
        "MATCH (p:person) WITH p, p.age AS a WHERE a > 50 RETURN count(p) AS n",
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
        // GP03. An OPTIONAL CALL that lets out anything but an element
        // it bound. The value has to be asked for under whether the
        // block matched, or the row the block matched nothing for would
        // carry it as though it had, and a case expression is not one
        // this executor has a kernel for yet, so the statement goes
        // back whole. The last of them is the same thing under a set
        // function, where the null decides the count.
        "MATCH (a:person) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) RETURN b.id AS bid } \
         RETURN a.id AS a, bid AS b",
        "MATCH (a:person) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) RETURN 1 AS one } \
         RETURN a.id AS a, one AS b",
        "MATCH (a:person) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) WHERE b.age > 90 \
         RETURN b.id AS bid } RETURN a.id AS a, bid AS b",
        "MATCH (a:person) OPTIONAL CALL (a) { MATCH (a)-[:knows]->(b) RETURN b.id AS bid } \
         RETURN count(bid) AS n",
        // GF10 and GF12. An item that reads a set function without being
        // one is a grouping and a projection standing behind it, and the
        // stage this pipeline runs above a grouping emits columns of the
        // tuple the sink holds rather than working anything out of them.
        // A column that is a sum times ten is not one of those, so the
        // statement goes back whole. Once bare, once with a key, and
        // once where the same set function is written twice.
        "MATCH (p:person) RETURN count(p) * 10 AS n",
        "MATCH (p:person) RETURN p.age AS age, count(p) + 1 AS n ORDER BY age LIMIT 20",
        "MATCH (p:person) RETURN sum(p.age) + count(p) AS n, sum(p.age) AS s",
        // A trim set that is a column is a different set a row, and
        // the kernel prepares one set for the chunk.
        "MATCH (p:person) RETURN btrim(p.name, p.name) AS b, p.id AS id ORDER BY id LIMIT 20",
        // Whether a string is in a normal form is a truth value, and a
        // truth value is not a column this executor carries: the
        // predicate has a register of its own and a projection wants a
        // vector. In a filter the same expression is claimed, which is
        // where a query writes it.
        "MATCH (p:person) RETURN p.tag IS NORMALIZED NFC AS b, p.id AS id ORDER BY id LIMIT 20",
        // A count that is a column, in a projection. A string has no
        // negative number of characters, and a computed column is
        // filled before the filter that would have dropped the row
        // asking for one, so this is the substring function's version
        // of the division whose divisor is a column.
        "MATCH (p:person) RETURN LEFT(p.name, p.age) AS s, p.id AS id ORDER BY id LIMIT 20",
    ]
}

/// How many pieces the two lists go out in. Eight because the runners
/// this has to finish on have four cores and the shards do not come
/// out the same size, so a few more than cores is what keeps the tail
/// short.
const SHARDS: usize = 8;

fn covered_shard(shard: usize) {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("parity.zu1"));
    for source in covered_queries().iter().skip(shard).step_by(SHARDS) {
        covered(&mut db, &catalog, &schema, source);
    }
}

fn fallback_shard(shard: usize) {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, catalog, schema) = setup(&dir.path().join("fallback.zu1"));
    for source in fallback_queries().iter().skip(shard).step_by(SHARDS) {
        falls_back(&mut db, &catalog, &schema, source);
    }
}

/// One test per shard of each list. The harness schedules on names,
/// so a name per shard is what it takes to get more than one core on
/// this file, and every body is the same call with a different
/// stride.
macro_rules! shard_tests {
    ($($parity:ident, $decline:ident => $shard:expr;)*) => { $(
        #[test]
        fn $parity() {
            covered_shard($shard);
        }

        #[test]
        fn $decline() {
            fallback_shard($shard);
        }
    )* };
}

shard_tests! {
    covered_shapes_match_the_old_engine_0, unclaimed_shapes_fall_back_0 => 0;
    covered_shapes_match_the_old_engine_1, unclaimed_shapes_fall_back_1 => 1;
    covered_shapes_match_the_old_engine_2, unclaimed_shapes_fall_back_2 => 2;
    covered_shapes_match_the_old_engine_3, unclaimed_shapes_fall_back_3 => 3;
    covered_shapes_match_the_old_engine_4, unclaimed_shapes_fall_back_4 => 4;
    covered_shapes_match_the_old_engine_5, unclaimed_shapes_fall_back_5 => 5;
    covered_shapes_match_the_old_engine_6, unclaimed_shapes_fall_back_6 => 6;
    covered_shapes_match_the_old_engine_7, unclaimed_shapes_fall_back_7 => 7;
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

/// TRIM takes one character and raises `22027` when it is handed
/// more. The compiler reads the written set and knows that condition
/// is coming, so the shape goes back to the old engine and the message
/// the statement gets is the one that engine writes, said once and in
/// one place.
#[test]
fn a_trim_of_more_than_one_character_still_raises() {
    let dir = tempfile::tempdir().unwrap();
    let (mut db, _, _) = setup(&dir.path().join("trim.zu1"));
    let err = query::run(
        "MATCH (p:person) RETURN TRIM('p0' FROM p.name) AS b",
        &mut db,
        &[],
    )
    .expect_err("a trim of two characters has no answer");
    let text = err.to_string();
    assert!(text.contains("22027"), "{text}");
    assert!(text.contains("btrim, ltrim and rtrim"), "{text}");
    // The set a query wrote out is trimmed on the pipeline, and the
    // same trim of a set nobody wrote is the default space.
    let r = query::run(
        "MATCH (p:person) WHERE p.id = 7 RETURN ltrim(p.name, 'p') AS b",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Str("7".to_string())]]);
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

/// An edge property is read off a rel variable, which the pipeline
/// reads by the ordinal the walk carries down beside the row. The
/// graph here has pairs joined by more than one edge, so a walk that
/// named the pair rather than the edge would read the first of the run
/// for every copy of it and the two engines would disagree.
#[test]
fn an_edge_property_runs_on_the_pipeline() {
    use crate::zu1::props::store_rel_props;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edgeprops.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    let n = N as u32;
    let mut edges: Vec<(u32, u32)> = (0..n).map(|i| (i, (i * 7 + 3) % n)).collect();
    edges.extend((0..n).map(|i| ((i * 13 + 5) % n, i)));
    edges.sort_unstable();
    edges.dedup();
    // Parallel edges, three ways: a pair carrying two, a pair carrying
    // four, and a self loop carrying two. Sorting after keeps the
    // copies together, which is how the loader wants them.
    for _ in 0..2 {
        edges.push((7, 11));
        edges.push((13, 13));
    }
    for _ in 0..4 {
        edges.push((21, 34));
    }
    edges.sort_unstable();
    bulk_load_keyed(&mut db, "person", "knows", N, &edges, None).unwrap();
    let age: Vec<u64> = (0..N).map(|i| (i * 37) % 100).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).unwrap();
    let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2000 + i % 25).collect();
    store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).unwrap();
    drop(db);

    let mut db = Zu1File::open(&path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    for source in [
        "MATCH (a:person)-[e:knows]->(b) WHERE e.since > 2020 RETURN count(b) AS n",
        "MATCH (b:person)<-[e:knows]-(a) WHERE e.since > 2020 RETURN count(a) AS n",
        "MATCH (a:person)-[e:knows]->(b) RETURN sum(e.since) AS s",
        "MATCH (a:person)-[e:knows]->(b) WHERE a.age > 50 AND e.since < 2010 RETURN count(*) AS n",
        "MATCH (a:person)-[e:knows]->(b) WHERE b.id < 40 RETURN b.id AS id, sum(e.since) AS s \
         ORDER BY id",
        "MATCH (a:person {id: 21})-[e:knows]->(b) RETURN e.since AS since ORDER BY since",
    ] {
        covered(&mut db, &catalog, &schema, source);
    }

    let late = since.iter().filter(|&&v| v > 2020).count() as i64;
    assert!(late > 0, "the bound has to keep some edges");
    let r = query::run(
        "MATCH (a:person)-[e:knows]->(b) WHERE e.since > 2020 RETURN count(b) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(late)]]);
}

/// A float column reads on the pipeline, on a node and on an edge.
///
/// The stored words are the IEEE bits, so the read is the integer read
/// and only the type on the vector differs. What that buys is the
/// finbench decay filter, an edge amount against the amount of the
/// edge before it in the chain, which is a value pinned on a level
/// below arriving broadcast into arithmetic.
///
/// The fixture puts values on both sides of a bound that a chain can
/// straddle: the graph is dense enough that a three hop chain exists
/// out of most rows, and the amounts rise and fall along it, so the
/// decay predicate keeps some chains and drops others rather than
/// answering nothing or everything and agreeing by accident.
#[test]
fn a_float_column_runs_on_the_pipeline() {
    use crate::zu1::props::store_rel_props;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("floats.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    let n = N as u32;
    let mut edges: Vec<(u32, u32)> = (0..n).map(|i| (i, (i * 7 + 3) % n)).collect();
    edges.extend((0..n).map(|i| ((i * 13 + 5) % n, i)));
    edges.extend((0..n).map(|i| (i, (i * 31 + 17) % n)));
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", N, &edges, None).unwrap();
    let age: Vec<u64> = (0..N).map(|i| (i * 37) % 100).collect();
    // Two floats with different shapes: one climbs and holds no
    // fraction, so a bound between two of them lands where an integer
    // bound would, and one is a fraction that repeats every 17 rows.
    let score: Vec<f64> = (0..N).map(|i| (i % 100) as f64).collect();
    let ratio: Vec<f64> = (0..N).map(|i| (i % 17) as f64 / 4.0).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&age)),
            ("score", PropValues::Float(&score)),
            ("ratio", PropValues::Float(&ratio)),
        ],
    )
    .unwrap();
    let amount: Vec<f64> = (0..edges.len())
        .map(|i| (i % 23) as f64 * 4.5 + 1.0)
        .collect();
    let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2000 + i % 25).collect();
    store_rel_props(
        &mut db,
        "knows",
        &[
            ("amount", PropValues::Float(&amount)),
            ("since", PropValues::Int(&since)),
        ],
    )
    .unwrap();
    drop(db);

    let mut db = Zu1File::open(&path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    for source in [
        // A float column against a constant, both ways round, and a
        // whole column summed into an aggregate over it.
        "MATCH (p:person) WHERE p.score > 50.0 RETURN count(p) AS n",
        "MATCH (p:person) WHERE 12.5 >= p.ratio RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.ratio <= 2.25 AND p.age > 40 RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.score = 7.0 RETURN p.id AS id ORDER BY id",
        // Two float columns of one row, and one against arithmetic on
        // the other, which is where the fraction matters.
        "MATCH (p:person) WHERE p.ratio < p.score RETURN count(p) AS n",
        "MATCH (p:person) WHERE p.score <= p.ratio * 8.0 RETURN count(p) AS n",
        // An edge amount, and the shape the finbench decay filter is:
        // a hop compared against the hop before it, scaled.
        "MATCH (a:person)-[e:knows]->(b) WHERE e.amount > 90.0 RETURN count(b) AS n",
        "MATCH (a:person)-[e:knows]->(b)-[f:knows]->(c) \
         WHERE f.amount <= e.amount * 0.9 RETURN count(c) AS n",
        "MATCH (a:person)-[e:knows]->(b)-[f:knows]->(c)-[g:knows]->(d) \
         WHERE e.since > 2005 AND f.amount <= e.amount * 0.9 AND g.amount <= f.amount * 0.9 \
         RETURN count(DISTINCT d.id) AS n",
        // The same for an integer read off a level below, which the
        // compiler used to hand back whatever it was multiplied by.
        "MATCH (a:person)-[e:knows]->(b)-[f:knows]->(c) \
         WHERE f.since <= e.since + 3 RETURN count(c) AS n",
        "MATCH (a:person)-[e:knows]->(b) WHERE b.age > a.age * 2 RETURN count(b) AS n",
    ] {
        covered(&mut db, &catalog, &schema, source);
    }

    // A float is not a group key here. The row engine holds 0.0 and
    // -0.0 in one group and every NaN in one group, a stored column
    // can hold both, and a key on the pipeline is the bytes the value
    // is stored as, so the grouping stays where the two agree.
    for source in [
        "MATCH (p:person) RETURN p.score AS s, count(p) AS n",
        "MATCH (p:person) RETURN count(DISTINCT p.ratio) AS n",
        "MATCH (a:person)-[e:knows]->(b) RETURN count(DISTINCT e.amount) AS n",
    ] {
        falls_back(&mut db, &catalog, &schema, source);
    }

    // The bound has to divide the column rather than take all of it or
    // none, or the two engines would agree without reading a float.
    let over = score.iter().filter(|&&v| v > 50.0).count() as i64;
    assert!(over > 0 && over < N as i64, "the bound divides the column");
    let r = query::run(
        "MATCH (p:person) WHERE p.score > 50.0 RETURN count(p) AS n",
        &mut db,
        &[],
    )
    .unwrap();
    assert_eq!(r.rows, [[Value::Int(over)]]);
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

/// GP05 and GP17. A binding variable definition is a query run before
/// the plan starts, whose answer goes into a parameter position, so an
/// engine that does not run one reads a position the caller left null.
/// The pipeline does not run one yet, so it hands the statement back
/// rather than putting the null on the plan as the value of the name.
#[test]
fn a_binding_variable_definition_falls_back_and_still_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bindings.zu1");
    let mut db = Zu1File::create(&path).unwrap();
    let n = 64u32;
    let mut edges: Vec<(u32, u32)> = (0..n).map(|i| (i, (i * 7 + 3) % n)).collect();
    edges.sort_unstable();
    edges.dedup();
    bulk_load_keyed(&mut db, "person", "knows", u64::from(n), &edges, None).unwrap();
    let age: Vec<u64> = (0..u64::from(n)).map(|i| i % 100).collect();
    store_props(&mut db, "person", &[("age", PropValues::Int(&age))]).unwrap();
    drop(db);

    let mut db = Zu1File::open(&path).unwrap();
    let (catalog, schema) = query::load_schema(&mut db).unwrap();
    let source = "VALUE t = 3 MATCH (p:person) WHERE p.age > 60 \
                  RETURN t AS t, p.age AS age ORDER BY age";
    // Not `falls_back`: a definition makes a parameter position of its
    // own and that helper is for the statements that take none.
    {
        let (query, plan, _) =
            query::compile_parsed(&zu_query::parser::parse(source).unwrap(), &schema).unwrap();
        let args = vec![Value::Null; query.params.len()];
        let mut snap = Zu1Snapshot::new(&mut db, catalog.clone());
        let new = zu_exec::try_execute(
            &plan,
            &query,
            &schema,
            &mut snap,
            &args,
            &Options::default(),
        )
        .unwrap();
        assert!(new.is_none(), "the pipeline does not work out a definition");
    }
    let r = query::run(source, &mut db, &[]).unwrap();
    assert!(!r.rows.is_empty(), "the fixture has rows over the bound");
    assert!(
        r.rows.iter().all(|row| row[0] == Value::Int(3)),
        "the definition reaches the projection: {:?}",
        r.rows
    );
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
