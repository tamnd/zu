//! Shared identifiers, errors, and constants used by every zu crate.
//!
//! The id layout is format-stable and documented in `docs/03-data-model.md`.

pub mod bytes;
pub mod decimal;
mod error;
pub mod gqlstatus;
mod id;
pub mod ids;
mod interrupt;
pub mod keywords;
mod order;
pub mod temporal;
pub mod types;
pub mod unicode;

pub use decimal::Decimal;
pub use error::ZuError;
pub use gqlstatus::{DiagnosticRecord, GqlStatus, Position, Severity};
pub use id::{Epoch, GROUP_ROWS, NodeGroupId, NodeId, NodeOffset, RelId, TableId};
pub use ids::{IdMap, IdSet};
pub use interrupt::Interrupt;
pub use order::int_key;
pub use temporal::{Clock, IntervalField, IntervalQualifier, Temporal};
pub use types::{
    DurationKind, Field, FloatBits, IntBits, LogicalType, PathType, PhysicalType, RecordType,
    type_by_name,
};

/// Result alias used across the workspace.
pub type Result<T> = std::result::Result<T, ZuError>;

/// The revision of the C ABI this workspace publishes (dx/02 §8).
///
/// Here rather than in `zu-capi` because the two things that report it
/// are the header, which is the ABI's specification, and `zu version`,
/// which is how a binding asks a build on the PATH what it is. Neither
/// should have to depend on the other to say one number, and a CLI that
/// linked the C ABI to read a string would export the whole of it.
/// `cargo xtask package` holds `ZU_ABI_VERSION` in the header to this.
pub const C_ABI_VERSION: &str = "0.15";
