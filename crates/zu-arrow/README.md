# zudb-arrow

Turns a zu query result into Arrow arrays, using the buffers the engine already filled.

For integers, floats, booleans, strings, dates, times, datetimes, and durations the conversion moves the column buffers instead of copying them, because the engine already lays them out the way Arrow does. Nodes, rels, paths, lists, and records are built by hand as Arrow structs and lists. The Python and JavaScript clients both use this crate, so every client converts a column the same way.

There are two optional features. `ffi` exports an Arrow C stream, which is what pyarrow, DuckDB, and other in-process readers take. `ipc` writes an Arrow IPC stream, which is what a JavaScript runtime needs.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-arrow` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_arrow`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
