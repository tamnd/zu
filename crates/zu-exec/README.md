# zudb-exec

A push-based, morsel-parallel pipeline executor for zu.

It runs the same logical plans as the executor in `zudb-query`, but the other way around. A compiler turns a plan into a pipeline of push operators, a scheduler splits the driving scan into morsels, and worker threads push each morsel through the whole pipeline into a thread-local sink. For any plan shape it does not handle yet it returns `Ok(None)` and the caller falls back to the old executor. Whatever it does handle has to give exactly the same answer as the old one, including row order and errors, and a parity test suite in `zudb` checks that.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-exec` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_exec`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
