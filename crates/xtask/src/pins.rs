//! The pinned toolchain table, and the check that the tree obeys it.
//!
//! Nine repositories build one release out of one set of versions. The
//! question "which Rust do we build against" has to have one answer, and
//! it stops having one the moment the answer is written in nine
//! workflows: one of them is bumped, the others are not, and the bug
//! that follows is found by a user on the platform nobody bumped. So
//! the versions are a table, `toolchains.toml`, and the places that
//! would otherwise each hold their own answer instead have to agree
//! with it.
//!
//! A pin is exact and a floor is a promise. The pin is the release CI
//! runs; the floor is the oldest release a user may have and is what
//! the conformance matrix also runs, because a library that only ever
//! builds against the newest release finds out its floor is broken from
//! a bug report.
//!
//! What is checked runs in both directions, which is the same shape the
//! API map has. Forward: every site the table names exists, holds the
//! key it says, and the value agrees with the version. Backward: every
//! toolchain a workflow pins is one the table names, so a job added
//! next month cannot quietly introduce a tenth answer. A component this
//! repository builds against and has no site for is reported too, since
//! a row nothing holds is a row nobody maintains.

use std::collections::BTreeMap;
use std::path::Path;

use crate::toml::Doc;

/// The table's schema version, which moves when the shape of the file
/// changes and not when the versions do.
pub const SCHEMA: i64 = 1;

/// Where the table is.
pub const PATH: &str = "toolchains.toml";

/// This repository's name in a component's `repos`, which is what makes
/// a row this repository's business.
pub const REPO: &str = "zu";

/// One component: what it is, what it is pinned to, and how old a
/// release is still promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub pinned: String,
    pub floor: Option<String>,
    pub repos: Vec<String>,
    pub doc: String,
    pub line: usize,
}

/// Which of a component's two versions a site holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holds {
    Pinned,
    Floor,
}

/// How closely a site's value has to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The value is the version, character for character. This is what
    /// a toolchain file or a workflow says, because those name a
    /// release to install and installing "the newest 1.97" is a range.
    Exact,
    /// The value names the series the version is in, which is what a
    /// cargo requirement of `59` is against `59.2.0`. A requirement is
    /// how a library says which releases it can be built against, and
    /// writing the patch there would be a lockfile in a manifest.
    Series,
}

/// One place a version is written, and what it has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub component: String,
    pub file: String,
    pub key: String,
    pub holds: Holds,
    pub matches: Match,
    pub line: usize,
}

/// The table.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub audited: String,
    pub components: Vec<Component>,
    pub sites: Vec<Site>,
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A site says one version and the table says another.
    Drift {
        file: String,
        line: usize,
        key: String,
        found: String,
        want: String,
        component: String,
    },
    /// A site the table names, in a file that has no such key. The
    /// version moved and the table did not follow it, which is the
    /// failure that makes every other answer here untrustworthy.
    Gone {
        file: String,
        key: String,
        component: String,
        line: usize,
    },
    /// A toolchain pinned somewhere the table does not name. This is
    /// the direction that catches a tenth answer being added rather
    /// than an existing one drifting.
    Unpinned {
        file: String,
        line: usize,
        key: String,
        found: String,
    },
    /// A component this repository builds against with nowhere to hold
    /// it, which is a row nothing can keep honest.
    Unheld { component: String, line: usize },
}

impl Note {
    pub fn file(&self) -> &str {
        match self {
            Note::Drift { file, .. } | Note::Gone { file, .. } | Note::Unpinned { file, .. } => {
                file
            }
            Note::Unheld { .. } => PATH,
        }
    }

    pub fn line(&self) -> usize {
        match self {
            Note::Drift { line, .. }
            | Note::Gone { line, .. }
            | Note::Unpinned { line, .. }
            | Note::Unheld { line, .. } => *line,
        }
    }
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Drift {
                file,
                line,
                key,
                found,
                want,
                component,
            } => write!(
                f,
                "{file}:{line}: {key} is {found:?} and {component} is pinned to {want:?}"
            ),
            Note::Gone {
                file,
                key,
                component,
                line,
            } => write!(
                f,
                "{PATH}:{line}: {file} has no {key}, so {component} is written somewhere else now"
            ),
            Note::Unpinned {
                file,
                line,
                key,
                found,
            } => write!(
                f,
                "{file}:{line}: {key} is {found:?}, which {PATH} does not pin"
            ),
            Note::Unheld { component, line } => write!(
                f,
                "{PATH}:{line}: {component} is built against here and no site holds it"
            ),
        }
    }
}

