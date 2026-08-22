//! zuQL against the sqlite engine: the same parse, bind, plan, and
//! execute pipeline as the zu1 facade, reading through [`SqliteStore`].
//! Adjacency goes through the lazy CSR cache so traversals run at
//! native speed once a group is built; properties read straight off
//! the B-tree row. The row contract matches zu1's dense domain: node
//! offsets are `zrow` values and loaders assign them densely from
//! zero, so the two engines answer identical queries with identical
//! ids and the differential corpus can compare them verbatim.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::binder::{self, BoundQuery, NodeDef, RelDef, Schema};
use zu_query::exec::{self, Graph, QueryResult, Value};
use zu_query::{optimizer, parser, plan};
use zu_sqlite::{CsrGroup, GROUP_ROWS, SqliteStore};
use zu_storage::Direction;

/// Builds the binder schema from the store's `zu_catalog`.
///
/// Rel tables created before schema version 2 carry no endpoints and
/// cannot be bound; recreate them through `create_rel_table`.
pub fn schema_of(store: &SqliteStore) -> Result<Schema> {
    let tables = store.tables()?;
    let ids: HashMap<&str, u32> = tables
        .iter()
        .filter(|t| t.kind == "node")
        .map(|t| (t.name.as_str(), t.id))
        .collect();
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    for table in &tables {
        match table.kind.as_str() {
            "node" => nodes.push(NodeDef {
                id: table.id,
                name: table.name.clone(),
                node_count: store.node_count(&table.name)? as u64,
                // The sqlite store has no label dictionary, so a table
                // name is the whole of what its rows carry and
                // `Schema::new` fills that in.
                labels: Vec::new(),
            }),
            "rel" => {
                let endpoint = |end: &Option<String>| {
                    end.as_deref()
                        .and_then(|name| ids.get(name).copied())
                        .ok_or_else(|| {
                            ZuError::InvalidArgument(format!(
                                "rel table '{}' has no recorded endpoints; \
                                 recreate it to bind queries against it",
                                table.name
                            ))
                        })
                };
                rels.push(RelDef {
                    id: table.id,
                    name: table.name.clone(),
                    from: endpoint(&table.src_table)?,
                    to: endpoint(&table.dst_table)?,
                    edge_count: store.rel_count(&table.name)? as u64,
                    undirected: table.undirected,
                });
            }
            other => {
                return Err(ZuError::Corrupt {
                    what: "sqlite catalog",
                    detail: format!("table '{}' has unknown kind '{other}'", table.name),
                });
            }
        }
    }
    Schema::new(nodes, rels)
}

/// The executor's view of one open sqlite store. Adjacency reads
/// through the store's CSR cache and keeps the last group pinned per
/// direction, so a scan-driven expand pays the cache lock once per
/// group instead of once per node.
pub struct SqliteGraph<'a> {
    store: &'a SqliteStore,
    names: HashMap<u32, String>,
    columns: HashMap<u32, HashSet<String>>,
    pinned: [Option<(u32, u64, Arc<CsrGroup>)>; 2],
}

impl<'a> SqliteGraph<'a> {
    pub fn new(store: &'a SqliteStore) -> Result<Self> {
        let names = store
            .tables()?
            .into_iter()
            .map(|t| (t.id, t.name))
            .collect();
        Ok(SqliteGraph {
            store,
            names,
            columns: HashMap::new(),
            pinned: [None, None],
        })
    }

    fn name(&self, table: u32) -> Result<&str> {
        self.names
            .get(&table)
            .map(String::as_str)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown table {table}")))
    }

    /// The CSR group covering `node`, from the pin when it still
    /// matches, else through the store's cache.
    fn group(&mut self, rel: u32, node: u64, dir: Direction) -> Result<Arc<CsrGroup>> {
        let group = node / u64::from(GROUP_ROWS);
        let slot = (dir == Direction::Bwd) as usize;
        if let Some((r, g, csr)) = &self.pinned[slot]
            && *r == rel
            && *g == group
        {
            return Ok(csr.clone());
        }
        let name = self
            .names
            .get(&rel)
            .cloned()
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown rel table {rel}")))?;
        let csr = self.store.csr(&name, group, dir)?;
        self.pinned[slot] = Some((rel, group, csr.clone()));
        Ok(csr)
    }

