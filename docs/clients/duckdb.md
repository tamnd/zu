# DuckDB's Python and TypeScript clients, measured against ours

DuckDB is the only embedded analytical database whose clients are used more than its engine is talked about, and its Python client is the most used embedded analytics client there is. That makes it the bar for `zu-python` and `zu-node`, and it makes the interesting question a narrow one. Not what DuckDB has that we do not, which is a long list and most of it is SQL. What a user would feel.

This page is the answer, measured rather than asserted. Everything below was run on one machine on 2026-08-19: an Apple M-series laptop, DuckDB 1.5.5 for Python and `@duckdb/node-api` 1.5.5 for TypeScript, Python 3.14.6 and Node 24.18. A million rows of three columns, `INT64`, `DOUBLE` and a `VARCHAR` of about five bytes, in a stored table on both sides.

The Python half is `tools/versus_duckdb.py` in `zu-python`, which is committed so that anybody can rerun it rather than take this page's word. It alternates the two calls, ours then theirs then ours again, nine of each, and keeps each one's fastest; timing all of one and then all of the other hands whichever went second whatever the machine was doing by then, and that is worth two times on a laptop. Read the ratios rather than the milliseconds: the ratios hold across runs to within a tenth, and the absolute figures move with whatever else is on the machine.

One row of each table below is not a comparison and is marked as such. DuckDB's `execute` and `run` hand back a result nobody has read yet, so the milliseconds they take are the cost of planning and of nothing else; ours has the whole answer in memory by the time it returns. The pair is printed because the difference between them is the subject of this page, not because 0.5 against 66 is a ratio anybody should quote.

The same trap sits one row down. DuckDB's `arrow()` gives back a `RecordBatchReader` that has read nothing, so it times at a fraction of a millisecond and is not the same call; `to_arrow_table` is, and that is what is compared here. Every row that claims to have read a million rows checks that it read a million rows.

## 1. What the numbers say

Python, one million rows out:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| execute, nothing read | 66 ms | 0.5 ms | not a comparison |
| execute and `fetchall` | 246 ms | 279 ms | 1.1x faster |
| execute and Arrow table | 98 ms | 22 ms | 4.5x slower |
| execute and pandas | 98 ms | 72 ms | 1.4x slower |

Python, one row out, and one registration of a million-row Arrow table:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| point read, whole call | 14.6 us | 237.5 us | 16.3x faster |
| `register` and `unregister`, numbers | 14.0 us | 124.0 us | 8.8x faster |
| `register` and `unregister`, with strings | 511.0 us | 137.9 us | 3.7x slower |

TypeScript, one million rows out:

| call | zu | DuckDB | ratio |
|---|---|---|---|
| statement only, rows dropped | 858 ms | 3.3 ms | not a comparison |
| rows as objects | 3198 ms | 615 ms | 5.2x slower |
| rows as columns | not offered | 333 ms | |
| point read, whole call | 0.1 ms | 0.2 ms | 2x faster |

Three things fall out of that, and they are the whole page.

## 2. We win latency, by a lot, and it is not an accident

A point read is sixteen times faster in Python and twice as fast in TypeScript, and a registration of somebody else's Arrow table is nine times faster. Neither number is about the query engine. They are about what a call costs before and after the query, which is the cost that dominates every workload made of small questions, and small questions are what an embedded database is for.

The registration figure is the clearer of the two because there is no query in it at all. DuckDB's replacement scan copies the frame's description and builds a table function around it; ours takes the pointers, the widths and the meanings and hands them to the engine, and a statement that names the frame reads the caller's own arrays where they lie. `docs/10-api-and-tooling.md` section 4 has the mechanism. The measurement here is that the mechanism is worth what it was supposed to be worth.

The third row of that table is the exception, and it is printed rather than averaged away. A frame with a string column in it costs about half a millisecond where the same frame of numbers costs fourteen microseconds, and the whole difference is one pass over the bytes: `arrow-rs` validates a string buffer as UTF-8 when it imports it across the C Data Interface, and nine megabytes at memory speed is what that is worth. Nothing else in the path grew. It is a real cost a caller pays, it is on the way in rather than on the way out, and whether the validation can be skipped for a producer that has already done it is an open question rather than a decision.

Nothing in this page proposes giving up the numeric case.

## 3. We lose bulk export, and the cause is one decision

The Arrow table on both sides is the same shape and the same bytes. What differs is what each engine had to start from.

DuckDB never has rows. A result is a sequence of data chunks and a chunk is a set of column vectors, so exporting to Arrow is a per-column handoff of a buffer and a validity bitmap, and for flat numerics it copies nothing at all.

We have rows. `QueryResult` in `crates/zu-query/src/exec.rs` is `rows: Vec<Vec<Value>>`, and the sink in `crates/zu-exec/src/sink.rs` fills it a row at a time out of the vectors the executor was working in. Everything columnar then has to put that back the way it was.

