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

    /// The two characters that open the code, which is the condition
    /// class: `22` for the data exceptions, `42` for the syntax
    /// errors, `40` for the rollbacks. A binding raises one exception
    /// type per class, so this is the field it switches on, and taking
    /// the first two characters of a five-character code is the sort of
    /// thing every binding would otherwise write for itself.
    pub fn class(self) -> &'static str {
        &self.condition().code[..2]
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

    /// The page that documents this condition. Derived from the code
    /// rather than stored, because a table of eighty urls is eighty
    /// chances for one of them to be the url of a different condition.
    pub fn doc_url(self) -> String {
        format!("{DOC_BASE}{}", self.code())
    }

    /// Whether running the same statement again could succeed.
    ///
    /// True for `40000 transaction rollback`, which is the engine
    /// saying it undid the work and nothing of it is in the file. False
    /// for `40003 statement completion unknown`, which is the same
    /// class and the opposite advice: a statement that may or may not
    /// have committed is one a retry could apply twice, and a caller
    /// that wants to retry it has to establish which happened first.
    /// False for everything else, since a statement that failed to
    /// parse parses no better the second time.
    pub fn retryable(self) -> bool {
        self.code() == "40000"
    }
}

/// Where a condition is written up. The code goes on the end, so an
/// error hands the reader a url rather than a code to go and search
/// for, which is the difference between a message a user can act on
/// and a message a user has to research.
pub const DOC_BASE: &str = "https://zu.dev/docs/errors/";

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

/// Where in the statement text a condition was raised, said three ways.
///
/// `line` and `column` are 1-based and the column counts characters
/// rather than bytes, so a line of multi-byte text does not read as
/// wider than it looks. That pair is what an editor or a shell wants:
/// it has printed the line already and needs somewhere to put the
/// caret.
///
/// `offset` is the same place as a byte index into the statement, and
/// it is here for the caller the pair does not serve: a tool that holds
/// the text and wants to slice it, an editor mapping into a buffer it
/// indexes by byte, a highlighter marking a range. Recovering it from
/// the pair means counting lines and characters again, over text the
/// engine had in hand when it raised the condition. It is always on a
/// character boundary of that text, so slicing at it cannot panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Bytes into the statement, 0-based.
    pub offset: u32,
    pub line: u32,
    pub column: u32,
}

impl Position {
    /// The place `offset` bytes into `source`. An offset past the end
    /// lands at the end, which is where an error about a query that
    /// stopped too early belongs, and an offset inside a character
    /// lands on the start of that character.
    pub fn of(source: &str, offset: usize) -> Self {
        let offset = boundary(source, offset);
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
        Position {
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            line,
            column,
        }
    }
}

