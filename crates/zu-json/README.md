# zudb-json

A small JSON reader and writer used by the zu CLI and the zu codegen tools.

The CLI has a size budget and only reads simple one-line JSON objects, so a few hundred lines here are cheaper than a JSON dependency. The same code reads rustdoc output for `cargo xtask model`. Object fields keep their insertion order and the writer never reorders anything, so the same input always produces the same bytes. If you need JSON in your own project, use `serde_json`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-json` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_json`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
