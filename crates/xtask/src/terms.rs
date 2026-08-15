//! The terminology table, and the check that the prose obeys it.
//!
//! <!-- terms: allow vertex -->
//!
//! Nine repositories in seven languages write about one database. A
//! node that is a vertex on one page and a node on the next is two data
//! models to a reader who does not already know it is one, and the fix
//! is one table rather than nine style guides that agree until they do
//! not. The table is `style/zu/terms.yml` in `tamnd/zu-web`, because
//! the site is where the program's prose is published from; the reader
//! is here, because the prose with the most readers is here and because
//! a table checked by nine repositories against nine readers would be
//! nine tables.
//!
//! What is checked is prose, and only prose. Markdown, and the doc
//! comments that become reference pages. Not identifiers, which answer
//! to the language they are written in; not code spans or fenced
//! blocks, because a check that fires on `Vertex` in a signature
//! teaches people to ignore it; not link targets, which are addresses.
//!
//! Two things are deliberately not covered yet and are not covered
//! quietly. The CLI's help text and the text of an error are prose a
//! user reads, and neither is extractable today: the help lives in
//! usage strings among a hundred other string literals, and an error's
//! message is built at the point it is raised. Both become extractable
//! when `cli.json` and `errors.json` exist, which are two items in the
//! same release artifact contract this check belongs to, and the right
//! time to check them is when they are generated rather than by
//! grepping for string literals and guessing which ones a user sees.
//!
//! The matcher walks each line of prose once. Forms are indexed by
//! their first word, so a line costs one hash lookup per word and a
//! comparison only where a word could start something, which is what
//! keeps the whole tree under a tenth of a second.

use std::collections::HashMap;
use std::path::Path;

use zu_corpus::yaml::{self, Node};

/// The table's schema version, which moves when the shape of the file
/// changes and not when the terms do.
pub const SCHEMA: i64 = 1;

/// The floor on a definition. It is here rather than in a test because
/// the table is read by nine repositories and a definition nobody wrote
/// is worse than a term nobody defined: the first looks like an answer.
const DOC_MIN: usize = 24;

/// One term: how it is spelled, what it means, and the forms that must
/// give way to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub term: String,
    pub group: String,
    pub doc: String,
    pub instead: Vec<String>,
}

/// The table, and the index the matcher walks.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub terms: Vec<Term>,
    /// Every form of every term, keyed by its first word lower cased.
    /// One lookup per word of prose, and a comparison only where a word
    /// could begin a form.
    index: HashMap<String, Vec<Form>>,
}

/// One match: the text that matched, the form of the table it matched,
/// and the term it gives way to.
struct Hit {
    found: String,
    form: String,
    term: String,
}

/// One form that must give way, prepared for matching.
#[derive(Debug, Clone)]
struct Form {
    /// The words of the form, lower cased.
    words: Vec<String>,
    /// The words as written, which is what an exact form compares
    /// against.
    raw: Vec<String>,
    /// The form as the table spells it, which is what an exemption
    /// names and what a message quotes.
    text: String,
    /// Which term it gives way to.
    term: usize,
    /// Whether case is the point. True when the form differs from its
    /// own term only in case, which is the one situation where a case
    /// insensitive match would refuse the term itself.
    exact: bool,
}

/// What the checker found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A form that must give way, and the term it gives way to.
    Use {
        file: String,
        line: usize,
        found: String,
        term: String,
    },
    /// An exemption for a form the file does not contain. It is
    /// reported for the reason the API map reports a classification for
    /// code that is gone: an exemption nobody needs is one nobody
    /// reread, and it will be the reason a real hit is missed later.
    Stale {
        file: String,
        line: usize,
        form: String,
    },
    /// An exemption for something that is not a form at all, which is
    /// almost always a spelling that will never exempt anything.
    Unknown {
        file: String,
        line: usize,
        form: String,
    },
}

impl Note {
    pub fn file(&self) -> &str {
        match self {
            Note::Use { file, .. } | Note::Stale { file, .. } | Note::Unknown { file, .. } => file,
        }
    }

