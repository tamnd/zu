# 10. API, CLI, Server, Tooling

## 1. Rust API (crate `zudb`)

```rust
use zudb::{Database, Config, Value};

let db = Database::open("social.zu1", Config::default())?;          // zu1
// Database::open("social.db?engine=sqlite", ...)                    // sqlite
// Database::open("s3://bucket/graphs/social", cfg.s3(s3_opts))      // s3

let conn = db.connect();                                             // cheap, Send
let mut stmt = conn.prepare(
    "MATCH (p:Person {id: $id})-[:Follows]->(f) RETURN f.name, f.born")?;
for row in stmt.query(params! { "id" => 42 })? {
    let (name, born): (&str, Option<zudb::Date>) = row.get()?;      // typed, borrowed
}

// Bulk
conn.execute("COPY Person FROM 'people.parquet'", [])?;
conn.execute("COPY Follows FROM 'follows.csv' WITH (REORDER = degree)", [])?;

// Arrow (feature `arrow`)
let batches: Vec<arrow::RecordBatch> = stmt.query_arrow(params!{})?.collect()?;

// Write txn
let tx = conn.begin_write()?;
tx.execute("CREATE (:Person {id: $id, name: $n})", params!{ ... })?;
tx.commit()?;
```

Design rules: `Database: Send + Sync` (one per process per graph), `Connection` cheap and single-threaded, rows borrow from a result arena (zero-copy strings), every blocking call has a `_timeout` variant, no async in the public core API (an `async` adapter feature wraps via a worker for s3-heavy apps).

## 2. CLI (`zu`)

```
zu shell social.zu1                # REPL: readline, tables, timing, EXPLAIN
zu query social.zu1 -c "MATCH ..." --format json|csv|table|arrow
zu copy --from follows.parquet --to social.zu1 --table Follows --reorder degree
zu convert social.db social.zu1    # engine ↔ engine
zu verify social.zu1               # CRC/structure audit
zu stat social.zu1                 # sizes, encodings, bits/edge, cache stats
zu analyze social.zu1              # rebuild the optimizer's COLOR summaries (§07 §6)
zu bench ldbc --sf 1 --engine zu1  # built-in benchmark harness (§11)
zu s3 gc s3://bucket/graphs/social # manual GC / checkpoint / inspect manifest
zu mcp social.zu1                  # MCP server over stdio (2026 table stakes)
```

REPL niceties are product features, not extras: `\d` schema, `\timing`, `EXPLAIN (ANALYZE, FORMAT text|json)` with per-operator rows/time/factorization stats, ASCII plan trees.

## 3. Server (optional, crate `zudb-server`)

- Thin: HTTP/1.1+2, endpoints `/query` (JSON), `/arrow` (Arrow IPC stream), `/healthz`, `/metrics` (Prometheus). Auth: bearer token. TLS via rustls.
- Statelessly serves one or more attached graphs; for s3 engine this is the horizontal read-scale deployment unit (readers poll manifests).
- Explicitly *not* a coordination layer, no cluster membership.

## 4. Bindings roadmap

Python first (PyO3, Arrow-native `.to_arrow()/.to_pandas()`), then Node (napi-rs), WASM (browser demo, zu1 read-only over HTTP range requests), Swift last (post-v1). All bindings sit on the C ABI crate `zu-capi` (stable, versioned, generated header).

## 5. Observability

- `tracing` spans (feature `trace`): parse/bind/optimize/execute, per-pipeline; s3 request log with cost counters ($ estimate per query!).
- `zu stat --live`: cache hit rates, S3 request rates vs budget, WAL depth, checkpoint lag.
- Every error carries a stable code (`ZU####`) documented in `docs/errors.md`.

## 6. Documentation deliverables (v1.0 gate)

Format spec (`docs/format-zu1.md`, byte-accurate, enough to write an independent reader), grammar EBNF, GQL conformance declaration, ops guide for s3 engine (cost tuning worked examples), migration guides (Neo4j/Kùzu → zu: data model mapping + Cypher dialect diffs).
