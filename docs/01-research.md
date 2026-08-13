# 01. Research: State of the Art (as of 2026-08)

Findings from a four-track survey (systems, storage formats, S3-native design, query processing). Every design decision in later documents cites back here.

## 1. Systems landscape

### 1.1 Kùzu: the reference design, now orphaned
- Kùzu (CIDR 2023, Jin et al., https://www.cidrdb.org/cidr2023/papers/p48-jin.pdf): embedded, columnar node properties, disk-based **CSR adjacency (fwd+bwd)**, vectorized (2048-tuple) push-based execution, **factorized** intermediates, ASP-Join (accumulate–semijoin–probe), morsel-driven parallelism, HyPer-style MVCC (v0.5.0+), single write txn at a time, redo-only page WAL, single-file format from v0.11 (July 2025). Node-group storage redesign: fixed ~128 K-node groups, per-group per-column compression, CSR-per-group with slack gaps (github.com/kuzudb/kuzu/issues/1474, #2529).
- Independent numbers (prrao87/kuzudb-study): ~18× faster ingest than Neo4j, OLAP queries up to 188× faster, biggest wins on multi-hop m:n paths.
- **Apple acquired Kùzu Inc. Oct 9 2025; repo archived Oct 10 2025** (disclosed publicly Feb 2026 via EU DMA filing). Forks: **LadybugDB** (main successor, adds multi-label nodes, "graph lakehouse" roadmap), Ryu, bighorn, Vela fork. Community assessment: "~six people understand the codebase"; format was in flux at archive time. Sources: theregister.com/2025/10/14/kuzudb_abandoned/, szarnyasg.org/posts/kuzu-forks/, thedataquarry.com/blog/from-kuzu-to-ladybug/.
- **Takeaways for zu**: (a) the Kùzu architecture (columnar + node-group CSR + factorized vectorized execution) is validated and is our baseline blueprint; (b) its most-cited modeling gap, one node table per label, no multi-label, must be fixed; (c) single-sponsor OSS risk is real; plain-MIT, boring-format, documented-bytes matters.

### 1.2 Other systems (deltas that matter)
- **DuckPGQ** (CWI; CIDR'23 p66-wolde.pdf): SQL/PGQ over stock DuckDB; builds **in-memory CSR per query**; MS-BFS path-finding (512 searches/AVX-512 word). VLDB 2025 (p4465-chakraborty.pdf) showed its pair-batched MS-BFS "very slow, does not parallelize well", MS-BFS only pays with thousands of sources.
- **Neo4j**: block format storage (8 KB blocks, node+props+hot rels co-located); parallel runtime for read-only queries; Infinigraph (Sept 2025) splits topology shard from property shards for 100 TB+; Cypher 25 converging to GQL.
- **FalkorDB**: GraphBLAS sparse-matrix engine; **rewritten in Rust 2025–26** (80 K LoC, columnar batch execution, MVCC), proof Rust is viable for a production graph engine.
- **Memgraph**: in-memory skip-lists + Delta-MVCC; parallel runtime added 3.8.
- **NebulaGraph v5**: first distributed **GQL** implementation; vectorized pipelined runtime; enterprise-only v5 line.
- **Rust natives**: HelixDB (LMDB → LSM-on-object-storage for cloud, runs on SlateDB), SurrealDB (own SurrealKV LSM, v3.0 Feb 2026), CozoDB (abandoned), IndraDB (dormant), GrafeoDB (new, GQL-native, unproven). None combine columnar+CSR+factorization; the performance lane is open.
- **Umbra/CedarDB**: compiled queries + WCOJ + recursive CTEs show a relational engine can serve graph workloads; CedarDB added FSST (Jan 2026). Umbra is our buffer-manager and MVCC reference (vmcache, virtual versions).
- **GraphScope Flex**: LDBC SNB Interactive audited records (130 K ops/s SF100; SF1000 = 2.9 B nodes/208 B edges @ 127.8 K ops/s). Distributed; not our lane, but sets the audited-throughput bar. Huawei GES took the SF300 record Dec 2025 (139.4 K ops/s).
- **Market**: GraphRAG is the demand driver (Gartner: 50% of AI inquiries touch graph); MCP servers and native vector+FTS are 2026 table stakes; consolidation (Apple←Kùzu, Istari←Dgraph, Samsung←RDFox).

## 2. Storage formats & compression

### 2.1 What replaced "Parquet + zstd"
- Zeng et al., VLDB 2023 (p148-zeng.pdf): block compression atop lightweight encodings is often *detrimental* end-to-end; favor decode speed; keep zone maps fine-grained.
- **BtrBlocks** (SIGMOD 2023, TUM): cascading scheme pool (RLE, dict, frequency, SIMD-FastPFOR/BP128, FSST, Pseudodecimal, Roaring nulls), depth ≤ 3, chosen by ~1% sampling (picks optimal 77% of time at 1.2% compression-time cost); decode ~200 GB/s in-memory class, 2.6–3.8× faster than Parquet variants; S3 scans 2.2× faster / 1.8× cheaper than Parquet.
- **FastLanes** (PVLDB 2023/2025): 1024-value unified transposed layout →
  >100 B ints/s decode with *scalar* code (auto-vectorizes any SIMD width);
  file format v0.1 decodes avg 43× faster than Parquet+snappy.
- **ALP** (SIGMOD 2024): doubles→decimal-int + FFOR; 31–64× faster decode than zstd/Chimp, beats zstd ratio on decimal data. Adopted by DuckDB, Kùzu, Vortex.
- **FSST** (PVLDB 2020): 255-symbol table, per-string random-access decode at LZ4+ speed; the string workhorse (DuckDB DICT_FSST, BtrBlocks, Lance, Vortex).
- **Lance 2.1** (arXiv:2504.15247): *structural* encoding is the key to NVMe random access, **mini-block** (narrow types, ~24–41 B metadata/chunk) vs **full-zip** (wide values, one contiguous read per row range); no row groups; stats live *outside* the file. Achieves tuned-Parquet random access (~100× better than default) without sacrificing scans.
- **Vortex** (LF incubation 2025): compute-on-compressed, zero-copy on-disk = in-memory, ~1.5 ms random access vs ~200 ms Parquet-default; DuckDB core extension Jan 2026 (TPC-H SF100 geomean 1.51 s vs 1.84 s Parquet V2).
- **zu conclusion**: adopt the consensus cascade (dict / FOR+bitpack in FastLanes layout / delta / RLE / constant / ALP+ALP_RD / FSST(+dict) / bool-bitpack / Roaring-style null bitmaps), sampled selection, depth ≤ 3, **no mandatory general-purpose compression** (optional zstd per segment for cold strings), Lance-style structural split for random access.

### 2.2 Graph-specific storage
- Kùzu node-group CSR with slack gaps = the proven updatable-CSR-on-disk design.
- Dynamic in-memory structures (GFE line): **Sortledton** (VLDB 2022), sorted neighborhood blocks, analytics within 1.22× of static CSR at 2.1× CSR memory; insight: *sequential neighborhood access* matters, not vertex-array purity. Teseo, LiveGraph, RadixGraph (arXiv:2601.01444, 2026 SOTA claim).
- **Adjacency compression** (WebGraph/BV, LLP, arXiv:1011.5425): web graphs 1.2–3.8 bits/link; social graphs 5–11 bits/link; **dense-ID relabeling (BFS/ degree ordering) is cheap and yields ~25%+ compression + traversal locality**; LLP gains another ~25% but costs hours at billions of edges. webgraph-rs (Vigna) is production Rust prior art. k²-trees: 2–5 bits/edge but µs-scale neighbor access, archival tier only.
- **zu conclusion**: CSR per node group = two integer columns (offsets bitpacked, neighbors delta+bitpacked); optional `REORDER` (BFS/degree) at bulk load; expect 4–8 bits/edge on reordered social graphs (T5).

### 2.3 Single-file design & buffer management
- **DuckDB file format** (best modern reference): 4 KB header ×3 with two alternating DatabaseHeaders → atomic root flip; fixed 256 KB blocks; meta-block chains; row groups 122,880; per-segment codec by analyze pass; WAL sidecar replayed on startup; **bulk loads bypass WAL** via optimistic new-block writes. MVCC optimized for few-large-writers/many-readers (duckdb.org/2024/10/30/analytics-optimized-concurrent-transactions).
- **SQLite**: WAL = single writer/many readers; BEGIN CONCURRENT + WAL2 still experimental branches in 2026 (no merge planned). B-tree row storage is the wrong shape for scans, hence engines like DuckDB/Kùzu exist.
- **CoW B-tree school** (LMDB/libmdbx/redb): elegant, but write-amplifies whole root-to-leaf paths per commit and falls apart under write load (vmcache paper TPC-C); redb is the healthy pure-Rust member.
- **mmap considered harmful** (Crotty/Pavlo CIDR 2022): TLB shootdowns, no write control. **vmcache** (Leis SIGMOD 2023): reserve virtual range, DBMS controls fault/evict explicitly; supports variable pages and arbitrary page graphs; ≈ LeanStore performance without swizzling's constraints; exmap kernel module only needed at extreme I/O rates. LeanStore-NVMe (VLDB 2024) + ZLeanStore (PVLDB 2026): out-of-place writes cut SSD write-amp 6–10×.
- **io_uring in Rust 2026**: `io-uring` crate healthy; tokio-uring stalled; glommio/monoio/compio active. Conclusion: don't marry a runtime; trait-based I/O backend, psync+O_DIRECT default, io_uring feature-gated.
- **zu conclusion**: DuckDB-style dual headers + fixed 256 KiB blocks + sidecar WAL + checkpoint-into-columns; vmcache-style explicit buffer manager; cache *compressed* segments (decode is cheap; RAM stretches by the compression ratio).

## 3. S3-native design

### 3.1 The proven recipe (SlateDB / WarpStream / turbopuffer / Neon / Quickwit)
- **SlateDB** (Rust, ~1.0, Apache-2.0; users: Dropbox, HelixDB): LSM entirely on object storage; single writer fenced via **manifest CAS (PUT If-Match)**, formally verified; WAL = batched SST-objects (flush_interval amortizes $5/M PUTs); compaction cost = requests, not bytes (in-region bandwidth free); foyer block cache; checkpoints are zero-copy clones.
- **WarpStream**: one PUT per agent per 250 ms regardless of partition count → request cost decoupled from throughput; >80% of Kafka TCO was cross-AZ ($0.05/GB), structurally eliminated; zone-aligned "distributed mmap" read cache dedupes GETs; S3EOZ tier: p99 produce 169 ms, +15% TCO for 3× latency.
- **turbopuffer**: object storage as source of truth; WAL group-commit via CAS; chose SPFresh (centroid) over HNSW/DiskANN *because graph-shaped indexes need many S3 round trips*, a warning directly relevant to graph adjacency; warm p50 14–16 ms, cold p50 ~500–900 ms; ~$70/TB/mo with ~50% NVMe cache vs $600–3600/TB/mo replicated-disk designs. 2.5 T vectors in production.
- **Quickwit**: immutable splits + **hotcache footer** (<0.1% of split) → open a 15 GB split in <60 ms with one ranged GET. The one-round-trip-open pattern.
- **Neon**: CoW layer files; **generation-number fencing** for split-brain.
- **DuckLake v1.0** (Apr 2026): metadata in a SQL DB, data in Parquet on S3; **data inlining** solves the small-write tax. Iceberg/Delta: commit = conditional PUT of next log/manifest (delta-rs defaults to native CAS since 0.23; DynamoDB lock now legacy).

### 3.2 Primitives & 2026 economics (us-east-1)
- S3 Standard: $0.023/GB-mo; PUT $5/M; GET $0.40/M; in-region bandwidth free. Latency: GET p50 ~25–50 ms, p99 85–300 ms; 3.5 K PUT + 5.5 K GET /s/prefix.
- **S3 Express One Zone** (post Apr 2025 cuts): $0.11/GB-mo; PUT $1.13/M; GET $0.03/M (+$0.0006/GB retrieval); p50 ~3 ms, p99 ~15 ms; single-AZ.
- **Conditional writes**: PutObject If-None-Match (Aug 2024), If-Match ETag CAS (Nov 2024, incl. multipart complete), free, enables leaderless commit. GCS generation-match and Azure ETag/leases are equivalents; R2 has them and zero egress ($0.015/GB-mo, Class A $4.50/M, Class B $0.36/M).
- Cost anatomy: naive S3-as-KV at 10 K ops/s ≈ $70 K/mo (SlateDB blog), the entire game is **batch PUTs, large objects, cache-bounded GETs, request-aware compaction**. Fixed-cost pricing (WarpStream, turbopuffer) is possible only because marginal costs are made flat first.
- **BG3** (ByteDance, SIGMOD 2024, DOI 10.1145/3626246.3653373): the only published graph DB on cloud object storage, Bw-tree RAM indices over append-only cloud pages, workload-aware GC, leader-follower over shared storage. Validates "hot topology in RAM/NVMe, cold pages on object storage" at TikTok scale. **No open-source equivalent exists → zu's opening.**

## 4. Query processing

- **Language**: GQL (ISO/IEC 39075:2024) + SQL/PGQ (SQL:2023) share the GPML pattern sublanguage. Neo4j Cypher 25 tracks GQL conformance; openCypher repositioned as migration path; Postgres SQL/PGQ patch targets PG19 (~2026). Verdict for a new engine: **Cypher-flavored GQL-conformant dialect**, GQL match/path modes first-class; skip Gremlin; ship an Arrow/dataframe API.
- **Execution blueprint** (validated by Kùzu/DuckDB/Umbra): vectorized push-based, morsel-driven; **factorized vectors** (flat/unflat) compress m:n multi-hop intermediates by orders of magnitude; **ASP-Join** as the default join (sideways info passing built in); **optimizer-injected hash-trie WCOJ** only for cyclic subplans (Umbra PVLDB 2020: up to 100× on cyclic; Free Join SIGMOD 2023 unifies binary+WCOJ; 2025 unified architecture arXiv:2505.19918 adds sort-based variants, avg 1.4× over Free Join).
- **Recursion**: dedicated fixpoint BFS operator with **hybrid source/frontier morsel scheduling** (Chakraborty & Salihoğlu, VLDB 2025, arXiv:2508.19379, robust across cardinalities; beats DuckPGQ MS-BFS); bidirectional search for point-to-point (instance-optimal, arXiv:2410.14638); **PMR** compact path representations for path-returning queries (PathFinder, arXiv:2306.02194); trail/simple modes are NP-hard in general → fast paths for shortest/bounded-walk, restricted fallbacks otherwise (LMCS 2023 trichotomy).
- **Cardinality**: COLOR quasi-stable coloring summaries (PVLDB 2024, up to 10³× better accuracy), degree statistics, **pessimistic upper bounds** as guardrails (arXiv:2412.00642); FaSTest showed GNN estimators lose to sampling by up to 10³×, **no learned estimators**.
- **Analytics**: algorithms as table functions compiled into the pipeline (GraphAlg/AvantGraph, arXiv:2601.06705), not a bolted-on Pregel API. Offer: PageRank, WCC/SCC, label propagation/Louvain, SSSP, betweenness.
- **Vector/FTS**: table stakes in 2026 (every surviving engine ships them). v1: HNSW over node properties + BM25 FTS, vector-top-k as a MATCH seed operator. On S3 engine, respect the turbopuffer lesson: HNSW only over cached/NVMe data, never traversed against cold S3.
- **Benchmarks**: LDBC SNB Interactive (short reads must be ~ms; audited bar is GraphScope/Huawei ~130 K ops/s distributed), SNB BI (columnar scans + bulk path-finding), Graphalytics (kernels). "SoK: Faults in our Graph Benchmarks" (arXiv:2404.00766): publish only reproducible, spec-compliant numbers.

## 5. Synthesis → zu design pillars

1. **Kùzu-blueprint core, de-risked**: columnar node groups + slack CSR + factorized vectorized execution, in Rust, with multi-label fixed, bytes documented, MIT.
2. **Format = 2024-26 encoding stack**: sampled cascades, FastLanes-layout bitpacking, ALP, FSST, structural mini-block/full-zip split; compute-on- compressed; cache compressed.
3. **Storage as a trait**: same query engine over zu1 / sqlite / s3. No other graph DB does this; DuckDB proved the catalog/storage split works.
4. **S3 engine = SlateDB recipe applied to graphs**: immutable segment objects, manifest CAS, epoch fencing, batched WAL, foyer RAM+NVMe cache, hotcache footer, request-aware compaction; hot adjacency never leaves cache (BG3 pattern).
5. **Low-resource discipline**: vmcache-style explicit memory, strict budgets, no mandatory runtime, no C deps in core, decode-instead-of-cache.