    pub fn line(&self) -> usize {
        match self {
            Note::Use { line, .. } | Note::Stale { line, .. } | Note::Unknown { line, .. } => *line,
        }
    }
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Use {
                file,
                line,
                found,
                term,
            } => write!(f, "{file}:{line}: {found:?} is {term:?} here"),
            Note::Stale { file, line, form } => write!(
                f,
                "{file}:{line}: nothing on this page says {form:?}, so the exemption is stale"
            ),
            Note::Unknown { file, line, form } => write!(
                f,
                "{file}:{line}: {form:?} is no form in the table, so the exemption exempts nothing"
            ),
        }
    }
}

/// Which comment rules a file follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every line is prose except a fenced block.
    Markdown,
    /// Only `//!` and `///` lines are prose.
    Rust,
}

impl Kind {
    /// The kind a path is read as, or none if it is neither.
    pub fn of(path: &Path) -> Option<Kind> {
        match path.extension()?.to_str()? {
            "md" => Some(Kind::Markdown),
            "rs" => Some(Kind::Rust),
            _ => None,
        }
    }
}

/// The table's path within `tamnd/zu-web`.
pub const PATH: &str = "style/zu/terms.yml";

/// Where the table is, if it is anywhere obvious.
///
/// Two places, because there are two ways to have zu-web on a machine
/// and neither is wrong. CI clones it into the workspace, so `zu-web`
/// is a directory here. A person clones it beside this repository, so
/// it is one level up. Anything else is what `--table` is for.
pub fn beside() -> Option<std::path::PathBuf> {
    ["zu-web", "../zu-web"]
        .iter()
        .map(|dir| Path::new(dir).join(PATH))
        .find(|path| path.exists())
}

