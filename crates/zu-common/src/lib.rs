//! Shared identifiers, errors, and constants used by every zu crate.
//!
//! The id layout is format-stable and documented in `docs/03-data-model.md`.

mod error;
mod id;
mod order;

pub use error::ZuError;
pub use id::{Epoch, GROUP_ROWS, NodeGroupId, NodeId, NodeOffset, RelId, TableId};
pub use order::int_key;

/// Result alias used across the workspace.
pub type Result<T> = std::result::Result<T, ZuError>;
