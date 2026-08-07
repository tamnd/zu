//! zuQL against an engine catalog: the facade that turns the storage
//! catalog into the binder's `Schema` and runs text through the
//! frontend. The binder itself is engine-agnostic; this is where zu1
//! table definitions become labels and relationship types.

use std::collections::HashMap;

use zu_common::{Result, ZuError};
use zu_query::binder::{self, BoundQuery, NodeDef, RelDef, Schema};
use zu_query::exec::{self, Graph, QueryResult, Value};
use zu_query::{optimizer, parser, plan};

use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::graph::{Direction, GraphReader};
use crate::zu1::props::{PropType, PropsReader, load_props};

/// Builds the binder schema from a zu1 catalog.
pub fn schema_of(catalog: &Catalog) -> Result<Schema> {
    let nodes = catalog
        .node_tables()
        .iter()
        .map(|n| NodeDef {
            id: n.id,
            name: n.name.clone(),
            node_count: n.node_count,
        })
        .collect();
    let rels = catalog
        .rel_tables()
        .iter()
        .map(|r| RelDef {
            id: r.id,
            name: r.name.clone(),
            from: r.from,
            to: r.to,
            edge_count: r.edge_count,
        })
        .collect();
    Schema::new(nodes, rels)
}

/// Parses and binds one query against a zu1 catalog.
pub fn bind(source: &str, catalog: &Catalog) -> Result<BoundQuery> {
    let parsed = parser::parse(source)?;
    binder::bind(&parsed, &schema_of(catalog)?)
}

/// Parses, binds, plans, and optimizes one query, returning the
/// EXPLAIN listing of the plan that would execute.
pub fn explain(source: &str, catalog: &Catalog) -> Result<String> {
    let schema = schema_of(catalog)?;
    let parsed = parser::parse(source)?;
    let query = binder::bind(&parsed, &schema)?;
    let built = plan::build(&query)?;
    let optimized = optimizer::optimize(built, &query, &schema)?;
    Ok(plan::explain(&optimized, &query, &schema))
}

/// The executor's view of one open zu1 file: readers load lazily per
/// rel table and cache their directories across calls, and props
/// readers load lazily per node table the same way.
pub struct Zu1Graph<'a> {
    db: &'a mut Zu1File,
    catalog: Catalog,
    readers: HashMap<u32, GraphReader>,
    props: HashMap<u32, Option<PropsReader>>,
}

impl<'a> Zu1Graph<'a> {
    pub fn new(db: &'a mut Zu1File, catalog: Catalog) -> Self {
        Zu1Graph {
            db,
            catalog,
            readers: HashMap::new(),
            props: HashMap::new(),
        }
    }

    fn ensure_reader(&mut self, rel: u32) -> Result<()> {
        if self.readers.contains_key(&rel) {
            return Ok(());
        }
        let name = self
            .catalog
            .rel_by_id(rel)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown rel table {rel}")))?
            .name
            .clone();
        let reader = GraphReader::load_table(self.db, &name)?;
        self.readers.insert(rel, reader);
        Ok(())
    }

    fn ensure_props(&mut self, table: u32) -> Result<()> {
        if self.props.contains_key(&table) {
            return Ok(());
        }
        let reader = load_props(self.db, table)?.map(PropsReader::new);
        self.props.insert(table, reader);
        Ok(())
    }
}

