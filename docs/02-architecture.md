# 02. Architecture

## 1. Layering

```
┌─────────────────────────────────────────────────────────────┐
│  APIs: Rust lib · CLI (zu) · optional server (HTTP/Arrow)   │
├─────────────────────────────────────────────────────────────┤
│  Query front end: Cypher/GQL parser → binder → logical plan │
├─────────────────────────────────────────────────────────────┤
│  Optimizer: rewrite rules · join order · WCOJ injection ·   │
│  cardinality (degree stats + color summaries + pess. bounds)│
├─────────────────────────────────────────────────────────────┤
│  Execution: vectorized push-based pipelines · morsel        │
│  scheduler · factorized vectors · recursive operators       │
├─────────────────────────────────────────────────────────────┤
│  Storage abstraction: GraphStore trait                      │
│  (node groups · CSR adjacency · segments · WAL · snapshot)  │
├───────────────────┬──────────────────┬──────────────────────┤
│  zu1 engine       │  sqlite engine   │  s3 engine           │
│  single file      │  rusqlite        │  manifest + objects  │
├───────────────────┴──────────────────┴──────────────────────┤
│  I/O + memory: buffer manager (vmcache-style) · IoBackend   │
│  (psync / io_uring) · ObjectStore · foyer hybrid cache      │
└─────────────────────────────────────────────────────────────┘
```

Everything above the `GraphStore` trait is engine-agnostic. Engines differ in *where bytes live and how commits become durable*, not in query semantics.

## 2. Crate layout (workspace `tamnd/zu`)

| Crate | Publishes as | Contents | Hard deps |
|---|---|---|---|
| `zu-common` | `zudb-common` | types, ids, errors, config, arena/allocs | none |
| `zu-encoding` | `zudb-encoding` | cascades: bitpack (FastLanes layout), delta, FOR, RLE, dict, constant, ALP/ALP_RD, FSST, null bitmaps; sampling selector | none |
| `zu-storage` | `zudb-storage` | `GraphStore` trait, node-group model, segment cache, buffer manager, WAL abstraction | `zu-encoding` |
| `zu-zu1` | `zudb-zu1` | native file engine (format in `04-…md`) | `zu-storage` |
| `zu-sqlite` | `zudb-sqlite` | SQLite engine | `rusqlite` (bundled) |
| `zu-s3` | `zudb-s3` | object engine, manifest/CAS, compactor | `object_store`, `foyer` |
| `zu-query` | `zudb-query` | parser, binder, optimizer, physical operators | `zu-storage` |
| `zu-core` | `zudb` | `Database`, `Connection`, txn mgmt; re-exports | all above (engines feature-gated) |
| `zu-cli` | bin `zu` | shell, COPY, EXPLAIN, bench, MCP server | `zu-core` |
| `zu-server` | `zudb-server` (opt) | HTTP + Arrow IPC endpoint | `zu-core` |

Feature flags on `zudb`: `zu1` (default), `sqlite`, `s3`, `io-uring`, `vector` (HNSW), `fts`, `server`, `arrow` (zero-copy Arrow interop). T12: `zudb` default features build with zero C/C++ (rusqlite only in `sqlite` feature, bundled; zstd optional behind `zstd` feature using `ruzstd` decode fallback).

## 3. The `GraphStore` trait (heart of the design)

Granularity: the engine serves **immutable snapshot views** of columnar data + accepts **committed write batches**. MVCC, WAL replay ordering, and visibility live *above* the trait in `zu-storage` shared code; each engine implements persistence primitives only.

```rust
/// One engine instance == one attached graph database.
pub trait GraphStore: Send + Sync {
    /// Open handles; must be O(1) I/O (T8): read headers/manifest only.
    fn catalog(&self) -> Arc<Catalog>;

    /// Pin a consistent snapshot (epoch). Cheap; readers hold Arc.
    fn snapshot(&self) -> Arc<dyn Snapshot>;

    /// Durably apply one committed transaction's effects.
    /// Called by the single writer thread only.
    fn commit(&self, batch: CommitBatch) -> Result<Epoch>;

    /// Fold WAL/deltas into base storage; engine-specific policy.
    fn checkpoint(&self, mode: CheckpointMode) -> Result<()>;

    /// Bulk-load fast path (bypasses WAL; see 08 §6).
    fn ingest(&self, groups: Vec<SealedNodeGroup>) -> Result<Epoch>;
}

pub trait Snapshot: Send + Sync {
    fn scan_column(&self, t: TableId, g: NodeGroupId, c: ColumnId)
        -> Result<SegmentRef>;              // compressed segment + encoding tree
    fn csr(&self, t: RelTableId, g: NodeGroupId, dir: Direction)
        -> Result<CsrRef>;                  // offsets + neighbor segments
    fn lookup_pk(&self, t: TableId, key: &Value) -> Result<Option<NodeOffset>>;
    fn epoch(&self) -> Epoch;
}
```

`SegmentRef`/`CsrRef` hand the executor **compressed bytes + encoding metadata** (zero-copy from the buffer manager or cache). Decoding happens in the executor's vector pipeline, engines never decompress. This is what makes three engines cheap to maintain: sqlite and s3 only need to produce the same segment bytes.

## 4. Process & threading model

- **Embedded, in-process.** `Database::open(path_or_url, Config)`.
- **Single writer**: one global write transaction at a time (Kùzu/DuckDB/SQLite precedent; §08). Writers queue on a mutex; readers never block.
- **Morsel scheduler**: fixed worker pool (default `min(cores, 8)`, configurable to 1, everything must run single-threaded for the 32 MiB floor). Work-stealing deque of morsels (2048-tuple targets); no tokio in the query path. The s3 engine runs its own small background executor (flusher, compactor, cache maintenance) on 1–2 threads; `object_store` async is bridged with a minimal current-thread runtime confined to `zu-s3`.
- **No background threads at all** when idle with `zu1`/`sqlite` (laptop/edge friendly): checkpoints are triggered by commit-size thresholds or explicit `CHECKPOINT`.

## 5. Catalog

- Stored as ordinary (system) tables inside whichever engine backs the database → moves with the file; no sidecar metadata.
- Contents: node/edge table defs, columns+types, primary keys, secondary labels, indexes (pk hash, vector, fts), stats (degree histograms, color summaries), format/feature versions.
- In-memory: immutable `Arc<Catalog>` swapped atomically on DDL commit (copy-on-write; DDL is rare).

## 6. Error handling & safety

- No panics on user input or corrupt files: every decode path returns `Result`; corrupt segment ⇒ `ZuError::Corrupt { location, detail }` with the file offset. `unsafe` confined to: bitpacking kernels, buffer-manager page transmutes, mmap of read-only bulk regions; each block carries a safety comment and a fuzz target (`cargo fuzz` corpus: headers, segments, WAL, manifest, Cypher parser).
- All on-disk integers little-endian; all structures versioned; CRC32C on every WAL record, segment, and header (hardware CRC via `crc32c` crate).
