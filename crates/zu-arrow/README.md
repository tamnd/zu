# zudb-arrow

Turns a zu query result into Arrow arrays. This is the fast way to get data out of zu: a result becomes one Arrow buffer per column instead of one object per row, and pandas, polars, DuckDB, and the JavaScript dataframe libraries can read it without copying it again.

## How it works

The engine does most of the work before this crate sees anything. On a plain projection the sink in `zudb-exec` fills column buffers, not rows, and they are already in the layout Arrow uses: values end to end, a validity bitmap that is left out when nothing is null, and strings as bytes plus offsets. `zudb-query` exposes them through `QueryResult::columnar` (borrowed) and `QueryResult::into_columns` (owned).

This crate puts an Arrow type and a bitmap around those buffers.

- `Table::of(&result, &tables)` borrows the result, so the buffers are copied once into arrays.
- `Table::taken(result, &tables)` consumes the result and moves the buffers in. For integers, floats, booleans, strings, dates, times, datetimes, and durations this costs a pointer, not a copy. If the sink built rows instead of columns, it quietly falls back to `of`.
- Nodes, rels, paths, lists, and records have no buffer to move. They are built by hand as Arrow structs and lists in the `values` module.
- A `Table` has a schema, one array per column, and a row count. `batches(n)` cuts it into `RecordBatch`es by slicing, which copies nothing. An empty result still gives one empty batch, so the reader learns the schema.

Node and rel values carry a table id. To turn that into a name, pass something that implements the `Tables` trait. `()` knows no names and is fine when the result has no node or rel columns.

A column has one type, which the engine decides. Two things are refused here because Arrow has no type for them: a time with a UTC offset, and a handle to a graph or a binding table. Errors come back as `Error::Type`, `Error::Value`, or `Error::Arrow`.

The code lives with the engine rather than in a client because every client should convert a column the same way. The Python and JavaScript clients both use it.

## Using it

```rust
use zu_arrow::{BATCH, Table};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = zudb::Database::memory()?;
    let mut conn = db.connect()?;
    conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")?;
    conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")?;

    let result = conn.query("MATCH (p:person) RETURN p.name AS name, p.uid AS uid")?;

    // `taken` consumes the result and moves its column buffers into
    // Arrow. `()` is the table-name lookup, which only node and rel
    // columns need.
    let table = Table::taken(result, &())?;
    println!("{} rows, schema {:?}", table.rows(), table.schema());
    for batch in table.batches(BATCH) {
        let batch = batch?;
        println!("{} columns, {} rows", batch.num_columns(), batch.num_rows());
    }
    Ok(())
}
```

That prints a schema with `name` as `Utf8` and `uid` as `Int64`, and one batch of two rows. `BATCH` is the default batch size, 65,536 rows.

## Features

Both features are off by default.

- `ffi` adds `Table::into_stream`, `stream`, and `stream_taken`, which return an `FFI_ArrowArrayStream`. That is the Arrow C Data Interface, the way to hand a result to pyarrow, DuckDB, Go, or the JVM in the same process. The buffers are shared with the reader and released when it is done.
- `ipc` adds `Table::into_ipc`, `ipc`, and `ipc_taken`, which return the bytes of an Arrow IPC stream. That is for runtimes that cannot read a C struct, which in practice means JavaScript.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-arrow` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_arrow`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
