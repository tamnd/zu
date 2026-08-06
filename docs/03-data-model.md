# 03. Data Model

## 1. Model

Typed property graph (schema-full, like Kùzu/relational; unlike Neo4j's schema-optional):

- **Node tables**: `CREATE NODE TABLE Person(id INT64 PRIMARY KEY, name STRING, born DATE)`. One *primary* table per node; a node lives in exactly one node table.
- **Secondary labels** (the fix for Kùzu's #1 modeling gap): a node may carry additional labels from a per-database label dictionary. `ALTER TABLE Person ADD LABEL Employee` declares; `SET n:Employee` assigns. Stored as a bitset column (`__labels`, ≤ 256 labels v1) on the node table, cheap to scan/filter, no second table. `MATCH (n:Employee)` unions the node tables that declare the label, with a zone-map-accelerated bitset filter.
- **Rel(ationship) tables**: `CREATE REL TABLE Follows(FROM Person TO Person, since DATE)`. Directed; multi-edges allowed (an internal `rel_offset` disambiguates). A rel table may declare multiple (FROM, TO) pairs (`FROM Person TO Person, FROM Person TO Org`), each pair is an internal sub-table sharing the property schema.
- Cardinality hints `MANY_MANY` (default) / `ONE_MANY` / `MANY_ONE` / `ONE_ONE` drive storage choice (single-column layout for *_ONE) and planner stats.

## 2. Identifiers

```
NodeId  = u64:  [ table_id: 14 bits | node_group: 22 bits | row: 17 bits ]  (+11 spare)
RelId   = u64:  same shape over the rel table's CSR position (group, slot)
Epoch   = u64   monotonically increasing commit sequence number
```

- `row` = position within a node group of 2^17 = **131,072** rows (§04). 22 bits of groups × 2^17 rows = 2^39 ≈ 550 B nodes per table; 2^14 tables. All limits are format-versioned constants, not hard-coded truths.
- NodeIds are **dense, internal, and stable within a snapshot**; user-facing identity is the PRIMARY KEY. Deletes leave tombstones; offsets are recycled only by `VACUUM`/re-ingest (which rewrites and may re-order).
- Primary-key → offset lookup: per-table hash index (space-efficient two-level: per-node-group buckets so it pages in lazily; §04 §7).

## 3. Value types (v1)

| Type | Storage | Notes |
|---|---|---|
| BOOL | bitpacked | |
| INT8/16/32/64 | FOR+bitpack / delta | INT64 default integer |
| UINT64 | same | internal use exposed |
| DOUBLE, FLOAT | ALP, ALP_RD fallback | |
| DECIMAL(p,s) ≤ 128b | int128 + FOR | exact money |
| STRING | dict + FSST, offsets mini-block | UTF-8 |
| BLOB | full-zip | |
| DATE / TIMESTAMP / INTERVAL | int32/int64 days/µs + delta | UTC canonical |
| UUID | 2×u64 bitpack | |
| LIST<T>, MAP<K,V>, STRUCT | offsets + child columns (Arrow-style) | nesting ≤ 8 |
| VECTOR(dim, f32|f16|i8) | full-zip, 64-B aligned | feature `vector` |
| NULL | validity bitmap per segment | any type nullable |

Type system mirrors Arrow so `arrow` feature interop is zero-copy for fixed-width columns and offset-compatible for strings/lists.

## 4. Schema evolution

- `ADD COLUMN` (default value materialized lazily, constant segment), `DROP COLUMN` (metadata-only tombstone), `RENAME`, `ADD LABEL`.
- Rel endpoint changes and PK changes require table rewrite (explicit `ALTER ... REBUILD`).
- Catalog versioned per commit epoch; readers use the snapshot's catalog.

## 5. Constraints (v1)

- PRIMARY KEY (unique, non-null) on node tables, enforced via pk index at commit time.
- Rel endpoint referential integrity: inserting a rel with a nonexistent endpoint fails; deleting a node with incident edges fails unless `DETACH DELETE`.
- No user-defined unique/check constraints in v1 (documented deferral).
