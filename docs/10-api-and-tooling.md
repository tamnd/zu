# 10. API, CLI, Server, Tooling

## 1. Rust API (crate `zudb`)

```rust
use zudb::{Database, Config, Value};

let db = Database::open("social.zu1")?;                              // zu1
// Database::open("social.db?engine=sqlite")                          // sqlite
// Database::open_with("s3://bucket/graphs/social", cfg.s3(s3_opts))  // s3

let conn = db.connect()?;                                            // cheap, Send
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

`Database::open` takes a path and nothing else, and `open_with` is the second constructor for the callers that configure something. A required configuration argument is a tax charged to every user to serve the few who set anything, and `Config::default()` typed by everybody is a phrase that teaches nothing. `Config` itself is a builder with defaults that work: the memory budget the caches size themselves from, the thread count the executor picks when it is zero, and read-only, which opens the file on a descriptor this process cannot write through so that a database on a read-only mount opens and a statement that would write is refused by name rather than by errno.

The split is what the rest of the program is built on. A `Database` is a path and a configuration that have been checked against a real file, holding no descriptor and no cache, which is why it is shareable without a lock. A `Connection` is a file handle, the caches above it, and a plan cache, which is why it is not: reads through one handle seek, so two threads sharing one would serialize on the seek position at best. Taking one costs an open and a catalog load, tens of microseconds, so a connection per thread or per request is the intended shape and a pool is an optimization rather than a necessity. Every binding on the C ABI inherits this split (`dx/02` §3), which is why the Rust API has it first, and the C ABI's runtime ownership check is the same rule this API gets from the borrow checker for free.

A connection reads the database as of when it connected. It keeps the header it opened at, so a write another connection published since is not visible to it, and a reader that wants the latest catalog takes a new one. Making it otherwise means re-reading two header slots per statement, which is the entire cost of a warm query, so the answer to it is the snapshot machinery of `docs/08-transactions-mvcc.md` and not a read on the hot path.

`crates/zu/benches/connect.rs` holds both halves of that to a number, and the second is the one that matters: the warm point read through a `Connection` measures 0.99 to 1.03 times the same read through the engine's own `Session`, which is the noise floor of two ten-thousand-read medians. A public API that costs something over the engine is one the people who care about latency route around, so it is gated rather than asserted.

Loading data goes through an appender rather than through statements. `conn.appender("person")` gives one, `append_row` takes a row as a tuple or a slice of fields, and `close` returns how many rows went in. A row is every column of the table in the order the table declares them, and a column is a position rather than a name, because naming the columns per row costs a lookup per value on the one path where per-value cost is the whole story. The reason for the type is the arithmetic: a statement per row is a commit per row, and a commit is the expensive part, so a million rows loaded that way is a million commits. Buffering makes it one.

A flush is that one commit. The buffered columns are sealed into the data file as segments and the log gets a single frame naming them, which is the bulk-load WAL bypass of `docs/08-transactions-mvcc.md` §6: the log stays the same handful of bytes whether the flush carries ten rows or ten million. The fold that follows seals those segments into the base the query path reads, so when a flush returns the rows are both durable and visible, and before it returns neither. That fold costs time proportional to the table rather than to the batch, which is the whole reason to buffer: flush once per load, not once per row.

`crates/zu/benches/append.rs` gates both ends. Buffering a row costs 26 to 42 ns on a two-column table, one column of which is a string and so costs a copy, which is what says the row path allocates nothing. Two thousand rows loaded with one flush cost 0.0007 to 0.0011 times the same rows loaded with a flush each, both timed in one run, which is the design stated as a ratio.

Because a commit is durable at the log frame and visible at the fold, a crash between the two would leave rows on disk that no statement could see. Every writable open therefore replays a sidecar log that still has something in it and folds it before the first statement runs, so that window closes at the next open rather than staying open forever. A read-only open cannot fold and does not pretend to: it reads the base as the last fold left it.

## 2. CLI (`zu`)

```
zu shell social.zu1                # REPL: readline, tables, timing, EXPLAIN
zu query social.zu1 -c "MATCH ..." --format json|csv|table|arrow
zu copy --from follows.parquet --to social.zu1 --table Follows --reorder degree
                                   # a column that is neither src nor dst loads
                                   # as an edge property of that name: from the
                                   # parquet schema, or from an LDBC style csv
                                   # header (:START_ID,:END_ID,name:INT64)
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

## 9. The pinned toolchain table

Nine repositories build one release out of one set of versions, and the question "which Rust do we build against" stops having one answer the moment it is written in nine workflows: one is bumped, the others are not, and the bug that follows is found by a user on the platform nobody bumped.

