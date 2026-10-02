# zudb-sqlite

The `sqlite` storage engine for zu: a graph stored in an ordinary SQLite database file.

There are two reasons it exists. A SQLite file can be opened, inspected and backed up with tools people already have, which matters where a custom format is a hard sell. And because it is simple and SQLite is well tested, it is the reference zu's native `zu1` engine is checked against: the same queries run on both, and the results have to match.

## How the graph maps onto SQLite

The mapping is specified in [`docs/05-storage-sqlite.md`](https://github.com/tamnd/zu/blob/main/docs/05-storage-sqlite.md). In this crate today:

- A node table `person` becomes `n_person (zrow INTEGER PRIMARY KEY, p_name ..., ...)`. `zrow` is the node id and each property column gets a `p_` prefix.
- A rel table `follows` becomes `r_follows (zrel INTEGER PRIMARY KEY, src, dst, p_...)` with two indexes, `(src, dst)` and `(dst, src)`, so neighbors in either direction are an index range scan.
- `zu_catalog` lists every table with a stable id, its kind, its DDL, the endpoint node tables of a rel table, and whether the edges are undirected. `zu_labels` holds any labels a node carries beyond its table name.
- The file is tagged with `application_id = 0x5A5531` (the bytes `ZU1`) and `user_version` is the schema version, currently 4. Older files are migrated on open. A file that belongs to some other application is rejected.
- Opening applies a fixed pragma profile: WAL journal, `synchronous=NORMAL`, 8 KiB pages, a 16 MiB page cache, no mmap, and a 5 second busy timeout.
- Types map onto SQLite's storage classes. Booleans, dates, times and durations are stored as integers, and the declared column type (`ColumnType`) records which one a column holds. List columns are stored as JSON text.

For traversals, `SqliteStore::csr` builds the adjacency of one node group (131,072 rows) in CSR form from the index and caches it. Each write bumps the version of only the groups it touches, so the cache is invalidated per group rather than per table, and a traversal over unchanged data runs off the cache instead of going back to SQLite.

Transactions are single-writer: `begin` is `BEGIN IMMEDIATE`, and `checkpoint` runs `wal_checkpoint(TRUNCATE)`. `epoch` gives a commit counter that moves on every write from any connection.

## Usage

`SqliteStore` is the whole API: open a file, create tables, write rows inside a transaction, and read properties and neighbors back.

```rust
use zu_sqlite::{ColumnType, Direction, SqliteStore, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut store = SqliteStore::open("social.db")?;
    store.create_node_table("person", &[("name", ColumnType::Text)])?;
    store.create_rel_table("follows", "person", "person", &[])?;

    store.begin()?;
    let ada = store.insert_node("person", &[Value::Text("ada".into())])?;
    let grace = store.insert_node("person", &[Value::Text("grace".into())])?;
    store.insert_rel("follows", ada, grace, &[])?;
    store.commit()?;

    let name = store.read_node_prop("person", grace, "name")?;
    let follows = store.neighbors("follows", ada, Direction::Fwd)?;
    println!("ada follows {follows:?}, which is {name:?}");

    store.checkpoint()?;
    Ok(())
}
```

This crate has no query language. To run zuQL against a SQLite file, `zudb` has `zudb::sqlite::run(statement, &store, params)`, which uses the same parser, planner and executor as the `zu1` engine. To move a graph between the two engines, use `zudb::convert::zu1_to_sqlite` and `zudb::convert::sqlite_to_zu1`, or `zu convert graph.zu1 graph.db` from the CLI.

SQLite is bundled through `rusqlite`, so nothing needs to be installed on the system.

## Status

`zudb::Database` opens `zu1` files only, so the SQLite engine is reached through `zudb::sqlite` and the conversion functions rather than through `Database`. It does not yet implement the `GraphStore` trait from `zudb-storage`. That waits on the shared buffer manager described in `docs/09`.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-sqlite` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_sqlite`.

## License

Apache-2.0
