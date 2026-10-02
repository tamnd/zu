# zudb-vector

Typed columnar vectors, selection vectors, and expression kernels for zu.

The executor used to move rows around as a vector of tagged values per cell. This crate replaces that with one contiguous buffer per column, validity as a bitmap, filters as selection vectors, strings as 16-byte views into shared buffers, and dictionary columns that stay as integer codes the whole way through. Memory comes from a per-morsel bump arena, so the steady-state operator paths do not allocate.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-vector` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_vector`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