    /// Property columns of node table `table`, from the store on
    /// first touch.
    fn ensure_columns(&mut self, table: u32) -> Result<&HashSet<String>> {
        if !self.columns.contains_key(&table) {
            let cols = self
                .store
                .node_columns(self.name(table)?)?
                .into_iter()
                .collect();
            self.columns.insert(table, cols);
        }
        Ok(&self.columns[&table])
    }
}

fn dir_of(reversed: bool) -> Direction {
    if reversed {
        Direction::Bwd
    } else {
        Direction::Fwd
    }
}

impl Graph for SqliteGraph<'_> {
    fn neighbors(&mut self, rel: u32, node: u64, reversed: bool, out: &mut Vec<u64>) -> Result<()> {
        let csr = self.group(rel, node, dir_of(reversed))?;
        out.clear();
        out.extend(csr.neighbors(node as i64).iter().map(|&n| n as u64));
        Ok(())
    }

    fn degree(&mut self, rel: u32, node: u64, reversed: bool) -> Result<u64> {
        let csr = self.group(rel, node, dir_of(reversed))?;
        Ok(csr.neighbors(node as i64).len() as u64)
    }

    fn degree_sum(&mut self, rel: u32, nodes: &[u64], reversed: bool) -> Result<u64> {
        let mut total = 0;
        for &node in nodes {
            total += self.degree(rel, node, reversed)?;
        }
        Ok(total)
    }

    fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool> {
        let csr = self.group(rel, src, Direction::Fwd)?;
        Ok(csr
            .neighbors(src as i64)
            .binary_search(&(dst as i64))
            .is_ok())
    }

    fn property(&mut self, table: u32, offset: u64, key: &str) -> Result<Value> {
        if self.ensure_columns(table)?.contains(key) {
            let name = self.name(table)?;
            return match self.store.read_node_prop(name, offset as i64, key)? {
                zu_sqlite::Value::Null => Ok(Value::Null),
                zu_sqlite::Value::Int(i) => Ok(Value::Int(i)),
                zu_sqlite::Value::Real(f) => Ok(Value::Float(f)),
                zu_sqlite::Value::Text(s) => Ok(Value::Str(s)),
                zu_sqlite::Value::Blob(_) => Err(ZuError::InvalidArgument(format!(
                    "blob property '{key}' has no zuQL value type"
                ))),
            };
        }
        // Without a stored `id` column the id is the offset, the same
        // dense contract the zu1 facade keeps, and a property no column
        // holds is the null of ISO 20.11 here for the same reason it is
        // there: one engine, one answer, whichever store is under it.
        match key {
            "id" => Ok(Value::Int(offset as i64)),
            _ => Ok(Value::Null),
        }
    }
}

/// Parses, plans, optimizes, and executes one query against an open
/// sqlite store, returning the result rows. Execution is sequential:
/// the store holds one connection, so there is no fork for morsel
/// workers to ride.
pub fn run(source: &str, store: &SqliteStore, params: &[(&str, Value)]) -> Result<QueryResult> {
    // The environment is read in the one place the facade reads it
    // rather than in the middle of a run, the same as the zu1 facade.
    run_with(source, store, params, &crate::query::env_options())
}

