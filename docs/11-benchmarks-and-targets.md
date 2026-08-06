# 11. Benchmarks, Targets, Verification

Discipline rule (SoK arXiv:2404.00766): publish only reproducible, spec-compliant numbers; every public claim has a `zu bench` command that reproduces it. No "up to N×" marketing without the harness.

## 1. Performance budgets (CI-gated once implemented)

| ID | Micro-benchmark | Target | Gate |
|---|---|---|---|
| B1 | 1-hop expand, warm, deg ≤ 100 (G1) | < 10 µs p50 | regress > 10% fails CI |
| B2 | pk point lookup, warm | < 2 µs |〃 |
| B3 | int64 column scan+sum, 100 M rows | > 2 GB/s/core decoded (G3) | 〃 |
| B4 | 2-hop factorized count, LDBC SF1 | < 10 ms | 〃 |
| B5 | triangle count (WCOJ), Graph500-22 | competitive with Umbra published class | tracked |
| B6 | COPY 100 M edges (G4) | > 1 M edges/s/core | 〃 |
| B7 | open 10 GB file (G8) | < 10 ms | 〃 |
| B8 | adjacency size, reordered LiveJournal | ≤ 8 bits/edge (G5) | 〃 |
| B9 | s3 cold 3-hop (Standard, frontier prefetch) | < 500 ms | tracked |
| B10 | s3 monthly bill, G9 scenario replay | < $40 ±10% | simulated from request log |

## 2. Macro benchmarks

- **LDBC SNB Interactive v2** SF1–SF100: short reads (IS*) p50 < 1 ms warm (G2); complex reads competitive with published Kùzu-class results; audited submission is a post-1.0 goal, spec-compliant unaudited runs before that.
- **LDBC SNB BI** SF100: exercises columnar scans + bulk path-finding.
- **LDBC Graphalytics** (BFS, PageRank, WCC, SSSP) on Graph500 + real graphs (LiveJournal, Twitter-2010, Friendster): table-function kernels.
- **Comparison set**: LadybugDB (Kùzu lineage), DuckPGQ, Neo4j (block format), FalkorDB Rust, Memgraph, same machine, published harness repo (`tamnd/zu-bench`), engine versions pinned.

## 3. Cost benchmarks (s3 engine, first-class, nobody else does this)

`zu bench s3cost` replays workload traces against the request accountant (unit-priced S3/R2/GCS tables checked into the repo with a date) and emits $/month + request histograms. Scenarios: G9 (1 TB/100 QPS), write-heavy agent memory (10 K writes/s batched), cold analytics (BI over 10 TB), PB partitioned. Assertions on *bill shape* (flat under 10× QPS spike with warm cache), not just totals.

## 4. Correctness verification

- **Differential testing**: same query corpus (openCypher TCK subset + generated patterns via grammar fuzzing) on zu1 vs sqlite engines, results must match modulo ordering. FalkorDB's 1,585-TCK-scenario bar is the reference for dialect coverage.
- **Crash injection**: deterministic fault harness truncates/reorders physical writes at every syscall boundary during commit/checkpoint (zu1) and drops/ duplicates PUTs (s3, against MinIO + LocalStack CAS semantics); invariant: pre- or post-state only (§08 §7).
- **Fuzzing**: cargo-fuzz targets, parser, every decoder, WAL replay, manifest, header. OSS-Fuzz application at 0.5.
- **Concurrency**: loom tests for buffer-manager state machine + snapshot epoch accounting; miri on encoding crates.
- **S3 CAS conformance suite**: verifies fencing protocol against real S3, GCS, Azure, R2, MinIO (nightly; the protocol's portability is a claim we test).
