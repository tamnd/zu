# The ADBC driver

Every client in the table beside this page is a repository, a release, a scorecard and a person who answers for it. This one is a crate. `crates/zu-adbc` builds a shared object that any ADBC driver manager opens by name, and a program in Python, R, Java, Go or C++ that has a driver manager already has a zu client, without a package of ours in its dependency list and without anybody writing one.

That is worth a page because it is the cheapest surface this engine has. ADBC is the one database interface whose result is Arrow rather than rows, and a zu result has been columnar since the sink stopped flattening, so the driver is a vocabulary and not a translation. What it hands back is the buffers `zu-arrow` builds for the Python and TypeScript clients, cut into batches, and no row is built anywhere on the path.

## 1. Running it

The driver is a `cdylib`, `libzu_adbc.dylib` on macOS, `libzu_adbc.so` on Linux and `zu_adbc.dll` on Windows, and its entrypoint is `ZuDriverInit`. It also exports `AdbcDriverInit`, which is what a manager looks for when nobody tells it otherwise.

```python
import adbc_driver_manager.dbapi as dbapi

conn = dbapi.connect(
    driver="target/debug/libzu_adbc.dylib",
    entrypoint="ZuDriverInit",
    db_kwargs={"uri": "graph.zu"},
)
with conn.cursor() as cur:
    cur.execute("UNWIND [1, 2, 3] AS n RETURN n, n * 2 AS twice")
    table = cur.fetch_arrow_table()
```

That program is not a sketch. It was run against this driver on 2026-08-19 with `adbc_driver_manager` 1.12.0 and pyarrow 25.0.1 on Python 3.14.6, and it prints a two column Arrow table of three rows. `fetchall`, `cursor.description`, `fetch_arrow_table` and `adbc_get_table_types` all answer, which means the DB-API a Python program already knows is reaching this engine through a driver manager and a C ABI and nothing else.

A path that is not there is created, which is what every zu client does and what a caller pointing a tool at a new file means. Read-only is the exception, because nothing creates a database it may not write to.

## 2. The options it takes

On the database, before init:

| Key | What |
|---|---|
| `uri` | `zu:`, `file:` or a bare path. `:memory:` for one that never touches the filesystem |
| `path` | the same, spelled as a path, for a caller with no URI to give |
| `zu.read_only` | `true` to open without the write side |
| `zu.threads` | how many threads a statement may use |
| `zu.memory_limit` | bytes |

The three spellings of a path reach the same file. An option nobody has heard of is refused and the refusal names the ones that exist, because a caller who typed `zu.readonly` needs to be told which key it was rather than left with a database that quietly ignored them. Every option is read before the file is opened and none can be set after, since `zu.read_only` decides whether a missing path is created and may arrive after the URI does.

On the connection, `adbc.connection.autocommit` and `adbc.connection.readonly`, both as ADBC spells them. Autocommit is a flag and three words: turning it off runs `START TRANSACTION`, a commit or a rollback runs the word and then starts the next one, and turning it back on commits what is open. On the statement, `zu.rows_per_batch`, which defaults to `zu_arrow::BATCH` and refuses anything under one, because a reader that hands back empty batches never ends.

## 3. What answers and what refuses

Executing a statement, in a transaction or out of one, and reading the result: that is the whole of what a caller does and it is all here. `get_info` answers with the vendor name, the versions, and the two facts a tool switches on, which are that this engine does not speak SQL and does not take Substrait. `get_table_types` answers `node` and `rel`, which are the two kinds of table a property graph has.

`get_objects` and `get_table_schema` refuse, and they refuse for one reason worth stating rather than burying. Both have to name a table's property columns, and the columns exist in the catalog with a `LogicalType` each, but nothing in this tree maps a `LogicalType` to an Arrow `DataType` yet. A driver that guessed would be wrong in a GUI's schema tree, which is exactly where those two calls are read, so v0 says so instead. Writing that mapping belongs in `zu-arrow`, it is what both calls need, and it is the first thing v1 owes.

Bulk ingest, bound parameters, Substrait plans and partitioned reads refuse as well, each saying which one of them it was. Partitions are the only one that will keep refusing: reading a result in partitions is for a database spread over several machines, and an embedded one is a file in this process.

`execute_schema` answers, and the way it does is worth knowing about. Learning a statement's columns means running it, and running an insert to find out what it returns would insert twice, so the run happens inside `START TRANSACTION` and `ROLLBACK` and is unmade. Inside a caller's own transaction it refuses, because there is no savepoint to roll back to and rolling back the caller's work to answer a question about types would be worse than the question going unanswered.

## 4. What a failure looks like from the other side

GQLSTATUS is five characters and ADBC's `sqlstate` is five characters, which is the piece of luck this driver is built on. The condition code goes across as itself, the ADBC status comes off its class, and everything ADBC has no field for goes into the detail keys that 1.1.0 added:

| Key | What |
|---|---|
| `zu.gqlstatus` | the condition, again, for a caller reading details rather than SQLSTATE |
| `zu.doc_url` | the page that explains it |
| `zu.retryable` | whether trying again could work |
| `zu.line`, `zu.column` | where in the statement, when the condition has a position |
| `zu.excerpt` | the text around it |

From Python, a syntax error arrives as `ProgrammingError`, `sqlstate` reads `42002`, and `err.details` holds `zu.doc_url` pointing at `https://zu.dev/docs/errors/42002`. A caller three languages away from this repository still gets the page to read, which is the whole point of the condition model and the part of it a binding usually drops.

## 5. How it is built and what keeps it honest

The forty function pointers of `AdbcDriver` are laid out by Apache's `adbc_ffi`, out of the ADBC project's own tree, and this crate implements the four safe traits behind them. That division is deliberate: a field written into the wrong slot of that struct is a jump through a wrong pointer, which is a segfault in the caller's process rather than a failed test here, and it is a layout somebody else already maintains.

The tests come in two halves. Twenty-eight unit tests call the Rust traits, which is the near side. Eight in `tests/c_abi.rs` go the whole way: they build the `cdylib`, open it by path through `adbc_driver_manager`, and check that a result, a result of several batches, the table types, a transaction opened and closed, a file created and reopened read-only, the driver's own name, a refusal carrying its doc URL and a refusal that is a sentence rather than a crash all survive the crossing. The second half is the one that matters, because every caller this crate is for arrives that way and none of them link it.

One thing about that suite is worth writing down for whoever changes it next. `cargo test` compiles the library as an rlib to link the test binary against and stops there, since nothing it knows about consumes a `cdylib`, so the shared object is often missing on a clean tree. The suite builds it rather than skipping, because a driver nobody can open is a driver nobody can use and that has to fail loudly.

## 6. What is left

A `LogicalType` to Arrow `DataType` mapping, which unblocks `get_objects` and `get_table_schema` and is the whole of what a schema browser needs. Bound parameters, which is the same bind path the clients use and which ADBC spells as an Arrow batch. Real streaming, so that a result arrives a chunk at a time as the executor produces it rather than as slices of a result that is already whole, which is the item the clients page has open too and the one a caller with more rows than memory needs. Ingest, last, because a table has to be creatable by a statement first.
