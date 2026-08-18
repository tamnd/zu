//! The vocabulary of zuQL, and the three things that colour it.
//!
//! A statement gets read in four places and run in one. The engine's
//! parser is the one that runs it and is the definition of the
//! language; the other three look at it. The shell colours a statement
//! as it is typed, the tree-sitter grammar parses one for an editor,
//! and the Shiki grammar colours one printed on a page. None of the
//! three can ask the parser, because two of them are not in this
//! process and the third is on the keystroke path.
//!
//! What they can share is the word list, and this is where it lives.
//! `grammar/vocabulary.toml` holds every word with what kind of word it
//! is, and this module puts it where each of the three needs it: the
//! tree-sitter highlight query and the TextMate grammar are written
//! from it, and the shell's table is checked against it, because that
//! table is sorted for a binary search and read by a scanner rather
//! than by a generator.
//!
//! The check runs both ways for the tree-sitter grammar and one way for
//! the shell. Every keyword the grammar spells has to be a word the
//! vocabulary knows, because a word the grammar parses and nothing
//! colours is a word that comes out plain in an editor. The reverse is
//! not required, and deliberately: the vocabulary holds the words the
//! parser refuses by name, `MERGE`, `FILTER`, `LET` and the rest, and
//! those are coloured everywhere and parsed nowhere.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use zu_json::Json;

use crate::toml::Doc;

/// The file's schema version, which moves when its shape changes and
/// not when the words do.
pub const SCHEMA: i64 = 1;

/// The word list.
pub const PATH: &str = "grammar/vocabulary.toml";

/// The shell's own table, checked against the list.
pub const SHELL: &str = "crates/zu-cli/src/highlight.rs";

/// The tree-sitter grammar, checked against the list.
pub const GRAMMAR: &str = "grammar/tree-sitter-gql/grammar.js";

/// The tree-sitter highlight query, written from the list.
pub const HIGHLIGHTS: &str = "grammar/tree-sitter-gql/queries/highlights.scm";

/// The TextMate grammar Shiki reads, written from the list.
pub const TEXTMATE: &str = "grammar/shiki/gql.tmLanguage.json";

/// What a word is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A word the parser gives meaning to by position.
    Keyword,
    /// `NULL` and the two booleans, which are values written as words.
    Literal,
    /// A built-in the binder resolves by name.
    Function,
    /// A graph algorithm, called as a table function.
    Algorithm,
    /// A value type name.
    Type,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "keyword" => Kind::Keyword,
            "literal" => Kind::Literal,
            "function" => Kind::Function,
            "algorithm" => Kind::Algorithm,
            "type" => Kind::Type,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword",
            Kind::Literal => "literal",
            Kind::Function => "function",
            Kind::Algorithm => "algorithm",
            Kind::Type => "type",
        }
    }

    /// Whether the shell's scanner shows this kind. It shows the three
    /// kinds a word belongs to whatever stands around it, and not the
    /// two that need a grammar to tell from a name: `DATE` is a type
    /// before a string and a variable everywhere else, and `pagerank`
    /// is a name a statement calls.
    pub fn shown(self) -> bool {
        matches!(self, Kind::Keyword | Kind::Literal | Kind::Function)
    }

    /// The TextMate scope a word of this kind is painted with. The
    /// names are the standard ones, because a theme colours scopes it
    /// has heard of and paints everything else as text.
    pub fn scope(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword.control.gql",
            Kind::Literal => "constant.language.gql",
            Kind::Function => "support.function.gql",
            Kind::Algorithm => "support.function.graph.gql",
            Kind::Type => "support.type.gql",
        }
    }
}

/// One group of words: a kind, a sentence about why they are together,
/// and the words themselves.
#[derive(Debug, Clone)]
pub struct Group {
    pub kind: Kind,
    pub doc: String,
    pub words: Vec<String>,
    pub line: usize,
}

/// The word list.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub audited: String,
    pub groups: Vec<Group>,
}

/// A file this module writes, and what should be in it.
#[derive(Debug, Clone)]
pub struct Generated {
    pub path: &'static str,
    pub text: String,
}

