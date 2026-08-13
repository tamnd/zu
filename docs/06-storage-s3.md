# 06. `s3` Engine: Object-Native, Fixed-Cost

Design lineage: SlateDB (manifest CAS + fenced single writer + batched WAL), WarpStream (PUT batching decoupled from logical write rate; zonal read cache), Quickwit (hotcache one-round-trip open), turbopuffer (cold-query byte-range discipline; never traverse graph-shaped indexes against S3), BG3/SIGMOD'24 (hot topology stays in RAM/NVMe), Neon (generation fencing). Research §3.

Works on any `object_store` backend: S3, GCS, Azure, R2, MinIO, Tigris (all provide the required CAS primitive: S3 If-Match/If-None-Match since Nov 2024; GCS generation-match; Azure ETag; R2 conditional puts).

## 1. Object layout

```
s3://bucket/prefix/
  manifest/CURRENT                      # tiny pointer object, CAS-updated (ETag If-Match)
  manifest/MANIFEST.{epoch:020}.zu      # immutable manifest snapshots
  wal/{seq:020}.zuwal                   # batched WAL objects (SST-like, CRC'd)
  seg/{ulid}.zuseg                      # immutable segment packs (8–64 MiB)
  ckpt/{name}                           # named zero-copy checkpoints (pins epochs)
```

### Segment packs (`.zuseg`)
- Pack = many `zu1`-format segments (same bytes as §04, encodings shared!) for a set of node groups, grouped **by table, column-major, traversal-locality ordered** (CSR groups adjacent to their offset segments).
- Target 8–64 MiB per object (storage-cost dominant, request-count minimal).
- **Footer (hotcache, Quickwit pattern)**: last ~0.1% of the object = directory of contained segments (byte ranges + SegmentMeta). Manifest stores each pack's footer range → any segment reachable in ≤ 1 ranged GET cold, 0 warm.

### Manifest
- Flat, versioned binary (same meta encoding as zu1): catalog epoch, table → group directory (group → pack id + byte ranges), WAL floor/ceiling seqs, writer epoch, checkpoint pins, deleted-object grace list.
- Size discipline: ~64 B/group-column entry; 1 B-node graph ≈ 8 K groups ⇒ manifests stay single-digit MiB; beyond that, partition (§7).

## 2. Write path (fixed-cost mechanics)

```
commit(batch):
  1. append batch to in-memory WAL buffer (and to local wal spill file, crash
     safety before durability, see Durability modes)
  2. flusher PUTs wal/{seq}.zuwal when: flush_interval elapses (default 100 ms)
     OR buffer ≥ flush_bytes (default 4 MiB)
  3. PUT ok ⇒ txns in that object are durable; waiters released
```

- **PUT rate is bounded by 1/flush_interval regardless of txn rate**, WarpStream's core trick. Defaults: ≤ 10 PUT/s ⇒ ≤ 26 K PUTs/month ⇒ $0.13/mo even at max flush rate (S3 Standard $5/M).
- Durability modes (per-database config, honest tradeoffs):
  - `durable` (default): `commit()` returns after WAL PUT (adds up to flush_interval + PUT p50 ~70 ms latency).
  - `async`: returns after local spill fsync; loses ≤ flush window on total node loss (spill replayed if node survives). For agents/analytics.
  - `express`: WAL bucket on S3 Express One Zone (PUT p50 ~3 ms, $1.13/M), commit latency ~10 ms at ~10× WAL request cost (still < $2/mo at defaults); segments/manifest stay on Standard (WarpStream S3EOZ tiering pattern).

### Checkpoint / L0 fold
Background task folds WAL objects into fresh segment packs (rewriting only dirty node groups), writes `MANIFEST.{epoch+1}`, then **CAS-swaps `manifest/CURRENT` with If-Match**. Old packs enter the grace list; GC deletes after `gc_grace` (default 24 h) if no checkpoint pins them.

## 3. Single-writer fencing (SlateDB protocol)

1. Writer candidate reads `CURRENT` (+ETag), reads manifest, increments `writer_epoch`, writes `MANIFEST.{n+1}` (If-None-Match), CAS-swaps `CURRENT`.
2. CAS success ⇒ it is *the* writer; failure ⇒ someone else won; re-read, retry or become reader.
3. Every subsequent manifest swap carries the epoch; a fenced (stale) writer's If-Match fails permanently ⇒ it demotes itself. WAL objects embed the writer epoch; readers ignore WAL objects from superseded epochs.
4. No leases, no clocks, no DynamoDB, no coordination service. Contention costs only retried requests.

Readers are stateless: read `CURRENT` → manifest → serve; poll `CURRENT` (default every 1 s, conditional GET = $0.40/M) or accept bounded staleness. Horizontal read scale-out is trivial (T10); zone-aligned deployments read their own cache tier first (WarpStream lesson), cross-AZ S3 traffic is free in-region, so this is a latency, not cost, optimization.

