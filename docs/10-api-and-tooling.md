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
                                   # a parquet column that is neither src nor
                                   # dst loads as an edge property of that name
zu convert social.db social.zu1    # engine ↔ engine
zu verify social.zu1               # CRC/structure audit
zu stat social.zu1 [--format json] # sizes, encodings, bits/edge, cache stats
zu analyze social.zu1              # rebuild the optimizer's COLOR summaries (§07 §6)
zu corpus conformance/cases        # run the cross-client conformance corpus (§7)
zu bench ldbc --sf 1 --engine zu1  # built-in benchmark harness (§11)
zu s3 gc s3://bucket/graphs/social # manual GC / checkpoint / inspect manifest
zu mcp social.zu1                  # MCP server over stdio (2026 table stakes)
```

REPL niceties are product features, not extras: `\d` schema, `\timing`, `EXPLAIN (ANALYZE, FORMAT text|json)` with per-operator rows/time/factorization stats, ASCII plan trees.

`zu shell --format jsonl` is the same session over a pipe, one JSON frame per line, for a harness or an editor rather than a person: `query`, `prepare`/`execute`/`close_stmt`, `explain`, `explain_analyze`, `quit`. The two explain frames are deliberately distinct. `explain_analyze` runs the statement and reports what each operator actually did; `explain` compiles and renders and runs nothing, which is the one a caller can afford beside a latency it measured separately, and the only one that is safe to ask about a statement that writes.

`zu stat --format json` prints the same facts as one object, including the store's size split into schema, free and data. The schema figure is what a database of this shape costs holding nothing, and it exists so that a tool dividing a store by the graph in it can subtract the part that is not the graph. Four blocks of 256 KiB is more than most test fixtures weigh, and a bits-per-edge figure that has not had them taken out of it is a measurement of the header.

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

## 6. The API model

Every generated description of the API is built from one file, `docs/api/model.json`, extracted from rustdoc's JSON by `cargo xtask model` and committed to the tree: the reference pages, the SDK feature matrix, the `zu.h` header, and the `api-map.toml` completeness check. `docs/api/README.md` documents its schema and what the generator does that rustdoc does not. CI runs `cargo xtask model --check` on a pinned nightly, so a pull request that changes the public API and does not regenerate the model fails, and one that does regenerate it shows the change in its own diff.

`docs/api/api-map.toml` is the other half: the model says what the API is, the map says what a binding owes it. Every entity is classified tier 1 (every tier-1 SDK exposes it), tier 2 (a binding may) or tier 3 (none does, and a reason is required). `cargo xtask api-map` joins the two and fails on a public symbol nothing classifies, on a classification for code that is gone, and, run against a binding repository's own map, on a tier-1 entity that binding stopped naming. The first of those runs as a test, so it fires on the machine of whoever added the symbol and not only in CI.

## 7. The cross-client conformance corpus

The map above says what a binding owes the API. The corpus says what a value means once it has crossed the boundary, which is the other half of the same problem and the one no API surface check can reach: nine clients in seven languages, each decoding values with code no other client shares, and the question is whether a value put in through one and taken out through another means the same thing.

`conformance/cases` holds it, as YAML files, each a statement and the rows it owes. Every value is a `{type, value}` pair, and the rule the encoding exists for is which types are written in quotes: INT64, UINT64, the floats and every temporal type are, because a bare `9007199254740993` is a number and a reader that hands it to a double gives back `9007199254740992`. A case that writes one of them bare is refused rather than read leniently. `conformance/README.md` is the schema and the reasoning.

`crates/zu-corpus` is the Rust runner, the first of nine and the reference for the rest; `zu corpus <dir>` is the same runner in the shipped binary, for an unpacked artifact. It runs as a test in this repository so it fires on the machine of whoever adds a case. The cases stay here because they are versioned with the engine and gate its releases (ADR 0005). `tamnd/zu-kit` holds the other eight runners and not the cases, because a case that lives beside one runner is a case that runner cannot fail.

`cargo xtask corpus-pack` builds the release artifact, `conformance-<version>.tar.zst`: the case files as committed, their README, and a `manifest.json` giving the engine version, the case counts and a CRC32C per file. It is a plain tar of text and every language has a reader for it, which is the point. The archive is reproducible, because every header field that could carry the packing machine's clock or user id is fixed, so two mirrors of one release are comparable rather than merely trusted. `--check` packs and writes nothing, and runs on every pull request, because packing parses every case and an artifact that does not load is not a thing to find out about on release day.

Not to be confused with `conformance.toml` at the repository root, which shares a word and nothing else: that is what this engine declares it can do, for the gql-compat harness, generated by `zu conformance --declare`.

## 8. The terminology table

Nine repositories in seven languages write prose about one database. An element that is a `vertex` on one page and a `node` on the next is two data models to a reader who does not already know it is one, and the fix is one table rather than nine style guides that agree until they do not.

`style/zu/terms.yml` in `tamnd/zu-web` is that table: every term, the group it belongs to, a one sentence definition somebody wrote, and the forms that must give way to it. It is in the site's repository because the site is where this program's prose is published from and because the glossary it renders is the table. `cargo xtask terms` is the reader, and it is here, because the prose with the most readers is here and because a table checked by nine repositories against nine readers would be nine tables. `style/README.md` has the schema and the rule for adding a term.

What is checked is prose, and only prose: markdown, and the `//!` and `///` comments that become reference pages. Not identifiers, which answer to the language they are written in; not code spans or fenced blocks, because a check that fires on `Vertex` in a signature teaches people to ignore it; not link targets, which are addresses. A form matches whole words only, the last word of a form may pick up a plural `s`, the words of a multi-word form have to be one phrase, and case is ignored except where a form differs from its own term only in case, which is what lets `zu` refuse `Zu` without refusing every other capitalised word.

A page that quotes somebody else, or writes about another system in that system's words, exempts the form and says which: `<!-- terms: allow row group, vertex -->`. It has to name the forms, so it can never become a blanket switch, and a form it names that the page does not contain is reported as stale, which is the rule the API map follows for a classification of code that is gone.

Two consumers are named in the checklist and are not covered yet, deliberately and not quietly: the CLI's help text and the text of an error. Neither is extractable today, the help living in usage strings among a hundred other literals and an error's message being built where it is raised, and both become extractable when `cli.json` and `errors.json` exist, which are two items of the same release artifact contract. Checking them by grepping for string literals and guessing which ones a user sees would be a check nobody could trust the silence of.

CI runs it with zu-web cloned beside this tree, tracking that repository's main rather than a pin, because a word that changed meaning has changed for this repository the moment it changed for the site. The whole tree is 3.7 MiB of source and the check is about 14 ms, because forms are indexed by their first word: a line costs one hash lookup per word no matter how large the table gets, which is what makes adding a term a decision about the word and not about the budget. `cargo bench -p xtask --bench terms` asserts both, the cost staying linear in the prose and flat in the size of the table.

## 9. Documentation deliverables (v1.0 gate)

Format spec (`docs/format-zu1.md`, byte-accurate, enough to write an independent reader), grammar EBNF, GQL conformance declaration, ops guide for s3 engine (cost tuning worked examples), migration guides (Neo4j/Kùzu → zu: data model mapping + Cypher dialect diffs).
