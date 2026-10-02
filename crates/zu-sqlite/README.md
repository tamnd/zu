# zudb-sqlite

The `sqlite` storage engine for zu.

It maps the graph onto an ordinary SQLite database file. That is useful for interop, since any SQLite tool can open the file, and it also serves as the reference engine that `zu1` is tested against. It covers the catalog, node and rel tables with their adjacency indexes, inserts, updates, deletes, typed property reads, neighbor queries, and transactions. SQLite is bundled through `rusqlite`, so there is nothing to install. The mapping is specified in `docs/05-storage-sqlite.md`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-sqlite` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_sqlite`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
