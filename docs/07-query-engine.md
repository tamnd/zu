# 07. Query Engine

## 1. Language: `zuQL` = Cypher-flavored, GQL-conformant

- Surface: openCypher-compatible core (MATCH/WHERE/RETURN/WITH/UNWIND/ CREATE/SET/DELETE/MERGE/OPTIONAL MATCH), plus GQL constructs: `FILTER`, `LET`, match modes (`REPEATABLE ELEMENTS` / `DIFFERENT EDGES` [default]), path modes (`WALK|TRAIL|SIMPLE|ACYCLIC`), selectors (`ALL|ANY|ANY SHORTEST|ALL SHORTEST|SHORTEST k`), `NEXT` statement chaining. Checklist = Neo4j's GQL-conformance appendix; goal: mandatory-GQL conformant by v1.0 (declared per standard §24.2, Ultipa precedent).
- Existence blocks: `EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 10 }` is a match written where a predicate goes, and `NOT` in front of it is the same match asked the other way round. The block sees the scope around it, which is what ties it to the row being tested, and the names it writes itself end with it, since it says whether a match was there and hands back nothing to read. It may be a whole conjunct of a WHERE and nowhere a value is wanted: a block under an OR is asking for a boolean, which is a mark join, and that is a separate operator.
- Edge patterns: the seven of ISO 18.9, `-[]->`, `<-[]-`, `<-[]->`, `~[]~`, `<~[]~`, `~[]~>` and `-[]-`, each with an abbreviated form that drops the bracket, so `->` is `-[]->` and `~` is `~[]~`. A dash asks for an edge that has a direction and a tilde for one that has none (GH02); whether it has one is a property of the rel table, so a pattern picks its tables by what it admits before it walks anything. An undirected edge is stored once and both adjacency indexes answer for it, which is why `~[]~` reads both lists and finds the edge from either end. `-[]-` admits either kind, either way round, which is what a query that does not care writes.
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
| Expand(CSR) | factorized neighbor expansion; direction fwd/bwd, resolved per rel table because an undirected one answers both ways (GH02); label/type mask; slack/tombstone aware; the pin is held per storage group rather than per source row, and when nothing above the expand reads the source level the neighbor lists concatenate across source rows so the pipeline below runs on full vectors |
| ASPJoin | default hash join: Accumulate → Semijoin-mask → Probe (sideways info passing pushes the mask into Expand/Scan below build side) |
| MultiwayIntersect (WCOJ) | galloping intersection over sorted CSR lists; injected for cyclic subpatterns only |
| Bracket | one group of operators run against each outer row, with the kind saying what an outer row the group found nothing for is worth: OPTIONAL MATCH keeps it with the group's slots bound null, `EXISTS` drops it and hands a hit up once however many matched, `NOT EXISTS` does the opposite. Filters written in the bracketed clause compile into the group, so they decide the match rather than the row |
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

## 9. Catalog statements

A catalog statement changes what a file declares rather than reading what it holds. There are six: `CREATE SCHEMA` and `DROP SCHEMA` (GC01, GC02), `CREATE GRAPH` and `DROP GRAPH` (GC04, GC05), and `CREATE GRAPH TYPE` and `DROP GRAPH TYPE` (GC03). The grammar is in `docs/grammar.ebnf`. They share the parser with queries and nothing after it: a catalog statement has no binding table, so it never reaches the binder or the planner, and it answers no columns, which the standard has a condition for (`00001 successful completion, omitted result`).

ISO writes an element type as the pattern an element of it matches, and almost every part of that pattern is optional. A statement lists them:

```
CREATE PROPERTY GRAPH TYPE IF NOT EXISTS social {
  NODE TYPE PersonType (:Person => :Employee
    {name :: STRING NOT NULL, nickname :: STRING}),
  (:Person)-[:KNOWS => :Close {since :: DATE}]->(:Company)
}
```

The labels before the `=>` are the key label set, the ones after it are the rest of the labels the element carries, and a pattern with no arrow declares no key at all, in which case the whole label set stands in for one (GG21, GG22). A name before the pattern is GG20, and it is what an endpoint elsewhere in the same graph type refers the type by: `(PersonType)-[:KNOWS]->(PersonType)` points at a type declared once, while `(:Person)-[:KNOWS]->(:Person)` declares it where it stands.

