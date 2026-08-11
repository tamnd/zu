# 07. Query Engine

## 1. Language: `zuQL` = Cypher-flavored, GQL-conformant

- Surface: openCypher-compatible core (MATCH/WHERE/RETURN/WITH/UNWIND/ CREATE/SET/DELETE/MERGE/OPTIONAL MATCH), plus GQL constructs: `FILTER`, `LET`, match modes (`REPEATABLE ELEMENTS` / `DIFFERENT EDGES` [default]), path modes (`WALK|TRAIL|SIMPLE|ACYCLIC`), selectors (`ALL|ANY|ANY SHORTEST|ALL SHORTEST|SHORTEST k`), `NEXT` statement chaining. Checklist = Neo4j's GQL-conformance appendix; goal: mandatory-GQL conformant by v1.0 (declared per standard §24.2, Ultipa precedent).
- Deliberate exclusions: no Gremlin, no LOAD CSV (use `COPY`), no dynamic labels in patterns (v1).
- Programmatic tier: prepared statements with typed params; Arrow RecordBatch results (`arrow` feature, zero-copy for fixed-width); `LOAD FROM 'x.parquet'/'x.csv'` table functions for ingestion/interop.
- Parser: hand-written recursive-descent (rationale: error quality, no build deps, fuzzable; grammar frozen in `docs/grammar.ebnf`).

## 2. Compilation pipeline

```
text → AST → bind (catalog, types) → logical plan (graph algebra)
     → rewrite (predicate pushdown, label pruning, pattern normalization)
     → join order (DP ≤ 12 rels, greedy beyond) + WCOJ injection
     → physical plan (pipelines) → morsel-parallel execution
```

Plans cached by (query text, catalog epoch, param types), LDBC short reads are parameterized point queries; plan cache is mandatory for G2 (< 1 ms includes zero planning on repeat).

## 3. Vectors & factorization (execution kernel)

- Vector size 2048; columnar `ValueVector`s with validity bitmaps and a selection vector; segments decode directly into vectors (or serve predicates compressed: zone-map skip, dict-code compare, constant short-circuit).
- **Factorized tuples** (Kùzu model): a `DataChunk` group = one flat prefix + unflat list vectors. Multi-hop m:n expansion emits `(src-flat, nbr-list)` without flattening; aggregations (`count`, `sum`, `collect`) and `DISTINCT` prefixes compute directly on factorized form; flatten happens lazily, only when an operator requires it (e.g., ORDER BY on leaf values).
- Morsel-driven scheduling: pipelines end in sinks (hash tables, sorts, results); morsels = node-group-aligned ranges (a morsel never spans groups, keeps segment decode, zone maps, and NUMA locality aligned).

## 4. Operators

| Operator | Notes |
|---|---|
| ScanNodeGroups | zone-map skip; predicate on compressed dict codes |
| IndexLookup | pk hash; drives point-query pipelines |
| Expand(CSR) | factorized neighbor expansion; direction fwd/bwd; label/type mask; slack/tombstone aware |
| ASPJoin | default hash join: Accumulate → Semijoin-mask → Probe (sideways info passing pushes the mask into Expand/Scan below build side) |
| MultiwayIntersect (WCOJ) | galloping intersection over sorted CSR lists; injected for cyclic subpatterns only |
| RecursiveBFS | fixpoint operator, hybrid source/frontier morsels (see §5) |
| ShortestPath | bidirectional BFS/Dijkstra (point-to-point); MS-BFS variant when ≥ 1024 sources |
| VectorTopK | HNSW scan as a MATCH seed (feature `vector`) |
| FtsScan | BM25 seed (feature `fts`) |
| HashAggregate / Sort / TopN / Limit / Distinct | factorization-aware |
| TableFunction | `pagerank()`, `wcc()`, `louvain()`, `sssp()` … return relations, composable with MATCH (GraphAlg direction; no Pregel API) |

**WCOJ policy** (Umbra/Free Join findings): binary ASPJoins by default; the optimizer marks a subplan for MultiwayIntersect iff the pattern is cyclic (triangle+) *or* estimated intermediate/output ratio exceeds a threshold (default 16×). Sorted CSR gives us the tries for free, no runtime trie build for pure-adjacency intersections (cheaper than Umbra's lazy hash tries).

## 5. Recursive & path queries

- `RecursiveBFS` implements variable-length and RPQ-style patterns as a frontier fixpoint: per-iteration dense/sparse frontier bitmaps over node groups; **hybrid morsel policy** (arXiv:2508.19379): few sources ⇒ frontier-partitioned morsels; many sources ⇒ multi-source morsels; switch at runtime by frontier statistics.
- Path semantics (GQL): default `DIFFERENT EDGES` (trail) with **shortest/ bounded-walk fast paths**; unbounded `WALK` requires a selector or explicit upper bound (avoids infinite path sets; standard-compliant). `SIMPLE/TRAIL` general cases use per-path filtering over PMR enumeration with documented exponential worst case (NP-hard territory; LMCS 2023).
- Path returns use **PMR (path multiset representation)**: predecessor DAG per BFS level; paths materialized lazily as `LIST<alternating NodeId/RelId>` only at RETURN (PathFinder design).
- Weighted `SHORTEST` (`COST` clause): bidirectional Dijkstra on CSR with binary heap; batched variant = Multi-Source Bellman-Ford.

## 6. Cardinality estimation (no ML)

1. Per-direction degree histograms (log2 buckets) per rel table, and the l1/l2/l3/linf norms of both degree sequences alongside them. A frontier that already holds one row per edge sits on nodes drawn in proportion to their degree, so the mean an expand off it sees is the degree-weighted mean and not the plain one; the mixed norm, the sum over nodes of out-degree times in-degree, is that mean's numerator when the two hops read opposite sides and makes a two hop count over one rel table exact.
2. Label-pair join selectivities from edge counts.
3. **Color summaries** (COLOR, PVLDB 2024): quasi-stable coloring, default ≤ 1024 colors, built by `ANALYZE`, stored in stats blocks; estimates multi-hop pattern cardinalities with graceful degradation under updates.
4. **Pessimistic caps** (AGM/degree-sequence bounds) clamp every estimate; join order falls back to the bound-optimal order when estimate and bound disagree by > 100× (robustness first).

## 7. Execution-time adaptivity (small, deterministic)

- ASPJoin sides swap if build side exceeds estimate by 8× (re-plan the pipeline locally, once).
- RecursiveBFS switches morsel policy per iteration (frontier stats).
- Expand chooses list-binary-search vs full-list-scan per morsel by mask density. No feedback loops persisted; EXPLAIN ANALYZE shows every decision.

## 8. DML execution

Writes compile to the same pipelines producing a `CommitBatch`: node inserts append to group deltas; rel inserts go to CSR slack (or delta overlay if slack exhausted; folded at checkpoint); `MERGE` = IndexLookup + conditional insert under the writer lock. Constraint checks (pk uniqueness, endpoint existence) execute inside the batch before commit (§08).