impl Table {
    /// Reads the table at `path`.
    pub fn load(path: &Path) -> Result<Table, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "reading {}: {e}. Clone tamnd/zu-web beside this repository, or pass --table",
                path.display()
            )
        })?;
        Table::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Reads and validates the table.
    ///
    /// Validation is not decoration. A form that is also somebody
    /// else's term would make one word both right and wrong, and a
    /// table that says that is worse than no table, because every hit
    /// it produces has to be argued about individually.
    pub fn parse(text: &str) -> Result<Table, String> {
        let doc = yaml::parse(text)?;
        if let Some(key) = doc.unknown(&["schema", "doc", "terms"]).first() {
            return Err(format!("line {}: a table has no key {key:?}", doc.line()));
        }
        let schema = field(&doc, "schema")?;
        if schema.parse::<i64>() != Ok(SCHEMA) {
            return Err(format!(
                "line {}: this reader reads schema {SCHEMA} and the file says {schema:?}",
                doc.line()
            ));
        }
        let table_doc = sentence(&doc, "doc")?;

        let nodes = doc
            .get("terms")
            .ok_or_else(|| format!("line {}: a table has no terms", doc.line()))?;
        let nodes = nodes
            .seq()
            .ok_or_else(|| format!("line {}: terms is {}", nodes.line(), nodes.kind()))?;
        let mut terms = Vec::with_capacity(nodes.len());
        for node in nodes {
            terms.push(term(node)?);
        }
        if terms.is_empty() {
            return Err("the table has no terms in it".to_string());
        }

        let mut index: HashMap<String, Vec<Form>> = HashMap::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (at, term) in terms.iter().enumerate() {
            let key = term.term.to_lowercase();
            if let Some(before) = seen.insert(key, at) {
                return Err(format!(
                    "{:?} and {:?} are one term written twice",
                    terms[before].term, term.term
                ));
            }
        }
        for (at, entry) in terms.iter().enumerate() {
            for form in &entry.instead {
                let lower = form.to_lowercase();
                let exact = lower == entry.term.to_lowercase();
                // A form that is somebody else's term, or a second
                // term's form, would make one word both right and
                // wrong. The one exception is a form that differs from
                // its own term only in case, which is the whole point
                // of an entry like `zu` against `Zu`.
                if !exact && let Some(&other) = seen.get(&lower) {
                    return Err(format!(
                        "{form:?} is a form of {:?} and also the term {:?}",
                        entry.term, terms[other].term
                    ));
                }
                let raw: Vec<String> = words(form).map(|(_, w)| w.to_string()).collect();
                let words: Vec<String> = raw.iter().map(|w| w.to_lowercase()).collect();
                if words.is_empty() {
                    return Err(format!("{form:?} is a form with no word in it"));
                }
                let bucket = index.entry(words[0].clone()).or_default();
                // Two exact forms of one term differ only in case, so
                // they are told apart by what they are written as and
                // not by what they lower case to.
                let same = |f: &&Form| {
                    f.exact == exact
                        && if exact {
                            f.raw == raw
                        } else {
                            f.words == words
                        }
                };
                if let Some(before) = bucket.iter().find(same) {
                    return Err(format!(
                        "{form:?} is a form of {:?} and of {:?}",
                        terms[before.term].term, entry.term
                    ));
                }
                bucket.push(Form {
                    words,
                    raw,
                    text: form.clone(),
                    term: at,
                    exact,
                });
            }
        }

        Ok(Table {
            doc: table_doc,
            terms,
            index,
        })
    }

    /// The number of forms that must give way, across every term.
    pub fn forms(&self) -> usize {
        self.terms.iter().map(|t| t.instead.len()).sum()
    }

    /// Checks one file's prose, naming it `file` in what it reports.
    pub fn check(&self, file: &str, text: &str, kind: Kind) -> Vec<Note> {
        let mut notes = Vec::new();
        let mut allowed: Vec<(usize, String, bool)> = Vec::new();
        let mut lines = Vec::new();
        for (line, prose) in prose(text, kind) {
            match allow(&prose) {
                Some(forms) => {
                    allowed.extend(forms.into_iter().map(|f| (line, f.to_string(), false)));
                }
                None => lines.push((line, prose)),
            }
        }
        // An exemption names a form of the table, not a word. Naming a
        // word the table never had is a spelling that would exempt
        // nothing, silently, forever.
        for (line, form, _) in &allowed {
            if !self.knows(form) {
                notes.push(Note::Unknown {
                    file: file.to_string(),
                    line: *line,
                    form: form.clone(),
                });
            }
        }

        for (line, prose) in lines {
            // An exemption is matched against the form as the table
            // spells it and not against the text that matched it, so
            // allowing `edge table` also allows `edge tables`, which is
            // the same rule the form itself is matched by.
            for Hit { found, form, term } in self.hits(&prose) {
                match allowed.iter_mut().find(|(_, allow, _)| *allow == form) {
                    Some((_, _, used)) => *used = true,
                    None => notes.push(Note::Use {
                        file: file.to_string(),
                        line,
                        found,
                        term,
                    }),
                }
            }
        }

        for (line, form, used) in allowed {
            if used || !self.knows(&form) {
                continue;
            }
            notes.push(Note::Stale {
                file: file.to_string(),
                line,
                form,
            });
        }
        notes.sort_by_key(|n| n.line());
        notes
    }

    /// Whether the table has this form, spelled this way.
    fn knows(&self, form: &str) -> bool {
        self.terms
            .iter()
            .any(|term| term.instead.iter().any(|f| f == form))
    }

    /// Every form in one line of prose, with the term it gives way to.
    ///
    /// One pass over the words. A form matches when its words match
    /// consecutive words of the prose, where the last word of the form
    /// may pick up a plural `s`, because `edge table` and `edge tables`
    /// are the same mistake and listing both in the table would double
    /// it for no reader's benefit.
    fn hits(&self, prose: &str) -> Vec<Hit> {
        let words: Vec<(usize, &str)> = words(prose).collect();
        let mut found = Vec::new();
        let mut at = 0;
        while at < words.len() {
            let mut best: Option<&Form> = None;
            for form in self.candidates(words[at].1) {
                // The longest form wins, so a table holding both `edge
                // table` and `edge` would report the phrase and not the
                // word inside it.
                if form.words.len() > words.len() - at
                    || best.is_some_and(|b| b.words.len() >= form.words.len())
                {
                    continue;
                }
                if form.matches(prose, &words[at..at + form.words.len()]) {
                    best = Some(form);
                }
            }
            match best {
                Some(form) => {
                    let last = words[at + form.words.len() - 1];
                    let text = &prose[words[at].0..last.0 + last.1.len()];
                    found.push(Hit {
                        found: text.to_string(),
                        form: form.text.clone(),
                        term: self.terms[form.term].term.clone(),
                    });
                    at += form.words.len();
                }
                None => at += 1,
            }
        }
        found
    }

    fn candidates(&self, word: &str) -> &[Form] {
        let key = lower(word);
        self.index.get(key.as_ref()).map_or(&[][..], Vec::as_slice)
    }
}