A property whose type admits null is one an element may leave out, so `NOT NULL` is what makes a property mandatory: one rule about null rather than two, and the value type grammar already had it.

A graph type written out in braces is closed, because a list nobody qualified is the whole list (GG02). Open graph types are spelled on the graph rather than on the type, which is `CREATE GRAPH g ANY` (GG01).

The catalog holds one name per element type, since a name is what an endpoint points at, and ISO's element types are anonymous. A pattern that carries no name gets one made of its labels, `KNOWS&Close` for the edge type above, with a number after it when that name is taken: GG24 is two node types over one label in one graph type, and the number is what keeps the second one from being the first one written twice. An endpoint written out is folded into a node type the graph type already has when it declares nothing of its own, so the three `(:Person)` patterns of a two edge type graph type name one node type between them.

One number in all of this is zu's rather than the standard's, impdef IL003: a written key label set names between 1 and 63 labels. Empty is a type nothing selects and 64 leaves no room in the 64 bit mask a label set is for the label the arrow adds, so the four conditions ISO reserves for the two ends of that range are raised here (`42012` to `42015`).

`CREATE GRAPH TYPE t LIKE g` is GG04: the closed type the tables of the graph named already describe, read off the catalog and not off the data, so it costs a catalog walk on a graph of any size. The tables stay the storage unit either way; a graph type describes them, it does not replace them (§03 §6).

### 9.1 Schemas and graphs

A schema is a directory the file holds and a graph lives in one. Both are names in the catalog and neither is a block on disk: `CREATE SCHEMA /app` adds a path, `CREATE GRAPH /app/social ANY` adds a graph in it, and a graph written as a bare name is a graph in the root schema. Every file has the root schema `/`, which is the one directory that is not one to drop, and every file starts with one graph in it called `home`. That home graph is what a file that was written before any of this said existed all along: a version 3 catalog reads as the root schema, the home graph, and every table in it, so an old file gains the vocabulary without gaining a rewrite.

Which graph a table belongs to is a field on the table rather than a list on the graph, so a table cannot be in two graphs and dropping a graph is a filter over the tables the catalog holds. A node table joins the graph a session is loading into and an edge table joins the graph of the table it comes from, which is what keeps an edge and its endpoints together.

A graph is created with the open type, with a graph type the file already holds (`CREATE GRAPH g :: social`), or with one written where the graph is created (`CREATE GRAPH g { (:Person {name :: STRING}) }`, which is GG03, and `CREATE GRAPH g LIKE h`, which is GG04 read at the graph). An inline type is kept on the graph and not added to the file's graph types, since nobody wrote a name for it.

`AS COPY OF` (GG05) says what the new graph starts with rather than what it is, so it is read after the type. A copy of an empty graph is a graph with no tables, which is exact. A copy of a graph that holds tables is refused today rather than approximated, because a props directory holds pointers inside block payloads and a copy that walked them wrong would be a copy that read as data.

`DROP GRAPH` is the one statement in zu that hands blocks back. It frees the props directory of every node table in the graph, the group directory of every edge table, the tombstone chain of every node table that has one, and the table index and stats chains it rewrites, then takes the graph and its tables out of the catalog. Nothing is published along the way: the checkpoint that stores the catalog makes the catalog, the table index and the stats visible together, so a drop that fails halfway leaves the file exactly as it was. Freed blocks become allocatable at the checkpoint after the one that published the free, and `block_count` is a high-water mark that never shrinks, so what a drop returns is measured in the free list and not in the size of the file. Dropping the home graph is allowed and is the reclamation path a file with one graph has; the next load puts an empty home graph back.

`DROP SCHEMA` is `RESTRICT` in the sense the standard gives it: a schema that still holds a graph is not one to drop, and the error says which graph is in the way.

`CREATE OR REPLACE` and `CREATE ... IF NOT EXISTS` are the two answers to a name that is taken, and a statement saying both says nothing, so it is refused. The replacement is built before the old type goes, which is why a replacement that cannot be kept leaves the type that was there.

Applying one is read, change, publish: the change happens on the catalog the call loaded and reaches the file through the same checkpoint every other write uses, so a statement that turns out to be impossible halfway through leaves the file exactly as it was, labels included. A session that runs one publishes a new epoch and refreshes itself, which drops the cached plans and readers that describe the catalog it just replaced.
