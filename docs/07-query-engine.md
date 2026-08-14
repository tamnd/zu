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

Plans cached by (query text, catalog epoch, param types), LDBC short reads are parameterized point queries; plan cache is mandatory for T2 (< 1 ms includes zero planning on repeat).

## 3. Vectors & factorization (execution kernel)

- Vector size 2048; columnar `ValueVector`s with validity bitmaps and a selection vector; segments decode directly into vectors (or serve predicates compressed: zone-map skip, dict-code compare, constant short-circuit).
- **Factorized tuples** (Kùzu model): a `DataChunk` group = one flat prefix + unflat list vectors. Multi-hop m:n expansion emits `(src-flat, nbr-list)` without flattening; aggregations (`count`, `sum`, `collect`) and `DISTINCT` prefixes compute directly on factorized form; flatten happens lazily, only when an operator requires it (e.g., ORDER BY on leaf values).
- Morsel-driven scheduling: pipelines end in sinks (hash tables, sorts, results); morsels = node-group-aligned ranges (a morsel never spans groups, keeps segment decode, zone maps, and NUMA locality aligned).

## 4. Operators

| Operator | Notes |
|---|---|
| ScanNodeGroups | zone-map skip; predicate on compressed dict codes |
| IndexLookup | pk hash; drives point-query pipelines |
| Expand(CSR) | factorized neighbor expansion; direction fwd/bwd; label/type mask; slack/tombstone aware; the pin is held per storage group rather than per source row, and when nothing above the expand reads the source level the neighbor lists concatenate across source rows so the pipeline below runs on full vectors |
| ASPJoin | default hash join: Accumulate → Semijoin-mask → Probe (sideways info passing pushes the mask into Expand/Scan below build side) |
| MultiwayIntersect (WCOJ) | galloping intersection over sorted CSR lists; injected for cyclic subpatterns only |
| RecursiveBFS | fixpoint operator, hybrid source/frontier morsels (see §5) |
| ShortestPath | bidirectional BFS/Dijkstra (point-to-point); MS-BFS variant when ≥ 1024 sources |
| VectorTopK | HNSW scan as a MATCH seed (feature `vector`) |
| FtsScan | BM25 seed (feature `fts`) |
| HashAggregate / Sort / TopN / Limit / Distinct | factorization-aware; an ORDER BY under a LIMIT of k runs as a bounded buffer per worker, which reads a row's sort keys, drops it against the k it already holds, and only materializes the row when it wins, so the ordered query builds k rows where the unordered one builds the whole fan |
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
3. **Color summaries** (COLOR, PVLDB 2024): quasi-stable coloring, default ≤ 1024 colors, built by `zu analyze`, stored in stats blocks; estimates multi-hop pattern cardinalities. Each summary is stamped with the epoch ANALYZE read and the edges the table held then. Writes land under a summary without moving it, so between builds the per-color counts scale by how far the table has grown and the coloring is trusted for the shape alone, which is the graceful degradation COLOR is picked for; past 8x drift either way it is a precise statement about a graph that no longer exists and the estimates fall back to the degree histograms. EXPLAIN prints a note naming the drift, and `zu stat` prints the stamp.
4. **Pessimistic caps** (AGM/degree-sequence bounds) clamp every estimate; join order falls back to the bound-optimal order when estimate and bound disagree by > 100x (robustness first). `ZU_BOUND_DISAGREEMENT` retunes the factor, and EXPLAIN prints a note naming the ratio whenever the fallback fires.
5. **Point seeds read their own degree at run time.** A `{id: $x}` lookup pins its slot to one node, and the step off that node reads one list, so a mean describes it only by accident: SF1 hands the short reads a person with 335 knows edges against a mean of 16.8. Join ordering cannot use this, because a plan is cached across parameter values and the answer differs between two of them; execution can, because by then the value is bound and the degree is an offsets subtraction away. The estimate walk therefore takes an optional reader, and where a point lookup pinned a slot the first step off it reports the count instead of estimating it. Nothing else consults the reader and the DP never gets one, so every plan is still chosen on statistics alone.

## 7. Execution-time adaptivity (small, deterministic)

The set is closed on purpose. An engine that may adapt anywhere is an engine whose plan does not predict its behaviour, and a slow query then has no explanation short of a profiler. Eight decisions, each named, each counted, each printed under the plan by EXPLAIN ANALYZE, is a budget: a new one has to displace an old one or argue its way in. Everything else about a query is settled before a row moves, and what is left here is only the choices that need a number the statistics do not have, because only the data itself has it.

1. **How the driving source is cut up.** The scheduler sizes the morsels off the rows the source actually reports and the workers it actually has, group-aligned so a morsel's CSR pins and zone reads stay inside one group. A seed's frontier is cut by weight rather than by position when the seed is a celebrity, since equal slices of a skewed neighbourhood are not equal work.
2. **Chunks the range pushdown empties before decoding.** The zone map answers off the chunk summary, so the payload bytes are never touched.
3. **Chunks that decode and then lose every row to the same range.** The summary said maybe and the values said no, which is the pushdown paying for itself halfway.
4. **A sideways filter switching itself off.** Each worker judges the filter it holds against the rows it drew, over a trial window, and stops asking once the filter is rejecting too little to pay for itself. Dropping it is always sound because the join behind it still has to match every row that comes through.
5. **A close that ends before it builds anything**, the far end of it having no edges at all.
6. **A bounded sink stopping a morsel** with the rows the limit asked for already in hand.
7. **Which worker takes which morsel.** Nothing hands them out in advance; a worker takes the next one when it has finished the last, so the spread between the busiest and the idlest worker is the only record of how evenly the work actually fell.
8. **Whether a neighbor list is read off a group decoded whole or out of the chunks that one list covers.** The walk knows how many of the group's lists it is about to want, storage knows how many chunks the group holds, and the pin starts paying at roughly a quarter as many lists as chunks. A scan morsel is far past that line and a point seed is far short of it, and the difference between the two on a seeded read is three orders of magnitude, since pinning decodes a group's two million edges to hand back one node's sixteen.

None of them persists. Nothing a run learns is written back into the statistics or carried into the next query, so two runs of the same query over the same data make the same decisions from the same evidence. What does move between runs is how the morsels landed on the workers, since decisions 4 and 7 are judged per worker off the rows that worker drew, and the rendering says which of the lines that applies to. The totals underneath are the same either way, because worker-local counts are added.

This list replaces three earlier sketches that did not survive contact with the engine. An ASPJoin side swap on an 8x estimate miss is not here because flipping the sides of a join is not a local re-plan: it was measured, and building the small side loses badly, since output rows come off a build side's payload list nearly free while every driven row costs a scan and a gather (see `crates/zu-exec/src/sip.rs`). A per-iteration morsel policy for RecursiveBFS waits on P4. Binary search against full list scan per morsel is a compile-time property of the hop, not something a morsel gets to choose, so it is a planning decision and belongs to section 4.

## 8. DML execution

Writes compile to the same pipelines producing a `CommitBatch`: node inserts append to group deltas; rel inserts go to CSR slack (or delta overlay if slack exhausted; folded at checkpoint); `MERGE` = IndexLookup + conditional insert under the writer lock. Constraint checks (pk uniqueness, endpoint existence) execute inside the batch before commit (§08).