impl Form {
    /// Whether this form matches these consecutive words.
    ///
    /// Consecutive is not enough on its own. The words of a form are
    /// one phrase, so what lies between them has to be what lies
    /// between the words of a phrase: spaces, or a hyphen, which makes
    /// `row-group` the same mistake as `row group`. Anything else and
    /// the two words are in different phrases, which is what keeps
    /// `row (group` from being read as one.
    fn matches(&self, prose: &str, words: &[(usize, &str)]) -> bool {
        for pair in words.windows(2) {
            let gap = &prose[pair[0].0 + pair[0].1.len()..pair[1].0];
            if gap.is_empty() || !gap.bytes().all(|b| matches!(b, b' ' | b'\t' | b'-')) {
                return false;
            }
        }
        for (at, (_, word)) in words.iter().enumerate() {
            let last = at + 1 == self.words.len();
            let want = if self.exact {
                &self.raw[at]
            } else {
                &self.words[at]
            };
            let have: std::borrow::Cow<'_, str> = if self.exact {
                std::borrow::Cow::Borrowed(*word)
            } else {
                lower(word)
            };
            let ok = have.as_ref() == want
                || (last
                    && have.len() == want.len() + 1
                    && have.ends_with('s')
                    && have[..want.len()] == *want);
            if !ok {
                return false;
            }
        }
        true
    }
}

/// One term of the table.
fn term(node: &Node) -> Result<Term, String> {
    if let Some(key) = node.unknown(&["term", "group", "doc", "instead"]).first() {
        return Err(format!("line {}: a term has no key {key:?}", node.line()));
    }
    let term = field(node, "term")?;
    if term.is_empty() {
        return Err(format!("line {}: a term with no word in it", node.line()));
    }
    let group = field(node, "group")?;
    if group.is_empty() {
        return Err(format!("line {}: {term:?} is in no group", node.line()));
    }
    let doc = sentence(node, "doc")?;
    let mut instead = Vec::new();
    if let Some(node) = node.get("instead") {
        let items = node
            .seq()
            .ok_or_else(|| format!("line {}: instead is {}", node.line(), node.kind()))?;
        for item in items {
            let form = item
                .str()
                .ok_or_else(|| format!("line {}: a form is {}", item.line(), item.kind()))?;
            instead.push(form.to_string());
        }
    }
    Ok(Term {
        term,
        group,
        doc,
        instead,
    })
}

/// A required scalar field.
fn field(node: &Node, key: &str) -> Result<String, String> {
    let value = node
        .get(key)
        .ok_or_else(|| format!("line {}: no {key}", node.line()))?;
    value
        .str()
        .map(str::to_string)
        .ok_or_else(|| format!("line {}: {key} is {}", value.line(), value.kind()))
}

/// A `doc:` field, held to being a sentence somebody wrote.
fn sentence(node: &Node, key: &str) -> Result<String, String> {
    let text = field(node, key)?;
    if text.len() < DOC_MIN || !text.ends_with('.') {
        return Err(format!(
            "line {}: {key} is {text:?}, which is not a sentence ending in a full stop",
            node.line()
        ));
    }
    Ok(text)
}