impl Vocabulary {
    pub fn load(path: &Path) -> Result<Vocabulary, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Vocabulary::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Vocabulary, String> {
        let doc = Doc::parse(text)?;
        if let Some(key) = doc.root.unknown(&["schema", "doc", "audited"]).first() {
            return Err(format!("the file has no key {key:?}"));
        }
        if let Some(name) = doc.unknown_arrays(&["group"]).first() {
            return Err(format!("the file has no `[[{name}]]`"));
        }
        match doc.root.int("schema") {
            Some(SCHEMA) => {}
            found => {
                return Err(format!(
                    "this reader reads schema {SCHEMA} and the file says {found:?}"
                ));
            }
        }
        let audited = doc
            .root
            .str("audited")
            .ok_or("the file does not say when it was audited")?
            .to_string();

        let mut groups = Vec::new();
        // Where each word was, so that a word written twice names both
        // places rather than only the second.
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for table in doc.array("group") {
            let line = table.line;
            let at = |msg: String| format!("line {line}: {msg}");
            if let Some(key) = table.unknown(&["kind", "doc", "words"]).first() {
                return Err(at(format!("a group has no key {key:?}")));
            }
            let kind = table
                .str("kind")
                .ok_or_else(|| at("a group with no `kind`".into()))?;
            let kind = Kind::parse(kind)
                .ok_or_else(|| at(format!("{kind:?} is not a kind of word this file has")))?;
            let doc_text = table
                .str("doc")
                .ok_or_else(|| at("a group that does not say what it is".into()))?;
            let words = table
                .list("words")
                .ok_or_else(|| at("a group with no `words`".into()))?;
            if words.is_empty() {
                return Err(at("a group with no words in it".into()));
            }
            for word in words {
                if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(at(format!(
                        "{word:?} is not a word: a word is letters, digits and underscores"
                    )));
                }
                if let Some(before) = seen.get(word.as_str()) {
                    return Err(at(format!("{word:?} is already a word, on line {before}")));
                }
                seen.insert(word.as_str(), line);
            }
            groups.push(Group {
                kind,
                doc: doc_text.to_string(),
                words: words.to_vec(),
                line,
            });
        }
        if groups.is_empty() {
            return Err("the file has no words in it".into());
        }
        Ok(Vocabulary { audited, groups })
    }

    /// Every word of one kind, in the order the file writes them.
    pub fn words(&self, kind: Kind) -> Vec<&str> {
        self.groups
            .iter()
            .filter(|g| g.kind == kind)
            .flat_map(|g| g.words.iter().map(String::as_str))
            .collect()
    }

    /// Every word, with what it is.
    pub fn all(&self) -> Vec<(&str, Kind)> {
        self.groups
            .iter()
            .flat_map(|g| g.words.iter().map(move |w| (w.as_str(), g.kind)))
            .collect()
    }

    /// The words the shell's table holds, in the order it holds them,
    /// which is by length and then by spelling because the lookup is a
    /// binary search that compares the length first.
    pub fn shell_table(&self) -> Vec<&str> {
        let mut words: Vec<&str> = self
            .all()
            .into_iter()
            .filter(|(_, kind)| kind.shown())
            .map(|(word, _)| word)
            .collect();
        words.sort_by_key(|w| (w.len(), *w));
        words
    }

    /// The files written from the list.
    pub fn generated(&self, grammar_js: &str) -> Vec<Generated> {
        vec![
            Generated {
                path: HIGHLIGHTS,
                text: self.highlights(grammar_js),
            },
            Generated {
                path: TEXTMATE,
                text: self.textmate(),
            },
        ]
    }

    /// The tree-sitter highlight query.
    ///
    /// A keyword is an anonymous node in the tree, so the query names
    /// it by its text, and a query naming a token the grammar does not
    /// have is one tree-sitter refuses to compile. So the keywords in
    /// here are the ones the grammar actually spells, which is why this
    /// is written from the two files rather than from the word list
    /// alone.
    pub fn highlights(&self, grammar_js: &str) -> String {
        let spelled = spelled(grammar_js);
        let mut out = String::new();
        out.push_str(
            "; Written by `cargo xtask grammar` from grammar/vocabulary.toml. Do not edit.\n;\n\
             ; The scopes are the ones every tree-sitter editor already has a colour for,\n\
             ; so a theme nobody wrote for this language still paints it.\n\n",
        );
        for kind in [
            Kind::Keyword,
            Kind::Literal,
            Kind::Function,
            Kind::Algorithm,
        ] {
            let words: Vec<&str> = self
                .words(kind)
                .into_iter()
                .filter(|w| spelled.contains(*w))
                .collect();
            if words.is_empty() {
                continue;
            }
            let capture = match kind {
                Kind::Literal => "@constant.builtin",
                Kind::Function => "@function.builtin",
                Kind::Algorithm => "@function.builtin",
                _ => "@keyword",
            };
            out.push_str(&format!("; {}s\n[\n", kind.name()));
            for word in words {
                out.push_str(&format!("  \"{word}\"\n"));
            }
            out.push_str(&format!("] {capture}\n\n"));
        }
        out.push_str(
            "; The rest is the shape of the tree rather than a list of words. A name is a\n\
             ; type where a type stands and a variable where a value does, which is the\n\
             ; whole reason an editor wants a grammar and not a word list.\n\
             (comment) @comment\n\
             (string) @string\n\
             (integer) @number\n\
             (float) @number\n\
             (parameter) @variable.parameter\n\
             (label) @type\n\
             (type_name) @type\n\
             (function_call name: (identifier) @function)\n\
             (call_clause name: (identifier) @function.builtin)\n\
             (path_constructor name: (identifier) @function.builtin)\n\
             (exists_block name: (identifier) @keyword)\n\
             (value_block name: (identifier) @keyword)\n\
             (variable) @variable\n\
             (property_access property: (identifier) @property)\n\
             (property_map key: (identifier) @property)\n\
             (projection_item alias: (identifier) @variable)\n\
             [\n  \"+\"\n  \"-\"\n  \"*\"\n  \"/\"\n  \"%\"\n  \"=\"\n  \"<>\"\n  \"<\"\n  \
             \"<=\"\n  \">\"\n  \">=\"\n  \"&\"\n  \"|\"\n  \"!\"\n] @operator\n\
             [\n  \"(\"\n  \")\"\n  \"[\"\n  \"]\"\n  \"{\"\n  \"}\"\n] @punctuation.bracket\n\
             [\n  \",\"\n  \".\"\n  \":\"\n  \";\"\n] @punctuation.delimiter\n",
        );
        out
    }

    /// The TextMate grammar, which is what Shiki colours a code block
    /// on the documentation site with.
    ///
    /// TextMate is a list of regular expressions and not a grammar, so
    /// it cannot tell a type from a variable of the same name and does
    /// not try: what it has is the word lists and the shapes that are
    /// unambiguous on their own, which is what a code block on a page
    /// needs. The tree-sitter grammar is the one an editor uses when it
    /// wants to be right.
    pub fn textmate(&self) -> String {
        let patterns = |names: &[&str]| -> Json {
            Json::Arr(names.iter().map(|n| obj(&[("include", str(n))])).collect())
        };
        let word_pattern = |kind: Kind, extra: &str| -> Json {
            let words = self.words(kind).join("|");
            obj(&[
                ("name", str(kind.scope())),
                ("match", str(&format!("(?i)\\b({words})\\b{extra}"))),
            ])
        };

        let repository = Json::Obj(vec![
            (
                "comment".into(),
                obj(&[(
                    "patterns",
                    Json::Arr(vec![
                        obj(&[
                            ("name", str("comment.line.double-slash.gql")),
                            ("match", str("//.*$")),
                        ]),
                        obj(&[
                            ("name", str("comment.block.gql")),
                            ("begin", str("/\\*")),
                            ("end", str("\\*/")),
                        ]),
                    ]),
                )]),
            ),
            (
                "string".into(),
                obj(&[(
                    "patterns",
                    Json::Arr(vec![
                        // The raw forms first: an `@` before the quote
                        // turns escapes off, and a rule that did not
                        // look for the `@` would paint the backslash
                        // in `@'a\b'` as an escape.
                        obj(&[
                            ("name", str("string.quoted.single.raw.gql")),
                            ("begin", str("@'")),
                            ("end", str("'(?!')")),
                        ]),
                        obj(&[
                            ("name", str("string.quoted.double.raw.gql")),
                            ("begin", str("@\"")),
                            ("end", str("\"(?!\")")),
                        ]),
                        obj(&[
                            ("name", str("string.quoted.single.gql")),
                            ("begin", str("'")),
                            ("end", str("'")),
                            (
                                "patterns",
                                Json::Arr(vec![obj(&[
                                    ("name", str("constant.character.escape.gql")),
                                    ("match", str("\\\\.")),
                                ])]),
                            ),
                        ]),
                        obj(&[
                            ("name", str("string.quoted.double.gql")),
                            ("begin", str("\"")),
                            ("end", str("\"")),
                            (
                                "patterns",
                                Json::Arr(vec![obj(&[
                                    ("name", str("constant.character.escape.gql")),
                                    ("match", str("\\\\.")),
                                ])]),
                            ),
                        ]),
                    ]),
                )]),
            ),
            (
                "identifier".into(),
                obj(&[
                    ("name", str("variable.other.quoted.gql")),
                    ("match", str("`[^`]*`")),
                ]),
            ),
            (
                "parameter".into(),
                obj(&[
                    ("name", str("variable.parameter.gql")),
                    ("match", str("\\$[A-Za-z0-9_]+")),
                ]),
            ),
            (
                "number".into(),
                obj(&[
                    ("name", str("constant.numeric.gql")),
                    (
                        "match",
                        str("(?i)\\b(0x[0-9a-f]+|0o[0-7]+|0b[01]+|\
                             [0-9]+(\\.[0-9]+)?(e[+-]?[0-9]+)?[mfd]?)\\b"),
                    ),
                ]),
            ),
            ("literal".into(), word_pattern(Kind::Literal, "")),
            // A function is a name with a bracket after it, which is
            // the one thing about a name TextMate can see.
            (
                "function".into(),
                word_pattern(Kind::Function, "(?=\\s*\\()"),
            ),
            (
                "algorithm".into(),
                word_pattern(Kind::Algorithm, "(?=\\s*\\()"),
            ),
            ("keyword".into(), word_pattern(Kind::Keyword, "")),
            // A type name after the words that introduce one. A bare
            // `DATE` is a variable far more often than it is a type,
            // and a page that painted every one of them as a type
            // would be wrong more often than it was right.
            (
                "type".into(),
                obj(&[
                    (
                        "match",
                        str(&format!(
                            "(?i)(\\bAS\\b|\\bTYPED\\b|::)\\s*({})\\b",
                            self.words(Kind::Type).join("|")
                        )),
                    ),
                    (
                        "captures",
                        Json::Obj(vec![
                            ("1".into(), obj(&[("name", str(Kind::Keyword.scope()))])),
                            ("2".into(), obj(&[("name", str(Kind::Type.scope()))])),
                        ]),
                    ),
                ]),
            ),
            // A type written as a name and a string. The name is one
            // word or two, because LOCAL and ZONED are halves of a name
            // rather than names of their own, and both halves are type
            // words so one list builds the pattern.
            (
                "temporal".into(),
                obj(&[
                    (
                        "match",
                        str(&format!(
                            "(?i)\\b((?:{types})\\s+)?({types})\\s*(?=@?['\"])",
                            types = self.words(Kind::Type).join("|")
                        )),
                    ),
                    (
                        "captures",
                        Json::Obj(vec![
                            ("1".into(), obj(&[("name", str(Kind::Type.scope()))])),
                            ("2".into(), obj(&[("name", str(Kind::Type.scope()))])),
                        ]),
                    ),
                ]),
            ),
            (
                "operator".into(),
                obj(&[
                    ("name", str("keyword.operator.gql")),
                    ("match", str("<-|->|<=|>=|<>|::|=>|\\.\\.|[-+*/%<>=&|!~]")),
                ]),
            ),
        ]);

        let grammar = obj(&[
            (
                "$schema",
                str(
                    "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
                ),
            ),
            // The name is the id the site asks for a code block in, and
            // the display name is what a language picker shows. Shiki
            // reads a TextMate grammar as a language registration and
            // takes the first as the id, so `gql` is the name and the
            // spelling with the capitals is beside it.
            ("name", str("gql")),
            ("displayName", str("zuQL")),
            ("scopeName", str("source.gql")),
            ("fileTypes", Json::Arr(vec![str("gql"), str("zuql")])),
            (
                "patterns",
                patterns(&[
                    "#comment",
                    "#string",
                    "#identifier",
                    "#parameter",
                    "#number",
                    "#literal",
                    "#temporal",
                    "#type",
                    "#function",
                    "#algorithm",
                    "#keyword",
                    "#operator",
                ]),
            ),
            ("repository", repository),
        ]);
        grammar.to_pretty()
    }

    /// What is wrong between the list and the two files that keep their
    /// own copy of part of it: the shell's table, and the keywords the
    /// tree-sitter grammar spells. This runs whether the generated
    /// files are being written or checked, because writing them does
    /// not fix either one.
    pub fn consistency(&self, root: &Path) -> Result<Vec<String>, String> {
        let mut notes = Vec::new();
        let shell_path = root.join(SHELL);
        let shell_text = std::fs::read_to_string(&shell_path)
            .map_err(|e| format!("reading {}: {e}", shell_path.display()))?;
        let shell = shell_table(&shell_text)
            .ok_or_else(|| format!("{}: no KEYWORDS table in it", shell_path.display()))?;
        let want = self.shell_table();
        if shell != want {
            let shell_set: BTreeSet<&str> = shell.iter().copied().collect();
            let want_set: BTreeSet<&str> = want.iter().copied().collect();
            for word in want_set.difference(&shell_set) {
                notes.push(format!(
                    "{SHELL} does not have {word:?}, which {PATH} says the shell colours"
                ));
            }
            for word in shell_set.difference(&want_set) {
                notes.push(format!(
                    "{SHELL} colours {word:?}, which is not a word in {PATH}"
                ));
            }
            if shell_set == want_set {
                notes.push(format!(
                    "{SHELL} holds the right words in the wrong order, and the lookup is a \
                     binary search by length and then by spelling"
                ));
            }
        }

        let grammar_path = root.join(GRAMMAR);
        let grammar_text = std::fs::read_to_string(&grammar_path)
            .map_err(|e| format!("reading {}: {e}", grammar_path.display()))?;
        let known: BTreeSet<&str> = self.all().into_iter().map(|(word, _)| word).collect();
        for word in spelled(&grammar_text) {
            if !known.contains(word.as_str()) {
                notes.push(format!(
                    "{GRAMMAR} parses {word:?} as a keyword and {PATH} has never heard of it, so \
                     it is a word an editor colours and the shell does not"
                ));
            }
        }
        Ok(notes)
    }

    /// The above, and the generated files against what they would be
    /// written as now.
    pub fn check(&self, root: &Path) -> Result<Vec<String>, String> {
        let mut notes = self.consistency(root)?;
        let grammar_path = root.join(GRAMMAR);
        let grammar_text = std::fs::read_to_string(&grammar_path)
            .map_err(|e| format!("reading {}: {e}", grammar_path.display()))?;
        for file in self.generated(&grammar_text) {
            let path = root.join(file.path);
            let found = std::fs::read_to_string(&path).unwrap_or_default();
            if found != file.text {
                notes.push(format!(
                    "{} is not what {PATH} writes; run `cargo xtask grammar`",
                    file.path
                ));
            }
        }
        Ok(notes)
    }

    /// Writes the generated files, and says which ones changed.
    pub fn write(&self, root: &Path) -> Result<Vec<&'static str>, String> {
        let grammar_path = root.join(GRAMMAR);
        let grammar_text = std::fs::read_to_string(&grammar_path)
            .map_err(|e| format!("reading {}: {e}", grammar_path.display()))?;
        let mut written = Vec::new();
        for file in self.generated(&grammar_text) {
            let path = root.join(file.path);
            if std::fs::read_to_string(&path).ok().as_deref() == Some(file.text.as_str()) {
                continue;
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("creating {}: {e}", dir.display()))?;
            }
            std::fs::write(&path, &file.text)
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
            written.push(file.path);
        }
        Ok(written)
    }
}

