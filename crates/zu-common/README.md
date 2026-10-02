# zudb-common

Shared ids, errors, and constants used by every zu crate.

This is the bottom of the dependency graph. It holds the node, rel, and table identifiers whose bit layout is part of the file format (described in `docs/03-data-model.md`), the error type the other crates return, and a few constants that more than one crate needs to agree on. It has no dependencies on the rest of zu.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-common` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_common`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
