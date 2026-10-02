# zudb-s3

The `s3` storage engine for zu: a graph that lives in object storage (S3, GCS, Azure Blob, R2, MinIO and anything else `object_store` supports), with a fixed number of requests per second no matter how busy the writers are.

Most of this engine is not built yet. This README describes the design and then says exactly which part exists today.

## The design

The full protocol is in [`docs/06-storage-s3.md`](https://github.com/tamnd/zu/blob/main/docs/06-storage-s3.md). The idea is to borrow what SlateDB, WarpStream and Quickwit learned about object storage:

- Data lives in immutable segment packs: many `zu1` format segments in one 8 to 64 MiB object, with a footer that indexes them, so a cold read of any segment is one ranged GET.
- Writes go to a WAL buffer that is flushed as one object every 100 ms or every 4 MiB, whichever comes first. That caps PUTs at about 10 per second however many transactions there are, which is what keeps the monthly bill flat.
- A manifest object is the root of each database state. A commit writes a new manifest and swings a pointer to it with a conditional PUT, so a half-finished commit is never visible.
- There is one writer at a time, enforced by epochs rather than leases or a lock service. A new writer takes over by committing the next epoch. After that, every conditional PUT from the old writer fails, so it can't overwrite anything.
- Reads go through a RAM and local NVMe cache, with the adjacency and footers pinned so a traversal does not go to S3 one node at a time.

## What exists today

The manifest format and the commit protocol with epoch fencing. That is the piece everything else depends on, because it decides who is allowed to write and what the current state is.

- `Manifest` is one snapshot: an `epoch`, the `writer_id` allowed to produce the next epoch, and the object keys of its segment packs. It encodes to a small versioned binary format (magic `ZUS3`, little endian, CRC32C at the end). Decoding never panics: truncated or corrupt bytes give an error.
- `ManifestStore` reads and commits manifests for one writer. Every commit first writes an immutable snapshot at `manifest/{epoch}.zum`, then replaces `manifest/CURRENT` with a conditional PUT. `CURRENT` holds the whole manifest rather than a pointer, so opening a database is one GET.
- `commit` requires the epoch to go up by exactly one and the writer to be the one the current manifest names. Losing a race, or being fenced by a newer writer, returns a `Conflict` error.
- `take_over` makes this store the writer: it republishes the current segment set under the next epoch with its own `writer_id`.

The crate has no async runtime of its own. It drives `object_store`'s futures on the calling thread, so you can call it from synchronous code.

Not built yet: the WAL objects and their flusher, segment packs, the checkpoint that folds the WAL into packs, garbage collection, and the cache. `zudb::Database` cannot open an S3 database yet.

## Usage

You bring an `object_store` backend. This example uses the in-memory one; for S3 you would build an `AmazonS3` store the same way and pass it in.

```rust
use std::sync::Arc;

use object_store::memory::InMemory;
use zu_s3::{Manifest, ManifestStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bucket = Arc::new(InMemory::new());

    // The first writer creates the database at epoch 0.
    let a = ManifestStore::new(bucket.clone(), 1);
    let first = Manifest { epoch: 0, writer_id: 1, segments: vec![] };
    let mut current = a.commit(&first, None)?;

    // Each commit advances the epoch by one.
    let next = Manifest { epoch: 1, writer_id: 1, segments: vec!["seg/0001.zuseg".into()] };
    current = a.commit(&next, Some(&current))?;

    // A second writer takes over, and from then on the first one is fenced.
    let b = ManifestStore::new(bucket.clone(), 2);
    let taken = b.take_over()?;
    println!("writer {:#x} owns epoch {}", taken.manifest.writer_id, taken.manifest.epoch);

    let stale = Manifest { epoch: 2, writer_id: 1, segments: vec![] };
    assert!(a.commit(&stale, Some(&current)).is_err());
    Ok(())
}
```

The backend has to support conditional PUTs. S3 (since late 2024), GCS, Azure and R2 all do.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and the versions of all the zu crates move together. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-s3` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_s3`.

## License

Apache-2.0
