# DuckDB's Python and TypeScript clients, measured against ours

DuckDB is the only embedded analytical database whose clients are used more than its engine is talked about, and its Python client is the most used embedded analytics client there is. That makes it the bar for `zu-python` and `zu-node`, and it makes the interesting question a narrow one. Not what DuckDB has that we do not, which is a long list and most of it is SQL. What a user would feel.

This page is the answer, measured rather than asserted. Everything below was run on one machine on 2026-08-19: an Apple M-series laptop, DuckDB 1.5.5 for Python and `@duckdb/node-api` 1.5.5 for TypeScript, `zudb` built from the tree at 9ed70a8, Python 3.14.6 and Node 24.18. A million rows of three columns, `INT64`, `DOUBLE` and a `VARCHAR` of about five bytes, in a stored table on both sides. Each figure is the fastest of five runs, because what is being compared is the code path and not the scheduler.

One row of each table below is not a comparison and is marked as such. DuckDB's `execute` and `run` hand back a result nobody has read yet, so the milliseconds they take are the cost of planning and of nothing else; ours has the whole answer in memory by the time it returns. The pair is printed because the difference between them is the subject of this page, not because 0.3 against 50.6 is a ratio anybody should quote.

## 1. What the numbers say

Python, one million rows out:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| execute, nothing read | 50.6 ms | 0.3 ms | not a comparison |
| execute and `fetchall` | 450 ms | 1365 ms | 3.0x faster |
| execute and Arrow table | 454 ms | 22 ms | 20.6x slower |
| execute and pandas | 372 ms | 111 ms | 3.4x slower |

Python, one row out, and one registration of a million-row Arrow table:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| point read, whole call | 11.1 us | 221.7 us | 20.0x faster |
| `register` and `unregister` | 15.9 us | 177.1 us | 11.1x faster |

TypeScript, one million rows out:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| statement only, rows dropped | 858 ms | 3.3 ms | not a comparison |
| rows as objects | 3198 ms | 615 ms | 5.2x slower |
| rows as columns | not offered | 333 ms | |
| point read, whole call | 0.1 ms | 0.2 ms | 2x faster |

Three things fall out of that, and they are the whole page.

## 2. We win latency, by a lot, and it is not an accident

A point read is twenty times faster in Python and twice as fast in TypeScript, and a registration of somebody else's Arrow table is eleven times faster. Neither number is about the query engine. They are about what a call costs before and after the query, which is the cost that dominates every workload made of small questions, and small questions are what an embedded database is for.

The registration figure is the clearer of the two because there is no query in it at all. DuckDB's replacement scan copies the frame's description and builds a table function around it; ours takes the pointers, the widths and the meanings and hands them to the engine, and a statement that names the frame reads the caller's own arrays where they lie. `docs/10-api-and-tooling.md` section 4 has the mechanism. The measurement here is that the mechanism is worth what it was supposed to be worth.

Nothing in this page proposes giving that up.

## 3. We lose bulk export, by about twenty, and it is one cause

`to_arrow` on a million rows takes 454 ms against DuckDB's 22 ms. The Arrow table on both sides is the same shape and the same bytes. What differs is what each engine had to start from.

DuckDB never has rows. A result is a sequence of data chunks and a chunk is a set of column vectors, so exporting to Arrow is a per-column handoff of a buffer and a validity bitmap, and for flat numerics it copies nothing at all.

We have rows. `QueryResult` in `crates/zu-query/src/exec.rs` is `rows: Vec<Vec<Value>>`, and the sink in `crates/zu-exec/src/sink.rs` fills it a row at a time out of the vectors the executor was working in. Every columnar consumer then transposes it back: `zu-python`'s `columns.rs` walks the whole result once per column to infer a kind, walks it again to gather a `Vec<&Value>` per batch, and builds Arrow arrays out of that. Three million boxed values are built and then read twice, and the 20x is the cost of doing that.

`record_batches` is the same code and is worse rather than better, which is worth saying because its name promises the opposite. It builds every batch into a `Vec<RecordBatch>` before it hands back a reader, so a caller who asked for batches because the result would not fit in memory gets the whole Arrow table built first and the original rows still alive beside it.

The same trade explains the row path going the other way. `fetchall` is three times faster than DuckDB's, and for exactly the reason Arrow is twenty times slower: our rows are already rows.

So this is not a bug and it is not a missing optimisation. It is one decision, visible from both ends, and it is the decision this audit says to revisit.

## 4. The vector layer we already have

The thing that makes the fix tractable is that the hard half is built.

`crates/zu-vector` is a DuckDB-shaped vector layer and has been since perf/02. `PhysType` is a physical type per vector rather than a tag per value. `VecEncoding` is flat, constant or dictionary, with strings flowing as codes end to end. Validity is an optional bitmap, absent when everything is valid, which is Arrow's own convention and Arrow's own layout. `StrView` is a sixteen byte view holding a length, a four byte prefix, and either twelve inline bytes or a buffer id and an offset, which is byte for byte Arrow's `Utf8View`, a fact `zu_frame_col_view` already relies on for reading a frame in.

The executor uses all of it. `run.rs`, `join.rs`, `group.rs` and `sip.rs` work in `ValueVector` and `DataChunk` at `VECTOR_SIZE` a time. The collapse happens at exactly one place, the sink, and `docs/10-api-and-tooling.md` section 4 already says so out loud about the C ABI's chunked reads: they are "a slice of what it already materialized", and will stop being "once a chunk is what the executor produced".

