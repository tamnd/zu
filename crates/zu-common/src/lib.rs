//! Shared identifiers, errors, and constants used by every zu crate.
//!
//! The id layout is format-stable and documented in `docs/03-data-model.md`.

mod error;
pub mod gqlstatus;
mod id;
mod order;
pub mod temporal;
pub mod types;

pub use error::ZuError;
pub use gqlstatus::{DiagnosticRecord, GqlStatus, Severity};
pub use id::{Epoch, GROUP_ROWS, NodeGroupId, NodeId, NodeOffset, RelId, TableId};
pub use order::int_key;
pub use temporal::Temporal;
pub use types::{
    DurationKind, Field, FloatBits, IntBits, LogicalType, PathType, PhysicalType, RecordType,
    type_by_name,
};

/// Result alias used across the workspace.
pub type Result<T> = std::result::Result<T, ZuError>;