/// The keywords a tree-sitter grammar spells, which are the arguments
/// of its `kw` helper. Read out of the text rather than by running the
/// grammar, because running it needs node and a generator and this
/// needs neither.
pub fn spelled(js: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = js;
    while let Some(at) = rest.find("kw(\"") {
        rest = &rest[at + 4..];
        if let Some(end) = rest.find('"') {
            let word = &rest[..end];
            // The helper's own definition, which spells no keyword.
            if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(word.to_string());
            }
            rest = &rest[end..];
        }
    }
    out
}

/// The words in the shell's `KEYWORDS` table, in the order they are
/// written.
pub fn shell_table(rust: &str) -> Option<Vec<&str>> {
    let open = rust.find("const KEYWORDS: &[&str] = &[")?;
    let rest = &rust[open..];
    let close = rest.find("];")?;
    let body = &rest[..close];
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find('"') {
        rest = &rest[at + 1..];
        let end = rest.find('"')?;
        out.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    Some(out)
}

/// Every statement in the corpus, as files a parser can be pointed at.
///
/// The queries are the strongest test a grammar written from the same
/// EBNF can be given, because they are the statements the engine
/// answers rather than the ones the grammar's author thought of. They
/// are written out by this side rather than read by the node side so
/// that the corpus has one reader here, the one in `zu-corpus` that
/// the engine's own runner uses.
///
/// A case that expects a syntax error goes to `refused/`, which is the
/// other half of the same question: a grammar that accepts everything
/// is not a grammar.
pub fn queries(cases: &Path, out: &Path) -> Result<(usize, usize), String> {
    /// The GQLSTATUS a statement that is not a statement raises.
    const SYNTAX_ERROR: &str = "42001";

    let suites = zu_corpus::load(cases)?;
    let refused_dir = out.join("refused");
    for dir in [out, &refused_dir] {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|e| format!("clearing {}: {e}", dir.display()))?;
        }
    }
    std::fs::create_dir_all(&refused_dir)
        .map_err(|e| format!("creating {}: {e}", refused_dir.display()))?;

    let (mut taken, mut refused) = (0, 0);
    for suite in &suites {
        for case in &suite.cases {
            let syntax = match &case.expect {
                zu_corpus::Expect::Raises(code) => code == SYNTAX_ERROR,
                zu_corpus::Expect::Rows { .. } => false,
            };
            let dir = if syntax { &refused_dir } else { out };
            let stem = format!("{}--{}", suite.name, case.name);
            write_query(&dir.join(format!("{stem}.gql")), &case.query)?;
            if syntax {
                refused += 1;
            } else {
                taken += 1;
            }
            // A setup statement is a statement the engine accepted, so
            // it is as much a case for the grammar as the one under
            // test, and the suites that write a graph write it there.
            for (n, setup) in case.setup.iter().enumerate() {
                write_query(&out.join(format!("{stem}--setup-{n}.gql")), setup)?;
                taken += 1;
            }
        }
    }
    Ok((taken, refused))
}

