# 03. Data Model

## 1. Model

Typed property graph (schema-full, like Kùzu/relational; unlike Neo4j's schema-optional):

- **Node tables**: `CREATE NODE TABLE Person(id INT64 PRIMARY KEY, name STRING, born DATE)`. One *primary* table per node; a node lives in exactly one node table. `INSERT (c:City {name: 'york', founded: 71})` where nothing is named City makes the table too, for the same reason a `SET` declares a label: GQL has no statement that makes one and the pattern means a table by the name the way it means one by a name the graph has. The columns come from the properties the pattern writes and their types from the values, so a value the plan would have to work out first (`founded: 70 + 1`) is refused rather than guessed at, since a guessed column is written down and the next statement is typed against it. A null is refused the same way, because it says what the row holds rather than what the column does. The table is made under the savepoint the statement holds, so a statement that raises after it, or a rolled back transaction, leaves no table behind.
- **Secondary labels** (the fix for Kùzu's #1 modeling gap): a node may carry additional labels from a per-graph label dictionary held in the catalog, a name at the position of its id, at most 64 of them because a node's label set is one word. A node table's own name is a label and is the first one it declares, carried by every row; `ALTER TABLE Person ADD LABEL Employee` declares another and `SET n:Employee` assigns it. `SET n:Employee` on a table that has not declared `Employee` declares it too, because GQL has no statement that declares a label and a row of the table now carries one: the declaration is published with the rows the statement changed and is undone with them, so a rolled back transaction leaves neither. `REMOVE n:Employee` declares nothing, since a label a table never declared is one no row of it carries and there is nothing to take off. The set a row carries is a bitset segment hanging off the node table's props directory, one word per row, not a user column: nothing can name it in a query and nothing can drop it with `DROP COLUMN`. A table that declares only its own name stores no bitset at all and the catalog is the whole answer. `MATCH (n:Employee)` narrows to the node tables that declare the label and tests the bit on the rows they hold; a label a table gives every row needs no test, so `MATCH (n:Person)` plans as a bare scan (§04 §3). A pattern may write a label *expression* rather than one name: `&` and `|` join, `!` negates, `%` holds of every element, parentheses group, and a repeated colon is the conjunction it has always been, so `(n:A:B)` and `(n:A&B)` are one pattern. The binder folds the expression against each table's declared set in three-valued logic, where a bit the table never declares is false and a bit it gives every row is true: a table the expression cannot hold on is dropped, a table it holds on throughout keeps its bare scan, and only what is left over reaches the row as a bit test. The same fold runs again once the relationship types have narrowed the endpoints, so `MATCH (n:Person|Employee)-[:KNOWS]->(m)` tests nothing at all when `KNOWS` starts at `Person`.
- **Rel(ationship) tables**: `CREATE REL TABLE Follows(FROM Person TO Person, since DATE)`. Directed unless the table says otherwise: an undirected rel table (GH02) holds edges whose ends are not a from and a to, and it stores them the way a directed one does, once, with both adjacency indexes answering for each, so an undirected edge costs a directed table nothing and the pattern half is where the difference shows (§07 §1). Multi-edges allowed (an internal `rel_offset` disambiguates), with one exception the storage makes: an edge property column is indexed by the edge's position in the load order, and that position is found from the endpoints, so a rel table that stores properties holds each `(from, to)` pair once and a load that would repeat one is refused (§04 §4). A rel table may declare multiple (FROM, TO) pairs (`FROM Person TO Person, FROM Person TO Org`), each pair is an internal sub-table sharing the property schema.
- Cardinality hints `MANY_MANY` (default) / `ONE_MANY` / `MANY_ONE` / `ONE_ONE` drive storage choice (single-column layout for *_ONE) and optimizer stats.

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

## 6. Graph types

ISO 39075 describes a graph's shape with a *graph type*: a set of element types, each of which says what a node or an edge in the graph may look like. zu carries them in the catalog beside the tables (§04 §1).

- **Element types** name a kind (node or edge), a label set, a key label set, and the properties an element of that type holds. An edge type also names the element types at its ends and whether it is undirected. `open` on an element type means an element may carry properties the type does not declare, which is how a schema-optional store behaves; a type that is not open holds exactly what it declares.
- **Key label sets** are what an element is matched to its type by, and there are three cases the catalog keeps apart because they select differently. A *declared* key is the one the user wrote, so `(:Person => Person KEY)` matches on `Person` alone even when the element carries more. An *inferred* key is the whole label set, which is what a type with no `KEY` clause gets. *None* is an element type nothing keys on, which is what an edge type inferred from a rel table has, and an empty key label set is contained in every label set, so such a type is selected by its endpoints rather than by its labels.
- A **closed graph type** is the whole answer about the graph: every element belongs to a type it names, and a property the graph type does not declare is not a property the graph has, so the optimizer resolves a property reference to a slot at plan time rather than looking it up per row. An **open graph type** names what it knows and admits elements it does not describe, so property resolution stays dynamic. A file whose tables were made with `CREATE NODE TABLE` and never given a graph type has one inferred for it: a closed type with a node element type per node table keyed on the table's own label, and an edge element type per rel table named by its endpoints.
- A closed graph type is a promise the writes keep, so it is checked where a write decides what an element carries rather than where a read looks. A `SET` or `REMOVE` of a label knows the label word the row ends with, and if no element type of the graph type is selected by that word the statement is refused with `G2000` naming the row, the table and the set it would have carried. Selection is the standard's: an element type holds an element when its key label set is contained in the element's labels and every label the element carries is one the type declares, so a label set that adds something the type never named is outside the type even when a type keys on part of it. An `INSERT` whose label names no node table (§1) makes no table in such a graph for the same reason, since the type has already said what the graph holds, and the message says whether the type describes that label and disagrees about the properties or says nothing about it at all. A graph with an open type, or with no type, promises nothing and takes the change.
- Two element types may share a key label set and disagree about the rest, which is the relaxed consistency the standard allows. It costs nothing here because property declarations hang off the element type rather than off the table, so the two types are two declaration lists that happen to select on the same labels.