The first version of this page measured that at twenty times DuckDB, and most of the twenty was the client rather than the engine: `zu-python`'s `columns.rs` walked the whole result once per column to settle a type, walked it again per column per batch to gather a `Vec<&Value>`, and built Arrow arrays by collecting iterators of `Option`s. Three passes over three million boxed values, strided every time, and a copy at the end of them.

That part is fixed. `zu::query::column` does the transpose once, in the engine, in two passes over the rows in row order, and hands back one owned buffer per column in the layout Arrow already uses. The client moves those buffers into Arrow arrays and copies nothing. Before and after, with the same script in one sitting on a quieter machine than section 1's, which is why the absolute figures are lower there than in that table and only the pair below should be compared:

| call | was | is |
|---|---|---|
| execute and Arrow table | 148 ms | 73 ms |
| execute and pandas | 148 ms | 79 ms |

The statement itself is 45 ms of each of those, so the export went from 103 ms to 26 ms. Of the 26 that are left, 22 are the transpose, which `cargo bench -p zu-query --bench columnar` times on its own. The Arrow half is about four milliseconds and there is not much left in it.

So the remaining gap is no longer a client problem. It is the sink, and it is one number: 22 ms to read a result whose rows the executor built out of vectors it then threw away. `record_batches` is fixed alongside it, having been worse rather than better than its name promised: it built every batch into a `Vec<RecordBatch>` before handing back a reader, and a batch is a slice of a finished column now.

The same trade explains the row path going the other way. `fetchall` is faster than DuckDB's, and for exactly the reason Arrow is slower: our rows are already rows. The first pass of this page put that at three times, on a measurement that did not alternate the two sides; alternated, it is a tenth, and a tenth is what should be believed.

So this is not a bug and it is not a missing optimisation. It is one decision, visible from both ends, and it is the decision this audit says to revisit.

## 4. The vector layer we already have

The thing that makes the fix tractable is that the hard half is built.

`crates/zu-vector` is a DuckDB-shaped vector layer and has been since perf/02. `PhysType` is a physical type per vector rather than a tag per value. `VecEncoding` is flat, constant or dictionary, with strings flowing as codes end to end. Validity is an optional bitmap, absent when everything is valid, which is Arrow's own convention and Arrow's own layout. `StrView` is a sixteen byte view holding a length, a four byte prefix, and either twelve inline bytes or a buffer id and an offset, which is byte for byte Arrow's `Utf8View`, a fact `zu_frame_col_view` already relies on for reading a frame in.

The executor uses all of it. `run.rs`, `join.rs`, `group.rs` and `sip.rs` work in `ValueVector` and `DataChunk` at `VECTOR_SIZE` a time. The collapse happens at exactly one place, the sink, and `docs/10-api-and-tooling.md` section 4 already says so out loud about the C ABI's chunked reads: they are "a slice of what it already materialized", and will stop being "once a chunk is what the executor produced".

That sentence is the work this page is asking for. A result that keeps the vectors it was built from makes Arrow export a handoff rather than a rebuild: for `Int64`, `Float64`, `Bool`, `Date`, `Timestamp` and strings, the buffer that goes into the Arrow array is the buffer the executor filled, released through Arrow's own release callback when the last reader is done with it. Zero copies, not fewer copies.

The row path keeps working because a row is a gather across vectors at a known offset, which is what `fetchall` would do instead of what it does now. Whether it stays as fast is the thing to measure rather than assume, and it is the reason this is staged behind a benchmark rather than declared.

## 5. TypeScript was a milestone behind, and is most of the way back

The Python client is a fast client with one slow path. The TypeScript client was a smaller client, and the list below is what it was missing when this page was first written.

What it had: `query`, `exec`, `cursor`, `stream` with async iteration and Web Streams, `AbortSignal` wired to the interrupt, `await using`, the four temporal value classes with `toTemporal()`, `bigint` by default with a documented opt out, and the full error model with codes, positions and doc URLs. That was a good client and the parts of it that existed were idiomatic.

What it had not, all of which `zu-python` had:

- no transactions, so a program could not make several statements one, which `transaction()` now does
- no appender, so there was no fast way to add rows to a table that exists, which `appender()` now does
- no `register`, so the zero-copy frame path, the fastest thing this engine does across a boundary, was not reachable from Node at all, which `register` now is
- no bulk load, so a program could not build a database from Node, which `load` now does
- no Arrow, no columnar read of any kind, of which `columnar` is the second half: the same buffers `zu::query::column` fills, handed over as typed arrays, with Arrow itself still owed
- no `rowsRead` and no progress callback, which is the one row of the list still open

The columnar read was the one to look at first alongside the numbers. `duck runAndReadAll getColumns` is 333 ms where the same rows as objects are 615 ms, and our own object path is 3198 ms. It was the single largest thing missing, and it was also the one that a columnar result made cheap to add.

