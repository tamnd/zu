# zu (図): Overview

> **zu** (図, *zu*), "diagram, figure, drawing." A graph you can hold.
> Repo: `tamnd/zu` · binary: `zu` · published crates: `zudb`, `zudb-*` (crate name `zu` is taken on crates.io)

**Status**: Specification v0.1 (2026-08-06). Ready-to-implement. **License**: Apache-2.0. **Language**: Rust (edition 2024, MSRV 1.97).

## 1. One-paragraph pitch

zu is an **embedded, in-process property-graph database** in the DuckDB/Kùzu mold: columnar, vectorized, factorized, single-writer/multi-reader MVCC, with one design decision no existing system makes: **storage is a first-class trait with three engines** sharing one query processor:

1. **`zu1`**, a native single-file, columnar, compressed binary format (cascading lightweight encodings + node-group CSR adjacency), the performance engine;
2. **`sqlite`**, the graph stored in an ordinary SQLite database file, the ubiquity/interop/durability engine;
3. **`s3`**, an object-storage-native engine (immutable segments + manifest CAS commit + NVMe/RAM cache) with **flat, predictable cost**, the PB-scale engine.

## 2. Why now (the 2026 gap)

- **Kùzu is dead upstream.** Apple acquired Kùzu Inc. Oct 2025; the repo was archived 2025-10-10. The embedded-graph niche fragmented into forks (LadybugDB, Ryu, bighorn) with a small maintainer pool. The acknowledged single-node performance leader has no canonical successor. (See `01-research.md` §1.)
- **No graph database is S3-native.** SlateDB, WarpStream, turbopuffer, Neon and LanceDB proved the zero-disk + conditional-write-CAS + hybrid-cache recipe; ByteDance's BG3 (SIGMOD'24) is the only published graph system on cheap cloud storage. There is no open-source "graph on S3 with fixed cost" engine. (§3.)
- **The format research landed.** Lance 2.1 structural encodings, BtrBlocks, FastLanes/ALP/FSST cascades, and Vortex demonstrate that lightweight compression now beats Parquet-class formats on *both* scan and random access, exactly the mix graph workloads need. (§2.)
- **GQL is real.** ISO/IEC 39075:2024 published; Neo4j Cypher 25, NebulaGraph v5, Spanner Graph, Fabric Graph all converge on it. A new engine can target a GQL-conformant Cypher dialect from day one instead of inventing syntax. (§4.)

## 3. Goals (measurable)

| # | Goal | Target |
|---|------|--------|
| T1 | Latency, hot 1-hop neighborhood (cached, ≤100 neighbors) | < 10 µs p50, < 100 µs p99 |
| T2 | Latency, LDBC SNB short reads (IS1–IS7 class), warm | < 1 ms p50 |
| T3 | Scan/decode throughput (int/float columns, in-memory) | > 2 GB/s/core decoded |
| T4 | Ingest (bulk COPY, zu1) | > 1 M edges/s/core |
| T5 | Compression vs raw CSV | ≥ 5× typical; adjacency ≤ 8 bits/edge on reordered social graphs |
| T6 | RAM floor | fully functional in 128 MiB budget; useful in 32 MiB |
| T7 | Binary size (`zu` CLI, release, stripped) | < 15 MiB default features |
| T8 | Cold start (open 10 GB zu1 file) | < 10 ms (no eager loads) |
| T9 | S3 engine cost, 1 TB graph, 100 QPS read-mostly | < $40/month total, bill flat ±10% month-over-month |
| T10 | S3 engine scale | 1 PB logical graph across partitioned manifests; stateless readers scale horizontally |
| T11 | Crash safety | power-cut safe at every instant, all engines; no fsck |
| T12 | Dependencies | core (`zudb-core` + `zu1`) builds with no C/C++ deps |

These were numbered G1–G12 until the GQL conformance milestones arrived, which are also numbered G0–G10 and tracked as issues. Two different G7s in one repository is the kind of thing that survives a long time and then costs somebody an afternoon, so the targets here are T for target and G now means a milestone everywhere.

## 4. Non-goals (v1)

- **Not a distributed transactional cluster.** Single logical writer per graph (per partition on S3). Read scale-out only. No Raft, no 2PC.
- **No Gremlin, no SPARQL/RDF.** Property graph + Cypher-flavored GQL only.
- **No multi-statement interactive server as the primary mode.** Embedded-first; a thin server (HTTP/Arrow IPC + MCP) is an optional feature, not the product.
- **No learned components** (no GNN cardinality estimators, no ML tuning). Deterministic, explainable behavior.
- **No triple-store semantics, no schema-less free-for-all.** Typed node/edge tables (with multi-label support, the #1 complaint against Kùzu's model).

## 5. Engine selection matrix

| | `zu1` (native file) | `sqlite` | `s3` |
|---|---|---|---|
| Deployment | single file on disk | single .db file | bucket + local cache dir |
| Best at | analytics + traversal speed | interop, small OLTP, ubiquity | PB scale, $/GB, serverless readers |
| Write model | single writer, MVCC readers | SQLite WAL semantics | single writer/partition, CAS-fenced |
| Cold read | NVMe page (~100 µs) | NVMe page | NVMe cache / S3 (3–300 ms) |
| Durability unit | WAL fsync | SQLite txn | batched PUT (flush interval) |
| Scale ceiling | ~10 TB / file practical | ~100 GB practical | ~PB (partitioned) |

One database = one engine at a time; `COPY TO / ATTACH` moves graphs between engines losslessly. (A delta-store hybrid, sqlite front, zu1 base, is noted in `12-implementation-plan.md` as post-v1.)

## 6. Document map

| File | Contents |
|---|---|
| `01-research.md` | State-of-the-art survey with citations (systems, formats, S3, query processing) |
| `02-architecture.md` | Layering, crate layout, StorageEngine trait, threading |
| `03-data-model.md` | Property graph model, IDs, schema, types |
| `04-storage-zu1-format.md` | Native single-file format, byte-level |
| `05-storage-sqlite.md` | SQLite engine mapping |
| `06-storage-s3.md` | S3-native engine, CAS protocol, caching, cost model |
| `07-query-engine.md` | Language, planner, vectorized/factorized execution |
| `08-transactions-mvcc.md` | MVCC, WAL, checkpointing, recovery |
| `09-memory-io-caching.md` | Buffer manager, I/O backends, resource budgets |
| `10-api-and-tooling.md` | Rust API, CLI, server, bindings |
| `11-benchmarks-and-targets.md` | Perf budgets, LDBC plan, CI regression gates |
| `12-implementation-plan.md` | Milestones M0–M6, dependency choices, risks |
| `gql-conformance.md` | Generated scoreboard: where zu and other engines stand on the ISO GQL corpus |