fn write_query(path: &Path, query: &str) -> Result<(), String> {
    // A newline at the end, because a file that does not have one is a
    // file half the tools of a terminal complain about.
    std::fs::write(path, format!("{query}\n"))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

fn obj(fields: &[(&str, Json)]) -> Json {
    Json::Obj(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    )
}

fn str(text: &str) -> Json {
    Json::Str(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
schema = 1
doc = \"the words\"
audited = \"2026-08-18\"

[[group]]
kind = \"keyword\"
doc = \"the clauses\"
words = [\"MATCH\", \"RETURN\", \"WHERE\"]

[[group]]
kind = \"literal\"
doc = \"the words that are values\"
words = [\"NULL\", \"TRUE\"]

[[group]]
kind = \"type\"
doc = \"the types\"
words = [\"INT\", \"STRING\"]
";

    fn read() -> Vocabulary {
        Vocabulary::parse(FILE).expect("the file reads")
    }

    #[test]
    fn a_group_is_a_kind_and_the_words_of_it() {
        let vocabulary = read();
        assert_eq!(vocabulary.audited, "2026-08-18");
        assert_eq!(
            vocabulary.words(Kind::Keyword),
            ["MATCH", "RETURN", "WHERE"]
        );
        assert_eq!(vocabulary.words(Kind::Type), ["INT", "STRING"]);
        assert_eq!(vocabulary.words(Kind::Algorithm), Vec::<&str>::new());
    }

    #[test]
    fn the_shell_table_is_sorted_the_way_the_shell_searches_it() {
        // By length and then by spelling, and without the types, which
        // are a name until the grammar around them says otherwise.
        assert_eq!(
            read().shell_table(),
            ["NULL", "TRUE", "MATCH", "WHERE", "RETURN"]
        );
    }

    #[test]
    fn a_word_written_twice_names_both_places() {
        let doubled = FILE.replace("\"INT\", \"STRING\"", "\"INT\", \"MATCH\"");
        let error = Vocabulary::parse(&doubled).expect_err("MATCH twice");
        assert!(error.contains("already a word"), "{error}");
        assert!(error.contains("line 5"), "{error}");
    }

    #[test]
    fn a_kind_nothing_colours_is_refused() {
        let error =
            Vocabulary::parse(&FILE.replace("kind = \"literal\"", "kind = \"punctuation\""))
                .expect_err("punctuation");
        assert!(error.contains("not a kind of word"), "{error}");
    }

    #[test]
    fn a_word_with_a_space_in_it_is_not_a_word() {
        let error = Vocabulary::parse(&FILE.replace("\"INT\"", "\"LOCAL TIME\"")).expect_err("two");
        assert!(error.contains("is not a word"), "{error}");
    }

    #[test]
    fn a_schema_this_reader_does_not_read_is_refused() {
        let error = Vocabulary::parse(&FILE.replace("schema = 1", "schema = 2")).expect_err("2");
        assert!(error.contains("reads schema 1"), "{error}");
    }

    #[test]
    fn a_key_nothing_reads_is_a_word_somebody_wrote_and_nothing_applied() {
        let error = Vocabulary::parse(&FILE.replace("doc = \"the clauses\"", "notes = \"a\""))
            .expect_err("notes");
        assert!(error.contains("no key"), "{error}");
    }

    #[test]
    fn the_keywords_a_grammar_spells_are_read_out_of_it() {
        let js = "seq(kw(\"MATCH\"), optional(kw(\"OPTIONAL\")), $.pattern) // kw(\"WHERE\")";
        assert_eq!(
            spelled(js),
            ["MATCH", "OPTIONAL", "WHERE"]
                .map(String::from)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn the_shells_table_is_read_out_of_the_file_it_is_written_in() {
        let rust =
            "pub(crate) const KEYWORDS: &[&str] = &[\n    \"AS\",\n    \"BY\",\n];\nfn f() {}";
        assert_eq!(shell_table(rust), Some(vec!["AS", "BY"]));
        assert_eq!(shell_table("fn f() {}"), None);
    }

    #[test]
    fn the_highlight_query_names_only_the_keywords_the_grammar_has() {
        let vocabulary = read();
        let query = vocabulary.highlights("seq(kw(\"MATCH\"), kw(\"NULL\"))");
        assert!(query.contains("\"MATCH\"\n"), "{query}");
        assert!(!query.contains("\"RETURN\""), "{query}");
        assert!(query.contains("@constant.builtin"), "{query}");
        // A query that named a token the grammar has no rule for is one
        // tree-sitter refuses to compile, which would take the editor
        // down rather than the check.
        assert!(!query.contains("\"WHERE\""), "{query}");
    }

    #[test]
    fn the_textmate_grammar_is_json_and_holds_every_word() {
        let text = read().textmate();
        let json = zu_json::parse(&text).expect("valid JSON");
        assert_eq!(
            json.get("scopeName").and_then(Json::as_str),
            Some("source.gql")
        );
        let repository = json.get("repository").expect("a repository");
        let keyword = repository
            .get("keyword")
            .and_then(|k| k.get("match"))
            .and_then(Json::as_str)
            .expect("the keyword pattern");
        assert!(keyword.contains("MATCH|RETURN|WHERE"), "{keyword}");
        assert!(keyword.starts_with("(?i)"), "{keyword}");
    }
}
