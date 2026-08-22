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

use crate::gqlstatus::{DiagnosticRecord, GqlStatus, Position, Subject};

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

    /// The same, raised at a byte offset into the statement text.
    ///
    /// The front end raises through this one rather than through
    /// [`Self::gql_at`], because the source is what turns an offset
    /// into a line and a column and it is also the only way to quote
    /// the line back. A caller three layers up has the message and the
    /// numbers and no longer has the text.
    pub fn gql_in(
        status: GqlStatus,
        source: &str,
        offset: usize,
        detail: impl std::fmt::Display,
    ) -> Self {
        ZuError::Gql(Box::new(DiagnosticRecord::in_source(
            status, source, offset, detail,
        )))
    }

    /// The same error, about a thing the statement named.
    ///
    /// A builder rather than a fourth constructor, so that a raise site
    /// adds a subject without changing which of the three it went
    /// through. It does nothing to an engine-internal failure, which
    /// has no record to put a subject on.
    pub fn about(mut self, subject: Subject) -> Self {
        if let ZuError::Gql(record) = &mut self {
            record.subject = Some(subject);
        }
        self
    }

    /// What the condition is about, when it is about something named.
    pub fn subject(&self) -> Option<&Subject> {
        match self {
            ZuError::Gql(record) => record.subject.as_ref(),
            _ => None,
        }
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

    /// The line of the statement this was raised on, when the statement
    /// text was in hand where it was raised.
    pub fn excerpt(&self) -> Option<&str> {
        match self {
            ZuError::Gql(record) => record.excerpt.as_deref(),
            _ => None,
        }
    }

    /// Whether running the same thing again could succeed, which is the
    /// one question a caller writing a retry loop is asking.
    ///
    /// A lost compare-and-swap is the case: a concurrent writer got
    /// there first, so the work was not applied and the same call on a
    /// fresh read may well win. A condition answers for itself, which
    /// is `40000 transaction rollback` and nothing else. Everything
    /// remaining is false, including an interruption, which is not the
    /// engine failing but the caller having asked it to stop.
    pub fn retryable(&self) -> bool {
        match self {
            ZuError::Gql(record) => record.retryable(),
            ZuError::Conflict(_) => true,
            _ => false,
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

    #[test]
    fn an_error_raised_in_a_statement_carries_the_line_it_was_raised_on() {
        let source = "MATCH (n)\nRETURN n.x + ";
        let offset = source.find("n.x").expect("n.x");
        let e = ZuError::gql_in(codes::C42001, source, offset, "expected an expression");
        assert_eq!(e.excerpt(), Some("RETURN n.x + "));
        let at = e.position().expect("a place");
        assert_eq!((at.line, at.column), (2, 8));
        assert_eq!(&source[at.offset as usize..][..3], "n.x");
        assert_eq!(
            e.to_string(),
            "42001: line 2, column 8: expected an expression"
        );
    }

    #[test]
    fn only_the_failures_worth_repeating_say_so() {
        // A writer that lost the race did not apply its write, so the
        // same call on a fresh read is a reasonable next move.
        assert!(ZuError::Conflict("lost the swap".into()).retryable());
        assert!(ZuError::gql(codes::C40000, "rolled back").retryable());
        // Text that will not parse will not parse again, and a caller
        // that asked for a stop got one.
        assert!(!ZuError::gql(codes::C42001, "bad syntax").retryable());
        assert!(!ZuError::Interrupted.retryable());
        assert!(
            !ZuError::Corrupt {
                what: "block",
                detail: "bad crc".into(),
            }
            .retryable()
        );
    }
}
