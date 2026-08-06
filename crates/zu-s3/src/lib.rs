//! The `s3` storage engine.
//!
//! Immutable segment packs, manifest commit via conditional PUT, epoch
//! fencing, batched WAL objects, and a hybrid NVMe/RAM cache with a
//! request accountant that keeps the bill flat.
//! Protocol and object layout are specified in `docs/06-storage-s3.md`.
//! Implementation lands in M5; object_store and foyer are added then.
