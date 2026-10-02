# zudb

zu is an embedded, in-process property-graph database written in Rust. There is no server: you link the crate, point it at a file, and run graph queries in your own process. Storage is columnar and compressed, the executor is vectorized and factorized, and queries are written in zuQL, a dialect of ISO GQL.

This crate is the one to depend on. It is published as `zudb` because the name `zu` was already taken on crates.io. The repository, the `zu` binary, and the `.zu1` file extension keep the short name.

```
cargo add zudb
```

zu is early. The specification in [docs/](https://github.com/tamnd/zu/tree/main/docs) is complete and the engine behind it is not, so this is a crate for trying the API and not yet one for keeping data you care about. Expect breaking changes between 0.0.x releases.

## Quickstart

```rust
use zudb::{Database, params};

fn main() -> zudb::Result<()> {
    let db = Database::create("social.zu1")?;
    let mut conn = db.connect()?;

    conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")?;
    conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")?;

    let rows = conn.query_with(
        "MATCH (p:person) WHERE p.uid >= $uid RETURN p.name AS name, p.uid AS uid",
        &params! { "uid" => 1 },
    )?;
    for row in rows.iter() {
        let (name, uid): (&str, i64) = row.get()?;
        println!("{name} {uid}");
    }
    Ok(())
}
```

There is no schema step. The first `INSERT` that names a label creates its table, with the properties it was given as the columns. `create` makes a new file and fails if one is already there, so a program that wants the file it made last time calls `open`.

## Databases and connections

A `Database` is a path and a configuration, checked against a real file when you open it. It holds no file handle and no cache, so it is cheap to clone and safe to share between threads. A `Connection` is where the state lives: a file handle, decoded column caches, and a cache of compiled plans. Take one connection per thread with `db.connect()`, or `conn.duplicate()` to get another one off an existing connection, which is how a pool is written.

| Call | What it does |
| --- | --- |
| `Database::create(path)` | Makes a new empty `.zu1` file. Fails if the path exists. |
| `Database::open(path)` | Opens an existing file. Reads 12 KiB up front and pages the rest in lazily. |
| `Database::memory()` | A database that never touches the filesystem. Same code paths, gone when the last handle is dropped. |
| `Database::open_with(path, Config)` | Open with settings: `memory_limit`, `threads`, `read_only`, `stale_bound`. |

A connection runs one statement at a time. Every method takes `&mut self` and the type is `Send` but not `Sync`, so the compiler stops two threads from sharing one. Each statement reads a snapshot taken when it starts, and a commit made on another connection is visible to the next statement without reconnecting. Writes are serialized per file: one connection holds the write side for the length of a write statement or an explicit transaction, and the others wait their turn.

## Running statements

- `conn.query(text)` runs a statement and returns a `QueryResult` with `columns` and `rows`.
- `conn.query_with(text, &params!{ "name" => value })` binds parameters by name. The names have no `$`.
- `conn.execute(text)` runs a statement for its effect.
- `conn.prepare(text)` compiles once and returns an id and the parameter names, then `conn.execute_prepared(id, &params)` runs it as often as you like and `conn.close_prepared(id)` releases it.
- `conn.query_stream(text, &params, |batch| ...)` hands rows to a closure in batches instead of collecting them. Return `Flow::Stop` to end the scan early.
- `conn.explain(text)` prints the plan without running it, `conn.explain_plan(text)` returns it as a tree, and `conn.profile(text, &params)` runs it with per-operator counters.
- `conn.interrupt()` gives you a handle another thread can use to stop the running statement, which then fails with `ZuError::Interrupted`.

Rows are read as Rust types. `row.get::<(&str, i64)>()` reads the whole row as a tuple, `row.get_at::<i64>(0)` reads one column by position and `row.get_by_name::<&str>("name")` by name. Strings are borrowed from the result, so reading a `&str` does not copy. Asking for the wrong type is a data error (GQLSTATUS `22G03`). Asking for a column the result does not have is `ZuError::InvalidArgument`, because that is a bug in the program and not something in the data.

Transactions are statements. `START TRANSACTION`, then your writes, then `COMMIT` or `ROLLBACK`. `START TRANSACTION READ ONLY` is there too. A statement outside a transaction commits on its own.

```rust
use zudb::{Database, params};

fn main() -> zudb::Result<()> {
    let db = Database::memory()?;
    let mut conn = db.connect()?;

    conn.execute("START TRANSACTION")?;
    conn.execute("INSERT (a:person {uid: 1, name: 'ada'})-[:knows {since: 1843}]->(b:person {uid: 2, name: 'charles'})")?;
    conn.execute("COMMIT")?;

    conn.execute("START TRANSACTION")?;
    conn.execute("INSERT (p:person {uid: 3, name: 'nobody'})")?;
    conn.execute("ROLLBACK")?;

    let (stmt, names) = conn.prepare(
        "MATCH (a:person)-[k:knows]->(b:person) WHERE a.uid = $uid RETURN b.name AS name, k.since AS since",
    )?;
    assert_eq!(names, ["uid"]);
    let rows = conn.execute_prepared(stmt, &params! { "uid" => 1 })?;
    for row in rows.iter() {
        let name: &str = row.get_by_name("name")?;
        let since: i64 = row.get_by_name("since")?;
        println!("ada knows {name} since {since}");
    }
    conn.close_prepared(stmt);

    let people = conn.query("MATCH (p:person) RETURN count(p) AS n")?;
    let n: i64 = people.row(0)?.get_at(0)?;
    println!("{n} people");
    Ok(())
}
```

## Loading data

Writing a row at a time through `INSERT` pays for a commit per statement. For bulk loads, use the appender. It buffers rows column by column in memory, and a flush writes them as sealed compressed segments with one small record in the log, however many rows there are. A row is every column of the table in declaration order, given as a tuple.

```rust
use zudb::{Database, Flow};

fn main() -> zudb::Result<()> {
    let db = Database::memory()?;
    let mut conn = db.connect()?;
    conn.execute("INSERT (p:person {uid: 0, name: 'first'})")?;

    let mut app = conn.appender("person")?;
    for uid in 1..=10_000i64 {
        app.append_row((uid, "someone"))?;
    }
    let loaded = app.close()?;
    println!("appended {loaded}");

    let mut total = 0i64;
    conn.query_stream("MATCH (p:person) RETURN p.uid AS uid", &[], |batch| {
        for row in batch.iter() {
            total += row.get_at::<i64>(0)?;
        }
        Ok(Flow::More)
    })?;
    println!("sum {total}");

    let plan = conn.explain("MATCH (p:person) WHERE p.uid > 9000 RETURN p.name AS name")?;
    println!("{plan}");
    Ok(())
}
```

You can also query columns you already hold in memory without loading them. `conn.register(frame)` makes a `Frame` visible as a table on that connection only, and nothing is copied.

For files on disk, the `zu` command-line tool (crate `zudb-cli`) has `zu copy` for edge lists and labelled CSV datasets. With the `arrow` feature of this crate it also reads Parquet edge lists.

## Errors

Everything returns `zudb::Result<T>`, which is `Result<T, ZuError>`. Query errors are `ZuError::Gql` and carry a diagnostic record with the standard GQLSTATUS code, a severity, a message, and the position in the statement text where that applies. `err.gqlstatus()` and `err.diagnostic()` read them. The other variants are for things that are not about the query: `Io`, `Corrupt` (a checksum or structure check failed), `Unsupported`, `InvalidArgument`, `Conflict` (lost a race to another writer), and `Interrupted`. Warnings that did not stop a statement come back in `QueryResult::notices` next to the rows.

## Storage engines

Storage is a trait (`GraphStore`, from `zudb-storage`) with three engines planned behind it. What you get through `Database` today is the native one.

- `zu1` is a single `.zu1` file: columnar node groups, lightweight compressed segments, CSR adjacency for edges, a write-ahead log beside the file, and a dual header so a checkpoint is atomic. This is what `Database` opens and creates.
- `sqlite` maps the same graph onto an ordinary SQLite file. It is reachable through `zudb::sqlite::run` and the converters `zudb::convert::zu1_to_sqlite` and `sqlite_to_zu1`, and it is used as the reference engine the `zu1` results are tested against. It is not wired into `Database` yet.
- `s3` keeps the graph in object storage. Only the manifest format and the commit protocol exist so far, so there is nothing to open yet.

## How it is put together

This crate is the public API and the glue. The real work is split across the `zudb-*` crates, all versioned together:

| Crate | Role |
| --- | --- |
| `zudb-common` | ids, `ZuError`, GQLSTATUS codes, logical types |
| `zudb-encoding` | FastLanes, ALP, FSST and the cascades over them |
| `zudb-storage` | the `GraphStore` trait and shared segment types |
| `zudb-zu1` | the native file format, log, MVCC epochs, bulk ingest |
| `zudb-sqlite`, `zudb-s3` | the other two engines |
| `zudb-vector` | typed column vectors, selection vectors, expression kernels |
| `zudb-query` | lexer, parser, binder, optimizer, plans, the row executor |
| `zudb-exec` | the push-based, morsel-parallel pipeline executor |

A statement goes through them like this. The `Connection` owns a `Session`, which keeps the catalog, statistics and compiled plans resident. On a cache miss the text is parsed and bound against the catalog (`zudb-query`), then the optimizer picks join order and filter placement and produces a logical plan. The plan is cached by its text, so a warm statement is a hash lookup and a parameter bind. Execution goes to the pipeline executor in `zudb-exec` first. It splits the scan into morsels, pushes them through the operators on worker threads, and reads the file through a vectorized `Snapshot` that skips chunks using zone maps before decoding them. A plan shape it does not cover yet falls back to the older row executor in `zudb-query`, which must give exactly the same answer and is kept as a test oracle. Results are filled column by column and only turned into rows if you read rows. A write statement goes through the shared write side of the file (`write` and `shared` in this crate), which logs it, applies it to an in-memory overlay, and folds it into the file at checkpoint.

The module docs on [docs.rs](https://docs.rs/zudb) go into each piece, and the design documents in the repository go further.

## Features

- `arrow`: Parquet edge list loading in the `zu1` ingest path. Off by default. Arrow export of query results is a separate crate, `zudb-arrow`.

## License

Apache-2.0