## 4. Caching (where latency is won)

Three tiers, managed by **foyer** (hybrid memory+disk cache, Rust; used by RisingWave/SlateDB/Chroma):

| Tier | Size default | Contents | Hit latency |
|---|---|---|---|
| RAM | 25% of budget | hot decoded CSR offsets + segment bytes | < 1 µs |
| NVMe (foyer block engine) | configurable, e.g. 5–30% of dataset | compressed segment ranges, WAL tail, footers | ~100 µs–1 ms |
| S3 | 100% | everything | 25–300 ms (Standard) / 3–15 ms (Express) |

- Admission: S3-FIFO (foyer default); **inflight request deduplication** (one GET per missing range, concurrent readers coalesce), foyer built-in.
- **Write-through on checkpoint**: freshly folded packs are inserted into NVMe cache before the manifest swap (SlateDB active maintenance), post-compaction reads never storm S3.
- **Pinned tier**: manifests, footers, CSR offset segments, pk-index buckets, and (if `vector`) HNSW upper layers are `pin=true`, the "hot topology never leaves cache" BG3 rule. Rule of thumb: topology ≈ 4–8 bits/edge ⇒ a 10 B-edge graph's full adjacency ≈ 5–10 GiB, pinnable on one NVMe.
- Cold-query discipline (turbopuffer lesson): planner annotates each pipeline with max S3 round trips; multi-hop expansion against uncached groups executes as **batched frontier prefetch** (gather all needed ranges per BFS level → parallel ranged GETs), never pointer-chase per node against S3.

## 5. Cost model (T9 verification, us-east-1 Standard, 2026 prices)

Scenario: 1 TB logical graph (compressed), read-mostly 100 QPS, 1 K writes/s batched, NVMe cache 128 GiB with 95% hit rate, checkpoint hourly.

| Item | Rate | Monthly |
|---|---|---|
| Storage 1 TB (+20% GC slack) | $0.023/GB | $27.60 |
| WAL PUTs (10/s max) | $5/M | $0.13 |
| Checkpoint PUTs (~200 packs/day) | $5/M | $0.03 |
| Read GETs: 100 QPS × 5% miss × ~2 ranges | $0.40/M | $10.37 |
| Manifest polling (1/s conditional) | $0.40/M | $1.04 |
| **Total object-storage bill** | | **≈ $39/mo** |

Flat because every term is bounded by config, not workload spikes: PUTs by flush_interval, GETs by cache hit rate (cache warms → bill *falls*), storage by data size. Compare turbopuffer's ~$70/TB/mo all-in and raw-S3 floor ~$23/TB/mo. The same math on R2 ⇒ ≈ $19/mo and zero egress for public-serving readers.

## 6. Failure model

| Failure | Handling |
|---|---|
| Writer crash | new writer CAS-takes over; replays WAL ≥ manifest floor; ≤ flush window lost only in `async` mode |
| Torn multi-object checkpoint | invisible until CURRENT swap (atomic); orphans GC'd |
| Split brain | impossible by CAS epoch fencing (fenced writer's swaps fail) |
| Cache node loss | stateless; re-warm from S3 (write-through repopulates) |
| S3 outage | reads serve from cache (degraded, bounded staleness); writes queue to spill up to `spill_max` then error |
| Clock skew | irrelevant (no leases) |

## 7. PB scale: partitions

A **graph partition** = an independent manifest/WAL/segments namespace under `part={id}/`, holding a subset of node groups (hash or range of NodeId; user- assignable by table). One writer *per partition* ⇒ horizontal write scaling without distributed transactions (cross-partition writes are two commits, eventually-visible edges; documented semantics, same tradeoff BG3/TAO accept). Readers open N partitions and union snapshots; the executor's morsel scheduler treats partitions as extra group sources. 1 PB ≈ 1000 × 1 TB partitions; manifests stay small; a root `graph.json` lists partitions. Cross-partition recursive queries run frontier-batched per level (§4 discipline).

## 8. Config surface (complete)

```toml
[s3]
url = "s3://bucket/prefix"            # or r2/gcs/azure/minio URL
durability = "durable"                # durable | async | express
flush_interval_ms = 100
flush_bytes = 4194304
checkpoint_interval = "1h"            # or wal_bytes threshold
gc_grace = "24h"
poll_interval_ms = 1000               # reader manifest poll
cache_dir = "/var/cache/zu"           # NVMe tier (empty = RAM-only)
cache_disk_bytes = "128GiB"
cache_mem_bytes = "auto"              # from global memory budget
pin_topology = true
express_wal_url = ""                  # optional S3EOZ bucket
```
