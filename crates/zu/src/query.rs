//! zuQL against an engine catalog: the facade that turns the storage
//! catalog into the binder's `Schema` and runs text through the
//! frontend. The binder itself is engine-agnostic; this is where zu1
//! table definitions become labels and relationship types.

use std::collections::HashMap;

use zu_common::{Result, ZuError};
use zu_query::binder::{self, BoundQuery, NodeDef, RelDef, Schema};
use zu_query::exec::{self, Graph, QueryResult, Value};
use zu_query::{optimizer, parser, plan};

use crate::zu1::algo;
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

/// The file handle behind a [`Zu1Graph`]: the caller's borrowed handle
/// on the main path, an owned reopen on the fork a morsel worker
/// drives. Both deref to the same [`Zu1File`] surface.
enum Db<'a> {
    Borrowed(&'a mut Zu1File),
    Owned(Box<Zu1File>),
}

impl std::ops::Deref for Db<'_> {
    type Target = Zu1File;
    fn deref(&self) -> &Zu1File {
        match self {
            Db::Borrowed(db) => db,
            Db::Owned(db) => db,
        }
    }
}

impl std::ops::DerefMut for Db<'_> {
    fn deref_mut(&mut self) -> &mut Zu1File {
        match self {
            Db::Borrowed(db) => db,
            Db::Owned(db) => db,
        }
    }
}

/// The executor's view of one open zu1 file: readers load lazily per
/// rel table and cache their directories across calls, and props
/// readers load lazily per node table the same way.
pub struct Zu1Graph<'a> {
    db: Db<'a>,
    catalog: Catalog,
    readers: HashMap<u32, GraphReader>,
    props: HashMap<u32, Option<PropsReader>>,
}