That sentence is the work this page is asking for. A result that keeps the vectors it was built from makes Arrow export a handoff rather than a rebuild: for `Int64`, `Float64`, `Bool`, `Date`, `Timestamp` and strings, the buffer that goes into the Arrow array is the buffer the executor filled, released through Arrow's own release callback when the last reader is done with it. Zero copies, not fewer copies.

The row path keeps working because a row is a gather across vectors at a known offset, which is what `fetchall` would do instead of what it does now. Whether it stays as fast is the thing to measure rather than assume, and it is the reason this is staged behind a benchmark rather than declared.

## 5. TypeScript is a milestone behind, not a factor behind

The Python client is a fast client with one slow path. The TypeScript client is a smaller client.

What it has: `query`, `exec`, `cursor`, `stream` with async iteration and Web Streams, `AbortSignal` wired to the interrupt, `await using`, the four temporal value classes with `toTemporal()`, `bigint` by default with a documented opt out, and the full error model with codes, positions and doc URLs. That is a good client and the parts of it that exist are idiomatic.

What it has not, all of which `zu-python` has:

- no transactions, so a program cannot make several statements one
- no appender, so there is no fast way to add rows to a table that exists
- no `register`, so the zero-copy frame path, the fastest thing this engine does across a boundary, is not reachable from Node at all
- no bulk load, so a program cannot build a database from Node
- no Arrow, no columnar read of any kind
- no `rowsRead` and no progress callback

The last one is the one to look at first alongside the numbers. `duck runAndReadAll getColumns` is 333 ms where the same rows as objects are 615 ms, and our own object path is 3198 ms. A columnar read is the single largest thing missing, and it is also the one that a columnar result would make cheap to add.

And a program cannot build a database from Node at all. `connect` creates the file, `CREATE NODE TABLE` answers `42001 not implemented`, and there is no loader and no appender on the surface, so the only way in is an `INSERT` per row. That is a first-run experience of an empty file and no way to fill it, and it is why the client scorecard's `api-map` item cannot pass for either client today: the ledger puts `explain`, `profile`, `prepare` and prepared statements at tier 1, and neither client names any of them.

## 6. The surface, item by item

DuckDB's Python connection has 71 public members and its module 181. Ours has 14 and 22, plus the DB-API, asyncio and notebook layers. Most of that difference is SQL surface we correctly do not have, such as `read_parquet` and the extension loader. What follows is only the part where a user would notice.

| DuckDB | zu-python | zu-node | verdict |
|---|---|---|---|
| `connect()` with no path, in memory | needs a path, and `':memory:'` makes a file called that | needs a path | owed, and the `':memory:'` behaviour is a bug |
| `sql` / `execute` / module-level default connection | `execute`, `sql` | `query`, `exec` | done, minus the module-level default |
| `fetchall`, `fetchone`, `fetchmany` | `fetchall`, `fetchone` | rows are an array | `fetchmany` owed for DB-API |
| `arrow()`, `fetch_arrow_table` | `to_arrow` | none | owed for Node, 20x for Python |
| `fetch_record_batch` streaming | `record_batches`, over a materialized result | none | owed, real streaming |
| `df()`, `pl()`, `fetchnumpy()`, `torch()`, `tf()` | `to_pandas`, `to_polars` | none | numpy owed, torch and tf are not our lane |
| `register` / `unregister` | `register` / `unregister`, faster | none | owed for Node |
| `append(table, df)` | `appender()` | none | owed for Node |
| `begin` / `commit` / `rollback` | `transaction()` | none | owed for Node |
| `interrupt()`, `query_progress()` | `interrupt()`, `rows_read` | `AbortSignal` only | progress owed for Node |
| `cursor()` / `duplicate()` | none | none | owed, it is how a pool is written |
| prepared statements | none | none | owed, tier 1 in the ledger |
| `explain`, profiling | none | none | owed, tier 1 in the ledger |
| `create_function` UDFs, Arrow-vectorised | none | none | not this milestone |
| relational API, 111 members | none | none | deliberately not |
| Spark API | none | none | deliberately not |
| DB-API 2.0 | `zudb.dbapi` | n/a | done |
| asyncio | `zudb.aio` | native | done |
| notebook rendering | `_repr_html_`, `%%gql` | n/a | done |

The relational API is the one row worth arguing about, and the answer is no. It is 111 methods that build a query without running it, and it exists because DuckDB's users want to compose SQL from Python. A graph query language composes in the language, and a hundred methods that shadow it would be a second dialect to document, test and keep in step for as long as the client lives.

## 7. What this becomes

In order, largest effect first.

1. A columnar result. The executor keeps the vectors, the result owns them, and `Vec<Vec<Value>>` becomes something the row path gathers rather than something every other path unpicks. This is the 20x and it is the only item here that touches the engine.
2. Arrow export as a handoff. Once the vectors survive, `to_arrow`, `__arrow_c_stream__` and `record_batches` become a description of buffers and a release callback, with no copy for the six physical types whose layout is already Arrow's.
3. The TypeScript client reaching the Python one: transactions, appender, register, bulk load, and a columnar read of a result.
4. Prepared statements, `explain` and `profile` in both, which is what the `api-map` scorecard item is blocked on.
5. Real streaming, meaning a result read a chunk at a time as the executor produces it rather than a slice of a result that is already whole.
6. The small ones: an in-memory connection that does not make a file, `cursor()`, `fetchmany`, `fetchnumpy`, and a progress callback in Node.

Items 1 and 2 are what the live measurement asks for. Items 3 and 4 are what the scorecard is blocked on. Item 5 is what a user with more rows than memory needs, and today neither client has an answer for them.
