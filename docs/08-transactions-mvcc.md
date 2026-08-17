# 08. Transactions, MVCC, WAL, Recovery

Model: **single writer, many snapshot readers**, the DuckDB/Kùzu/SQLite consensus for embedded engines (research §2.3). Serializable trivially for writes (they're serial); readers get snapshot isolation.

## 1. Semantics

- `BEGIN` (read), pins a snapshot epoch; sees a fully consistent graph.
- `BEGIN WRITE`, acquires the global writer lock (queue, FIFO, with timeout).
- Auto-commit for single statements. Interactive write txns bounded by `max_write_txn_bytes` (default 256 MiB), beyond that, use bulk `COPY`.
- Isolation: writes serializable; reads snapshot isolation (anomaly-free for read-only). Cross-engine identical semantics.

## 1a. Explicit transactions, as they are implemented today (GT01 to GT03)

`START TRANSACTION [READ ONLY | READ WRITE]`, `COMMIT` and `ROLLBACK` are statements a session runs, not a query: they have no binding table and no plan, so `zu::session::Session` takes them before the plan cache is asked for anything. A one-shot `zu::query::run` refuses them by saying so, because a transaction runs across statements and that needs a session.

The unit that is taken back is a **file savepoint** (`zu_zu1::Zu1File::begin_savepoint`), not a deferred commit. A read goes through the sealed file, so every statement folds its overlays into new segments and flips the header on the way out, and by the time the second statement of a transaction runs the first one is already published. Undo is therefore at the file: the savepoint keeps the pre-transaction `DatabaseHeader`, the free list, and the blocks the free list is written in, and a rollback republishes the kept header. Two guards make that sound while a savepoint is held:

- The free list splits in two. A checkpoint inside the transaction publishes the blocks the transaction freed, because the epoch it publishes has let go of them, but it holds them out of allocation until the transaction ends, since the kept header still reads them. What was already free when the transaction began stays allocatable, because it is free in the kept state too, so whatever the transaction writes into it is garbage in a block nothing reads. That is what keeps a write statement from growing the file: `bench/write` measures 0 bytes of growth per statement for `SET` and `INSERT`, the same as before transactions existed.
- Blocks that were freed before the transaction and not published yet are frozen with the rest, because the kept header still references them as live.

The epoch is the one thing not restored. The kept header is republished one past the newest epoch in the file, since a header behind the other slot would lose the next open. Blocks written past the kept high-water mark fall outside the file the restored header describes, so the next writer hands them out again; blocks the transaction freed are dropped rather than published, because the restored header still references them. A transaction that published nothing costs no epoch at all: the rollback forgets the staged blocks and does not flip.

Two things fall out of the same machinery. A statement outside an explicit transaction takes and owns a savepoint of its own, so a multi-part write whose later clause raises is undone whole (an implicit transaction, `INSERT ... WITH p RETURN p.name / 0` leaves no row behind). And a catalog statement stages under the same savepoint, which is the transaction-local catalog GP18 asks for: `CREATE GRAPH TYPE` publishes immediately so the next statement sees it, and a rollback unmakes it together with the rows. A data statement that changes the catalog is the same thing from the other side: `SET p:Manager` on a table that has not declared `Manager` declares it, publishing the widened catalog before it stages the bit, because what turns a label change away is the catalog in the file and the fold reads it from there. The declaration is a write of the statement like the rows are, so it goes back with them. An `INSERT` whose label names no node table makes one and is the larger version of the same move: the table is created before the statement compiles, because the binder resolves the label to a table at bind time, and it is created inside the savepoint, so the statement that wanted it is what keeps it.

One thing this does not buy: a crash in the middle of an explicit transaction. Every statement of it has already published, so what a reopen finds is the statements that ran, not the state the transaction started from. The savepoint lives in memory, which is why it is worth saying out loud: rollback is a statement, and a process that dies is not one. Making the savepoint durable is a header field and a recovery step, and it is on the list rather than done.

Codes: `25G01` for a transaction already running, `25G03` for a statement that writes inside one started `READ ONLY` (raised before the statement is compiled and before anything is staged), `2D000` for a `COMMIT` or `ROLLBACK` with nothing running. A session dropped mid-transaction rolls back, which is why ending one with a statement is worth doing: the statement can say what went wrong and a drop cannot.

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
       LabelUpdate{table, offsets, add, remove},
       DdlCatalog{delta}, TxnCommit{epoch},
       IngestRef{sealed group ptrs}, CheckpointNote{epoch}
```

- Commit = append records + fsync (zu1: `fdatasync` per commit, group-commit window 1 ms when writers queue; s3: durability per §06 modes).
- `RelDelete` names the rows an edge runs between rather than an offset, because an edge has none: the fold drops the pair out of the CSR it rebuilds, so there is nothing for a reader to filter by afterwards.
- `RelUpdate` names its edges the same way and for the same reason, and it carries one column per record: an edge property column is dense over the edges in the order the table holds them, so the fold rewrites the whole column and the pair is what survives the reorder an added edge causes.
- `LabelUpdate` carries two masks rather than a word, because a row's label word is read-modify-written and the file the txn would read it from does not hold the txn's own earlier changes yet. The masks are disjoint, the fold applies `(word | add) & !remove`, and two changes to one row compose to one pair of masks, so a label going on and another coming off in one statement both land.
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
