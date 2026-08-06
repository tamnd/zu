//! The `s3` storage engine.
//!
//! Immutable segment packs, manifest commit via conditional PUT, epoch
//! fencing, batched WAL objects, and a hybrid NVMe/RAM cache with a
//! request accountant that keeps the bill flat.
//! Protocol and object layout are specified in `docs/06-storage-s3.md`.
//! This M5 slice implements the manifest format and the CAS commit protocol with epoch fencing; WAL batching, segment packs, and caching land next.

mod manifest;
mod rt;
mod store;

pub use manifest::Manifest;
pub use store::{CURRENT_KEY, Current, ManifestStore};
