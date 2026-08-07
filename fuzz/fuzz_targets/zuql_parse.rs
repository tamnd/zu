//! Hostile text against the zuQL frontend: any byte string, lossily
//! decoded, must lex and parse without panicking, overflowing the stack,
//! or hanging. When the parser accepts, the structural invariant the
//! planner relies on must hold (the query is non-empty and ends in
//! RETURN), the binder must resolve or reject the query against an
//! LDBC-shaped schema without panicking, and every bound query must
//! build and optimize into a logical plan without panicking either.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use zu_query::ast::Clause;
use zu_query::binder::{self, NodeDef, RelDef, Schema};
use zu_query::{optimizer, parser, plan};

fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        Schema::new(
            vec![
                NodeDef {
                    id: 0,
                    name: "Person".into(),
                    node_count: 9000,
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 1400,
                },
            ],
            vec![
                RelDef {
                    id: 2,
                    name: "KNOWS".into(),
                    from: 0,
                    to: 0,
                    edge_count: 180_000,
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 9000,
                },
            ],
        )
        .expect("schema")
    })
}

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    if let Ok(query) = parser::parse(&source) {
        assert!(
            matches!(query.clauses.last(), Some(Clause::Return { .. })),
            "accepted query must end in RETURN"
        );
        // Binding may reject, never panic; the same goes for planning
        // and optimizing whatever binds.
        if let Ok(bound) = binder::bind(&query, schema()) {
            if let Ok(built) = plan::build(&bound) {
                let _ = optimizer::optimize(built, &bound, schema());
            }
        }
    }
});
