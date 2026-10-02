# zudb-corpus

The zu cross-client conformance corpus and its Rust runner.

zu has clients in several languages, and each one decodes values with its own code. The corpus checks that a value written through one client means the same thing when read through another. It is a set of YAML files, each one a statement and the rows it should return, with values written in a `{type, value}` form that survives every language without rounding. This crate holds the cases, a reader for the YAML subset they use, the bulk load data, the Arrow export checks (behind the default `arrow` feature), and the runner that checks them against this engine. The runners for the other clients live in [tamnd/zu-kit](https://github.com/tamnd/zu-kit).

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-corpus` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_corpus`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