`toolchains.toml` at the root of this repository is that table. A row is a component, the version it is pinned to, the oldest version still promised, the repositories that build against it, and a sentence saying why the numbers are what they are. A pin is exact, because "the newest 1.97" is a range and a range is how two machines end up building different things. A floor is a promise rather than a fact, and the conformance matrix runs the floor as well as the pin, because a library that only ever builds against the newest release finds out its floor is broken from a bug report. The table records the date it was audited against the registries.

A component this repository builds against says where its version is written, in a `[[site]]` naming the file, the key, whether the site holds the pin or the floor, and whether it has to say the version exactly or name the series it is in. Exact is what `rust-toolchain.toml` and a workflow say, since those name a release to install. A series is what a cargo requirement of `59` says against `59.2.0`, since writing the patch level in a manifest is a lockfile in the wrong file.

`cargo xtask pins` checks it in both directions, which is the shape the API map already has. Forward: every site exists, holds the key it claims, and agrees with the version. Backward: every `toolchain:` a workflow pins is one the table names, so a job added next month cannot quietly introduce a tenth answer, and a component this repository builds against with no site is reported, since a row nothing holds is a row nobody maintains. It runs as a test on all three platforms of the matrix and as a command in CI, the second because a command nothing exercises is a command that rots.

The whole table against the whole tree is under a millisecond, because a site is one scan of the file it names and there are as many scans as sites. `cargo bench -p xtask --bench pins` asserts that shape rather than the number, the per-site cost staying flat as the table grows, since the way a check like this dies is the accidental square that costs nothing at six sites and is unaffordable at two hundred.

Bumping a version is therefore a pull request that touches this table and every site of it together, which is the entire point of writing it down.

## 10. The platform table and the libzu matrix

Seven targets are tier 1: linux x86_64 and aarch64 against glibc, the same two against musl, both Macs, and Windows on x86_64. Tier 1 means a prebuilt binary for every SDK, a full matrix per release, and a release that stops when one of them fails, which is a promise worth exactly what CI proves of it. So all seven build on every pull request rather than for the first time on the day of a release.

`platforms.toml` is the table, and it is the tiers of dx/14 §2 as data rather than as a paragraph nine repositories each read differently. A row is a target, its tier, the runner that is that machine, the image it builds in where that matters, what the library and the CLI are called there, and whether the runner can run what it built. `cargo xtask platforms` holds `.github/workflows/libzu.yml` to it in both directions: a tier-1 target the matrix does not build is a promise nothing keeps, and a matrix row for a target the table does not have is a platform being shipped by nobody's decision. The check runs as a test, so it fires on the machine of whoever edited one of the two files.

The glibc floor is 2.28, which is manylinux_2_28 and covers RHEL 8 and everything newer, so the two gnu rows build inside that image rather than against the runner's own glibc. A library linked against a newer glibc loads on the machine that built it and dies on the user's, which arrives as a bug report saying the install is broken. The musl rows are what makes a container work, and they are built inside Alpine rather than on the runner: a musl shared library links musl's libc and the unwinder beside it, which an Ubuntu machine does not have, so the row would fail to link here long before it failed to load there. Building where the artifact runs also makes the smoke test the real one. Rows a hosted runner cannot run at all, freebsd and riscv64, are recorded as tier 2 with no runner rather than left out, since the table is the promise and the promise is smaller there.

Every row that can run what it built runs a C program whose only knowledge of zu is `zu.h`, compiled by a compiler that is not rustc and linked against the shared library that job produced. The Rust test in `zu-capi` calls the same functions and proves something else, because it links the rlib the compiler had in hand: the artifact test is the one that fails when a symbol is not exported, when the header and the library disagree about a type, or when the build for a platform picked up the wrong libc. It opens a graph, counts its nodes, and takes a refusal and frees the message, so the failing path crosses the boundary too.

The same table carries the size ceilings of dx/14 §4, and `cargo xtask platforms --measure` weighs what a build produced against them on every platform. Binary size is a real adoption factor for serverless and mobile targets and it only ever drifts upward, so it is a number with a limit rather than a graph somebody looks at once a quarter. Today `libzu` is 2.3 MiB against a ceiling of 14 and the CLI is 4.6 against 15. A file the build did not produce is an error rather than a zero, since a missing artifact is otherwise the cheapest way to pass a size gate.

## 11. The release-artifact contract

A release is a tag on this repository and a run that drives the eight others (dx/14 §6). Every one of them builds against what this one published, which makes the list of what gets published a contract rather than a step in a workflow. A binding that fetches `model.json` for the version it pins and finds nothing cannot tell a release that dropped the artifact from a version that never had it, and the failure surfaces in somebody else's CI a day later, which is the worst place for it.

