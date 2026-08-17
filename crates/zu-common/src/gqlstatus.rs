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
//! carries one row per class, the class code followed by `000`, because
//! that is a GQLSTATUS value too and it is the one a statement reports
//! when nothing in particular happened: `00000 successful completion`.
//! The artifact only writes those rows out for the four classes that
//! have no subclasses, so the generator synthesises the other eight. That
//! is why the totals here are 80 and 68 rather than one number.

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
    /// twelve class rows, whose code ends in `000`.
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

/// Where in the statement text a condition was raised. Both are 1-based
/// and columns count characters rather than bytes, so a line of
/// multi-byte text does not read as wider than it looks.
///
/// This is a pair rather than a byte offset because a byte offset is
/// only useful to something holding the source, and the caller that
/// most wants a position is an editor or a shell that has already
/// printed the line and wants to point under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl Position {
    /// The pair `offset` bytes into `source`. An offset past the end
    /// lands at the end, which is where an error about a query that
    /// stopped too early belongs.
    pub fn of(source: &str, offset: usize) -> Self {
        let mut line = 1u32;
        let mut column = 1u32;
        for (ix, ch) in source.char_indices() {
            if ix >= offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        Position { line, column }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// What zu hands back when a condition is raised: the standard's code and
/// zu's own account of what happened.
///
/// The two are kept apart on purpose. `status` is comparable across
/// engines and is what a conformance run grades. `detail` is ours and can
/// say whatever is most useful, including the line and column, without
/// putting that in the field a harness matches on.
///
/// `position` is that line and column again, as a pair. The detail keeps
/// saying it because a message a user reads should be complete on its
/// own, and the pair exists because a caller that wants to underline the
/// offending token should not have to parse English back into numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticRecord {
    pub status: GqlStatus,
    pub detail: String,
    /// Where in the statement, for the conditions that happen somewhere.
    /// A division by zero happens at runtime and has no token to point
    /// at, so this is `None` rather than a guess at one.
    pub position: Option<Position>,
}

impl DiagnosticRecord {
    pub fn new(status: GqlStatus, detail: impl Into<String>) -> Self {
        DiagnosticRecord {
            status,
            detail: detail.into(),
            position: None,
        }
    }

    /// The same, raised at a place. The position is written into the
    /// detail as well, ahead of it and separated by a colon, which is
    /// the form every parser message already had.
    pub fn at(status: GqlStatus, position: Position, detail: impl fmt::Display) -> Self {
        DiagnosticRecord {
            status,
            detail: format!("{position}: {detail}"),
            position: Some(position),
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
        assert_eq!(CONDITIONS.len(), 80, "12 class rows plus 68 subclass rows");
        assert_eq!(
            subclass_rows().count(),
            68,
            "68 subclass rows, the conformance denominator"
        );
    }

    #[test]
    fn a_statement_that_did_nothing_notable_has_a_code_for_that() {
        // The artifact spells `00` as a container with `001` inside it,
        // so a generator that only emits class rows for the self-closing
        // classes has no way to say "this worked". Everything the shell
        // reports on a successful statement hangs off this.
        assert_eq!(codes::C00000.code(), "00000");
        assert_eq!(codes::C00000.standard_text(), "successful completion");
        assert_eq!(codes::C00000.severity(), Severity::Success);
        assert_eq!(
            codes::C00001.standard_text(),
            "successful completion, omitted result"
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

    #[test]
    fn a_position_reads_the_same_way_it_did_when_it_was_only_prose() {
        // The pair is added to the record, not swapped for the words:
        // a message that stopped saying where would be a message a
        // user has to go and look something up to act on.
        let at = Position { line: 1, column: 7 };
        let raised = DiagnosticRecord::at(codes::C42001, at, "expected MATCH");
        assert_eq!(
            raised.to_string(),
            "42001: line 1, column 7: expected MATCH"
        );
        assert_eq!(raised.position, Some(at));
        assert_eq!(at.to_string(), "line 1, column 7");
        // And a record made the old way has no position rather than
        // one parsed back out of its own detail.
        assert!(DiagnosticRecord::new(codes::C22012, "").position.is_none());
    }

    #[test]
    fn an_offset_lands_on_the_line_and_column_a_reader_would_count() {
        let source = "MATCH (n)\nWHERE n.x\nRETURN n";
        assert_eq!(Position::of(source, 0), Position { line: 1, column: 1 });
        assert_eq!(Position::of(source, 6), Position { line: 1, column: 7 });
        // The newline itself belongs to the line it ends, and the byte
        // after it starts the next one at column 1.
        assert_eq!(
            Position::of(source, 9),
            Position {
                line: 1,
                column: 10
            }
        );
        assert_eq!(Position::of(source, 10), Position { line: 2, column: 1 });
        assert_eq!(Position::of(source, 20), Position { line: 3, column: 1 });
        // Past the end is the end, which is where an error about a
        // query that stopped too early belongs.
        assert_eq!(Position::of(source, 9999), Position { line: 3, column: 9 });
    }

    #[test]
    fn a_column_counts_characters_and_not_the_bytes_they_took() {
        // Four characters in, on a line where those four took ten
        // bytes. A column of 11 would point past the end of a line the
        // user sees as four wide.
        let source = "RETURN 'héllo wörld'";
        let offset = source.find('w').expect("w");
        assert_eq!(
            Position::of(source, offset),
            Position {
                line: 1,
                column: 15
            }
        );
    }
}
