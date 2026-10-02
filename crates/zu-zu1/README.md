# zudb-zu1

The `zu1` storage engine: zu's native single-file columnar format.

A `.zu1` file is one file on disk holding the whole graph, with columnar node groups, compressed segments, and CSR adjacency for edges. This crate owns the byte-level format: the headers, block I/O, the dual-header checkpoint flip that makes a commit atomic, and the meta-block chains behind every root pointer. The format is specified in `docs/04-storage-zu1-format.md`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-zu1` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_zu1`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