/// [`run`] under switches the caller chose rather than the ones the
/// environment names, so the parity corpus can hold both stores to the
/// same join without putting it in the environment for every other test
/// in the binary to find (#513).
pub fn run_with(
    source: &str,
    store: &SqliteStore,
    params: &[(&str, Value)],
    options: &exec::Options,
) -> Result<QueryResult> {
    let schema = schema_of(store)?;
    let parsed = parser::parse(source)?;
    let query: BoundQuery = binder::bind(&parsed, &schema)?;
    let built = plan::build(&query)?;
    let optimized = optimizer::optimize(built, &query, &schema)?;
    let mut args = Vec::with_capacity(query.params.len());
    for name in &query.params {
        match params.iter().find(|(n, _)| n == name) {
            Some((_, v)) => args.push(v.clone()),
            None => {
                // Same condition the zu1 facade raises, for the same
                // reason: a parameter with no value is a reference in
                // the statement that resolves to nothing.
                return Err(
                    ZuError::gql(codes::C42002, format!("missing parameter ${name}"))
                        .about(zu_common::gqlstatus::Subject::Variable(name.to_string())),
                );
            }
        }
    }
    let mut graph = SqliteGraph::new(store)?;
    // The caller's join and executor, and the defaults for the rest.
    // The store holds one connection, so the thread count and the
    // morsel size are not this facade's to take from anybody.
    let options = exec::Options {
        wcoj: options.wcoj,
        engine: options.engine,
        ..exec::Options::default()
    };
    exec::execute(&optimized, &query, &schema, &mut graph, &args, &options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zu_sqlite::{ColumnType, Value as SqlValue};

    /// Five people, seven edges, loaded densely from row zero so ids
    /// match the zu1 contract.
    fn seeded(path: &std::path::Path) -> SqliteStore {
        let mut store = SqliteStore::open(path).unwrap();
        store
            .create_node_table(
                "person",
                &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
            )
            .unwrap();
        store
            .create_rel_table("knows", "person", "person", &[])
            .unwrap();
        for (row, (age, name)) in [
            (31, "ada"),
            (25, "kay"),
            (47, "joe"),
            (25, "amy"),
            (60, "eva"),
        ]
        .iter()
        .enumerate()
        {
            store
                .insert_node_at(
                    "person",
                    row as i64,
                    &[SqlValue::Int(*age), SqlValue::Text((*name).to_owned())],
                )
                .unwrap();
        }
        for (src, dst) in [(0, 1), (0, 3), (1, 2), (2, 4), (3, 4), (4, 0), (4, 2)] {
            store.insert_rel("knows", src, dst, &[]).unwrap();
        }
        store
    }

    #[test]
    fn schema_binds_and_queries_run_on_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded(&dir.path().join("q.db"));
        let schema = schema_of(&store).unwrap();
        let person = schema.node_by_name("person").unwrap();
        assert_eq!(person.node_count, 5);
        let knows = schema.rel_by_name("knows").unwrap();
        assert_eq!(
            (knows.from, knows.to, knows.edge_count),
            (person.id, person.id, 7)
        );

        let r = run(
            "MATCH (a:person {id: $src})-[:knows]->(b) \
             RETURN b.name AS name, b.age AS age ORDER BY name",
            &store,
            &[("src", Value::Int(0))],
        )
        .unwrap();
        assert_eq!(
            r.rows,
            [
                [Value::Str("amy".into()), Value::Int(25)],
                [Value::Str("kay".into()), Value::Int(25)],
            ]
        );

        let r = run(
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS walks",
            &store,
            &[],
        )
        .unwrap();
        let mut expected = 0i64;
        let edges = [(0, 1), (0, 3), (1, 2), (2, 4), (3, 4), (4, 0), (4, 2)];
        for (_, mid) in edges {
            expected += edges.iter().filter(|(s, _)| *s == mid).count() as i64;
        }
        assert_eq!(r.rows, [[Value::Int(expected)]]);

        let r = run(
            "MATCH (a:person) WHERE a.age = $age RETURN a.id AS id ORDER BY id",
            &store,
            &[("age", Value::Int(25))],
        )
        .unwrap();
        assert_eq!(r.rows, [[Value::Int(1)], [Value::Int(3)]]);

        // The same null a property no column holds reads on zu1, since
        // which store is under the engine is not something ISO 20.11
        // knows about.
        let r = run("MATCH (a:person) RETURN a.nope AS x LIMIT 1", &store, &[]).unwrap();
        assert_eq!(r.rows, [[Value::Null]]);
    }

    #[test]
    fn unbound_legacy_rel_tables_error_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let store = seeded(&path);
        drop(store);
        // Simulate a version 1 entry: endpoints wiped, as migration
        // leaves them.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE zu_catalog SET src_table = NULL, dst_table = NULL WHERE kind = 'rel'",
            [],
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&path).unwrap();
        let err = schema_of(&store).unwrap_err();
        assert!(
            err.to_string().contains("no recorded endpoints"),
            "got: {err}"
        );
    }
}
