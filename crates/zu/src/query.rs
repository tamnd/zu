//! zuQL against an engine catalog: the facade that turns the storage
//! catalog into the binder's `Schema` and runs text through the
//! frontend. The binder itself is engine-agnostic; this is where zu1
//! table definitions become labels and relationship types.

use zu_common::Result;
use zu_query::binder::{self, BoundQuery, NodeDef, RelDef, Schema};
use zu_query::{optimizer, parser, plan};

use crate::zu1::catalog::Catalog;

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
}
