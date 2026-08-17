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
| 0 | 8 | magic: `0xE5 0x9B 0xB3 'Z' 'U' '1' 0x00 0x0A` (UTF-8 図 + `ZU1\0\n`) |
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

Open procedure (T8: O(1) I/O): read 12 KiB, pick valid header with max epoch, lazily page everything else. `BlockPtr = u64` block index; meta-block chains are linked lists of 256 KiB blocks whose payload is a versioned, length-prefixed binary encoding (hand-rolled, spec'd in `format/meta.rs`; no serde on disk).

### Catalog (catalog version 5)

The catalog chain holds, in order: the tables, the label dictionary and each table's declared set (version 2), the graph types (version 3), and the schemas and the graphs (version 4). Version 4 also puts one field on every table, the id of the graph it belongs to, which is what makes dropping a graph a filter over the tables rather than a list kept beside them. A graph is an id, a name, the schema it is in, and what it is: open, a graph type the file holds by name, or one written where the graph was created and kept on the graph. A version 3 file reads as the root schema `/`, one graph called `home`, and every table in it, which is the file it always was (§07 §9.1). A graph type is a name, whether it is closed, and its element types; an element type is a name, a kind (node or edge), the labels it carries, its key label set, its endpoint type names when it is an edge, and its property declarations, each a name, a type and whether it admits null. A key label set is one of three things and the byte before the ids says which: *declared*, which the user wrote; *inferred*, which zu derived from the labels; or *none*, for an element type nothing keys on. Version 5 puts one byte on every rel table saying whether its edges have a direction (GH02): an undirected edge is written once, from whichever end it was loaded, and both adjacency indexes answer for it, so it costs a directed table nothing and a version 4 file reads as the file it was, every edge directed. A reader at version 2 stops after the labels, so an older file reads as a file with no graph types and a newer reader loses nothing; a writer always writes the current version, so a graph type is not a thing an older binary can round-trip. Property declarations are keyed by element type rather than by table, which is what lets two element types share a key label set and disagree about the rest (§03 §6).

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

<!-- terms: allow row group -->

### Structural encodings (random access without row groups)
- **MiniBlock** (types ≤ 16 B): values packed in chunks of 1024 values (FastLanes transposed layout inside); chunk index = one u32 cumulative end offset per chunk, followed by one u64 fence per chunk holding the chunk's last value (~12 B/chunk metadata; the width byte travels inside the chunk because every chunk is a self-describing cascade). Point read = 1 chunk decode (≤ 4 KiB touch). Matches Lance mini-block ~24–41 B/chunk finding. The fences double as zone maps for sorted row ranges: within a CSR neighbor list, a binary search over the fences names the single chunk that can hold a value, so an edge probe decodes one chunk regardless of degree, and the full-scan path cross-checks every fence against its chunk's decoded tail.
- **FullZip** (STRING > dict threshold, BLOB, VECTOR, LIST payloads): values zipped with their lengths so a row range is one contiguous byte range; per-1024-row offset samples for O(1) seek. Each 1024-row chunk stores the zipped form (`len: u32` before each value's bytes) either plain or behind a self-contained FSST table, whichever is smaller, behind an index of cumulative compressed and zipped sizes per chunk; a point read jumps to the chunk holding its row and walks at most 1023 inline lengths. A chunk's zipped form holds at most 16 MiB (format rule): the index sizes and verifies every chunk before the reader touches it, and larger single values wait for the continuation design that lands with the column catalog. The structural id travels as one byte in `SegmentMeta` (directory version 7); FullZip metas carry `min = max = 0` since byte payloads have no u64 zone, and `uncompressed_bytes` holds the value-byte total the full scan cross-checks. Every chunk carries the table it was encoded with, but the writer trains one table for the whole column, on a sample drawn at an even stride through it, because training costs about 3.4ms against 0.13ms to encode a chunk with a table in hand and a column of chunks that are alike wants the same table in all of them. A chunk whose code stream comes out half again what the column's own sample ratio predicts is not that column, so it trains a table of its own and keeps it if it is smaller. Nothing in the encoded form says which of the two happened, so a reader cannot tell and none of this is a format change.

### Label sets (props directory version 5)
A node table's props directory carries one optional segment beside its columns: the **label bitset**, one u64 per row, bit `i` set where the row carries dictionary label `i` (§03 §1). It is a lane segment like any other and reads the same way, but it is not in the column list, so no query names it and no schema change drops it. `None` means every row carries the table's own label and nothing else, which is what a file written before labels existed says and what a table nobody declared a label on keeps saying. The catalog holds the dictionary and each table's declared set; `verify` cross-checks the two, rejecting a row whose word holds a bit its table never declared or drops the bit its table gives every row. A store that writes columns carries the bitset across untouched and a store that writes labels leaves the columns where they are, because the two say nothing about each other. A fold that grows the row domain extends the bitset with the table's own label, which is what an appended row carries.

### Value encodings (encoding ids are format-stable)
`0 Plain · 1 Constant · 2 RLE · 3 Dict · 4 FOR+BitPack(FastLanes) · 5 Delta+BitPack · 6 ALP · 7 ALP_RD · 8 FSST · 9 BoolBitpack · 10 Frequency · 11 Zstd(leaf, optional) · 12 Delta+Patch(exceptions)`
- Selection: BtrBlocks-style sampling, 8 runs × 128 values (~0.8%), estimate size for each legal cascade, pick min; tie → fewer stages. Depth ≤ 3.
- Dict cap (format rule): a dictionary holds at most 8192 entries, exactly the 64 KiB scratch ceiling once materialized as u64. The selector never offers Dict past that cardinality and readers reject a container claiming more.
- Validity: separate bitmap (RLE if runs) per segment, not interleaved.
- **Decoding contract**: every encoding decodes ≥ 1 GB/s/core scalar; nothing requires more than 64 KiB scratch; all decoders fuzzed. The optional Zstd leaf (id 11) is the deliberate exception on both counts: it serves cold string segments where ratio wins, libzstd manages its own context, and its floors in budgets.toml track regressions rather than the hot-path contract.

## 4. Adjacency: CSR per node group (per direction)

For each rel table and each direction (FWD keyed by src, BWD keyed by dst):

```
CsrGroup {
  offsets:  MiniBlock segment of u32 slot-offsets  (131,073 entries, FOR+bitpack)
  nbr_ids:  segment of u64 NodeIds → stored as (table_id dict) + delta+bitpack row ids
  edge_base: u64 ordinal of this group's first FWD edge (directory version 8)
  slack:    per-list gap slots (see below)
}
```

- **Two row domains** (directory version 9): the group directory carries a row count per end, `from_count` for the node table the edges leave and `to_count` for the one they arrive at, so a rel table can run between two different node tables. FWD is numbered in the from domain and BWD in the to domain, which is what a source id and a destination id already mean, and a lookup range-checks a node against the count for the direction it walks. Both directions share the one group array, padded with empty CSR groups out to whichever end is longer, because a reader already reaches a group by row and direction and an empty group costs a header rather than an offsets array per row. The two counts agree for a rel table whose ends name the same node table, which is the shape every version 8 file had, and the readers that are only meaningful over one node table (the graph algorithms, the colour maps) ask for the single domain and are refused when the ends differ.

- **Edge properties** (directory version 8): a rel table's columns are one `PropsDirectory`, the same container a node table's columns use, rooted in the group directory rather than in the table index, whose slot for a rel id already holds that directory. A column's row domain is the **edge ordinal**: an edge's position in the load order, which is sorted by source and then by destination, which is also its position in the concatenated FWD neighbor arrays. So the ordinal is `edge_base + slot`, and the slot is what the binary search inside a list already finds, which makes a backward-reached edge cost the one search a probe costs and a forward walk cost nothing beyond counting. No permutation is stored in either direction. `zu copy --reorder` rewrites the load order, since relabeling the nodes resorts the edges, so it moves the property columns by that same permutation before they are stored and carries nothing on disk: `reorder::load_order` sorts the edge list and says where every edge came from, and a column is gathered through it. A pair may run more than once, and then it is that many edges with that many ordinals and that many values. The endpoints alone do not name one of them: `edge_ordinal` answers with the first of the run, which is all a caller holding nothing but the pair can be told, and `neighbor_ordinals_into` counts a whole list out so a walk gets each copy's own row. Forward that is the group base plus the list's place in it; backward each run of one source is looked up once and its copies counted from there, so a run of k copies fills k slots in each direction and the two directions agree on which copy is which. That agreement is what makes an edge read forward and the same edge read backward carry the same value. A column arrives with the edge list it belongs to: a parquet field that is neither endpoint, typed by the schema, or a csv column named and typed by an LDBC style header, `:START_ID,:END_ID,since:INT64`, where the endpoints are the cells that say so and the first two columns when nothing does.

- **Dataset load**: `zu copy --node <Table>=<nodes.csv> --rel <Table>=<From>:<To>:<rels.csv>` loads a directory of node files and rel files as the tables it names, one node table per node file and one rel table per rel file, each rel table bound to the two node tables its ends declare. The edge list load is the other shape and stays what it was, one node table and one rel table, because that is the whole of a SNAP or GAP graph. A node file's header marks one column `:ID` and the rest are its typed property columns; a rel file's header is the LDBC one above. Ids are the dataset's own and need not be dense, start at zero, or be disjoint from another label's, because each label maps into its own row space: a row is the line it was written on, and the id it came in with is kept twice, as the `id` property so `RETURN n.id` answers with it and as the primary-key index so `{id: $k}` finds the row without a scan. The index is a node-table structure that lives on a rel table's directory (§5), so it rides on the first rel table that leaves the node table and the later ones would only be the same map written again. A table whose ids are already its row numbers is stored with neither, since the dense contract answers both. An endpoint no node file declared is an error naming the id, not a row invented for it.

- **Sorted neighbor lists** (by neighbor id) → delta+bitpack compresses to the 4–8 bits/edge target on reordered graphs (research §2.2); binary search inside a list for `(a)-[]->(b)` existence checks; galloping intersect for WCOJ.
- **Slack gaps** (updatable CSR, Kùzu #1474): at build, each list gets `ceil(len * 0.2) + 1` empty slots (growth factor 1.2). Inserts fill slack; a full list triggers **group-local rebuild** (rewrite that CsrGroup only, ~O(≤ few MB)); never a global rebuild. Deletes tombstone in-place (validity bit), compacted at checkpoint when `deleted > 12.5%`.
- Degenerate high-degree nodes (celebrity problem): a list > 2^17 slots spills to a **continuation chain** of dedicated blocks; offsets store a spill marker.
- `ONE_*` rel tables skip CSR: single neighbor column, nullable.

## 5. Optional `REORDER` at bulk load

`COPY ... WITH (REORDER = degree | bfs | none)` relabels NodeIds before sealing groups (BFS from max-degree roots default). Gains: ~25%+ adjacency compression + traversal locality (WebGraph/LLP literature). The pk index makes relabeling invisible to users. LLP-class orderings deferred (hours at 10⁹ edges).

## 6. Zone maps & statistics

- Per segment: min/max/null (16-B truncated) → predicate skip at group and chunk level. The u64 min/max pair lives in `SegmentMeta` (directory version 5): an edge probe for a value outside a segment's zone answers absent from the directory alone without reading the payload, and the full-scan path cross-checks the zone against the decoded values the same way it cross-checks every chunk fence.
- Per table: row/tombstone counts; per rel table: degree histogram (log2 buckets, per direction), the lp norms of both degree sequences plus the sum over nodes of out-degree times in-degree, edge count; color summaries (§07) refreshed by `ANALYZE` and incrementally at checkpoint.

## 7. Primary-key index

Two-level static-hybrid design: a sealed level built at bulk load plus a small mutable overlay for the current delta (the overlay arrives with the updatable CSR). The sealed level stores the `(key, row)` pairs sorted by key as two ordinary MiniBlock segments in the group directory: the keys delta-pack tightly because they are sorted, and a lookup fence-searches the key segment for its one candidate chunk, decodes it, and reads the single matching row value, so it costs two chunk decodes however many keys exist and misses outside the key zone map cost nothing. `COPY ... REORDER` persists the original ids this way, which is what makes relabeling invisible to users. No global rebuild on insert once the overlay exists; a closed-addressing hash (~10 bits/key) remains the candidate for the overlay's sealed compaction if lookup latency ever needs the extra chunk decode removed.

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
