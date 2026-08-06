# 04. `zu1`: Native Single-File Format

Design lineage: DuckDB file layout (dual headers, fixed blocks, sidecar WAL, checkpoint-into-columns), Kùzu node-group CSR (slack gaps), Lance 2.1 structural encodings, BtrBlocks sampled cascades. All integers little-endian. CRC32C everywhere. Format id: `zu1`, version 1.

## 1. File layout

```
offset 0        ┌────────────────────────────┐
                │ FileHeader (4 KiB)          │  magic, version, block size
offset 4096     │ DatabaseHeader A (4 KiB)    │  ┐ alternating; higher epoch
offset 8192     │ DatabaseHeader B (4 KiB)    │  ┘ with valid CRC wins
offset 12288    │ padding to 256 KiB          │
offset 256 KiB  │ Block 1                     │  fixed 256 KiB blocks
                │ Block 2 …                   │  (data, metadata, free)
                └────────────────────────────┘
sidecar         <db>.zu1.wal                    redo WAL (§08)
```

### FileHeader (write-once)
| off | size | field |
|---|---|---|
| 0 | 8 | magic: `0xE5 0x9B 0xB3 'Z' 'U' '1' 0x00 0x0A` (UTF-8 図 + "ZU1\0\n") |
| 8 | 2 | format_version = 1 |
| 10 | 2 | min_reader_version |
| 12 | 4 | block_size = 262144 |
| 16 | 16 | database UUID |
| 32 | 8 | flags (bit0: encrypted†, bit1: created-by-bulk) |
| 40 | 24 | reserved |
| 64 | 4 | crc32c of bytes 0..64 |
† encryption reserved for v2; not in scope.

### DatabaseHeader (one per checkpoint, alternating A/B, atomic root flip)
| field | type | meaning |
|---|---|---|
| epoch | u64 | checkpoint epoch |
| catalog_root | BlockPtr | meta-block chain: catalog |
| table_index_root | BlockPtr | meta-block chain: per-table group directories |
| free_list_root | BlockPtr | free block list |
| block_count | u64 | high-water mark |
| wal_seq | u64 | WAL sequence covered by this checkpoint |
| stats_root | BlockPtr | statistics blocks |
| crc32c | u32 | over the header |

