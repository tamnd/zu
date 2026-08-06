//! Workspace-wide error type.
//!
//! Every error carries a stable `ZU####` code once the catalog in
//! `docs/errors.md` exists. Variants are added as subsystems land.

/// Top-level error for all zu operations.
#[derive(Debug, thiserror::Error)]
pub enum ZuError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("corrupt {what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    #[error("unsupported {what} id {id}")]
    Unsupported { what: &'static str, id: u32 },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}
