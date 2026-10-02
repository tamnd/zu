# zudb-exec

The push-based, morsel-parallel executor for zu. It runs the same `LogicalPlan`s as the executor in `zudb-query`, but it compiles them into a pipeline of push operators over column vectors and runs that pipeline on several threads at once.

## How it works

`try_execute` takes an optimized plan from `zudb-query` and goes through three steps.

1. `compile` turns the plan into one pipeline: a scan at the bottom, then filters and expands, then a sink. If any part of the plan is a shape it does not handle, it returns `None` and nothing runs.
2. `run` splits the driving scan into morsels aligned to the storage's node groups. A shared counter hands them out in scan order to a persistent worker pool (`pool`). The calling thread is worker 0, and the others read through forked snapshot handles that share the block cache. Each worker pushes its morsel through the whole pipeline into its own sink, with no locks.
3. `sink` merges the per-worker results. Batches are put back together by morsel index, so a parallel run returns the same rows in the same order as a sequential one. LIMIT stops all the workers early once enough rows are in.

All reads go through `zu_query::snapshot::Snapshot`, which hands out decoded columns a chunk at a time and pinned CSR slices, instead of the one-value-per-call `Graph` trait.

The other modules:

| Module | What it holds |
| --- | --- |
| `group` | the hash table behind GROUP BY and DISTINCT, with open addressing and keys packed into one flat buffer |
| `join` | the hash table behind hash joins, built once and shared by the workers instead of rebuilt by each one |
| `sip` | sideways information passing, where a join's build side tells the probe side which keys can match so it can skip the rest early |
| `decide` | the eight decisions the pipeline makes at runtime from what the data shows, each one counted and printed by EXPLAIN ANALYZE |
| `columns` | the sink for a plain projection, which keeps column vectors instead of building rows, so Arrow export can take them as they are |

`PlanCache` holds the compiled pipeline of one statement so the next run with new parameters skips the compile step. The cache is checked against the snapshot epoch and the options before it is reused.

## Using it

Every entry point returns `Ok(None)` for a plan it does not cover, and the caller is expected to fall back to the row executor. This is the shape `zudb` uses:

```rust
use zu_common::Result;
use zu_query::binder::{BoundQuery, Schema};
use zu_query::exec::{self, Graph, Options, QueryResult, Value};
use zu_query::plan::LogicalPlan;
use zu_query::snapshot::Snapshot;

/// Try the pipeline first, and fall back to the row executor in
/// zu_query for any plan shape it does not cover yet.
fn run(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<QueryResult> {
    if let Some(result) = zu_exec::try_execute(plan, query, schema, snap, params, options)? {
        return Ok(result);
    }
    exec::execute(plan, query, schema, graph, params, options)
}
```

The other entry points have the same contract. `try_execute_cached` reuses a `PlanCache`, `try_execute_streaming` hands rows over in batches as they are made, and `try_execute_profiled` also returns the runtime decisions for EXPLAIN ANALYZE. With `options.flat` set they all return `None`, and `Options::engine` set to `Engine::Rows` makes `zudb` skip this crate entirely.

You need a `Snapshot` implementation to call any of this. The real one reads a `zu1` file and lives in `zudb`.

## What it covers today

The linear read pipeline: one node scan, filters, single-hop expands (including a second pattern branch sharing a variable), and one final projection or aggregation with its DISTINCT, ORDER BY, SKIP, and LIMIT. Variable-length expands, bracketed groups, closing joins, UNWIND, table functions, and rel values still fall back to the row executor. Whatever this crate does run has to match the old executor exactly, including row order, group order, and errors on overflow, and the parity test suite in `zudb` holds it to that.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-exec` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_exec`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
