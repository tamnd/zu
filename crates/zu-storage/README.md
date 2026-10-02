# zudb-storage

The contract between the [zu](https://github.com/tamnd/zu) query engine and its storage engines.

zu has one query processor and three storage engines: the native single-file `zu1` format, an ordinary SQLite file, and object storage (`s3`). This crate is where the trait they are meant to share lives, so the query layer can be written once against it.

## What is in it

- `GraphStore` is a storage engine. It hands out the `Catalog`, takes a consistent `Snapshot` for reading, applies a `CommitBatch` and returns the new `Epoch`, runs a checkpoint, and ingests sealed node groups from a bulk load.
- `Snapshot` is a read view at one epoch. It returns a column segment for a node group (`scan_column`), the CSR adjacency for a node group in one `Direction` (`csr`), and looks up a node by primary key (`lookup_pk`).
- `Direction` (`Fwd` or `Bwd`) and `CheckpointMode` (`Normal` or `Full`).

Both traits are `Send + Sync`, and snapshots are handed out as `Arc<dyn Snapshot>`, so one store can serve readers on many threads while a single writer commits.

## Status

This crate is mostly a skeleton today, and it is worth being plain about that. `Catalog`, `CommitBatch`, `SealedNodeGroup`, `SegmentRef` and `CsrRef` are empty structs, and none of the three engines implements `GraphStore` yet. Each engine currently exposes its own API, and the `zudb` crate wires them to the query engine directly. What is used for real right now is `Direction`. The full trait surface is specified in `docs/02-architecture.md`, and this crate will grow into it as the engines converge on a shared buffer manager.

## Example

The trait is small enough to show whole. This is a store with nothing in it:

```rust
use std::sync::Arc;

use zu_common::{Epoch, NodeGroupId, NodeOffset, Result, TableId};
use zu_storage::{
    Catalog, CheckpointMode, CommitBatch, CsrRef, Direction, GraphStore, SealedNodeGroup,
    SegmentRef, Snapshot,
};

/// A store with nothing in it, to show the shape of the contract.
struct Empty;

impl Snapshot for Empty {
    fn scan_column(&self, _t: TableId, _g: NodeGroupId, _c: u32) -> Result<SegmentRef> {
        Ok(SegmentRef {})
    }
    fn csr(&self, _t: TableId, _g: NodeGroupId, _dir: Direction) -> Result<CsrRef> {
        Ok(CsrRef {})
    }
    fn lookup_pk(&self, _t: TableId, _key: &[u8]) -> Result<Option<NodeOffset>> {
        Ok(None)
    }
    fn epoch(&self) -> Epoch {
        0
    }
}

impl GraphStore for Empty {
    fn catalog(&self) -> Arc<Catalog> {
        Arc::new(Catalog::default())
    }
    fn snapshot(&self) -> Arc<dyn Snapshot> {
        Arc::new(Empty)
    }
    fn commit(&self, _batch: CommitBatch) -> Result<Epoch> {
        Ok(1)
    }
    fn checkpoint(&self, _mode: CheckpointMode) -> Result<()> {
        Ok(())
    }
    fn ingest(&self, _groups: Vec<SealedNodeGroup>) -> Result<Epoch> {
        Ok(1)
    }
}

fn main() -> Result<()> {
    let store: Arc<dyn GraphStore> = Arc::new(Empty);
    let snap = store.snapshot();
    assert_eq!(snap.lookup_pk(0, b"ada")?, None);
    store.checkpoint(CheckpointMode::Normal)
}
```

## Should you depend on this?

Probably not directly. This crate is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and all the zu crates are released together with exact version pins between them. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-storage` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_storage`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