/// The forms an exemption on this line allows, if it is one.
///
/// `<!-- terms: allow vertex, vertices -->`. It has to name them, so
/// that it can never become a blanket switch, and a form it names that
/// the page does not contain is reported.
fn allow(prose: &str) -> Option<Vec<&str>> {
    let rest = prose.trim().strip_prefix("<!-- terms: allow ")?;
    let rest = rest.strip_suffix("-->")?;
    Some(
        rest.split(',')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .collect(),
    )
}

/// The prose of a file, line by line, with the line it was on.
///
/// A fenced block is not prose and neither is a code span, for the same
/// reason: they are the language's words and not ours. In a Rust file
/// only a doc comment is prose at all, and a fence opened inside one
/// does not survive the item it documented, so an example that forgot
/// to close cannot swallow the next page.
fn prose(text: &str, kind: Kind) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for (at, raw) in text.lines().enumerate() {
        let line = match kind {
            Kind::Markdown => raw,
            Kind::Rust => {
                let trimmed = raw.trim_start();
                match trimmed
                    .strip_prefix("//!")
                    .or_else(|| trimmed.strip_prefix("///"))
                {
                    Some(rest) => rest,
                    None => {
                        fence = None;
                        continue;
                    }
                }
            }
        };
        let trimmed = line.trim_start();
        let marker = trimmed
            .strip_prefix("```")
            .map(|_| "```")
            .or_else(|| trimmed.strip_prefix("~~~").map(|_| "~~~"));
        match (&fence, marker) {
            (None, Some(marker)) => {
                fence = Some(marker.to_string());
                continue;
            }
            (Some(open), Some(marker)) if open == marker => {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            (None, None) => {}
        }
        out.push((at + 1, strip(line)));
    }
    out
}

/// A line with everything that is not prose taken out of it: code
/// spans, link destinations and autolinks.
fn strip(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let open = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                match find_run(&bytes[i + open..], open) {
                    // An unclosed span is one backtick somebody typed,
                    // not a span, so the rest of the line is still
                    // prose.
                    None => {
                        out.push('`');
                        i += 1;
                    }
                    Some(end) => {
                        out.push(' ');
                        i += open + end + open;
                    }
                }
            }
            // The destination of a link, not its text, which is why
            // this waits for the bracket to have closed.
            b'(' => match bytes[i..].iter().position(|&b| b == b')') {
                Some(end) if i > 0 && bytes[i - 1] == b']' => {
                    out.push(' ');
                    i += end + 1;
                }
                _ => {
                    out.push('(');
                    i += 1;
                }
            },
            b'<' => match bytes[i..].iter().position(|&b| b == b'>') {
                Some(end) if line[i..i + end].contains("://") => {
                    out.push(' ');
                    i += end + 1;
                }
                _ => {
                    out.push('<');
                    i += 1;
                }
            },
            // A run of ordinary bytes, taken at once. It steps over the
            // first byte before looking, because every arm above can
            // decline and fall through to here, and a run that could be
            // empty is a loop that does not end.
            _ => {
                let start = i;
                i += 1;
                while i < bytes.len() && !matches!(bytes[i], b'`' | b'(' | b'<') {
                    i += 1;
                }
                out.push_str(&line[start..i]);
            }
        }
    }
    out
}

/// Where a run of exactly `n` backticks starts, if there is one.
fn find_run(bytes: &[u8], n: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        if run == n {
            return Some(i);
        }
        i += run;
    }
    None
}

