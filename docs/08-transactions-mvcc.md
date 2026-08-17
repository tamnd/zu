# 08. Transactions, MVCC, WAL, Recovery

Model: **single writer, many snapshot readers**, the DuckDB/Kùzu/SQLite consensus for embedded engines (research §2.3). Serializable trivially for writes (they're serial); readers get snapshot isolation.

## 1. Semantics

- `BEGIN` (read), pins a snapshot epoch; sees a fully consistent graph.
- `BEGIN WRITE`, acquires the global writer lock (queue, FIFO, with timeout).
- Auto-commit for single statements. Interactive write txns bounded by `max_write_txn_bytes` (default 256 MiB), beyond that, use bulk `COPY`.
- Isolation: writes serializable; reads snapshot isolation (anomaly-free for read-only). Cross-engine identical semantics.

## 2. MVCC (HyPer-derived, node-group-grained, Kùzu #2529 lineage)

- Committed state = **base epoch data + in-memory delta overlays** per (table, node group): appended rows, tombstone bitmaps, property update chains (newest-first, epoch-stamped), CSR slack fills / overlay edges.
- Readers at epoch E: base + overlays with commit_epoch ≤ E; version chains resolved per vector (batch visibility check, not per-tuple branches).
- Overlays are bounded: checkpoint folds them into new sealed segments and bumps the base epoch. Old segments are freed when no snapshot pins them (epoch refcounts).
- Rationale: version data never hits the columnar files (Umbra "no versions on disk" principle); recovery never sees MVCC state.

## 3. WAL (engine-shared logical format)

Redo-only, logical-physiological records; one format for all engines (zu1 sidecar file, s3 batched objects; sqlite engine delegates to SQLite's own WAL and skips this section):

```
WalRecord { len u32 | crc32c u32 | epoch u64 | kind u8 | payload }
kinds: TxnBegin, NodeInsert{table, rows(columnar)}, RelInsert{...},
       Update{table, group, col, offsets, values}, Delete{ids},
       RelDelete{rel, src, dst}, RelUpdate{rel, col, src, dst, values},
       DdlCatalog{delta}, TxnCommit{epoch},
       IngestRef{sealed group ptrs}, CheckpointNote{epoch}
```

- Commit = append records + fsync (zu1: `fdatasync` per commit, group-commit window 1 ms when writers queue; s3: durability per §06 modes).
- `RelDelete` names the rows an edge runs between rather than an offset, because an edge has none: the fold drops the pair out of the CSR it rebuilds, so there is nothing for a reader to filter by afterwards.
- `RelUpdate` names its edges the same way and for the same reason, and it carries one column per record: an edge property column is dense over the edges in the order the table holds them, so the fold rewrites the whole column and the pair is what survives the reorder an added edge causes.
- `IngestRef` is the DuckDB trick: bulk loads write sealed segments directly to free blocks/objects, WAL only references them, no double write.
- Replay: idempotent by epoch (records ≤ checkpointed epoch skipped); stops at first CRC failure (torn tail = uncommitted).

## 4. Checkpoint

Trigger: WAL ≥ `checkpoint_wal_bytes` (default 64 MiB) at commit boundary, or explicit `CHECKPOINT`, or clean close.

```
1. take writer lock (queued like any writer)
2. fold overlays → rewrite dirty node groups as new segments (new blocks/objects)
3. write new catalog/stats/meta chains
4. flip: zu1 = write alternate DatabaseHeader + fsync;  s3 = manifest CAS swap
5. truncate WAL (zu1) / advance WAL floor (s3); release old blocks when unpinned
```

Only dirty groups are rewritten (write cost ∝ churn, not DB size). Crash at any step ⇒ old header/manifest still valid; new blocks are unreferenced garbage (zu1: reclaimed by free-list audit on next checkpoint; s3: GC grace list).

## 5. Constraint enforcement point

Inside `commit()` before WAL append: pk uniqueness (probe index + batch dedup), endpoint existence (probe base+overlay), DETACH rules. Violation ⇒ whole txn rejected (no partial application).

## 6. Bulk ingest path (`COPY`)

- Sort/partition input by (table, group), build sealed node groups + CSR with full compression off-thread (parallel by group), optional REORDER (§04 §5), then a single `ingest()` commit (IngestRef WAL record).
- Not MVCC-overlaid: ingest of *new* groups is invisible until commit epoch; ingest into existing tables appends whole groups (fast path) or falls back to normal DML for partial-group tails.
- Target T4 (> 1 M edges/s/core) is met by never touching the WAL with data and compressing group-parallel.

## 7. Recovery matrix

| Engine | Crash recovery |
|---|---|
| zu1 | pick valid max-epoch header → replay WAL tail → open (ms-scale; no scan of data blocks) |
| sqlite | SQLite WAL recovery (theirs, proven) |
| s3 | read CURRENT → manifest → list+replay WAL objects > floor with matching writer epoch |

Invariant tested by the crash-injection suite (`11-…md` §4): *any* prefix of physical writes yields either the pre-txn or post-txn state, never a hybrid.