A program could not build a database from Node at all. `connect` created the file, `CREATE NODE TABLE` answers `42001 not implemented`, and there was no loader and no appender on the surface, so the only way in was an `INSERT` per row: a first-run experience of an empty file and no way to fill it. `load` and `appender()` are the answer to that, and `prepare`, `explain` and `profile` are the answer to the client scorecard's `api-map` item, which the ledger puts at tier 1 and which neither client named when this was written.

## 6. The surface, item by item

DuckDB's Python connection has 71 public members and its module 181. Ours has 14 and 22, plus the DB-API, asyncio and notebook layers. Most of that difference is SQL surface we correctly do not have, such as `read_parquet` and the extension loader. What follows is only the part where a user would notice.

| DuckDB | zu-python | zu-node | verdict |
|---|---|---|---|
| `connect()` with no path, in memory | `connect()`, and `':memory:'` means the same | `connect()`, and `':memory:'` means the same | done |
| `sql` / `execute` / module-level default connection | `execute`, `sql` | `query`, `exec` | done, minus the module-level default |
| `fetchall`, `fetchone`, `fetchmany` | `fetchall`, `fetchone`, `fetchmany` | rows are an array | done, and a block is 3.1x a row at a time |
| `arrow()`, `fetch_arrow_table` | `to_arrow`, off the engine's own buffers | `columnar`, the same buffers as typed arrays | Arrow itself owed for Node, 4.5x for Python |
| `fetch_record_batch` streaming | `record_batches`, zero-copy slices of a materialized result | `stream`, as batches of rows | owed, real streaming |
| `df()`, `pl()`, `fetchnumpy()`, `torch()`, `tf()` | `to_pandas`, `to_polars` | none | numpy owed, torch and tf are not our lane |
| `register` / `unregister` | `register` / `unregister`, faster | `register` / `unregister` | done |
| `append(table, df)` | `appender()` | `appender()`, and `load` for a whole database | done |
| `begin` / `commit` / `rollback` | `transaction()` | `transaction()` | done |
| `interrupt()`, `query_progress()` | `interrupt()`, `rows_read` | `AbortSignal` only | progress owed for Node |
| `cursor()` / `duplicate()` | `cursor()`, and `duplicate()` under the name that says what it does | `duplicate()`, since `cursor()` is a cursor over rows here | done |
| prepared statements | `prepare()` | `prepare()` | done |
| `explain`, profiling | `explain()`, `profile()` | `explain()`, `profile()` | done |
| `create_function` UDFs, Arrow-vectorised | none | none | not this milestone |
| relational API, 111 members | none | none | deliberately not |
| Spark API | none | none | deliberately not |
| DB-API 2.0 | `zudb.dbapi` | n/a | done |
| asyncio | `zudb.aio` | native | done |
| notebook rendering | `_repr_html_`, `%%gql` | n/a | done |

The relational API is the one row worth arguing about, and the answer is no. It is 111 methods that build a query without running it, and it exists because DuckDB's users want to compose SQL from Python. A graph query language composes in the language, and a hundred methods that shadow it would be a second dialect to document, test and keep in step for as long as the client lives.

## 7. What this becomes

In order, largest effect first.

1. A columnar result, in two halves. The first is a result read down its columns at all, in the engine rather than in each client, which is `zu::query::column` and is done: one buffer per column in Arrow's own layout, filled in two passes over the rows instead of two per column, so the transpose happens once and correctly. The second is the executor keeping the vectors it already computes in, so that `Vec<Vec<Value>>` becomes something the row path gathers rather than something every other path unpicks and the transpose stops happening at all. The first half is what a client can use today; the second is the one that touches the engine, and it changes no client, because the type it fills is already the one they read.
2. Arrow export as a handoff, which is done for Python. `to_arrow`, `__arrow_c_stream__` and `record_batches` are a description of the buffers `zu::query::column` filled and a release callback, with no copy for the physical types whose layout is already Arrow's, and a batch is a slice cut when the reader asks for it. The export alone went from 103 ms to 26 ms on a million rows, and 22 of the 26 are the transpose that item 1's second half removes. The same handoff is owed for Node, where it is item 3.
3. The TypeScript client reaching the Python one: transactions, appender, register, bulk load, and a columnar read of a result. Done, all five. Arrow itself over the C Data Interface is what is left of it, and it is item 2's other half.
4. Prepared statements, `explain` and `profile` in both, which is what the `api-map` scorecard item is blocked on. Done in both.
5. Real streaming, meaning a result read a chunk at a time as the executor produces it rather than a slice of a result that is already whole.
6. The small ones: an in-memory connection that does not make a file and a second connection made from the first are both done in both clients, and both are the engine's own answer rather than a special case in either one. What is left of this row is `fetchnumpy` and a progress callback in Node.

Item 1's second half is the only thing left that the live measurement asks for, and it is 22 of the 26 milliseconds. Item 5 is what a user with more rows than memory needs, and today neither client has an answer for them.