impl Table {
    /// Reads the table at `path`.
    pub fn load(path: &Path) -> Result<Table, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Table::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Reads and validates the table.
    ///
    /// A site for a component that does not exist, or two components
    /// with one name, would each make the table say two things at once,
    /// which is the condition it exists to prevent.
    pub fn parse(text: &str) -> Result<Table, String> {
        let doc = Doc::parse(text)?;
        if let Some(key) = doc.root.unknown(&["schema", "doc", "audited"]).first() {
            return Err(format!("a table has no key {key:?}"));
        }
        if let Some(name) = doc.unknown_arrays(&["component", "site"]).first() {
            return Err(format!("a table has no [[{name}]]"));
        }
        match doc.root.int("schema") {
            Some(SCHEMA) => {}
            found => {
                return Err(format!(
                    "this reader reads schema {SCHEMA} and the file says {found:?}"
                ));
            }
        }
        let doc_text = doc
            .root
            .str("doc")
            .ok_or("a table with no doc")?
            .to_string();
        let audited = doc
            .root
            .str("audited")
            .ok_or("a table that does not say when it was audited")?
            .to_string();

        let mut components = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for table in doc.array("component") {
            let line = table.line;
            if let Some(key) = table
                .unknown(&["name", "pinned", "floor", "repos", "doc"])
                .first()
            {
                return Err(format!("line {line}: a component has no key {key:?}"));
            }
            let name = table
                .str("name")
                .ok_or_else(|| format!("line {line}: a component with no name"))?
                .to_string();
            let pinned = table
                .str("pinned")
                .ok_or_else(|| format!("line {line}: {name} is pinned to nothing"))?
                .to_string();
            let repos = table
                .list("repos")
                .ok_or_else(|| format!("line {line}: {name} says no repositories build it"))?
                .to_vec();
            if repos.is_empty() {
                return Err(format!("line {line}: {name} says no repositories build it"));
            }
            let doc = table
                .str("doc")
                .ok_or_else(|| format!("line {line}: {name} has no doc"))?
                .to_string();
            if let Some(before) = seen.insert(name.clone(), line) {
                return Err(format!(
                    "line {line}: {name} is written twice, and first on line {before}"
                ));
            }
            components.push(Component {
                name,
                pinned,
                floor: table.str("floor").map(str::to_string),
                repos,
                doc,
                line,
            });
        }
        if components.is_empty() {
            return Err("the table pins nothing".to_string());
        }

        let mut sites = Vec::new();
        for table in doc.array("site") {
            let line = table.line;
            if let Some(key) = table
                .unknown(&["component", "file", "key", "holds", "match"])
                .first()
            {
                return Err(format!("line {line}: a site has no key {key:?}"));
            }
            let component = table
                .str("component")
                .ok_or_else(|| format!("line {line}: a site for no component"))?
                .to_string();
            let at = *seen
                .get(&component)
                .ok_or_else(|| format!("line {line}: {component:?} is nothing this table pins"))?;
            let file = table
                .str("file")
                .ok_or_else(|| format!("line {line}: a site in no file"))?
                .to_string();
            let key = table
                .str("key")
                .ok_or_else(|| format!("line {line}: a site with no key"))?
                .to_string();
            let holds = match table.str("holds") {
                Some("pinned") => Holds::Pinned,
                Some("floor") => Holds::Floor,
                other => {
                    return Err(format!(
                        "line {line}: holds is {other:?}, which is neither pinned nor floor"
                    ));
                }
            };
            if holds == Holds::Floor
                && components
                    .iter()
                    .find(|c| c.name == component)
                    .is_some_and(|c| c.floor.is_none())
            {
                return Err(format!(
                    "line {line}: this holds the floor of {component}, which has none (line {at})"
                ));
            }
            let matches = match table.str("match") {
                Some("exact") => Match::Exact,
                Some("series") => Match::Series,
                other => {
                    return Err(format!(
                        "line {line}: match is {other:?}, which is neither exact nor series"
                    ));
                }
            };
            sites.push(Site {
                component,
                file,
                key,
                holds,
                matches,
                line,
            });
        }

        Ok(Table {
            doc: doc_text,
            audited,
            components,
            sites,
        })
    }