impl<'a> Zu1Graph<'a> {
    pub fn new(db: &'a mut Zu1File, catalog: Catalog) -> Self {
        Zu1Graph {
            db: Db::Borrowed(db),
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
        let reader = GraphReader::load_table(&mut self.db, &name)?;
        self.readers.insert(rel, reader);
        Ok(())
    }

    fn ensure_props(&mut self, table: u32) -> Result<()> {
        if self.props.contains_key(&table) {
            return Ok(());
        }
        let reader = load_props(&mut self.db, table)?.map(PropsReader::new);
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
        // reader keeps decoded groups resident within its byte budget;
        // a smarter shared policy is the buffer manager's job (docs/09,
        // M3).
        let nbrs = readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbors_dir(db, node, dir)?;
        out.extend_from_slice(nbrs);
        Ok(())
    }

    fn with_neighbors(
        &mut self,
        rel: u32,
        node: u64,
        reversed: bool,
        f: &mut dyn FnMut(&[u64]),
    ) -> Result<()> {
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // Lends the resident slice instead of copying it out. The
        // intersection's seed side is the caller that matters: one
        // fresh list per configuration, most of it skipped by the
        // gallop, so the copy was the query's dominant memory traffic
        // on hub-heavy graphs.
        let nbrs = readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbors_dir(db, node, dir)?;
        f(nbrs);
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

    fn fork(&self) -> Option<Box<dyn Graph + Send>> {
        // Data blocks hit the file as they are staged and only the
        // header flip waits for the checkpoint, so a reopen carrying
        // this handle's in-memory header reads exactly what this
        // handle reads. The fork starts with cold reader caches and
        // warms its own; a worker sweeps its own morsels' groups, so
        // sharing decoded state would only add contention.
        let db = self.db.reopen().ok()?;
        Some(Box::new(Zu1Graph {
            db: Db::Owned(Box::new(db)),
            catalog: self.catalog.clone(),
            readers: HashMap::new(),
            props: HashMap::new(),
        }))
    }

    fn table_function(&mut self, name: &str, rel: u32, args: &[Value]) -> Result<Vec<Vec<Value>>> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        match name {
            "pagerank" => Ok(algo::pagerank(db, reader, algo::PAGERANK_ITERATIONS)?
                .into_iter()
                .map(|rank| vec![Value::Float(rank)])
                .collect()),
            "wcc" => Ok(algo::wcc(db, reader)?
                .into_iter()
                .map(|label| vec![Value::Int(label as i64)])
                .collect()),
            "sssp" => {
                let Some(Value::Int(source)) = args.first() else {
                    return Err(ZuError::InvalidArgument(
                        "sssp needs a source node offset".into(),
                    ));
                };
                Ok(algo::sssp(db, reader, *source as u64)?
                    .into_iter()
                    .map(|dist| {
                        vec![if dist == u64::MAX {
                            Value::Null
                        } else {
                            Value::Int(dist as i64)
                        }]
                    })
                    .collect())
            }
            "louvain" => Ok(algo::louvain(db, reader)?
                .into_iter()
                .map(|label| vec![Value::Int(label as i64)])
                .collect()),
            other => Err(ZuError::InvalidArgument(format!(
                "zu1 has no table function '{other}'"
            ))),
        }
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
    let mut schema = schema_of(&catalog)?;
    // The stats chain feeds the optimizer's degree histograms; a file
    // written before stats existed simply attaches nothing and every
    // estimate falls back to the count ratios.
    let stats = crate::zu1::stats::Stats::load(db)?;
    schema.set_color_summaries(
        stats
            .rels
            .iter()
            .filter_map(|(id, r)| {
                let c = r.colors.as_ref()?;
                Some((
                    *id,
                    binder::ColorSummary {
                        counts: c.counts.clone(),
                        triples: c.triples.clone(),
                    },
                ))
            })
            .collect(),
    );
    schema.set_degree_hists(
        stats
            .rels
            .into_iter()
            .map(|(id, r)| (id, [r.out_hist, r.in_hist]))
            .collect(),
    );
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
/// zu1 file, returning the result rows. Scan-driven stages run on the
/// morsel scheduler with `min(cores, 8)` workers; `ZU_THREADS` in the
/// environment overrides the count, `ZU_THREADS=1` forces sequential
/// execution.
pub fn run(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<QueryResult> {
    let p = prepare(source, db, params)?;
    let mut graph = Zu1Graph::new(db, p.catalog);
    let options = env_options();
    exec::execute(&p.plan, &p.query, &p.schema, &mut graph, &p.args, &options)
}

/// The execution options both entry points honor, so a profile always
/// describes the plan `run` would execute under the same environment.
fn env_options() -> exec::Options {
    let mut options = exec::Options::default();
    if let Some(threads) = std::env::var("ZU_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
    {
        options.threads = threads;
    }
    // The optimizer marks cyclic closes on its own; ZU_WCOJ stays as
    // a manual override, 1 forcing the fusion wherever it fits and 0
    // pinning the binary join for baseline comparisons.
    options.wcoj = match std::env::var("ZU_WCOJ").as_deref() {
        Ok("1") => exec::Wcoj::Force,
        Ok("0") => exec::Wcoj::Off,
        _ => exec::Wcoj::Auto,
    };
    options
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
        &env_options(),
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

        // ANY SHORTEST against a BFS levels oracle: one row per
        // reached node, hops equal to its level.
        let mut level = vec![u32::MAX; 97];
        level[src as usize] = 0;
        let mut frontier = vec![src];
        let mut depth = 0u32;
        while !frontier.is_empty() {
            depth += 1;
            let mut next = Vec::new();
            for &(s, t) in &edges {
                if frontier.contains(&s) && level[t as usize] == u32::MAX {
                    level[t as usize] = depth;
                    next.push(t);
                }
            }
            frontier = next;
        }
        let reached: Vec<u32> = (0..97u32)
            .filter(|&v| v != src && level[v as usize] != u32::MAX)
            .collect();
        let hop_sum: i64 = reached.iter().map(|&v| i64::from(level[v as usize])).sum();
        let r = run(
            "MATCH ANY SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN count(b) AS n, sum(size(r)) AS hops",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("any shortest");
        assert_eq!(
            r.rows,
            [[Value::Int(reached.len() as i64), Value::Int(hop_sum)]]
        );

        // ALL SHORTEST against a path-counting oracle: dynamic
        // programming over the shortest-path DAG the levels induce.
        let mut ways = vec![0i64; 97];
        ways[src as usize] = 1;
        let mut order = reached.clone();
        order.sort_unstable_by_key(|&v| level[v as usize]);
        for &v in &order {
            ways[v as usize] = edges
                .iter()
                .filter(|(s, t)| {
                    *t == v
                        && level[*s as usize] != u32::MAX
                        && level[*s as usize] + 1 == level[v as usize]
                })
                .map(|(s, _)| ways[*s as usize])
                .sum();
        }
        let shortest_paths: i64 = reached.iter().map(|&v| ways[v as usize]).sum();
        let r = run(
            "MATCH ALL SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN count(b) AS paths",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("all shortest");
        assert_eq!(r.rows, [[Value::Int(shortest_paths)]]);

        // WALK against an adjacency power oracle: walks of exactly
        // three hops, edge and node repeats allowed.
        let mut at = vec![0i64; 97];
        at[src as usize] = 1;
        for _ in 0..3 {
            let mut next = vec![0i64; 97];
            for &(s, t) in &edges {
                next[t as usize] += at[s as usize];
            }
            at = next;
        }
        let walks: i64 = at.iter().sum();
        let r = run(
            "MATCH WALK (a:person {id: $src})-[:follows*3..3]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("walk count");
        assert_eq!(r.rows, [[Value::Int(walks)]]);

        // ACYCLIC against a brute-force node-distinct DFS oracle over
        // one to three hops.
        fn acyclic(edges: &[(u32, u32)], path: &mut Vec<u32>, total: &mut i64) {
            let cur = *path.last().expect("nonempty");
            for &(s, t) in edges {
                if s == cur && !path.contains(&t) {
                    *total += 1;
                    if path.len() < 3 {
                        path.push(t);
                        acyclic(edges, path, total);
                        path.pop();
                    }
                }
            }
        }
        let mut acyclic_total = 0i64;
        acyclic(&edges, &mut vec![src], &mut acyclic_total);
        let r = run(
            "MATCH ACYCLIC (a:person {id: $src})-[:follows*1..3]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("acyclic count");
        assert_eq!(r.rows, [[Value::Int(acyclic_total)]]);

        // Path returns on real storage, checked structurally against
        // the BFS levels: every path alternates node, rel, node, its
        // length is twice the endpoint's level plus one, its rels are
        // exactly the r list, and each hop's endpoints chain.
        let r = run(
            "MATCH p = ANY SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN p, r, b.id AS id ORDER BY id",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("path return");
        assert_eq!(r.rows.len(), reached.len());
        for row in &r.rows {
            let (Value::List(p), Value::List(rels), Value::Int(b)) = (&row[0], &row[1], &row[2])
            else {
                panic!("expected a path, a rel list, and an id, got {row:?}");
            };
            assert_eq!(p.len(), 2 * level[*b as usize] as usize + 1);
            assert_eq!(p.len(), 2 * rels.len() + 1);
            assert_eq!(
                p[0],
                Value::Node {
                    table: 0,
                    offset: u64::from(src)
                }
            );
            assert_eq!(
                p[p.len() - 1],
                Value::Node {
                    table: 0,
                    offset: *b as u64
                }
            );
            for (hop, rel) in rels.iter().enumerate() {
                assert_eq!(&p[2 * hop + 1], rel, "rel {hop} diverges from r");
                let Value::Rel {
                    src: rs, dst: rd, ..
                } = rel
                else {
                    panic!("expected a rel at hop {hop}, got {rel:?}");
                };
                assert_eq!(
                    p[2 * hop],
                    Value::Node {
                        table: 0,
                        offset: *rs
                    }
                );
                assert_eq!(
                    p[2 * hop + 2],
                    Value::Node {
                        table: 0,
                        offset: *rd
                    }
                );
            }
        }

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
    fn stored_histograms_steer_the_plan_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hubs.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Ten hub sources hold two hundred edges each while every
        // target holds one, so the stored histograms say fan-out 200
        // forward and 1 backward. The count ratio alone says 0.67
        // either way and cannot tell the directions apart.
        let edges: Vec<(u32, u32)> = (0..10u32)
            .flat_map(|h| (0..200u32).map(move |k| (h, 1000 + h * 200 + k)))
            .collect();
        graph::bulk_load_as(&mut db, "person", "follows", 3000, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        // Walking backwards multiplies by 1 per hop instead of 200,
        // so the plan the optimizer picks runs both expands reversed.
        assert!(text.contains("(c)<-[:follows]-(b)"), "got:\n{text}");
        assert!(text.contains("(b)<-[:follows]-(a)"), "got:\n{text}");
    }

    #[test]
    fn analyze_built_colors_steer_the_plan_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("colors.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Ten knows hubs fan into ten sinks that hold no further knows
        // edges. Both directions average ten wide, so the means say an
        // unseeded triangle probes two hundred thousand rows and the
        // close upgrades to the hash join; the coloring sees the walk
        // die at the sinks, so almost nothing survives to probe and the
        // upgrade would pay its accumulate sweep for nothing.
        let knows: Vec<(u32, u32)> = (0..10u32)
            .flat_map(|h| (0..10u32).map(move |t| (h, 10 + t)))
            .collect();
        graph::bulk_load_as(&mut db, "person", "knows", 2010, &knows).expect("load knows");
        // The undirected close keeps the WCOJ fusion out, so the asp
        // mark alone decides between the probe and the hash join.
        let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]-(c) \
                      RETURN count(*) AS n";
        let before = explain_analyze(source, &mut db, &[]).expect("explain before");
        assert!(before.contains("AspJoin"), "got:\n{before}");
        crate::zu1::colors::analyze(&mut db).expect("analyze");
        let after = explain_analyze(source, &mut db, &[]).expect("explain after");
        assert!(!after.contains("AspJoin"), "got:\n{after}");
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

        // A table function source in key space: 9000 is row 0, and the
        // undirected hop distances come back under stored ids.
        let r = run(
            "CALL sssp('knows', 9000) YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY distance, id",
            &mut db,
            &[],
        )
        .expect("sssp");
        assert_eq!(
            r.rows,
            [
                [Value::Int(9000), Value::Int(0)],
                [Value::Int(17), Value::Int(1)],
                [Value::Int(333), Value::Int(1)],
                [Value::Int(12_884_901_888), Value::Int(2)],
                [Value::Int(4025), Value::Int(3)],
            ]
        );
    }

    #[test]
    fn table_functions_run_through_call_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("call.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Two components: a chain 0 -> 1 -> 2 and a pair 3 -> 4.
        let edges: [(u32, u32); 3] = [(0, 1), (1, 2), (3, 4)];
        graph::bulk_load_as(&mut db, "person", "follows", 5, &edges).expect("load");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");

        let r = run(
            "CALL wcc('follows') YIELD node, component \
             RETURN node.id AS id, component ORDER BY id",
            &mut db,
            &[],
        )
        .expect("wcc");
        let ints = |r: &QueryResult| -> Vec<Vec<Value>> { r.rows.clone() };
        assert_eq!(
            ints(&r),
            [
                [Value::Int(0), Value::Int(0)],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(0)],
                [Value::Int(3), Value::Int(3)],
                [Value::Int(4), Value::Int(3)],
            ]
        );

        // Distances from row 1 walk both directions; the other
        // component stays null.
        let r = run(
            "CALL sssp('follows', 1) YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY id",
            &mut db,
            &[],
        )
        .expect("sssp");
        assert_eq!(
            ints(&r),
            [
                [Value::Int(0), Value::Int(1)],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(1)],
                [Value::Int(3), Value::Null],
                [Value::Int(4), Value::Null],
            ]
        );

        let r = run(
            "CALL pagerank('follows') YIELD node, rank \
             RETURN count(node) AS n, sum(rank) AS total",
            &mut db,
            &[],
        )
        .expect("pagerank");
        assert_eq!(r.rows[0][0], Value::Int(5));
        let Value::Float(total) = r.rows[0][1] else {
            panic!("expected a float, got {:?}", r.rows[0][1]);
        };
        assert!((total - 1.0).abs() < 1e-9, "ranks sum to {total}");

        let r = run(
            "CALL louvain('follows') YIELD node, community \
             RETURN count(DISTINCT community) AS communities",
            &mut db,
            &[],
        )
        .expect("louvain");
        assert_eq!(r.rows, [[Value::Int(2)]]);

        // The yielded nodes are real rows: expanding from the small
        // component finds exactly its one edge.
        let r = run(
            "CALL wcc('follows') YIELD node, component \
             WITH node, component WHERE component = 3 \
             MATCH (node)-[:follows]->(m) \
             RETURN count(*) AS inside",
            &mut db,
            &[],
        )
        .expect("compose");
        assert_eq!(r.rows, [[Value::Int(1)]]);
    }

    /// The morsel scheduler over a real zu1 file: workers fork their
    /// own reopened handles, and 64-row morsels split the 500-person
    /// scan across them. Parallel results must equal the sequential
    /// run exactly, rows and order both.
    #[test]
    fn parallel_scan_matches_sequential_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("par.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..3000u32)
            .map(|i| (i % 499, (i * 13 + 7) % 500))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 500, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let sources = [
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            "MATCH (a:person)-[:follows]->(b) RETURN a.id AS a, b.id AS b",
            "MATCH (a:person)-[:follows]->(b) \
             RETURN a.id AS a, count(*) AS deg ORDER BY deg DESC, a LIMIT 10",
        ];
        for source in sources {
            let p = prepare(source, &mut db, &[]).expect("prepare");
            let mut graph = Zu1Graph::new(&mut db, p.catalog);
            let sequential = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    threads: 1,
                    ..exec::Options::default()
                },
            )
            .expect("sequential");
            let parallel = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    threads: 4,
                    morsel_rows: 64,
                    ..exec::Options::default()
                },
            )
            .expect("parallel");
            assert_eq!(
                sequential.rows, parallel.rows,
                "parallel diverged from sequential on: {source}"
            );
        }
    }

    /// The WCOJ intersection over a real zu1 file: triangle queries
    /// run once through the binary-join baseline pinned off and once
    /// through the default path, where the optimizer's mark routes
    /// the close through MultiwayIntersect, and both must equal each
    /// other and a reference count computed straight from the edge
    /// list.
    #[test]
    fn wcoj_matches_the_binary_join_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wcoj.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..4000u32)
            .map(|i| (i % 397, (i * 31 + 11) % 400))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 400, &edges).expect("load");

        let set: std::collections::HashSet<(u32, u32)> = edges.iter().copied().collect();
        let mut by_src: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
        for &(src, dst) in &edges {
            by_src.entry(src).or_default().push(dst);
        }
        let mut reference = 0i64;
        for &(a, b) in &edges {
            for &c in by_src.get(&b).map_or(&[][..], |v| v) {
                if set.contains(&(a, c)) {
                    reference += 1;
                }
            }
        }
        assert!(reference > 0, "seed produced no triangles, no coverage");

        let sources = [
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c), (a)-[:follows]->(c) \
             RETURN count(*) AS triangles",
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c), (a)-[:follows]->(c) \
             RETURN a.id AS a, b.id AS b, c.id AS c ORDER BY a, b, c LIMIT 50",
        ];
        for (ix, source) in sources.into_iter().enumerate() {
            let p = prepare(source, &mut db, &[]).expect("prepare");
            let mut graph = Zu1Graph::new(&mut db, p.catalog);
            let baseline = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    wcoj: exec::Wcoj::Off,
                    ..exec::Options::default()
                },
            )
            .expect("baseline");
            let fused = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options::default(),
            )
            .expect("wcoj");
            assert_eq!(
                baseline.rows, fused.rows,
                "wcoj diverged from the binary join on: {source}"
            );
            if ix == 0 {
                assert_eq!(fused.rows, vec![vec![Value::Int(reference)]]);
            }
        }
    }
}
