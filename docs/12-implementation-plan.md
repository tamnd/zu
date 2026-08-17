# 12. Implementation Plan

## 0. Dependency decisions (final)

| Need | Choice | Rationale |
|---|---|---|
| Encodings | **write our own** in `zu-encoding` (FastLanes layout, ALP, FSST ports) | core IP; Vortex crates exist but API-unstable + dep-heavy; papers + reference impls (cwida/ALP, cwida/fsst, BtrBlocks) are the spec |
| SQLite | `rusqlite` (bundled) | de-facto standard |
| Object storage | `object_store` | Arrow-ecosystem, S3/GCS/Azure/R2, supports conditional puts; opendal bridge later if exotic backends demanded |
| Hybrid cache | `foyer` | proven (RisingWave/SlateDB/Chroma), S3-FIFO, inflight dedup |
| Hashing | `foldhash`/`ahash`-class | vector hash joins |
| CRC | `crc32c` | hardware accelerated |
| Parallelism | own morsel scheduler on `std::thread` + crossbeam deques | no rayon/tokio in query path |
| Arrow | `arrow` behind feature | interop only, not internal representation |
| Parser | hand-written | error quality, fuzzability, zero deps |
| zstd | `zstd` (feature) + `ruzstd` read-fallback | T12 pure-Rust default |

## 1. Milestones

### M0, Skeleton + encodings (foundation)
Workspace, CI (fmt/clippy/miri/fuzz scaffolding, 32 MiB job), `zu-common`, `zu-encoding` complete with all §04 encodings, sampled cascade selector, criterion benches hitting B3-class decode targets. **Exit: encodings fuzzed, ≥ 1 GB/s/core each.**

### M1, zu1 read/write + bulk load
File headers, blocks, meta chains, node groups, segments, CSR build (bulk), `COPY FROM parquet/csv` (arrow reader behind feature), REORDER, pk index, `zu stat`/`zu verify`. No transactions yet (single-shot build + read). **Exit: B6 (1 M edges/s/core), B7, B8 met on LiveJournal.**

### M2, Query engine v0
Parser (MATCH/WHERE/RETURN/WITH/UNWIND core), binder, DP join order with degree stats, pipelines: Scan/IndexLookup/Expand/ASPJoin/Aggregate/Sort/Limit, factorized vectors, morsel scheduler. **Exit: B1/B2/B4; LDBC IS+IC subset running SF1; EXPLAIN ANALYZE.**

### M3, Transactions + MVCC + sqlite engine
WAL, single-writer commit, overlays, checkpoint, recovery, crash-injection harness; sqlite engine complete; differential testing online. **Exit: §08 invariants under fault harness; TCK-subset parity zu1↔sqlite.**

### M4, Recursion + WCOJ + algorithms
RecursiveBFS (hybrid morsels), path modes/selectors, PMR path returns, bidirectional shortest, MultiwayIntersect + optimizer injection, COLOR summaries + pessimistic bounds, table functions (pagerank, wcc, sssp, cdlp, lcc, louvain). **Exit: B5; Graphalytics kernels; SNB IC13/14-class path queries.**

### M5, s3 engine
Manifest/CAS/fencing, WAL objects + durability modes, segment packs + footers, foyer integration, frontier prefetch, checkpoint/GC, request accountant, CAS conformance suite, partitions. **Exit: B9/B10; T9 scenario demonstrated on real S3 + R2; chaos suite green.**

### M6, Polish → v1.0
GQL conformance declaration, vector (HNSW) + FTS features, Python binding, server + MCP, docs deliverables (§10 §6), benchmark publication + harness repo, OSS-Fuzz, format freeze (`min_reader_version` policy).

Ordering rationale: risk fronted, encodings and CSR performance (M0/M1) validate the thesis before the long tail; s3 (M5) reuses zu1 bytes so it lands late but cheap; sqlite (M3) arrives exactly when differential testing pays.

## 2. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Scope (3 engines) | shared segment bytes + `GraphStore` trait keep engines thin; sqlite ≈ 2 KLoC, s3 core ≈ manifest+flusher over object_store |
| Updatable CSR complexity (Kùzu's hardest part) | slack + group-local rebuild only; delta overlay fallback; property-graph workloads are read-heavy, checkpoint folds cover the rest |
| Factorization correctness | differential testing vs flat execution mode (debug flag forces flatten) |
| S3 API drift / provider quirks | conformance suite against 5 providers, nightly |
| Cypher dialect breadth | TCK subset scoreboard public from M2; conformance appendix as the checklist |
| Solo-maintainer bus factor (the Kùzu lesson) | byte-accurate format docs from day one; boring formats; independent-reader test in CI (a second minimal reader impl) |

## 3. Repo conventions

`tamnd/zu`: Apache-2.0; `docs/` holds these specs; ADRs in `docs/adr/` with their index at `docs/adr/README.md`; conventional commits; `cargo xtask` for codegen (encoding tables) and bench orchestration; no unsafe without `// SAFETY:` + fuzz coverage; public benchmark harness in `tamnd/graph-bench`.

This repository is one of nine. The boundary between them is the C ABI: code compiling against the engine's internals stays here, code compiling against the frozen ABI gets its own repository. The rule and its consequences are ADR 0005, the map is `docs/adr/0005-the-repository-split.md` and the `Clients` section of the README, and the full topology with per-repository layout is `Spec/2064g/dx/18-repository-topology.md`.
