# zudb-zu1

The `zu1` storage engine: zu's native format, where one `.zu1` file holds the whole graph.

When you call `zudb::Database::create("social.zu1")`, this is the crate that writes the file. It owns the bytes on disk: headers, blocks, column segments, adjacency, the catalog, and the write-ahead log next to the file. The query engine in `zudb-query` reads through it, and `zudb` puts the public API on top.

## How a file is laid out

The format is specified byte by byte in [`docs/04-storage-zu1-format.md`](https://github.com/tamnd/zu/blob/main/docs/04-storage-zu1-format.md). The short version:

- Block 0 holds a write-once 4 KiB file header (magic `図ZU1`, format version, block size) and two 4 KiB database header slots. Everything after that is fixed 32 KiB blocks.
- Opening reads those 12 KiB and picks the valid database header with the highest epoch. New data always goes to free blocks, and a checkpoint publishes it by writing the other header slot. A torn header write leaves the previous one intact, so committed state is never damaged.
- The database header points at a few roots: the catalog (node and rel tables), the table index (where each table's data lives), statistics, and the free list. Each root is a meta chain, a linked list of checksummed blocks.
- Rows live in node groups of 131,072. Each property column is a segment. Fixed-width values use the MiniBlock layout: chunks of 1024 values, each compressed on its own with the cascades from `zudb-encoding`, so a point read only decodes the chunk it needs. Variable-width values (strings, blobs, lists) use FullZip, which keeps any row range contiguous and makes seeking to a row cheap. Byte rows of equal length use Stride.
- Edges are stored as CSR per node group, once keyed by source and once keyed by destination, so out-neighbors and in-neighbors are both direct reads.

## Modules

| Module | What it does |
| --- | --- |
| `file` | `Zu1File`: headers, block I/O, the block cache hookup, the dual-header checkpoint, savepoints |
| `meta` | Meta chains behind every root pointer |
| `catalog` | Table definitions and the table index |
| `segment`, `fullzip`, `rows` | The MiniBlock, FullZip and Stride column layouts |
| `graph` | Bulk-loaded CSR adjacency, edge list readers, `GraphReader` |
| `props` | Node and edge property columns with their logical types |
| `keys`, `reorder` | Primary-key index, and relabeling node ids by degree or BFS before a load so neighbors get close ids |
| `wal`, `txn`, `epoch` | Redo-only WAL, the single-writer commit path with MVCC overlays, and the snapshot epochs readers pin |
| `fold` | The checkpoint that folds overlays into new sealed segments and publishes them with one header flip |
| `ingest` | Bulk loads that write segments directly and log only a reference to them |
| `cache`, `vfs` | The shared block cache, and the file shim that the crash harness and in-memory databases swap in for the real filesystem |
| `stats`, `colors` | Degree histograms and color summaries the optimizer uses for cardinality estimates |
| `algo` | Whole-graph kernels over the CSR: BFS, PageRank, WCC, SSSP, CDLP, LCC, triangle count, Louvain |
| `parquet` | Parquet edge lists, behind the `arrow` feature |

The crate also exports `verify(path)`, which walks every checksum in a file and cross-checks the structures against each other, and `layout(path)`, which tells you how much of a file is schema, free space, and graph. These are what `zu verify` and `zu stat` print.

## Usage

Most code should use `zudb`, which runs queries against a zu1 file and handles transactions for you. Using this crate directly is for tools that work with the file itself, like bulk loaders and checkers. Here is a bulk load of an edge list and a neighbor read:

```rust
use std::path::Path;

use zu_zu1::file::Zu1File;
use zu_zu1::graph::{GraphReader, bulk_load_as};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("follows.zu1");

    // Five people, and who follows whom, as dense row ids.
    let mut edges = vec![(0, 1), (0, 2), (1, 2), (3, 0), (4, 3)];
    edges.sort_unstable();

    let mut db = Zu1File::create(path)?;
    bulk_load_as(&mut db, "person", "follows", 5, &edges)?;

    let mut reader = GraphReader::load_table(&mut db, "follows")?;
    println!("0 follows {:?}", reader.neighbors(&mut db, 0)?);

    let bytes = zu_zu1::verify(path)?;
    let layout = zu_zu1::layout(path)?;
    println!("verified {bytes} bytes, {} blocks of graph", layout.data_blocks);
    Ok(())
}
```

`zudb` re-exports this crate as `zudb::zu1`, so if you already depend on `zudb` you can reach these types without adding this crate.

## Status

This is the most complete of the three engines and the one `zudb::Database` uses. The format version is 1. Until zu reaches 1.0 the format can still change between releases, and `verify` is the tool to run if you suspect a file.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-zu1` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_zu1`.

## License

Apache-2.0