impl Graph for Zu1Graph<'_> {
    fn neighbors(&mut self, rel: u32, node: u64, reversed: bool, out: &mut Vec<u64>) -> Result<()> {
        self.ensure_reader(rel)?;
        out.clear();
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // The cached-group path, not the point path: scans and expands
        // revisit the same groups constantly, and B4 lives on the second
        // hop being a slice copy instead of a chunk decode per row. The
        // reader holds one decoded group per direction of use; a smarter
        // policy is the buffer manager's job (docs/09, M3).
        let nbrs = readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbors_dir(db, node, dir)?;
        out.extend_from_slice(nbrs);
        Ok(())
    }

    fn degree(&mut self, rel: u32, node: u64, reversed: bool) -> Result<u64> {
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // On the cached-group path a degree is the difference of two
        // decoded offsets; the neighbor values never get copied.
        let nbrs = readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbors_dir(db, node, dir)?;
        Ok(nbrs.len() as u64)
    }

    fn degree_sum(&mut self, rel: u32, nodes: &[u64], reversed: bool) -> Result<u64> {
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // One reader lookup for the whole vector; each node then costs
        // a group locate and an offset difference against the cached
        // decode, so a counting expand stays out of virtual dispatch
        // per node.
        let reader = readers.get_mut(&rel).expect("just loaded");
        let mut total = 0;
        for &node in nodes {
            total += reader.neighbors_dir(db, node, dir)?.len() as u64;
        }
        Ok(total)
    }

    fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get(&rel)
            .expect("just loaded")
            .has_edge(db, src, dst)
    }

    fn property(&mut self, table: u32, offset: u64, key: &str) -> Result<Value> {
        self.ensure_props(table)?;
        let Self { db, props, .. } = self;
        if let Some(reader) = props.get_mut(&table).expect("just loaded")
            && let Some(col) = reader.col(key)
        {
            return match reader.columns()[col].ty {
                PropType::Int => Ok(Value::Int(reader.read_int(db, col, offset)? as i64)),
                PropType::Str => {
                    let mut bytes = Vec::new();
                    reader.read_str(db, col, offset, &mut bytes)?;
                    let text = String::from_utf8(bytes).map_err(|_| ZuError::Corrupt {
                        what: "props column",
                        detail: format!("'{key}' row {offset} is not UTF-8"),
                    })?;
                    Ok(Value::Str(text))
                }
            };
        }
        // Without a stored `id` column the id is the offset, the dense
        // contract every load without REORDER keeps.
        match key {
            "id" => Ok(Value::Int(offset as i64)),
            other => Err(ZuError::InvalidArgument(format!(
                "unknown property '{other}' on table {table}"
            ))),
        }
    }

    fn lookup_key(&mut self, table: u32, key: u64) -> Result<Option<u64>> {
        // The primary-key index lives in the group directory of a rel
        // table loaded over this node table's rows, so find one and ask
        // it. A table with no keyed rel keeps the dense contract where
        // the id is the offset.
        let Some(rel) = self
            .catalog
            .rel_tables()
            .iter()
            .find(|r| r.from == table)
            .map(|r| r.id)
        else {
            return Ok(Some(key));
        };
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        if reader.directory().keys.is_none() {
            return Ok(Some(key));
        }
        reader.lookup_key(db, key)
    }
}

/// Everything a query needs before touching graph data: the optimized
/// plan, the bound query, and the parameter values in binder order.
struct Prepared {
    catalog: Catalog,
    schema: Schema,
    query: BoundQuery,
    plan: plan::LogicalPlan,
    args: Vec<Value>,
}

