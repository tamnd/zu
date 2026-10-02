# zudb-query

The zu query engine. It takes the text of a zuQL statement (zu's dialect of ISO GQL) and turns it into rows: lexer, parser, binder, logical plan, optimizer, and a factorized vectorized executor. It knows nothing about files. It reads the graph through a small trait, so the same engine runs over all three of zu's storage engines.

## How a statement runs

```
text -> lexer -> parser -> AST -> binder (+ Schema) -> BoundQuery
     -> plan::build -> LogicalPlan -> optimizer -> exec::execute (+ Graph) -> QueryResult
```

- `lexer` and `parser` are hand-written. The parser is recursive descent over the grammar in `docs/grammar.ebnf` and covers MATCH, OPTIONAL MATCH, WHERE, CALL, UNWIND, WITH, and RETURN, plus the data-changing statements. Keywords match case-insensitively, errors name the line and column, and expression nesting is capped so hostile input cannot overflow the stack.
- `ast` is a plain description of the text. Anything that needs the catalog is left to the binder.
- `binder` resolves every variable to a slot, every label and rel type to a table, and types every expression. It binds against a `Schema`, a plain description of the node and rel tables plus whatever statistics the engine has, not against a storage engine. That is why it binds the same way over `zu1`, SQLite, and S3, and why its tests need no file.
- `plan` builds a left-deep `LogicalPlan`: scans introduce nodes, expands walk rels, predicates filter at the earliest point their variable exists. `plan::explain` renders it for EXPLAIN.
- `optimizer` orders joins by dynamic programming over the relationship subsets, driven by degree statistics when the engine has them, and places each filter at the earliest point it can run.
- `exec` splits the plan into stages at every projection or aggregation. Inside a stage, operators pull `Chunk`s of up to 2048 values. An expand keeps its output factorized, as a list of neighbors per source, until something needs one row at a time. Stages can run morsel-parallel when the graph handle can be forked.

Around that core:

| Module | What it holds |
| --- | --- |
| `functions`, `procedures` | the builtin function table and the procedure catalog the binder resolves names against |
| `cast`, `typed`, `value_type` | `CAST`, `IS TYPED`, and the type names that map onto the type lattice in `zudb-common` |
| `recursive` | the frontier BFS behind variable-length patterns, switching between sparse and dense frontiers at runtime |
| `csr`, `kernels` | an in-memory CSR and the graph kernels on it: BFS, shortest path, triangle count, PageRank, connected components |
| `column`, `row` | reading a result down its columns (what Arrow export uses) or as typed Rust tuples |
| `frame` | registering caller-owned columns, such as a dataframe, as a table a statement can read |
| `snapshot` | the vectorized read surface that the pipeline executor in `zudb-exec` uses |
| `refs` | graph and binding table reference values |

## Using it

The storage side is the `exec::Graph` trait. Only `neighbors`, `has_edge`, and `property` have to be written; the rest have defaults. This runs a two-hop chain held in a struct, which is about the smallest graph the engine will accept. `Result` comes from `zudb-common`, so add that crate as well if you try it.

```rust
use zu_common::Result;
use zu_query::binder::{self, NodeDef, RelDef, Schema};
use zu_query::exec::{self, Graph, Options, Value};
use zu_query::{optimizer, parser, plan};

/// Three people in a chain: 0 knows 1, 1 knows 2.
struct Chain;

impl Graph for Chain {
    fn neighbors(
        &mut self,
        _rel: u32,
        node: u64,
        reversed: bool,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        out.clear();
        let next = if reversed {
            node.checked_sub(1)
        } else {
            Some(node + 1).filter(|&n| n < 3)
        };
        out.extend(next);
        Ok(())
    }

    fn has_edge(&mut self, _rel: u32, src: u64, dst: u64) -> Result<bool> {
        Ok(dst == src + 1 && dst < 3)
    }

    fn property(&mut self, _table: u32, offset: u64, key: &str) -> Result<Value> {
        Ok(match key {
            "name" => Value::Str(["ada", "grace", "edsger"][offset as usize].into()),
            _ => Value::Null,
        })
    }
}

fn main() -> Result<()> {
    let schema = Schema::new(
        vec![NodeDef {
            id: 0,
            name: "person".into(),
            node_count: 3,
            labels: Vec::new(),
        }],
        vec![RelDef {
            id: 1,
            name: "knows".into(),
            from: 0,
            to: 0,
            edge_count: 2,
            undirected: false,
        }],
    )?;

    let parsed = parser::parse("MATCH (a:person)-[:knows]->(b:person) RETURN a.name, b.name")?;
    let query = binder::bind(&parsed, &schema)?;
    let optimized = optimizer::optimize(plan::build(&query)?, &query, &schema)?;
    println!("{}", plan::explain(&optimized, &query, &schema));

    let result = exec::execute(
        &optimized,
        &query,
        &schema,
        &mut Chain,
        &[],
        &Options::default(),
    )?;
    for row in result.rows.iter() {
        println!("{row:?}");
    }
    Ok(())
}
```

That prints the plan and then two rows:

```
Project a.name, b.name
  Expand (a)-[#1:knows]->(b)
    ScanNodes a: person

[Str("ada"), Str("grace")]
[Str("grace"), Str("edsger")]
```

This is the same sequence `zudb` runs for every read. In `zudb` the `Schema` comes from the file's catalog and the `Graph` is a reader over the file, and parameters, transactions, and caching of compiled plans are all handled there.

## Two executors

The executor in this crate is the older, pull-based one. Plans that `zudb-exec` covers run there, and this one stays as the fallback and as the reference the new executor is tested against. The full plan for the language and the operators is in `docs/07-query-engine.md`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-query` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_query`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
