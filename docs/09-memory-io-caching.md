# 09. Memory, I/O, Caching, Resource Budgets

## 1. Global memory budget

Single knob: `memory_limit` (default: min(25% of RAM, 4 GiB); floor 32 MiB). Accounted arenas (soft-quota, spill or degrade on pressure):

| Arena | Default share | Pressure response |
|---|---|---|
| Segment cache (compressed) | 50% | evict (SIEVE) |
| Execution (vectors, hash tables, sorts) | 30% | spill sorts/aggregates to temp blocks; abort with clear error if unspillable |
| Overlay/MVCC deltas | 10% | force checkpoint |
| Catalog/stats/plan cache | 10% | evict plans |

Key lever (research §2): **cache compressed, decode on touch**. Decode at ≥ 1 GB/s/core means re-decoding beats caching decoded data at 3–6× RAM effectiveness. Only CSR *offsets* and hot dictionaries get a small decoded pin pool.

## 2. Buffer manager (zu1), vmcache-style, no raw mmap

mmap rejected per Crotty/Pavlo CIDR 2022 (no write control, TLB storms). vmcache (Leis SIGMOD 2023) adapted:

- Reserve a virtual region = file size (grows by remap); page state machine per 256 KiB block in a packed atomic u64 array: `Evicted → Loading → Resident → Dirty`, epoch-stamped for optimistic readers.
- Read path: optimistic, load state word, if Resident, read bytes, re-check state+epoch (seqlock pattern); miss ⇒ CAS to Loading, `pread` into the region, publish Resident.
- Eviction: SIEVE (NSDI 2024, beats LRU-family with one visited bit and a hand; no promotion writes on hit path) over Resident blocks; Dirty blocks are only written by checkpoint (shadow publishing, eviction of Dirty is impossible by construction; dirty set is bounded by checkpoint threshold).
- No exmap dependency; syscall-per-miss is acceptable at our I/O rates (research §2.3), keeps it portable (macOS/Linux/Windows: plain `pread`/`ReadFile` fallback without the reservation trick).

## 3. I/O backend trait

```rust
pub trait IoBackend: Send + Sync {
    fn read_at(&self, file: &FileId, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&self, file: &FileId, offset: u64, buf: &[u8]) -> Result<()>;
    fn sync(&self, file: &FileId, mode: SyncMode) -> Result<()>;   // data | all
    fn read_gather(&self, reqs: &mut [ReadReq]) -> Result<()>;     // batched
}
```

- `PsyncBackend` (default): positional I/O, portable, zero deps.
- `UringBackend` (`io-uring` feature, Linux ≥ 5.8): batched submission for `read_gather` (frontier prefetch, checkpoint writes); raw `io-uring` crate; **no async runtime**, completion polled by the calling worker (thread-per- core friendly; tokio-uring is stalled upstream, research §2.3).
- `O_DIRECT` optional (`direct_io = true`): aligned 4 KiB I/O, skips page cache double-buffering; default off (page cache is a fine L2 for laptops).
- Object I/O: `object_store` crate behind the same `read_gather` semantics (ranged GETs, coalescing adjacent ranges within `coalesce_gap = 1 MiB`).

## 4. Hybrid cache (s3 engine), foyer

Config in §06 §4. Integration notes:
- foyer memory tier replaces the segment-cache arena for s3 databases (single accounting domain; zu's budget passes through).
- Disk tier: foyer block engine on the cache dir; entries = compressed segment ranges keyed by (pack ulid, range); TTL none; eviction S3-FIFO; admission: reject single-touch scans (query hints mark sequential-scan reads `no-admit`, BI scans must not flush the traversal working set).
- Inflight dedup covers thundering herds on hot groups (foyer built-in).

## 5. Prefetching

- **Frontier prefetch** (the s3/NVMe latency killer): RecursiveBFS and Expand emit next-level group demands; scheduler issues one `read_gather` per level (≤ 64 concurrent ranges default). Cold 3-hop over S3 Standard ⇒ ~3 batched round trips ≈ 100–300 ms, not thousands of serial GETs (turbopuffer rule).
- Sequential scan readahead: 4-block window when ≥ 2 adjacent misses.
- Zone-map-driven skip happens *before* prefetch (never fetch skipped groups).

## 6. Resource discipline (G6/G7 enforcement)

- 32 MiB floor CI job: full test suite under `memory_limit = 32 MiB`, `threads = 1` (spills exercised).
- No allocation on the per-vector hot path: vectors, masks, and hash-table scratch come from a per-morsel bump arena reset between morsels.
- Binary size: `opt-level = "s"` on parser/CLI crates, `panic = "abort"` on release CLI, no default `regex`/`chrono` heavy deps (own date code); measured in CI (G7 gate: 15 MiB).
- CPU: workers park when idle (no spinning); background tasks (s3 flusher) tick at flush_interval only when dirty.