/// The nearest character boundary of `source` at or before `offset`,
/// with an offset past the end landing on the end.
fn boundary(source: &str, offset: usize) -> usize {
    let mut offset = offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The longest line quoted back as an excerpt. A line longer than this
/// was written by a program rather than by a person, and nobody is
/// going to read four kilobytes of it under a caret. It is left out
/// rather than cut short, because a cut line puts the column somewhere
/// other than where the column says it is.
const EXCERPT_LIMIT: usize = 4096;

/// The line `offset` falls on, without its newline, or `None` when
/// that line is empty or too long to quote.
fn line_at(source: &str, offset: usize) -> Option<&str> {
    let offset = boundary(source, offset);
    let start = source[..offset].rfind('\n').map_or(0, |ix| ix + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |ix| offset + ix);
    let line = source[start..end].trim_end_matches('\r');
    (!line.is_empty() && line.len() <= EXCERPT_LIMIT).then_some(line)
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

/// What a condition is about, when it is about something a statement
/// named.
///
/// ISO 23.2 calls this the subject of the diagnostic record, and what it
/// buys is the difference between a message and a fact. `42002` says a
/// name is not defined and the detail says which one in English; the
/// subject says which one in a field, so a client underlining the name,
/// a linter counting the labels a graph is missing, or a driver mapping
/// the condition onto its own exception type never has to read the
/// sentence back.
///
/// The variants are the kinds of thing a GQL statement can name, and
/// there is deliberately no `Other(String)`: a condition about something
/// with no kind here has no subject, which is honest, where a catch-all
/// would be a second message field wearing the subject's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A graph, by the name the statement reached it under.
    Graph(String),
    /// A schema, by its path.
    Schema(String),
    /// A label, which is what names a node type or an edge type.
    Label(String),
    /// A property, by the name written after the dot or before the
    /// colon.
    Property(String),
    /// A binding variable.
    Variable(String),
    /// A value type, spelled the way a statement would spell it.
    Type(String),
    /// A function or a procedure, by name.
    Function(String),
}

impl Subject {
    /// What kind of thing this is, in one word, for a caller rendering
    /// the record rather than matching on it.
    pub const fn kind(&self) -> &'static str {
        match self {
            Subject::Graph(_) => "graph",
            Subject::Schema(_) => "schema",
            Subject::Label(_) => "label",
            Subject::Property(_) => "property",
            Subject::Variable(_) => "variable",
            Subject::Type(_) => "type",
            Subject::Function(_) => "function",
        }
    }

    /// The name itself, without the kind.
    pub fn name(&self) -> &str {
        match self {
            Subject::Graph(s)
            | Subject::Schema(s)
            | Subject::Label(s)
            | Subject::Property(s)
            | Subject::Variable(s)
            | Subject::Type(s)
            | Subject::Function(s) => s,
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind(), self.name())
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
///
/// `excerpt` is the line that position falls on, kept because the
/// caller furthest from the statement is the one most likely to be
/// showing this to a person: a driver, a notebook cell, a log line. The
/// engine has the text in hand when it raises the condition and the
/// caller may not have it at all by the time it prints one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticRecord {
    pub status: GqlStatus,
    pub detail: String,
    /// Where in the statement, for the conditions that happen somewhere.
    /// A division by zero happens at runtime and has no token to point
    /// at, so this is `None` rather than a guess at one.
    pub position: Option<Position>,
    /// The whole line `position` is on, without its newline, so that
    /// `column` indexes into it. `None` when there is no position, when
    /// the line is empty, and when it is longer than a person is going
    /// to read, which are the three cases where quoting it back helps
    /// nobody.
    pub excerpt: Option<String>,
    /// What the condition is about, where it is about something the
    /// statement named. `None` for the conditions that are about no
    /// particular thing: a division by zero is about an expression and
    /// an expression has no name.
    pub subject: Option<Subject>,
    /// The graph the statement was running against, and the schema that
    /// graph was reached through. Both are filled at the session
    /// boundary rather than at the raise site, because that is the one
    /// place that knows them and there are two hundred raise sites.
    pub graph: Option<String>,
    pub schema: Option<String>,
}

impl DiagnosticRecord {
    pub fn new(status: GqlStatus, detail: impl Into<String>) -> Self {
        DiagnosticRecord {
            status,
            detail: detail.into(),
            position: None,
            excerpt: None,
            subject: None,
            graph: None,
            schema: None,
        }
    }

    /// The same, about a thing the statement named.
    ///
    /// Written as a builder so a raise site reads as one expression and
    /// so that adding the subject to a site never reshapes the call it
    /// was already making.
    pub fn about(mut self, subject: Subject) -> Self {
        self.subject = Some(subject);
        self
    }

    /// Where the statement was running, filled in on the way out.
    ///
    /// It does not overwrite: a condition raised about a graph other
    /// than the working one has already said which, and the session
    /// saying otherwise on the way past would be the session being
    /// wrong.
    pub fn within(&mut self, graph: &str, schema: &str) {
        self.graph.get_or_insert_with(|| graph.to_string());
        self.schema.get_or_insert_with(|| schema.to_string());
    }

    /// The same, raised at a place. The position is written into the
    /// detail as well, ahead of it and separated by a colon, which is
    /// the form every parser message already had.
    pub fn at(status: GqlStatus, position: Position, detail: impl fmt::Display) -> Self {
        DiagnosticRecord {
            position: Some(position),
            ..DiagnosticRecord::new(status, format!("{position}: {detail}"))
        }
    }

    /// The same again, raised at a byte offset into the statement text.
    ///
    /// This is the form to reach for wherever the text is in hand,
    /// which is every place inside the front end, because it is the
    /// only form that can fill the excerpt: the line comes from the
    /// source and nothing downstream of here still has it.
    pub fn in_source(
        status: GqlStatus,
        source: &str,
        offset: usize,
        detail: impl fmt::Display,
    ) -> Self {
        let position = Position::of(source, offset);
        DiagnosticRecord {
            excerpt: line_at(source, offset).map(str::to_string),
            ..DiagnosticRecord::at(status, position, detail)
        }
    }

    pub fn severity(&self) -> Severity {
        self.status.severity()
    }

    /// The page that documents the condition this record carries.
    pub fn doc_url(&self) -> String {
        self.status.doc_url()
    }

    /// Whether running the statement again could succeed.
    pub fn retryable(&self) -> bool {
        self.status.retryable()
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
        let at = Position {
            offset: 6,
            line: 1,
            column: 7,
        };
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
        let at = |offset, line, column| Position {
            offset,
            line,
            column,
        };
        assert_eq!(Position::of(source, 0), at(0, 1, 1));
        assert_eq!(Position::of(source, 6), at(6, 1, 7));
        // The newline itself belongs to the line it ends, and the byte
        // after it starts the next one at column 1.
        assert_eq!(Position::of(source, 9), at(9, 1, 10));
        assert_eq!(Position::of(source, 10), at(10, 2, 1));
        assert_eq!(Position::of(source, 20), at(20, 3, 1));
        // Past the end is the end, which is where an error about a
        // query that stopped too early belongs, and the offset says the
        // end too rather than the number nobody could index with.
        assert_eq!(Position::of(source, 9999), at(28, 3, 9));
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
                offset: 15,
                line: 1,
                column: 15
            }
        );
        // The offset is a byte index into that same text, so slicing at
        // it is the point of having it, and an offset landing inside a
        // character walks back to the start of it rather than cutting
        // one in half.
        let at = Position::of(source, offset);
        assert_eq!(&source[at.offset as usize..], "wörld'");
        let inside = source.find('é').expect("e") + 1;
        assert_eq!(Position::of(source, inside).offset as usize, inside - 1);
    }

    #[test]
    fn an_excerpt_is_the_line_the_column_counts_into() {
        let source = "MATCH (n)\nWHERE n.x = 1\nRETURN n";
        let offset = source.find("n.x").expect("n.x");
        let record = DiagnosticRecord::in_source(codes::C42001, source, offset, "no such property");
        let excerpt = record.excerpt.as_deref().expect("a line to quote");
        assert_eq!(excerpt, "WHERE n.x = 1");
        // Which is what makes the column usable: it counts characters
        // into the excerpt, so a caret goes under the token.
        let position = record.position.expect("a place");
        assert_eq!(position.line, 2);
        let caret: String = excerpt
            .chars()
            .take(position.column as usize - 1)
            .map(|_| ' ')
            .chain(['^'])
            .collect();
        assert_eq!(caret, "      ^");
        assert_eq!(&source[position.offset as usize..][..3], "n.x");
        // The words are unchanged by any of it, so a caller that only
        // prints the message still reads the same sentence.
        assert_eq!(
            record.to_string(),
            "42001: line 2, column 7: no such property"
        );
    }

    #[test]
    fn a_line_nobody_would_read_is_left_out_rather_than_cut() {
        // An empty line quotes as nothing, so it says nothing.
        let record = DiagnosticRecord::in_source(codes::C42001, "\n\nRETURN 1", 1, "empty");
        assert!(record.excerpt.is_none());
        // And a generated line past the limit is dropped whole, since
        // a cut one would put the column somewhere it is not.
        let long = format!("RETURN {}", "1 + ".repeat(2000));
        let record = DiagnosticRecord::in_source(codes::C42001, &long, 7, "long");
        assert!(long.len() > 4096, "the fixture has to pass the limit");
        assert!(record.excerpt.is_none());
        // A record made without the text has no excerpt either, rather
        // than an empty one that reads as a blank line.
        assert!(DiagnosticRecord::new(codes::C22012, "").excerpt.is_none());
    }

    #[test]
    fn a_condition_says_where_it_is_written_up_and_whether_to_try_again() {
        assert_eq!(codes::C42001.class(), "42");
        assert_eq!(
            codes::C42001.doc_url(),
            "https://zu.dev/docs/errors/42001",
            "the code is the page, so a message hands over a link rather than a thing to search for"
        );
        // A rollback undid the work, so the same statement can run
        // again. Statement completion unknown is the same class and the
        // opposite advice, because a retry could apply it twice.
        assert!(codes::C40000.retryable());
        assert!(!codes::C40003.retryable());
        assert!(!codes::C42001.retryable());
        assert!(!codes::C22012.retryable());
    }
}
