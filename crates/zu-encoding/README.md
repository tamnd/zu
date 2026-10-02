# zudb-encoding

Lightweight columnar encodings for zu.

FastLanes bit packing, ALP for floats, FSST for strings, and the cascades that combine them. Every encoding has a stable id that is written into the file, so the id table here is part of the on-disk format (see `docs/04-storage-zu1-format.md`). The target is to decode at 1 GB/s per core or better with at most 64 KiB of scratch memory.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-encoding` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_encoding`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