fn prepare(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<Prepared> {
    let catalog = Catalog::load(db)?;
    let schema = schema_of(&catalog)?;
    let parsed = parser::parse(source)?;
    let query = binder::bind(&parsed, &schema)?;
    let built = plan::build(&query)?;
    let plan = optimizer::optimize(built, &query, &schema)?;
    let mut args = Vec::with_capacity(query.params.len());
    for name in &query.params {
        match params.iter().find(|(n, _)| n == name) {
            Some((_, v)) => args.push(v.clone()),
            None => {
                return Err(ZuError::InvalidArgument(format!(
                    "missing parameter ${name}"
                )));
            }
        }
    }
    Ok(Prepared {
        catalog,
        schema,
        query,
        plan,
        args,
    })
}

/// Parses, plans, optimizes, and executes one query against an open
/// zu1 file, returning the result rows.
pub fn run(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<QueryResult> {
    let p = prepare(source, db, params)?;
    let mut graph = Zu1Graph::new(db, p.catalog);
    exec::execute(
        &p.plan,
        &p.query,
        &p.schema,
        &mut graph,
        &p.args,
        &exec::Options::default(),
    )
}

/// Executes one query with per-operator counters and returns the
/// rendered EXPLAIN ANALYZE listing: pulls, rows, average vector
/// length, and self time per operator, per stage. The grammar has no
/// EXPLAIN keyword yet, so this is the API entry point.
pub fn explain_analyze(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<String> {
    let p = prepare(source, db, params)?;
    let mut graph = Zu1Graph::new(db, p.catalog);
    let (_, profile) = exec::execute_profiled(
        &p.plan,
        &p.query,
        &p.schema,
        &mut graph,
        &p.args,
        &exec::Options::default(),
    )?;
    Ok(profile.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::file::Zu1File;
    use crate::zu1::graph;

    #[test]
    fn binds_against_a_real_zu1_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bind.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let catalog = Catalog::load(&mut db).expect("catalog");
        let q = bind(
            "MATCH (a:person {id: $src})-[:follows]->(b) \
             RETURN b.id AS friend ORDER BY friend LIMIT 10",
            &catalog,
        )
        .expect("bind");
        assert_eq!(q.params, ["src"]);
        assert_eq!(q.columns, ["friend"]);
        let a = q.variables.iter().find(|v| v.name == "a").expect("a");
        let person = catalog.node_by_name("person").expect("person").id;
        assert_eq!(a.node_tables, [person]);

        let err = bind("MATCH (a:nope) RETURN a", &catalog).expect_err("unknown label");
        assert!(err.to_string().contains("unknown label"), "got: {err}");

        let text = explain(
            "MATCH (a:person {id: $src})-[:follows]->(b) RETURN b.id AS friend",
            &catalog,
        )
        .expect("explain");
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project b.id AS friend",
                "Expand (a)-[#1:follows]->(b)",
                "Filter a.id = $src",
                "ScanNodes a: person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn runs_queries_on_a_real_zu1_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("run.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let src = 3u32;

        let mut friends: Vec<i64> = edges
            .iter()
            .filter(|(s, _)| *s == src)
            .map(|(_, d)| i64::from(*d))
            .collect();
        friends.sort_unstable();
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]->(b) \
             RETURN b.id AS friend ORDER BY friend",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("one hop");
        assert_eq!(r.columns, ["friend"]);
        let got: Vec<i64> = r
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Int(i) => *i,
                other => panic!("expected an int, got {other:?}"),
            })
            .collect();
        assert_eq!(got, friends);

        let two_hop: i64 = edges
            .iter()
            .filter(|(s, _)| *s == src)
            .map(|(_, mid)| edges.iter().filter(|(s, _)| s == mid).count() as i64)
            .sum();
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]->(b)-[:follows]->(c) \
             RETURN count(c) AS paths",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("two hop count");
        assert_eq!(r.rows, [[Value::Int(two_hop)]]);

        let undirected = edges.iter().filter(|(s, d)| *s == src || *d == src).count() as i64;
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]-(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("undirected count");
        assert_eq!(r.rows, [[Value::Int(undirected)]]);

        // Trails of one or two hops: a second edge may repeat a node
        // but never the first edge.
        let mut trails = 0i64;
        for &(s1, d1) in &edges {
            if s1 != src {
                continue;
            }
            trails += 1;
            for &(s2, d2) in &edges {
                if s2 == d1 && (s2, d2) != (s1, d1) {
                    trails += 1;
                }
            }
        }
        let r = run(
            "MATCH (a:person {id: $src})-[:follows*1..2]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("var-length count");
        assert_eq!(r.rows, [[Value::Int(trails)]]);

        // Left-outer semantics on real storage: people with no edge
        // into the high ids keep one row with a null friend, so
        // count(a) sees every row and count(b) only the matches.
        let t = 80i64;
        let mut people = 0i64;
        let mut matched = 0i64;
        let mut misses = 0i64;
        for s in 0..97u32 {
            let n = edges
                .iter()
                .filter(|(a, b)| *a == s && i64::from(*b) >= t)
                .count() as i64;
            people += n.max(1);
            matched += n;
            misses += i64::from(n == 0);
        }
        assert!(misses > 0, "threshold too low to exercise the null path");
        let r = run(
            "MATCH (a:person) OPTIONAL MATCH (a)-[:follows]->(b) WHERE b.id >= $t \
             RETURN count(a) AS people, count(b) AS friends",
            &mut db,
            &[("t", Value::Int(t))],
        )
        .expect("optional count");
        assert_eq!(r.rows, [[Value::Int(people), Value::Int(matched)]]);
    }

    #[test]
    fn explain_analyze_profiles_a_real_zu1_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("analyze.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let src = 3u32;
        let friends = edges.iter().filter(|(s, _)| *s == src).count();
        let text = explain_analyze(
            "MATCH (a:person {id: $src})-[:follows]->(b) RETURN b.id AS friend",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("explain analyze");
        assert!(
            text.contains(&format!("stage 1: Project [{friends} rows,")),
            "got:\n{text}"
        );
        assert!(
            text.contains("IndexLookup a: person [id = $src]"),
            "got:\n{text}"
        );
        assert!(text.contains("Expand (a)-[:follows]->(b)"), "got:\n{text}");
        assert!(text.contains("pulls"), "got:\n{text}");

        // The unfiltered 2-hop count runs on degrees, not lists, all
        // the way through real storage.
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            &mut db,
            &[],
        )
        .expect("explain analyze count");
        assert!(
            text.contains("ExpandCount (b)-[:follows]->(c)"),
            "got:\n{text}"
        );
    }

    #[test]
    fn keyed_ids_and_stored_props_flow_through_run() {
        use crate::zu1::props::{PropValues, store_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("props.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Row r holds original id keys[r], the shape a REORDER load
        // leaves behind: sparse keys in no particular order.
        let keys: [u64; 5] = [9000, 17, 4025, 333, 12_884_901_888];
        let edges: [(u32, u32); 4] = [(0, 1), (0, 3), (2, 4), (3, 4)];
        graph::bulk_load_keyed(&mut db, "person", "knows", 5, &edges, Some(&keys)).expect("load");
        let names: [&[u8]; 5] = [b"Ada", b"Grace", b"Edsger", b"Barbara", b"Tony"];
        let cities: [u64; 5] = [608, 707, 608, 411, 500];
        store_props(
            &mut db,
            "person",
            &[
                ("id", PropValues::Int(&keys)),
                ("firstName", PropValues::Str(&names)),
                ("cityId", PropValues::Int(&cities)),
            ],
        )
        .expect("store props");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        // The `{id: ...}` predicate resolves through the primary-key
        // index and the id property reads the stored column, so both
        // ends of the query stay in the original key space.
        let r = run(
            "MATCH (a:person {id: $src})-[:knows]->(b) \
             RETURN b.firstName AS name, b.id AS id ORDER BY id",
            &mut db,
            &[("src", Value::Int(9000))],
        )
        .expect("one hop");
        assert_eq!(
            r.rows,
            [
                [Value::Str("Grace".into()), Value::Int(17)],
                [Value::Str("Barbara".into()), Value::Int(333)],
            ]
        );

        // A key naming no row matches nothing instead of erroring or
        // treating the key as an offset.
        let r = run(
            "MATCH (a:person {id: $src}) RETURN a.firstName AS name",
            &mut db,
            &[("src", Value::Int(2))],
        )
        .expect("miss");
        assert!(r.rows.is_empty(), "got: {:?}", r.rows);

        // An integer column other than id, addressed by original key.
        let r = run(
            "MATCH (a:person {id: $src}) RETURN a.cityId AS city",
            &mut db,
            &[("src", Value::Int(4025))],
        )
        .expect("city");
        assert_eq!(r.rows, [[Value::Int(608)]]);

        // Property filters scan in key space too: both people in city
        // 608 come back under their original ids.
        let r = run(
            "MATCH (a:person) WHERE a.cityId = $c RETURN a.id AS id ORDER BY id",
            &mut db,
            &[("c", Value::Int(608))],
        )
        .expect("filter");
        assert_eq!(r.rows, [[Value::Int(4025)], [Value::Int(9000)]]);

        let err = run("MATCH (a:person) RETURN a.nope AS x", &mut db, &[]).expect_err("unknown");
        assert!(err.to_string().contains("unknown property"), "got: {err}");
    }
}