/// The words of a line, each with where it started.
///
/// A word is a run of letters, digits and underscores, so `node/edge`
/// is two words and `zu1` is one. Anything else is a boundary, which is
/// what makes a form match a whole word and never part of one.
fn words(line: &str) -> impl Iterator<Item = (usize, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() && !is_word(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let start = i;
        while i < bytes.len() && is_word(bytes[i]) {
            i += 1;
        }
        Some((start, &line[start..i]))
    })
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A word lower cased, without allocating when it already is, which is
/// almost every word of almost every line.
fn lower(word: &str) -> std::borrow::Cow<'_, str> {
    match word.bytes().any(|b| b.is_ascii_uppercase()) {
        false => std::borrow::Cow::Borrowed(word),
        true => std::borrow::Cow::Owned(word.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "schema: 1\ndoc: The words zu uses, and the words it does not.\n\nterms:\n  \
                         - term: node\n    group: graph model\n    doc: An element of a graph, held \
                         by one node table.\n    instead:\n      - vertex\n      - vertices\n  - \
                         term: rel table\n    group: graph model\n    doc: The table an edge lives \
                         in, declared by CREATE REL TABLE.\n    instead:\n      - edge table\n  - \
                         term: zu\n    group: names\n    doc: The database, the repository and the \
                         binary, in lower case.\n    instead:\n      - Zu\n";

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    fn found(notes: &[Note]) -> Vec<String> {
        notes
            .iter()
            .map(|n| match n {
                Note::Use { found, .. } => found.clone(),
                Note::Stale { form, .. } => format!("stale {form}"),
                Note::Unknown { form, .. } => format!("unknown {form}"),
            })
            .collect()
    }

    #[test]
    fn a_form_is_reported_with_the_term_it_gives_way_to() {
        let notes = table().check("a.md", "A vertex is not a node.\n", Kind::Markdown);
        assert_eq!(
            notes,
            vec![Note::Use {
                file: "a.md".to_string(),
                line: 1,
                found: "vertex".to_string(),
                term: "node".to_string(),
            }]
        );
    }

    #[test]
    fn a_form_matches_a_whole_word_and_never_part_of_one() {
        let notes = table().check("a.md", "The vertexes and the reverted.\n", Kind::Markdown);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn the_last_word_of_a_form_may_be_plural() {
        let notes = table().check("a.md", "Typed node/edge tables.\n", Kind::Markdown);
        assert_eq!(found(&notes), ["edge tables"]);
    }

    #[test]
    fn the_words_of_a_form_have_to_be_one_phrase() {
        let table = table();
        assert_eq!(
            found(&table.check("a.md", "An edge-table is one too.\n", Kind::Markdown)),
            ["edge-table"]
        );
        let split = table.check("a.md", "The edge (table it belongs to).\n", Kind::Markdown);
        assert!(split.is_empty(), "{split:?}");
    }

    #[test]
    fn a_form_that_differs_from_its_term_only_in_case_is_matched_on_case() {
        let notes = table().check("a.md", "Zu is not zu.\n", Kind::Markdown);
        assert_eq!(found(&notes), ["Zu"]);
    }

    #[test]
    fn a_form_that_does_not_differ_only_in_case_ignores_case() {
        let notes = table().check("a.md", "A Vertex and a VERTEX.\n", Kind::Markdown);
        assert_eq!(found(&notes), ["Vertex", "VERTEX"]);
    }

    #[test]
    fn a_code_span_is_not_prose() {
        let notes = table().check(
            "a.md",
            "The `Vertex` type and the ``a `vertex` b`` span.\n",
            Kind::Markdown,
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_fenced_block_is_not_prose() {
        let text = "Before.\n\n```rust\nstruct Vertex;\n```\n\nA vertex after.\n";
        let notes = table().check("a.md", text, Kind::Markdown);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].line(), 7);
    }

    #[test]
    fn a_link_target_is_an_address_and_not_prose() {
        let notes = table().check(
            "a.md",
            "See [the paper](https://example.com/vertex.pdf) and <https://x/vertex>.\n",
            Kind::Markdown,
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_bracket_that_opens_nothing_is_an_ordinary_character() {
        // Every arm of the stripper can decline, and the run that
        // catches what is left has to move even so. This line has a
        // paren that is not a link, an angle that is not an autolink
        // and a backtick that closes nothing, which is all three.
        let notes = table().check(
            "a.md",
            "A vertex (see below < and a ` too.\n",
            Kind::Markdown,
        );
        assert_eq!(found(&notes), ["vertex"]);
    }

    #[test]
    fn only_a_doc_comment_is_prose_in_a_rust_file() {
        let text = "//! A vertex here counts.\nstruct Vertex;\n/// And a vertex here.\nfn vertex() \
                    {}\n";
        let notes = table().check("a.rs", text, Kind::Rust);
        assert_eq!(
            notes.iter().map(Note::line).collect::<Vec<_>>(),
            [1, 3],
            "{notes:?}"
        );
    }

    #[test]
    fn a_fence_in_a_doc_comment_does_not_survive_the_item_it_documented() {
        let text = "/// ```\n/// let vertex = 1;\nfn a() {}\n\n/// A vertex here.\nfn b() {}\n";
        let notes = table().check("a.rs", text, Kind::Rust);
        assert_eq!(notes.iter().map(Note::line).collect::<Vec<_>>(), [5]);
    }

    #[test]
    fn an_exemption_has_to_name_the_form_it_allows() {
        let text = "<!-- terms: allow vertex -->\n\nA vertex, quoting somebody else, and a \
                    vertices.\n";
        let notes = table().check("a.md", text, Kind::Markdown);
        assert_eq!(found(&notes), ["vertices"]);
    }

    #[test]
    fn an_exemption_nothing_needs_is_reported() {
        let notes = table().check(
            "a.md",
            "<!-- terms: allow vertex -->\n\nAll fine.\n",
            Kind::Markdown,
        );
        assert_eq!(found(&notes), ["stale vertex"]);
    }

    #[test]
    fn an_exemption_covers_every_shape_of_the_form_it_names() {
        let text = "<!-- terms: allow edge table -->\n\nTwo edge tables, quoted from a paper that \
                    calls them that.\n";
        let notes = table().check("a.md", text, Kind::Markdown);
        assert_eq!(found(&notes), [] as [String; 0]);
    }

    #[test]
    fn an_exemption_for_something_that_is_no_form_is_refused() {
        let notes = table().check(
            "a.md",
            "<!-- terms: allow nodes -->\n\nAll fine.\n",
            Kind::Markdown,
        );
        assert_eq!(found(&notes), ["unknown nodes"]);
    }

    #[test]
    fn a_term_written_twice_is_refused() {
        let text = format!(
            "{TABLE}  - term: node\n    group: graph model\n    doc: The same term a second time \
             over.\n"
        );
        let error = Table::parse(&text).expect_err("two terms with one name");
        assert!(error.contains("one term written twice"), "{error}");
    }

    #[test]
    fn a_form_that_is_somebody_elses_term_is_refused() {
        let text = format!(
            "{TABLE}  - term: element\n    group: graph model\n    doc: A word that is already a \
             term above.\n    instead:\n      - node\n"
        );
        let error = Table::parse(&text).expect_err("a form that is a term");
        assert!(error.contains("also the term"), "{error}");
    }

    #[test]
    fn a_definition_that_is_not_a_sentence_is_refused() {
        let text = "schema: 1\ndoc: The words zu uses, and the words it does not.\n\nterms:\n  - \
                    term: node\n    group: graph model\n    doc: an element\n";
        let error = Table::parse(text).expect_err("a doc that is not a sentence");
        assert!(error.contains("not a sentence"), "{error}");
    }

    #[test]
    fn a_schema_this_reader_does_not_read_is_refused() {
        let text = TABLE.replacen("schema: 1", "schema: 2", 1);
        let error = Table::parse(&text).expect_err("a schema from the future");
        assert!(error.contains("reads schema 1"), "{error}");
    }

    #[test]
    fn the_committed_table_loads_if_it_is_beside_us() {
        let Some(path) = beside() else { return };
        let table = Table::load(&path).expect("the committed table loads");
        assert!(table.terms.len() >= 40, "{} terms", table.terms.len());
        assert!(table.forms() >= 10, "{} forms", table.forms());
    }
}
