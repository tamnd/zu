//! GQLSTATUS: the ISO/IEC 39075:2024 condition codes and the diagnostic
//! record that carries one back to a caller (Spec/2064g/gql/plan/07).
//!
//! Every condition GQL defines is a five-character code, a two-character
//! class and a three-character subclass, and the standard fixes both the
//! code and the natural-language name that goes with it. Those names are
//! not ours to invent, so the table in `generated.rs` is machine-written
//! from `artifacts/gql-conditions.xml`, the digital artifact published
//! alongside the standard. `tests/gqlstatus_table.rs` regenerates it and
//! fails on drift.
//!
//! A [`GqlStatus`] is an index into that table, so it is two bytes, it is
//! `Copy`, and a code that is not in the standard cannot be constructed.
//! That last property is the point: an engine that answers with a code it
//! made up has not raised a condition, it has printed a string.
//!
//! The conformance denominator is the 68 subclass rows. The table also
//! carries the four class-only codes (`02000`, `03000`, `2D000`,
//! `G2000`), which are real GQLSTATUS values but are not separately
//! counted, so the totals here are 72 and 68 rather than one number.

mod generated;

use std::fmt;

use generated::CONDITIONS;
pub use generated::codes;

/// What a condition means for the statement that raised it, taken from
/// the class category letter in the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// `S`: the statement succeeded.
    Success,
    /// `N`: the statement succeeded and returned nothing.
    NoData,
    /// `W`: the statement succeeded and has something to say about it.
    Warning,
    /// `I`: informational.
    Informational,
    /// `X`: the statement did not succeed.
    Exception,
}

impl Severity {
    /// True when the statement kept its result. Warnings ride along with
    /// an answer; exceptions replace it.
    pub const fn is_success(self) -> bool {
        !matches!(self, Severity::Exception)
    }
}

/// One row of the standard's condition table.
#[derive(Debug, Clone, Copy)]
pub struct Condition {
    /// The five-character GQLSTATUS value, for example `22012`.
    pub code: &'static str,
    pub severity: Severity,
    /// The class name, for example `data exception`.
    pub class: &'static str,
    /// The subclass name, for example `division by zero`. `None` on the
    /// four class-only rows.
    pub subclass: Option<&'static str>,
}

/// A GQLSTATUS value. Constructible only from the generated table, so
/// every value of this type is a code the standard defines.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GqlStatus(u16);

impl GqlStatus {
    /// Looks a code up by its five characters. Returns `None` for
    /// anything the standard does not define, which is how a code
    /// arriving from outside zu gets checked rather than trusted.
    pub fn from_code(code: &str) -> Option<Self> {
        CONDITIONS
            .iter()
            .position(|c| c.code == code)
            .map(|i| GqlStatus(i as u16))
    }

    /// The row this status names.
    pub fn condition(self) -> &'static Condition {
        &CONDITIONS[self.0 as usize]
    }

    pub fn code(self) -> &'static str {
        self.condition().code
    }

    pub fn severity(self) -> Severity {
        self.condition().severity
    }

    /// The standard's own words for this condition: the class name, and
    /// the subclass name after it when there is one. This is the text a
    /// conformance harness compares against, so zu never paraphrases it.
    /// Anything zu wants to add goes in [`DiagnosticRecord::detail`].
    pub fn standard_text(self) -> String {
        let c = self.condition();
        match c.subclass {
            Some(sub) => format!("{}, {sub}", c.class),
            None => c.class.to_string(),
        }
    }
}

impl fmt::Debug for GqlStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GqlStatus({})", self.code())
    }
}

impl fmt::Display for GqlStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// What zu hands back when a condition is raised: the standard's code and
/// zu's own account of what happened.
///
/// The two are kept apart on purpose. `status` is comparable across
/// engines and is what a conformance run grades. `detail` is ours and can
/// say whatever is most useful, including the line and column, without
/// putting that in the field a harness matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticRecord {
    pub status: GqlStatus,
    pub detail: String,
}

impl DiagnosticRecord {
    pub fn new(status: GqlStatus, detail: impl Into<String>) -> Self {
        DiagnosticRecord {
            status,
            detail: detail.into(),
        }
    }

    pub fn severity(&self) -> Severity {
        self.status.severity()
    }
}

impl fmt::Display for DiagnosticRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}: {}", self.status.code(), self.status.standard_text())
        } else {
            write!(f, "{}: {}", self.status.code(), self.detail)
        }
    }
}

/// Every condition the standard defines, in code order.
pub fn all() -> &'static [Condition] {
    CONDITIONS
}

/// The 68 subclass rows, which are the conformance denominator.
pub fn subclass_rows() -> impl Iterator<Item = &'static Condition> {
    CONDITIONS.iter().filter(|c| c.subclass.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_standard_table() {
        assert_eq!(CONDITIONS.len(), 72, "72 GQLSTATUS values in the artifact");
        assert_eq!(
            subclass_rows().count(),
            68,
            "68 subclass rows, the conformance denominator"
        );
    }

    #[test]
    fn codes_are_unique_and_sorted() {
        let mut seen: Vec<&str> = CONDITIONS.iter().map(|c| c.code).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "duplicate GQLSTATUS code in the table");
        let codes: Vec<&str> = CONDITIONS.iter().map(|c| c.code).collect();
        assert_eq!(codes, seen, "table is not in code order");
    }

    #[test]
    fn lookup_round_trips_and_rejects_inventions() {
        for c in CONDITIONS {
            let status = GqlStatus::from_code(c.code).expect("code in table");
            assert_eq!(status.code(), c.code);
        }
        assert!(GqlStatus::from_code("99999").is_none());
        assert!(GqlStatus::from_code("ZU001").is_none());
        assert!(GqlStatus::from_code("").is_none());
    }

    #[test]
    fn severity_follows_the_class_letter() {
        assert_eq!(codes::C00001.severity(), Severity::Success);
        assert_eq!(codes::C01G11.severity(), Severity::Warning);
        assert_eq!(codes::C02000.severity(), Severity::NoData);
        assert_eq!(codes::C22012.severity(), Severity::Exception);
        assert!(codes::C01G11.severity().is_success());
        assert!(!codes::C42001.severity().is_success());
    }

    #[test]
    fn standard_text_is_the_standards_words() {
        assert_eq!(
            codes::C22012.standard_text(),
            "data exception, division by zero"
        );
        assert_eq!(
            codes::C42001.standard_text(),
            "syntax error or access rule violation, invalid syntax"
        );
        // A class-only row has no comma to append.
        assert_eq!(codes::CG2000.standard_text(), "graph type violation");
    }

    #[test]
    fn a_record_prefers_our_detail_but_never_loses_the_code() {
        let bare = DiagnosticRecord::new(codes::C22012, "");
        assert_eq!(bare.to_string(), "22012: data exception, division by zero");
        let ours = DiagnosticRecord::new(codes::C42001, "line 1, column 7: expected MATCH");
        assert_eq!(ours.to_string(), "42001: line 1, column 7: expected MATCH");
        assert_eq!(ours.status.code(), "42001");
    }
}
