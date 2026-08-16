//! zu: an embedded, in-process property-graph database.
//!
//! Columnar, vectorized, factorized, single-writer MVCC, with three storage
//! engines behind one trait: the native `zu1` file, SQLite, and S3.
//! The public API described in `docs/10-api-and-tooling.md` is
//! [`Database`] and [`Connection`], in [`db`]; the rest of what is
//! public here is the engine those two are built out of.
//!
//! Published on crates.io as `zudb` (the name `zu` is taken).

pub use zu_common::gqlstatus;
pub use zu_common::{DiagnosticRecord, Epoch, GqlStatus, NodeId, Result, Severity, ZuError};
pub use zu_storage::{CheckpointMode, Direction, GraphStore, Snapshot};
pub use zu_zu1 as zu1;

pub use append::{Appender, Field};
pub use db::{Config, Connection, Database};

pub mod append;
pub mod catalog_stmt;
pub mod convert;
pub mod db;
#[cfg(test)]
mod exec2_tests;
pub mod list_text;
pub mod query;
pub mod session;
pub mod snapshot;
pub mod sqlite;
