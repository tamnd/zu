# zudb-query

The zu query engine: parser, binder, planner, optimizer, and the factorized executor.

It has a hand-written parser for zuQL (zu's GQL dialect) covering MATCH, WHERE, UNWIND, WITH, and RETURN, a binder, a logical plan with EXPLAIN output, dynamic programming join ordering and filter placement, CSR adjacency, the table function kernels, and a factorized executor. The language and the operators are specified in `docs/07-query-engine.md` and the grammar is in `docs/grammar.ebnf`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-query` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_query`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