    /// The component of this name.
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name == name)
    }

    /// Checks the tree under `root` against the table.
    pub fn check(&self, root: &Path) -> Result<Vec<Note>, String> {
        let mut notes = Vec::new();
        let mut held: BTreeMap<&str, ()> = BTreeMap::new();

        for site in &self.sites {
            held.insert(site.component.as_str(), ());
            let component = self
                .component(&site.component)
                .expect("every site names a component the table has");
            let want = match site.holds {
                Holds::Pinned => &component.pinned,
                Holds::Floor => component
                    .floor
                    .as_ref()
                    .expect("a site holding a floor is refused unless there is one"),
            };
            let path = root.join(&site.file);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            // Channels are dropped before the count, so a file whose
            // only remaining value is a channel reads as the site
            // having gone, which is what it would mean: the version
            // this row holds is no longer written there.
            let found: Vec<_> = values(&text, &site.key)
                .into_iter()
                .filter(|(_, value)| !is_channel(value))
                .collect();
            if found.is_empty() {
                notes.push(Note::Gone {
                    file: site.file.clone(),
                    key: site.key.clone(),
                    component: site.component.clone(),
                    line: site.line,
                });
                continue;
            }
            for (line, value) in found {
                if !agrees(&value, want, site.matches) {
                    notes.push(Note::Drift {
                        file: site.file.clone(),
                        line,
                        key: site.key.clone(),
                        found: value,
                        want: want.clone(),
                        component: site.component.clone(),
                    });
                }
            }
        }

        for component in &self.components {
            if component.repos.iter().any(|r| r == REPO)
                && !held.contains_key(component.name.as_str())
            {
                notes.push(Note::Unheld {
                    component: component.name.clone(),
                    line: component.line,
                });
            }
        }

        notes.extend(self.workflows(root)?);
        notes.sort_by(|a, b| a.file().cmp(b.file()).then(a.line().cmp(&b.line())));
        Ok(notes)
    }

    /// Every `toolchain:` a workflow names, held to being one the table
    /// pins. A workflow added next month is the likeliest place for a
    /// tenth answer to appear, and it appears as one line.
    fn workflows(&self, root: &Path) -> Result<Vec<Note>, String> {
        let dir = root.join(".github/workflows");
        let mut files = Vec::new();
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("reading {}: {e}", dir.display()))?
                .path();
            if path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
                files.push(path);
            }
        }
        files.sort();

        let mut notes = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            for (line, value) in values(&text, "toolchain") {
                if is_channel(&value) || self.components.iter().any(|c| c.pinned == value) {
                    continue;
                }
                notes.push(Note::Unpinned {
                    file: path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                    line,
                    key: "toolchain".to_string(),
                    found: value,
                });
            }
        }
        Ok(notes)
    }
}

/// Whether a toolchain is a channel rather than a version.
///
/// A channel is what rustup calls the moving target, and a job that
/// names one is saying it wants whatever that is today: the format
/// check runs on the newest stable on purpose, and the fuzzer and miri
/// want the newest nightly. There is no version written down, so there
/// is nothing for the table to hold it to, and demanding a pin here
/// would be demanding the opposite of what those jobs are for. A dated
/// nightly is a version and is held like any other.
fn is_channel(value: &str) -> bool {
    matches!(value, "stable" | "beta" | "nightly")
}

/// Whether a value written in a file agrees with a version.
///
/// A series matches at a boundary, so `59` covers `59.2.0` and `5` does
/// not, which is the difference between a cargo requirement and a
/// coincidence of digits.
fn agrees(found: &str, want: &str, matches: Match) -> bool {
    match matches {
        Match::Exact => found == want,
        Match::Series => {
            found == want
                || (want.starts_with(found) && want.as_bytes().get(found.len()) == Some(&b'.'))
        }
    }
}

/// Every value written against `key` in a file, with the line it was
/// on.
///
/// One scan that reads both of the two formats a version is written in
/// here, because `channel = "1.97.1"` in a toolchain file and a
/// workflow's `toolchain: 1.97.1` are the same statement in two files'
/// syntax, and a second reader for the second syntax would be a second
/// thing to get wrong. The value is
/// taken from inside the quotes when there are any, and from a nested
/// `version` when the value is an inline table, which is how a cargo
/// dependency with features on it writes the same fact.
fn values(text: &str, key: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (at, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches("- ").trim();
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) else {
            continue;
        };
        let rest = strip_comment(rest.trim());
        let value = match rest.strip_prefix('{') {
            Some(inner) => match inner.split_once("version") {
                Some((_, after)) => quoted(after.trim_start().trim_start_matches('=').trim()),
                None => continue,
            },
            None => quoted(rest),
        };
        if !value.is_empty() {
            out.push((at + 1, value));
        }
    }
    out
}

