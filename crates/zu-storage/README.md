# zudb-storage

The storage engine trait that every zu engine implements, plus the segment types they share.

zu has three storage engines (the native `zu1` file, SQLite, and S3) and one query processor. This crate is the contract between them: the trait the query layer reads through, and the types an engine hands back. The full surface is described in `docs/02-architecture.md`. Parts of it are still skeletons that get filled in as each engine lands.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-storage` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_storage`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
