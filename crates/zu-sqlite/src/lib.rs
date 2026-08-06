//! The `sqlite` storage engine.
//!
//! Maps the graph onto an ordinary SQLite database file for interop and
//! durability, and doubles as the differential-testing oracle for zu1.
//! Schema mapping is specified in `docs/05-storage-sqlite.md`.
//! Implementation lands in M3; rusqlite (bundled) is added then.
