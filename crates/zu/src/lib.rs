//! zu: an embedded, in-process property-graph database.
//!
//! Columnar, vectorized, factorized, single-writer MVCC, with three storage
//! engines behind one trait: the native `zu1` file, SQLite, and S3.
//! The public API described in `docs/10-api-and-tooling.md` is
//! [`Database`] and [`Connection`], in [`db`]; the rest of what is
//! public here is the engine those two are built out of.
//!
//! Published on crates.io as `zudb` (the name `zu` is taken).

/// How a byte string is written down and read back, GV35: the `X'00AB'`
/// spelling a query writes one with and a result column hands one back
/// as.
pub use zu_common::bytes;
pub use zu_common::gqlstatus;
pub use zu_common::{
    C_ABI_VERSION, DiagnosticRecord, DurationKind, Epoch, FloatBits, GqlStatus, IntBits, Interrupt,
    LogicalType, NodeId, Position, Result, Severity, Temporal, ZuError,
};
/// The switches a statement runs under. [`query::run`] reads them from
/// the environment and [`query::run_with`] takes them, which is how one
/// statement asks for something the process cannot be told per
/// statement.
pub use zu_query::exec::{Engine, Options, Sink, Sip, Wcoj};
pub use zu_query::exec::{OpProfile, Profile, StageProfile, Streamed};
/// Registering a caller's own columns as a table of a connection: the
/// replacement scan a client's `register()` is built on.
pub use zu_query::frame::{Column, Frame, Layout};
pub use zu_query::params;
pub use zu_query::plan::{PlanNode, QueryPlan, ScalarPlan};
pub use zu_query::row::{Batch, Flow, FromRow, FromValue, Row, RowIter};
pub use zu_storage::{CheckpointMode, Direction, GraphStore, Snapshot};
pub use zu_zu1 as zu1;

pub use append::{Appender, Field};
pub use db::{Config, Connection, Database};

pub mod append;
pub mod catalog_stmt;
pub mod convert;
pub mod dataset;
pub mod db;
mod declare;
mod delete;
mod deleted;
#[cfg(test)]
mod exec2_tests;
mod insert;
pub mod list_text;
mod merge;
pub mod query;
pub mod session;
mod set;
pub mod shared;
pub mod snapshot;
mod split;
pub mod sqlite;
pub mod write;
#[cfg(test)]
mod write_parity;
