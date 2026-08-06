# 05. `sqlite` Engine

## 1. Why a SQLite engine at all

- **Ubiquity & trust**: a zu graph inside a plain SQLite file is inspectable by every tool on earth, backed up by Litestream/sqlite3 .backup, embeddable where policy says "SQLite only" (mobile, air-gapped, medical/edge).
- **Write-heavy small graphs**: SQLite's B-tree + WAL beats rewriting columnar segments for high-rate tiny transactions (< ~10 M nodes working sets).
- **Correctness baseline**: differential testing, every query in CI runs on zu1 and sqlite engines; results must match. Cheap oracle for the fancy engine.
- **Proven limits** (research §2.3): single-writer WAL is native to our model; BEGIN CONCURRENT/WAL2 remain experimental upstream, we do not depend on them.

## 2. Schema mapping

One SQLite database per zu database. `application_id = 0x5A5531` ("ZU1"), `user_version` = zu schema version. All zu catalog data in `_zu_catalog`.

```sql
-- node table Person(id INT64 PK, name STRING, born DATE) →
CREATE TABLE n_person (
  zrow INTEGER PRIMARY KEY,          -- dense rowid == NodeId.row (per-table)
  pk   INTEGER NOT NULL UNIQUE,      -- user primary key (type-mapped)
  labels INTEGER NOT NULL DEFAULT 0, -- secondary-label bitset (≤63 in sqlite engine)
  p_name TEXT, p_born INTEGER        -- properties, zu type → sqlite affinity
);
-- rel table Follows(FROM Person TO Person, since DATE) →
CREATE TABLE r_follows (
  zrel INTEGER PRIMARY KEY,
  src  INTEGER NOT NULL,             -- NodeId (packed u64 as INTEGER)
  dst  INTEGER NOT NULL,
  p_since INTEGER
);
CREATE INDEX r_follows_fwd ON r_follows(src, dst);
CREATE INDEX r_follows_bwd ON r_follows(dst, src);
```

Type mapping: INT*/DATE/TIMESTAMP→INTEGER, DOUBLE→REAL, STRING→TEXT, BLOB/UUID/VECTOR→BLOB, DECIMAL→TEXT (lossless), LIST/MAP/STRUCT→BLOB (zu-encoded, documented), nested types are opaque in this engine (v1).

## 3. Configuration (set at open, non-negotiable defaults)

```
PRAGMA journal_mode=WAL;  PRAGMA synchronous=NORMAL;
PRAGMA page_size=8192;    PRAGMA cache_size=-16384;   -- 16 MiB, budget-scaled
PRAGMA mmap_size=0;       -- buffer management is ours; avoid mmap double-cache
PRAGMA foreign_keys=OFF;  -- integrity enforced by zu layer (same rules as zu1)
PRAGMA busy_timeout=5000; PRAGMA wal_autocheckpoint=2000;
```

`rusqlite` with `bundled` (pinned SQLite ≥ 3.50) under the `sqlite` feature only, keeps G12 for default builds.

## 4. Serving the `GraphStore` contract

The executor consumes segments; SQLite stores rows. Bridge = **lazy CSR cache**:

- `Snapshot::csr(t, g, dir)`: on miss, one range query (`SELECT src, dst, zrel FROM r_x INDEXED BY r_x_fwd WHERE src BETWEEN lo AND hi ORDER BY src, dst`) builds an in-memory CsrGroup (same struct as zu1, same encodings) for that node group; cached in the shared segment cache with an invalidation epoch. Write commits bump per-(table, group, dir) epochs → targeted invalidation.
- `scan_column`: `SELECT p_x FROM n_t WHERE zrow BETWEEN lo AND hi ORDER BY zrow` materialized into a transient segment (encoded Plain; no compression work).
- `lookup_pk`: direct `SELECT zrow FROM n_t WHERE pk = ?` (uses UNIQUE index).
- Effect: cold queries pay SQLite B-tree costs; hot traversals run at native zu vector speed over cached CSR. Documented honestly: this engine's cold multi-hop analytics are 10–100× slower than zu1, it is not the analytics tier.

## 5. Transactions

zu's single-writer lock wraps a SQLite IMMEDIATE transaction; commit = SQLite commit (WAL fsync per `synchronous=NORMAL` semantics). Snapshots map to SQLite read transactions (WAL readers see a stable snapshot). zu-level MVCC epochs piggyback on `data_version`/commit counter. Checkpoint = `PRAGMA wal_checkpoint(TRUNCATE)`, no zu-specific checkpoint machinery.

## 6. Interop & migration

- `COPY graph TO 'file.zu1'` / `COPY graph TO 'file.db' (ENGINE sqlite)`, lossless both ways (nested types round-trip via zu-encoded blobs).
- `ATTACH 'file.db' AS g (ENGINE sqlite)` alongside a zu1 database; cross- database MATCH is allowed read-only (executor is engine-agnostic).
- Third-party SQLite writers are tolerated but detected: schema hash stored in `_zu_catalog`; mismatch ⇒ open read-only with warning.

## 7. Limits (engine-specific, documented)

| Dimension | Limit |
|---|---|
| Secondary labels | 63 (bitset in INTEGER) |
| Practical size | ~100 GB (SQLite fine beyond, but CSR build cost dominates) |
| Vector index | brute-force scan only in v1 (no HNSW persistence here) |
| Concurrency | SQLite single writer == zu single writer (aligned) |
