# zu (図)

zu is an embedded, in-process property-graph database written in Rust.
The name is the Japanese word for diagram or figure, because a graph should be something you can hold: one file, one process, no cluster.

It is columnar, vectorized, and factorized in the DuckDB and Kùzu mold, with one design decision no existing system makes: storage is a first-class trait with three engines sharing one query processor.

- `zu1` is the native engine, a single columnar file with cascading lightweight compression and CSR adjacency, built for latency and scan speed.
- `sqlite` stores the graph in an ordinary SQLite database file, for interop and as the differential-testing oracle.
- `s3` is object-storage-native with immutable segments, compare-and-swap manifest commits, and a request accountant that keeps the monthly bill flat.

## Status

Early. The specification is complete and lives in [docs/](docs/), starting with the [overview](docs/00-overview.md).
Implementation is tracked by milestone issues: [M0](https://github.com/tamnd/zu/issues/1) encodings, [M1](https://github.com/tamnd/zu/issues/2) zu1 read/write, [M2](https://github.com/tamnd/zu/issues/3) query engine, [M3](https://github.com/tamnd/zu/issues/4) transactions and sqlite, [M4](https://github.com/tamnd/zu/issues/5) recursion and WCOJ, [M5](https://github.com/tamnd/zu/issues/6) s3 engine, [M6](https://github.com/tamnd/zu/issues/7) polish to v1.0.

Nothing is usable yet. If you need an embedded graph database today, look at the Kùzu forks.

## Goals

The full measurable list is in the [overview](docs/00-overview.md), but the shape of the project is:

- Hot 1-hop neighborhood reads under 10 µs, LDBC short reads under 1 ms warm.
- Bulk ingest above 1 M edges/s per core, adjacency at or under 8 bits/edge on reordered social graphs.
- Fully functional in a 128 MiB memory budget, CLI binary under 15 MiB, 10 GB file opens in under 10 ms.
- A 1 TB graph served from S3 at 100 QPS for under $40 a month, with a bill that stays flat when traffic spikes.
- Power-cut safe at every instant on every engine, with a crash-injection harness to prove it.

## Layout

```
crates/zu-common     ids, errors, shared constants
crates/zu-encoding   lightweight encodings (FastLanes, ALP, FSST, cascades)
crates/zu-storage    the GraphStore trait every engine implements
crates/zu-zu1        native single-file engine
crates/zu-sqlite     SQLite engine
crates/zu-s3         object-storage engine
crates/zu-query      parser, planner, factorized executor
crates/zu            the public embedded API (published as zudb)
crates/zu-cli        the zu binary
docs/                the specification, byte-level where it matters
```

The crate is published as `zudb` because `zu` is taken on crates.io; the repo, binary, and file extension stay `zu`.

## Building

```
make build   # cargo build --workspace --all-features
make test    # cargo test --workspace --all-features
make lint    # rustfmt check + clippy -D warnings
```

Requires Rust 1.97 (pinned in `rust-toolchain.toml`).

## License

Apache-2.0. See [LICENSE](LICENSE).