Open procedure (G8: O(1) I/O): read 12 KiB, pick valid header with max epoch, lazily page everything else. `BlockPtr = u64` block index; meta-block chains are linked lists of 256 KiB blocks whose payload is a versioned, length-prefixed binary encoding (hand-rolled, spec'd in `format/meta.rs`; no serde on disk).

## 2. Node groups

- Fixed logical size **131,072 rows (2^17)** per node group (Kùzu ≈128 K, DuckDB 122 880; power of two simplifies id math).
- Unit of: compression, zone maps, MVCC versioning, checkpoint rewrite, cache admission, and S3 object packing (§06).
- Per node table, the *group directory* (meta-block chain) maps `node_group → GroupMeta { row_count, deleted_count, per-column SegmentMeta[], csr refs, zone maps, version_epoch }`.

## 3. Column segments

A **segment** = one column × one node group, stored in ≤ N contiguous blocks.

```
SegmentMeta {
  encoding_tree: EncodingNode,     // ≤ depth 3 cascade, e.g. Dict→FSST, Delta→BitPack
  structural: MiniBlock | FullZip, // Lance 2.1 split (§ below)
  blocks: [BlockPtr],              // contiguous runs preferred
  value_count: u32, null_count: u32,
  zone: { min, max (truncated to 16 B), has_null },
  uncompressed_bytes: u64, crc32c: u32,
}
```

### Structural encodings (random access without row groups)
- **MiniBlock** (types ≤ 16 B): values packed in chunks of 1024 values (FastLanes transposed layout inside); chunk index = one u32 cumulative end offset per chunk, followed by one u64 fence per chunk holding the chunk's last value (~12 B/chunk metadata; the width byte travels inside the chunk because every chunk is a self-describing cascade). Point read = 1 chunk decode (≤ 4 KiB touch). Matches Lance mini-block ~24–41 B/chunk finding. The fences double as zone maps for sorted row ranges: within a CSR neighbor list, a binary search over the fences names the single chunk that can hold a value, so an edge probe decodes one chunk regardless of degree, and the full-scan path cross-checks every fence against its chunk's decoded tail.
- **FullZip** (STRING > dict threshold, BLOB, VECTOR, LIST payloads): values zipped with their lengths so a row range is one contiguous byte range; per-1024-row offset samples for O(1) seek.

### Value encodings (encoding ids are format-stable)
`0 Plain · 1 Constant · 2 RLE · 3 Dict · 4 FOR+BitPack(FastLanes) · 5 Delta+BitPack · 6 ALP · 7 ALP_RD · 8 FSST · 9 BoolBitpack · 10 Frequency · 11 Zstd(leaf, optional) · 12 Delta+Patch(exceptions)`
- Selection: BtrBlocks-style sampling, 8 runs × 128 values (~0.8%), estimate size for each legal cascade, pick min; tie → fewer stages. Depth ≤ 3.
- Validity: separate bitmap (RLE if runs) per segment, not interleaved.
- **Decoding contract**: every encoding decodes ≥ 1 GB/s/core scalar; nothing requires more than 64 KiB scratch; all decoders fuzzed.

## 4. Adjacency: CSR per node group (per direction)

For each rel table and each direction (FWD keyed by src, BWD keyed by dst):

```
CsrGroup {
  offsets:  MiniBlock segment of u32 slot-offsets  (131,073 entries, FOR+bitpack)
  nbr_ids:  segment of u64 NodeIds → stored as (table_id dict) + delta+bitpack row ids
  rel_cols: parallel segments per rel property, CSR order
  slack:    per-list gap slots (see below)
}
```

- **Sorted neighbor lists** (by neighbor id) → delta+bitpack compresses to the 4–8 bits/edge target on reordered graphs (research §2.2); binary search inside a list for `(a)-[]->(b)` existence checks; galloping intersect for WCOJ.
- **Slack gaps** (updatable CSR, Kùzu #1474): at build, each list gets `ceil(len * 0.2) + 1` empty slots (growth factor 1.2). Inserts fill slack; a full list triggers **group-local rebuild** (rewrite that CsrGroup only, ~O(≤ few MB)); never a global rebuild. Deletes tombstone in-place (validity bit), compacted at checkpoint when `deleted > 12.5%`.
- Degenerate high-degree nodes (celebrity problem): a list > 2^17 slots spills to a **continuation chain** of dedicated blocks; offsets store a spill marker.
- `ONE_*` rel tables skip CSR: single neighbor column, nullable.

## 5. Optional `REORDER` at bulk load

`COPY ... WITH (REORDER = degree | bfs | none)` relabels NodeIds before sealing groups (BFS from max-degree roots default). Gains: ~25%+ adjacency compression + traversal locality (WebGraph/LLP literature). The pk index makes relabeling invisible to users. LLP-class orderings deferred (hours at 10⁹ edges).

## 6. Zone maps & statistics

- Per segment: min/max/null (16-B truncated) → predicate skip at group and chunk level.
- Per table: row/tombstone counts; per rel table: degree histogram (log2 buckets, per direction), edge count; color summaries (§07) refreshed by `ANALYZE` and incrementally at checkpoint.

## 7. Primary-key index

Two-level static-hybrid hash: per node group a closed-addressing table (~10 bits/key) sealed at checkpoint + a small mutable overlay for the current delta. Point lookup: hash → group candidate(s) → verify against key column chunk (1 chunk decode). No global rebuild on insert.

## 8. Free space & file growth

Free list = meta-block chain of block ids; blocks are recycled at checkpoint (after the old header epoch is superseded and no snapshot pins it). `VACUUM` rewrites live groups densely and truncates the tail. Minimum file: 768 KiB + 1 block.

## 9. Torn-write & corruption posture

- Blocks are written to *free* space then published by the header flip (shadow publishing) → torn block writes never damage committed state.
- WAL records individually CRC'd; replay stops at first invalid record.
- `zu verify` walks headers → chains → segments checking CRCs (also fuzz target).

## 10. Forward compatibility rules

- Readers must reject `min_reader_version >` self.
- Unknown encoding id / structural id / meta record type ⇒ error naming the id (never skip silently on data path).
- New optional meta record types are skippable *only* in the stats/hints section (explicitly marked `SKIPPABLE` flag bit).