/// The inside of a quoted value, or the value itself when it is bare,
/// which is how YAML writes one.
fn quoted(text: &str) -> String {
    match text.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default().to_string(),
        None => text
            .split([' ', ',', '}', '\t'])
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// Everything from an unquoted `#` on. Both formats comment that way.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quoted = false;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b'#' if !quoted => return line[..i].trim_end(),
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = concat!(
        "schema = 1\n",
        "doc = \"The versions zu builds against.\"\n",
        "audited = \"2026-08-15\"\n",
        "\n",
        "[[component]]\n",
        "name = \"rust\"\n",
        "pinned = \"1.97.1\"\n",
        "floor = \"1.97\"\n",
        "repos = [\"zu\", \"zu-python\"]\n",
        "doc = \"The compiler.\"\n",
        "\n",
        "[[site]]\n",
        "component = \"rust\"\n",
        "file = \"rust-toolchain.toml\"\n",
        "key = \"channel\"\n",
        "holds = \"pinned\"\n",
        "match = \"exact\"\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    /// A tree with these files in it, plus the workflow directory the
    /// backward check reads. The name keeps two tests running at once
    /// out of one directory.
    fn tree(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zu-pins-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".github/workflows"))
            .expect("the scratch dir is writable");
        for (name, text) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("the scratch dir is writable");
            }
            std::fs::write(path, text).expect("writes");
        }
        dir
    }

    #[test]
    fn a_site_that_says_the_pinned_version_is_what_the_table_asked_for() {
        let dir = tree(
            "a_site_that_says_the_pinned_version_is_what_the_table_asked_for",
            &[("rust-toolchain.toml", "[toolchain]\nchannel = \"1.97.1\"\n")],
        );
        assert_eq!(table().check(&dir).expect("checks"), []);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_site_that_drifted_says_which_way() {
        let dir = tree(
            "a_site_that_drifted_says_which_way",
            &[("rust-toolchain.toml", "[toolchain]\nchannel = \"1.98.0\"\n")],
        );
        let notes = table().check(&dir).expect("checks");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].to_string().contains("channel is \"1.98.0\""),
            "{}",
            notes[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_moved_out_of_its_file_is_reported() {
        let dir = tree(
            "a_key_that_moved_out_of_its_file_is_reported",
            &[("rust-toolchain.toml", "[toolchain]\ncomponents = []\n")],
        );
        let notes = table().check(&dir).expect("checks");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].to_string().contains("has no channel"),
            "{}",
            notes[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toolchain_no_row_pins_is_reported() {
        let dir = tree(
            "a_toolchain_no_row_pins_is_reported",
            &[
                ("rust-toolchain.toml", "channel = \"1.97.1\"\n"),
                (
                    ".github/workflows/ci.yml",
                    "jobs:\n  a:\n    steps:\n      - uses: ./.github/actions/rust\n        with:\n          toolchain: nightly-2026-01-01\n",
                ),
            ],
        );
        let notes = table().check(&dir).expect("checks");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].to_string().contains("does not pin"),
            "{}",
            notes[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toolchain_the_table_pins_is_left_alone() {
        let dir = tree(
            "a_toolchain_the_table_pins_is_left_alone",
            &[
                ("rust-toolchain.toml", "channel = \"1.97.1\"\n"),
                (
                    ".github/workflows/ci.yml",
                    "      - uses: ./.github/actions/rust\n        with:\n          toolchain: 1.97.1\n",
                ),
            ],
        );
        assert_eq!(table().check(&dir).expect("checks"), []);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_job_that_asks_for_a_channel_is_left_alone() {
        let dir = tree(
            "a_job_that_asks_for_a_channel_is_left_alone",
            &[
                ("rust-toolchain.toml", "channel = \"1.97.1\"\n"),
                (
                    ".github/workflows/ci.yml",
                    "      - uses: ./.github/actions/rust\n        with:\n          toolchain: \
                     nightly\n          components: miri\n",
                ),
            ],
        );
        assert_eq!(table().check(&dir).expect("checks"), []);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_component_this_repository_builds_and_nothing_holds_is_reported() {
        let text = format!(
            "{TABLE}\n[[component]]\nname = \"arrow-rs\"\npinned = \"59.2.0\"\nrepos = \
             [\"zu\"]\ndoc = \"Arrow.\"\n"
        );
        let table = Table::parse(&text).expect("parses");
        let dir = tree(
            "a_component_this_repository_builds_and_nothing_holds_is_reported",
            &[("rust-toolchain.toml", "channel = \"1.97.1\"\n")],
        );
        let notes = table.check(&dir).expect("checks");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].to_string().contains("no site holds it"),
            "{}",
            notes[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_component_another_repository_builds_needs_no_site() {
        let text = format!(
            "{TABLE}\n[[component]]\nname = \"maturin\"\npinned = \"1.14.1\"\nrepos = \
             [\"zu-python\"]\ndoc = \"The wheel builder.\"\n"
        );
        let table = Table::parse(&text).expect("parses");
        let dir = tree(
            "a_component_another_repository_builds_needs_no_site",
            &[("rust-toolchain.toml", "channel = \"1.97.1\"\n")],
        );
        assert_eq!(table.check(&dir).expect("checks"), []);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_requirement_matches_the_series_it_names() {
        assert!(agrees("59", "59.2.0", Match::Series));
        assert!(agrees("59.2", "59.2.0", Match::Series));
        assert!(agrees("59.2.0", "59.2.0", Match::Series));
        // The digits of one version are not a prefix of another's
        // unless they end where a version's part ends.
        assert!(!agrees("5", "59.2.0", Match::Series));
        assert!(!agrees("59", "59.2.0", Match::Exact));
    }

    #[test]
    fn a_dependency_with_features_on_it_says_the_same_thing() {
        let text =
            "parquet = { version = \"59\", default-features = false, features = [\"arrow\"] }\n";
        assert_eq!(values(text, "parquet"), [(1, "59".to_string())]);
    }

    #[test]
    fn a_version_in_yaml_needs_no_quotes() {
        let text = "        with:\n          toolchain: nightly-2026-08-14  # pinned\n";
        assert_eq!(
            values(text, "toolchain"),
            [(2, "nightly-2026-08-14".to_string())]
        );
    }

    #[test]
    fn a_key_that_is_the_start_of_another_key_is_not_that_key() {
        let text = "arrow-array = \"59\"\narrow-schema = \"59\"\n";
        assert_eq!(values(text, "arrow-array"), [(1, "59".to_string())]);
    }

    #[test]
    fn a_site_for_a_component_that_is_not_there_is_refused() {
        let text = format!(
            "{TABLE}\n[[site]]\ncomponent = \"gcc\"\nfile = \"a\"\nkey = \"b\"\nholds = \
             \"pinned\"\nmatch = \"exact\"\n"
        );
        let error = Table::parse(&text).expect_err("a site for nothing");
        assert!(error.contains("is nothing this table pins"), "{error}");
    }

    #[test]
    fn a_site_holding_a_floor_that_does_not_exist_is_refused() {
        let text = format!(
            "{TABLE}\n[[component]]\nname = \"go\"\npinned = \"1.26.6\"\nrepos = \
             [\"zu-go\"]\ndoc = \"Go.\"\n\n[[site]]\ncomponent = \"go\"\nfile = \"go.mod\"\nkey \
             = \"go\"\nholds = \"floor\"\nmatch = \"exact\"\n"
        );
        let error = Table::parse(&text).expect_err("a floor that is not there");
        assert!(error.contains("which has none"), "{error}");
    }

    #[test]
    fn a_component_written_twice_is_refused() {
        let text = format!(
            "{TABLE}\n{}",
            &TABLE[TABLE.find("[[component]]").unwrap()..]
        );
        let error = Table::parse(&text).expect_err("two rows with one name");
        assert!(error.contains("is written twice"), "{error}");
    }

    #[test]
    fn a_schema_this_reader_does_not_read_is_refused() {
        let error = Table::parse(&TABLE.replace("schema = 1", "schema = 2")).expect_err("schema 2");
        assert!(error.contains("reads schema 1"), "{error}");
    }

    #[test]
    fn the_committed_table_holds_this_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = Table::load(&root.join(PATH)).expect("the committed table loads");
        let notes = table.check(&root).expect("the tree is readable");
        assert!(notes.is_empty(), "{notes:#?}");
    }
}