`artifacts.toml` is that list, and the release workflow has none of its own: it assembles from the table and reads the directory back against it. A row is a name, where it comes from, the repositories that fetch it, and a sentence saying why they do. There are four ways a row comes to exist, because there are four ways an artifact does. A `file` is a path this tree already holds, `zu.h` and `model.json` being the two. A `corpus` is packed by the packer of §7. A `platform` row is one artifact per tier-1 target, expanded against `platforms.toml`, so the seven move with that table rather than with this one. And a `later` row is an artifact the contract names that nothing makes yet, carrying the milestone that will make it: `cli.json` with D1, `gql.json` and `errors.json` with D2. Naming those early is the point rather than an oversight, since a consumer needs to know what a release will eventually carry and the alternative is eight repositories each guessing.

`cargo xtask artifacts --assemble` gathers a release into a directory and `--verify` reads it back, in both directions like every other table here: an artifact the contract publishes and the directory does not hold is a consumer's failure tomorrow, and a file in the release that no row accounts for is something somebody will fetch that nothing promises. The check that runs on every pull request is the third direction, holding the table to the tree and to the workflow: a `file` row whose file has moved, and a release workflow that does not assemble from the table, which is what would turn the table back into a document. Who the consumers are is checked by §12, because the list of repositories is one list and this is not where it lives. Assembling a release of ten files takes about as long as compressing them, and the bookkeeping either side of it is a hundredth of a millisecond.

A platform's build stages the library, the CLI, the import library where there is one, and the header into a directory named for the target, and that directory is the artifact: `libzu-<target>.tar.zst` unpacks to four files rather than to four levels of somebody's build path. Both archives are written by the same tar writer, reproducibly, so two mirrors of one release can be compared rather than trusted.

`.github/workflows/release.yml` is the train of dx/14 §6 with its publish steps as no-ops. What is real is the build, which is the same matrix every pull request runs, the assemble, and the verify. What prints instead of running is the publishing, in the order it will happen: crates.io before the repositories that build against it, and the Go tag last of the registries because pushing a tag is the one publish that cannot be taken back. A rehearsal runs on `workflow_dispatch` with a version rather than a tag, because a release train is exactly the machinery that must not run for the first time on the day of a release.

## 12. The repository table and the conductor

The split of ADR 0005 put eight repositories outside this one, and a split is a decision made once: after it, the list of what was split out is a thing every part of the project quotes. The release train dispatches to it, the README publishes it to everybody arriving through a package manager, and the artifact contract of §11 names its consumers from it. Three copies of one list is how a repository ends up on the train and off the README, which is a release publishing to a registry that no page tells anybody about.

`repos.toml` is the list. A row is a name, what the repository is to the train, its support tier, the milestone it appears at, the workflow the train dispatches there, and what it reports back. The last column is the one that makes the collect step of dx/14 §6 checkable rather than aspirational: a release collects scorecards, map completeness in both directions, perf and sizes, and which repository owes which is written down before any of them can quietly owe nothing. The roles are the reason a row can be checked at all: the engine is the one repository the train does not dispatch to, because the train is its own tag, and the site is the one without a tier, because a tier is a promise about an SDK.

`cargo xtask repos` holds three files to it, each in both directions. The conductor's matrix, so a repository the table drives and nothing dispatches to is caught along with a dispatch to a repository nobody decided to drive. The README's client table, including the tier column, since the tier is the promise a user reads before they depend on something. And the artifact contract's consumers, since a consumer that is not a repository is a fetch nobody will ever make and a repository that fetches nothing is either a gap in the contract or a repository that should not have been split out. Nine rows will not be nine forever, so the cost of holding all three is measured per repository and is about two microseconds of it.

`.github/workflows/conductor.yml` is the dispatching half of the train, and every dispatch in it is a no-op, because none of the eight has a release workflow to dispatch to until DX1 through DX5. What is real is the shape, which is the expensive part to get wrong late: two stages, the seven bindings and the kit first and the site after them, because a site deployed against a release whose wheels failed their corpus run documents something nobody can install. Each dispatch collects a file per report it owes, every one of them saying `pending`, and the collect step prints the eight repositories and their thirty-one empty results. When the dispatches become real, the gate is already there and the only change is what the files say.

## 13. Documentation deliverables (v1.0 gate)

Format spec (`docs/format-zu1.md`, byte-accurate, enough to write an independent reader), grammar EBNF, GQL conformance declaration, ops guide for s3 engine (cost tuning worked examples), migration guides (Neo4j/Kùzu → zu: data model mapping + Cypher dialect diffs).
