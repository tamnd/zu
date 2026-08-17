//! Workspace-wide error type.
//!
//! Every error carries a stable `ZU####` code once the catalog in
//! `docs/errors.md` exists. Variants are added as subsystems land.
//!
//! Anything a GQL statement can raise goes through [`ZuError::Gql`],
//! which carries an ISO/IEC 39075 GQLSTATUS value rather than only a
//! message. The other variants are engine-internal: an io error opening
//! a file is not a condition the standard has a code for, and inventing
//! one would be worse than admitting it.

use crate::gqlstatus::{DiagnosticRecord, GqlStatus, Position};

/// Top-level error for all zu operations.
#[derive(Debug, thiserror::Error)]
pub enum ZuError {
    /// A GQL condition, with the standard's code and our own detail.
    #[error("{0}")]
    Gql(Box<DiagnosticRecord>),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("corrupt {what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    #[error("unsupported {what} id {id}")]
    Unsupported { what: &'static str, id: u32 },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A compare-and-swap or ownership check lost to a concurrent writer.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller asked the statement to stop and it did, at the next
    /// boundary the executor checks ([`crate::Interrupt`]). No condition
    /// code, because the standard has none for this and the statement
    /// did not fail: it was stopped, which is a fact about the caller
    /// rather than about the query.
    #[error("interrupted")]
    Interrupted,
}

impl ZuError {
    /// Raises `status` with our own account of what happened.
    pub fn gql(status: GqlStatus, detail: impl Into<String>) -> Self {
        ZuError::Gql(Box::new(DiagnosticRecord::new(status, detail)))
    }

    /// The same, raised at a place in the statement text. The message
    /// reads as it did before, with the line and column ahead of the
    /// detail, and the pair is also kept as a pair so that a caller can
    /// point at the token instead of parsing the sentence.
    pub fn gql_at(status: GqlStatus, position: Position, detail: impl std::fmt::Display) -> Self {
        ZuError::Gql(Box::new(DiagnosticRecord::at(status, position, detail)))
    }

    /// Where in the statement this was raised, when it was raised
    /// somewhere. An engine-internal failure and a runtime condition
    /// both answer `None`.
    pub fn position(&self) -> Option<Position> {
        match self {
            ZuError::Gql(record) => record.position,
            _ => None,
        }
    }

    /// The GQLSTATUS value for this error, if it has one. Engine-internal
    /// errors return `None` rather than a code that would be a guess.
    pub fn gqlstatus(&self) -> Option<GqlStatus> {
        match self {
            ZuError::Gql(record) => Some(record.status),
            _ => None,
        }
    }

    /// The diagnostic record for this error, if it has one.
    pub fn diagnostic(&self) -> Option<&DiagnosticRecord> {
        match self {
            ZuError::Gql(record) => Some(record),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gqlstatus::codes;

    #[test]
    fn a_gql_error_keeps_its_code_and_reads_as_our_detail() {
        let e = ZuError::gql(codes::C42001, "line 1, column 1: expected MATCH");
        assert_eq!(e.gqlstatus(), Some(codes::C42001));
        assert_eq!(e.to_string(), "42001: line 1, column 1: expected MATCH");
    }

    #[test]
    fn an_engine_error_has_no_code_rather_than_a_guessed_one() {
        let e = ZuError::Corrupt {
            what: "block",
            detail: "bad crc".into(),
        };
        assert_eq!(e.gqlstatus(), None);
        assert!(e.diagnostic().is_none());
    }
}
