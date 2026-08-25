# zu (図)

zu is an embedded, in-process property-graph database written in Rust.
The name is the Japanese word for diagram or figure, because a graph should be something you can hold: one file, one process, no cluster.

It is columnar, vectorized, and factorized in the DuckDB and Kùzu mold, with one design decision no existing system makes: storage is a first-class trait with three engines sharing one query processor.

- `zu1` is the native engine, a single columnar file with cascading lightweight compression and CSR adjacency, built for latency and scan speed.
- `sqlite` stores the graph in an ordinary SQLite database file, for interop and as the differential-testing oracle.
- `s3` is object-storage-native with immutable segments, compare-and-swap manifest commits, and a request accountant that keeps the monthly bill flat.

## Sixty seconds

```rust
use zudb::{Database, params};

fn main() -> zudb::Result<()> {
    let db = Database::create("social.zu1")?;
    let mut conn = db.connect()?;

    conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")?;
    conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")?;

    let rows = conn.query_with(
        "MATCH (p:person) WHERE p.uid >= $uid RETURN p.name AS name, p.uid AS uid",
        &params! { "uid" => 1 },
    )?;
    for row in rows.iter() {
        let (name, uid): (&str, i64) = row.get()?;
        println!("{name} {uid}");
    }
    Ok(())
}
```

That is `cargo add zudb` and the whole program: no server, no schema step, no cluster. `create` makes the file and `open` is what you use the second time, because a create that found a database and opened it instead is the call that quietly writes into somebody else's data. The same sixty seconds in Python, `import zudb`, `zudb.connect`, `.to_pandas()`, is in [zu-python](https://github.com/tamnd/zu-python).

The snippet above is a program in this repository, `crates/zu-snippets/examples/sixty-seconds.rs`, and a test holds this README to it character for character and then runs it. A quickstart is the most read and least compiled code a project has, which is how it comes to be wrong.

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
crates/zu-snippets   the snippets this README prints, compiled and run
docs/                the specification, byte-level where it matters
```

The crate is published as `zudb` because `zu` is taken on crates.io; the repo, binary, and file extension stay `zu`.

## Clients

This repository holds the engine, the Rust SDK, the CLI, the C ABI and its generated `zu.h`, and the conformance corpus. Everything that compiles against the frozen C ABI instead of against the engine's internals lives in its own repository, which is ADR 0005 and the reason the list below is not a directory listing.

| Repository | What it is | Tier |
|---|---|---|
| [zu-c](https://github.com/tamnd/zu-c) | C and C++ developer kit: examples, the header-only C++ wrapper, CMake, vcpkg, Conan, the sanitizer suites. `zu.h` itself is generated here, in this repository | 1 |
| [zu-python](https://github.com/tamnd/zu-python) | `zudb` on PyPI. PyO3, three wheels per platform | 1 |
| [zu-node](https://github.com/tamnd/zu-node) | `zudb` on npm. napi-rs, plus the WASM build, for Node, Bun, Deno, and the browser | 1 |
| [zu-go](https://github.com/tamnd/zu-go) | `github.com/tamnd/zu-go`. cgo, with a `purego` path | 1 |
| [zu-java](https://github.com/tamnd/zu-java) | `dev.zudb` on Maven Central. Panama, with a JNI fallback | 1 |
| [zu-dotnet](https://github.com/tamnd/zu-dotnet) | `ZuDb` on NuGet. Source-generated P/Invoke, NativeAOT-clean | 2 |
| [zu-kotlin](https://github.com/tamnd/zu-kotlin) | `dev.zudb:zu-kotlin` on Maven Central. Kotlin/JVM over the Panama layer, coroutines and `Flow` | 2 |
| [zu-scala](https://github.com/tamnd/zu-scala) | `dev.zudb::zu-scala` on Maven Central. Scala 3 and 2.13, with Cats Effect and ZIO modules kept apart | 2 |
| [zu-swift](https://github.com/tamnd/zu-swift) | Swift Package Manager. The C ABI through the clang importer, `AsyncSequence` over rows | 2 |
| [zu-dart](https://github.com/tamnd/zu-dart) | `zudb` on pub.dev. `dart:ffi`, with the declarations generated from `zu.h` | 2 |
| [zu-kit](https://github.com/tamnd/zu-kit) | The binding kit: generated FFI declarations, corpus runners, a reference binding, the scorecard tool | 3 |
| [zu-web](https://github.com/tamnd/zu-web) | The documentation site. Two thirds of it is generated from this repository's release artifacts | |

A tier is a promise, so who answers for each client and what its tier asks of it are published rather than implied: [docs/clients/overview.md](docs/clients/overview.md) is the maintainer, the repository and the scorecard of every one of them, rendered from `clients.toml` and held to the table above.

If a bug reproduces through the `zu` CLI it belongs here, whichever client you found it through. Every client repository's bug template asks that first, because engine bugs filed in client trackers are the standard way a multi-repository project loses track of them.

## Installing

```
curl -fsSL https://raw.githubusercontent.com/tamnd/zu/main/install.sh | sh     # macOS, Linux
irm https://raw.githubusercontent.com/tamnd/zu/main/install.ps1 | iex          # Windows
brew install tamnd/tap/zu
scoop install zu
docker run --rm -v "$PWD:/data" ghcr.io/tamnd/zu stat graph.zu1
```

Each of these lands the same thing: the release archive for your platform, unpacked as an install prefix, so `bin/zu` arrives with `include/zu.h`, both library forms, the pkg-config file and the CMake package config beside it. Every one of them fetches the release's `SHA256SUMS` first and refuses to unpack an archive that is not what it says it is.

There is no release yet, so none of these fetch anything today. They are here, tested and held to the platform table, because the install path is the first thing a user runs and the last thing anybody wants to be writing on release day.

## Building

```
make build   # cargo build --workspace --all-features
make test    # cargo test --workspace --all-features
make lint    # rustfmt check + clippy -D warnings
```

Requires Rust 1.98 (pinned in `rust-toolchain.toml`).

## License

Apache-2.0. See [LICENSE](LICENSE).
