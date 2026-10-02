# zudb-s3

The `s3` storage engine for zu: a graph that lives in object storage.

The design is immutable segment packs, manifest commits through a conditional PUT, epoch fencing so two writers cannot both win, batched WAL objects, and a cache in front of it all with a request counter that keeps the monthly bill predictable. What is implemented today is the manifest format and the compare-and-swap commit protocol with fencing. WAL batching, segment packs, and the cache come next. The protocol is specified in `docs/06-storage-s3.md`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-s3` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_s3`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
