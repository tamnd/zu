//! The release-artifact contract, and the release that has to keep it.
//!
//! A release is a tag on this repository and a run that drives the eight
//! others (dx/14 section 6). Every one of them builds against what this
//! one published, which makes the list of what gets published a
//! contract rather than a step in a workflow. A binding that fetches
//! `model.json` for the version it pins and finds nothing cannot tell a
//! release that dropped the artifact from a version that never had it,
//! and the failure surfaces in somebody else's CI a day later.
//!
//! So `artifacts.toml` is the list, once, and the release workflow has
//! none of its own: it assembles from this table and verifies the
//! directory back against it. An artifact that stopped being produced
//! then fails the release that dropped it, which is the run that can
//! still do something about it.
//!
//! Four kinds of row, because there are four ways an artifact comes to
//! exist here. A `file` is a path the tree already holds. A `corpus` is
//! packed by the packer beside this. A `platform` row is one artifact
//! per tier 1 target, expanded against `platforms.toml` so the seven
//! move with that table rather than with this one. And a `later` row is
//! an artifact the contract names that nothing makes yet, carrying the
//! milestone that will make it: naming it early is the point, since a
//! consumer needs to know what a release will eventually carry, and the
//! alternative is eight repositories each guessing.

use std::collections::BTreeMap;
use std::path::Path;

use crate::toml::Doc;
use crate::{corpus, platforms, tarball};

/// The table's schema version, which moves when the shape of the file
/// changes and not when the artifacts do.
pub const SCHEMA: i64 = 1;

/// Where the table is.
pub const PATH: &str = "artifacts.toml";

/// The workflow that has to assemble from it.
pub const WORKFLOW: &str = ".github/workflows/release.yml";

/// The nine repositories of dx/18 section 2.
pub const REPOS: [&str; 9] = [
    "zu",
    "zu-c",
    "zu-python",
    "zu-node",
    "zu-go",
    "zu-java",
    "zu-dotnet",
    "zu-kit",
    "zu-web",
];

/// The one that makes the artifacts, and therefore the one that cannot
/// be listed as consuming them.
pub const MAKER: &str = "zu";

/// The cases the corpus row is packed from, and the README that ships
/// beside them so an unpacked artifact explains itself.
pub const CASES: &str = "conformance/cases";
pub const README: &str = "conformance/README.md";

/// Where an artifact comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Made {
    /// A path this tree holds, copied into the release as it is.
    File { from: String },
    /// The conformance corpus, packed for the version being released.
    Corpus,
    /// One artifact per tier 1 target of `platforms.toml`.
    Platform,
    /// Named by the contract, made by nobody yet, and this is the
    /// milestone that changes that.
    Later { milestone: String },
}

impl Made {
    /// What the assemble step prints, and what `--list` says a row is.
    pub fn kind(&self) -> &'static str {
        match self {
            Made::File { .. } => "file",
            Made::Corpus => "corpus",
            Made::Platform => "platform",
            Made::Later { .. } => "later",
        }
    }
}

/// One row of the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The name it is published under, which may hold `<version>` or
    /// `<target>`.
    pub name: String,
    pub made: Made,
    /// The repositories that fetch it. A row nobody fetches is a row
    /// the release carries for its own amusement.
    pub consumers: Vec<String>,
    pub doc: String,
    pub line: usize,
}

impl Artifact {
    /// Whether a release publishes this today.
    pub fn published(&self) -> bool {
        !matches!(self.made, Made::Later { .. })
    }

    /// The name with its placeholders filled in.
    pub fn expand(&self, version: &str, target: &str) -> String {
        self.name
            .replace("<version>", version)
            .replace("<target>", target)
    }
}

/// One artifact the assemble step produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shipped {
    pub name: String,
    pub bytes: u64,
    pub kind: &'static str,
}

impl std::fmt::Display for Shipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<42} {:>7} KiB  {}",
            self.name,
            self.bytes.div_ceil(1024),
            self.kind
        )
    }
}

/// What the table says about the tree it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A `file` row whose file is not there. The release would publish
    /// the name and nothing under it.
    Absent {
        name: String,
        from: String,
        line: usize,
    },
    /// A repository the split created that fetches nothing. Either the
    /// contract forgot it or it should not have been split out.
    Idle { repo: String },
    /// The release workflow does not run the table, which makes the
    /// table a document rather than a contract.
    Unheld { command: String },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Absent { name, from, line } => write!(
                f,
                "{PATH}:{line}: {name} is published from {from}, which is not in this tree"
            ),
            Note::Idle { repo } => write!(
                f,
                "{PATH}: {repo} is one of the nine repositories and consumes nothing a release publishes"
            ),
            Note::Unheld { command } => write!(
                f,
                "{WORKFLOW}: nothing runs `{command}`, so {PATH} is a document and not a contract"
            ),
        }
    }
}

/// A release directory read back against the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// A name the contract publishes and the directory does not hold.
    Missing { name: String },
    /// A name the directory holds and nothing wrote anything into.
    Empty { name: String },
    /// A file in the release that no row accounts for.
    Stranger { file: String },
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Missing { name } => write!(f, "{name} is in {PATH} and not in the release"),
            Fault::Empty { name } => write!(f, "{name} is in the release and is empty"),
            Fault::Stranger { file } => {
                write!(f, "{file} is in the release and not in {PATH}")
            }
        }
    }
}

/// The contract.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub audited: String,
    pub artifacts: Vec<Artifact>,
}

impl Table {
    /// Reads the table at `path`.
    pub fn load(path: &Path) -> Result<Table, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Table::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Reads and validates the table.
    pub fn parse(text: &str) -> Result<Table, String> {
        let doc = Doc::parse(text)?;
        if let Some(key) = doc.root.unknown(&["schema", "doc", "audited"]).first() {
            return Err(format!("a table has no key {key:?}"));
        }
        if let Some(name) = doc.unknown_arrays(&["artifact"]).first() {
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

        let mut artifacts = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for table in doc.array("artifact") {
            let line = table.line;
            if let Some(key) = table
                .unknown(&["name", "made", "from", "milestone", "consumers", "doc"])
                .first()
            {
                return Err(format!("line {line}: an artifact has no key {key:?}"));
            }
            let name = table
                .str("name")
                .ok_or_else(|| format!("line {line}: an artifact with no name"))?
                .to_string();
            if name.contains('/') {
                return Err(format!(
                    "line {line}: {name} is published as a file in one directory, so it has no /"
                ));
            }
            let from = table.str("from").map(str::to_string);
            let milestone = table.str("milestone").map(str::to_string);
            let made = match table.str("made") {
                Some("file") => Made::File {
                    from: from
                        .clone()
                        .ok_or_else(|| format!("line {line}: {name} is a file with no from"))?,
                },
                Some("corpus") => Made::Corpus,
                Some("platform") => {
                    if !name.contains("<target>") {
                        return Err(format!(
                            "line {line}: {name} is one artifact per platform and its name is the \
                             same for all of them"
                        ));
                    }
                    Made::Platform
                }
                Some("later") => Made::Later {
                    milestone: milestone.clone().ok_or_else(|| {
                        format!("line {line}: {name} is made later and by no milestone")
                    })?,
                },
                found => {
                    return Err(format!(
                        "line {line}: {name} is made {found:?}, and the ways are file, corpus, \
                         platform and later"
                    ));
                }
            };
            // The three keys that only one kind of row has. A `from` on
            // a corpus row is somebody expecting a copy that will not
            // happen, and it is worth more as an error than as a
            // comment nobody reads.
            if from.is_some() && !matches!(made, Made::File { .. }) {
                return Err(format!(
                    "line {line}: {name} is made {} and has a from, which only a file has",
                    made.kind()
                ));
            }
            if milestone.is_some() && !matches!(made, Made::Later { .. }) {
                return Err(format!(
                    "line {line}: {name} is made {} and names a milestone, which is what a row \
                     nothing makes yet does",
                    made.kind()
                ));
            }
            if let Made::Later { milestone } = &made
                && !is_milestone(milestone)
            {
                return Err(format!(
                    "line {line}: {name} waits for {milestone:?}, and the milestones are DX0 to \
                     DX6 and D0 to D6"
                ));
            }
            let consumers: Vec<String> = table
                .list("consumers")
                .ok_or_else(|| format!("line {line}: {name} is fetched by nobody"))?
                .iter()
                .map(|c| c.to_string())
                .collect();
            if consumers.is_empty() {
                return Err(format!("line {line}: {name} is fetched by nobody"));
            }
            for consumer in &consumers {
                if consumer == MAKER {
                    return Err(format!(
                        "line {line}: {MAKER} makes {name} and is not a consumer of it"
                    ));
                }
                if !REPOS.contains(&consumer.as_str()) {
                    return Err(format!(
                        "line {line}: {name} is fetched by {consumer:?}, which is not one of the \
                         nine repositories"
                    ));
                }
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
            artifacts.push(Artifact {
                name,
                made,
                consumers,
                doc,
                line,
            });
        }
        if artifacts.is_empty() {
            return Err("a release that publishes nothing".to_string());
        }

        Ok(Table {
            doc: doc_text,
            audited,
            artifacts,
        })
    }

    /// The row of this name, as written rather than expanded.
    pub fn artifact(&self, name: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.name == name)
    }

    /// Every name a release of `version` publishes, in table order,
    /// with the platform row expanded across `targets`.
    pub fn names(&self, version: &str, targets: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for artifact in self.artifacts.iter().filter(|a| a.published()) {
            match artifact.made {
                Made::Platform => out.extend(targets.iter().map(|t| artifact.expand(version, t))),
                _ => out.push(artifact.expand(version, "")),
            }
        }
        out
    }

    /// Checks the tree under `root` against the table.
    pub fn check(&self, root: &Path) -> Result<Vec<Note>, String> {
        let mut notes = Vec::new();
        for artifact in &self.artifacts {
            if let Made::File { from } = &artifact.made
                && !root.join(from).exists()
            {
                notes.push(Note::Absent {
                    name: artifact.name.clone(),
                    from: from.clone(),
                    line: artifact.line,
                });
            }
        }
        for repo in REPOS.iter().filter(|r| **r != MAKER) {
            if !self
                .artifacts
                .iter()
                .any(|a| a.consumers.iter().any(|c| c == repo))
            {
                notes.push(Note::Idle {
                    repo: (*repo).to_string(),
                });
            }
        }
        // A workflow that assembles from somewhere else is the second
        // list this table exists to prevent, so the check is that the
        // release runs these two commands and not that it mentions the
        // artifacts.
        let text = std::fs::read_to_string(root.join(WORKFLOW)).unwrap_or_default();
        for command in ["artifacts --assemble", "artifacts --verify"] {
            if !text.contains(command) {
                notes.push(Note::Unheld {
                    command: command.to_string(),
                });
            }
        }
        Ok(notes)
    }

    /// Gathers a release of `version` into `out`, from the tree at
    /// `root` and the platform builds under `built`.
    ///
    /// What is assembled is the table and nothing else, which is what
    /// makes the table the list. A row nothing makes yet is skipped and
    /// reported rather than silently absent, since "the release has six
    /// files" should be a sentence somebody can check against seven
    /// rows.
    pub fn assemble(
        &self,
        root: &Path,
        built: &Path,
        out: &Path,
        version: &str,
        targets: &[String],
    ) -> Result<Vec<Shipped>, String> {
        if version.is_empty() || version.contains(['/', '\\', ' ']) {
            return Err(format!("{version:?} is not a version"));
        }
        std::fs::create_dir_all(out).map_err(|e| format!("creating {}: {e}", out.display()))?;
        let mut made = Vec::new();
        for artifact in self.artifacts.iter().filter(|a| a.published()) {
            match &artifact.made {
                Made::File { from } => {
                    let name = artifact.expand(version, "");
                    let source = root.join(from);
                    let bytes = std::fs::read(&source)
                        .map_err(|e| format!("reading {}: {e}", source.display()))?;
                    made.push(write(out, &name, &bytes, "file")?);
                }
                Made::Corpus => {
                    let readme = root.join(README);
                    let packed = corpus::pack(
                        &root.join(CASES),
                        readme.exists().then_some(readme.as_path()),
                        version,
                    )?;
                    let name = artifact.expand(version, "");
                    made.push(write(out, &name, &packed.archive, "corpus")?);
                }
                Made::Platform => {
                    for target in targets {
                        let name = artifact.expand(version, target);
                        // The build uploaded one directory per target
                        // under the name the release publishes it as,
                        // so the two agree by construction rather than
                        // by a second convention written down here.
                        let prefix = format!("libzu-{target}");
                        let archive = pack(&built.join(&prefix), &prefix)?;
                        made.push(write(out, &name, &archive, "platform")?);
                    }
                }
                Made::Later { .. } => unreachable!("published rows only"),
            }
        }
        Ok(made)
    }

    /// Reads a release directory back against the table.
    ///
    /// In both directions, for the same reason every other table in
    /// this repository is: a missing artifact is a consumer's failure
    /// tomorrow, and an unexpected one is a file somebody will fetch
    /// and nothing promises.
    pub fn verify(
        &self,
        dir: &Path,
        version: &str,
        targets: &[String],
    ) -> Result<(Vec<Shipped>, Vec<Fault>), String> {
        let mut found: BTreeMap<String, u64> = BTreeMap::new();
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry
                .metadata()
                .map_err(|e| format!("reading {name}: {e}"))?;
            if meta.is_dir() {
                return Err(format!(
                    "{}/{name} is a directory, and a release is files",
                    dir.display()
                ));
            }
            found.insert(name, meta.len());
        }

        let mut shipped = Vec::new();
        let mut faults = Vec::new();
        for name in self.names(version, targets) {
            match found.remove(&name) {
                None => faults.push(Fault::Missing { name }),
                Some(0) => faults.push(Fault::Empty { name }),
                Some(bytes) => shipped.push(Shipped {
                    name,
                    bytes,
                    kind: "published",
                }),
            }
        }
        faults.extend(found.into_keys().map(|file| Fault::Stranger { file }));
        Ok((shipped, faults))
    }
}

/// The tier 1 targets, which are the platform table's business and not
/// this one's.
pub fn tier1(root: &Path) -> Result<Vec<String>, String> {
    let table = platforms::Table::load(&root.join(platforms::PATH))?;
    Ok(table
        .platforms
        .iter()
        .filter(|p| p.tier == platforms::TIER1)
        .map(|p| p.target.clone())
        .collect())
}

/// Whether this is a milestone of the two programs, which is what a row
/// nothing makes yet has to wait for.
fn is_milestone(name: &str) -> bool {
    let digit = |rest: &str| matches!(rest.parse::<u8>(), Ok(0..=6));
    match name.strip_prefix("DX") {
        Some(rest) => digit(rest),
        None => name.strip_prefix('D').is_some_and(digit),
    }
}

/// One file of the release, written and weighed.
fn write(out: &Path, name: &str, bytes: &[u8], kind: &'static str) -> Result<Shipped, String> {
    let path = out.join(name);
    std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(Shipped {
        name: name.to_string(),
        bytes: bytes.len() as u64,
        kind,
    })
}

/// A directory packed under `prefix`, which is how a platform's build
/// becomes one file of the release.
///
/// Everything the build uploaded goes in, sorted, so that adding a file
/// to the workflow's upload is the whole of adding it to the artifact.
/// An empty directory is an error, because a platform that built
/// nothing is otherwise the cheapest way to publish a release without
/// it.
fn pack(dir: &Path, prefix: &str) -> Result<Vec<u8>, String> {
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    if files.is_empty() {
        return Err(format!("{} built nothing", dir.display()));
    }
    files.sort();
    let mut under = Vec::with_capacity(files.len());
    for name in files {
        let bytes = std::fs::read(dir.join(&name))
            .map_err(|e| format!("reading {}: {e}", dir.join(&name).display()))?;
        under.push((format!("{prefix}/{name}"), bytes));
    }
    tarball::compress(&tarball::tar(&under)?)
}

/// Every file under `dir`, named relative to `root`, with `/` on every
/// platform because that is what a tar holds.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .path();
        if path.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let name: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        out.push(name.join("/"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    const TABLE: &str = concat!(
        "schema = 1\n",
        "doc = \"What a release publishes.\"\n",
        "audited = \"2026-08-16\"\n",
        "\n",
        "[[artifact]]\n",
        "name = \"libzu-<target>.tar.zst\"\n",
        "made = \"platform\"\n",
        "consumers = [\"zu-c\"]\n",
        "doc = \"One per tier 1 target.\"\n",
        "\n",
        "[[artifact]]\n",
        "name = \"zu.h\"\n",
        "made = \"file\"\n",
        "from = \"crates/zu-capi/include/zu.h\"\n",
        "consumers = [\"zu-c\", \"zu-go\"]\n",
        "doc = \"The C ABI.\"\n",
        "\n",
        "[[artifact]]\n",
        "name = \"gql.json\"\n",
        "made = \"later\"\n",
        "milestone = \"D2\"\n",
        "consumers = [\"zu-web\"]\n",
        "doc = \"The language as data.\"\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    /// A scratch tree, so a test can assemble into something.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zu-artifacts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
        dir
    }

    #[test]
    fn a_row_is_read_as_written() {
        let table = table();
        let header = table.artifact("zu.h").expect("the header is a row");
        assert_eq!(
            header.made,
            Made::File {
                from: "crates/zu-capi/include/zu.h".to_string()
            }
        );
        assert!(header.published());
        assert_eq!(header.consumers, ["zu-c", "zu-go"]);
        let gql = table.artifact("gql.json").expect("gql.json is a row");
        assert!(!gql.published(), "nothing makes it yet");
        assert_eq!(gql.made.kind(), "later");
    }

    #[test]
    fn a_name_is_expanded_per_version_and_per_target() {
        let targets = ["x86_64-apple-darwin".to_string()];
        let names = table().names("0.5.0", &targets);
        assert_eq!(names, ["libzu-x86_64-apple-darwin.tar.zst", "zu.h"]);
    }

    #[test]
    fn a_row_nothing_makes_yet_needs_the_milestone_that_will() {
        let text = TABLE.replace("milestone = \"D2\"\n", "");
        let error = Table::parse(&text).expect_err("later and no milestone");
        assert!(error.contains("by no milestone"), "{error}");
        let text = TABLE.replace("milestone = \"D2\"", "milestone = \"D9\"");
        let error = Table::parse(&text).expect_err("a milestone that does not exist");
        assert!(error.contains("DX0 to DX6"), "{error}");
    }

    #[test]
    fn a_key_that_belongs_to_another_kind_of_row_is_refused() {
        let text = TABLE.replace(
            "made = \"later\"\nmilestone = \"D2\"",
            "made = \"later\"\nmilestone = \"D2\"\nfrom = \"docs/gql.json\"",
        );
        let error = Table::parse(&text).expect_err("a later row with a from");
        assert!(error.contains("which only a file has"), "{error}");

        let text = TABLE.replace(
            "made = \"file\"\nfrom = \"crates/zu-capi/include/zu.h\"",
            "made = \"file\"\nfrom = \"crates/zu-capi/include/zu.h\"\nmilestone = \"DX1\"",
        );
        let error = Table::parse(&text).expect_err("a file row with a milestone");
        assert!(error.contains("nothing makes yet does"), "{error}");
    }

    #[test]
    fn a_file_row_with_nothing_to_copy_is_refused() {
        let text = TABLE.replace("from = \"crates/zu-capi/include/zu.h\"\n", "");
        let error = Table::parse(&text).expect_err("a file with no from");
        assert!(error.contains("a file with no from"), "{error}");
    }

    #[test]
    fn a_platform_row_that_names_one_artifact_is_refused() {
        let text = TABLE.replace("libzu-<target>.tar.zst", "libzu.tar.zst");
        let error = Table::parse(&text).expect_err("one name for seven platforms");
        assert!(error.contains("same for all of them"), "{error}");
    }

    #[test]
    fn an_artifact_the_maker_consumes_is_refused() {
        let text = TABLE.replace("consumers = [\"zu-web\"]", "consumers = [\"zu\"]");
        let error = Table::parse(&text).expect_err("zu consuming its own release");
        assert!(error.contains("is not a consumer"), "{error}");
    }

    #[test]
    fn a_consumer_that_is_not_a_repository_is_refused() {
        let text = TABLE.replace("consumers = [\"zu-web\"]", "consumers = [\"zu-perl\"]");
        let error = Table::parse(&text).expect_err("a tenth repository");
        assert!(error.contains("nine repositories"), "{error}");
    }

    #[test]
    fn an_artifact_written_twice_is_refused() {
        let at = TABLE.find("[[artifact]]").expect("a row");
        let text = format!("{TABLE}\n{}", &TABLE[at..]);
        let error = Table::parse(&text).expect_err("two rows with one name");
        assert!(error.contains("is written twice"), "{error}");
    }

    #[test]
    fn a_schema_this_reader_does_not_read_is_refused() {
        let error = Table::parse(&TABLE.replace("schema = 1", "schema = 2")).expect_err("schema 2");
        assert!(error.contains("reads schema 1"), "{error}");
    }

    #[test]
    fn a_file_the_tree_does_not_hold_is_reported() {
        let root = scratch("absent");
        let notes = table().check(&root).expect("the check runs");
        assert!(
            notes
                .iter()
                .any(|n| matches!(n, Note::Absent { name, .. } if name == "zu.h")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| matches!(n, Note::Unheld { .. })),
            "a tree with no release workflow holds the table to nothing: {notes:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_release_assembles_and_verifies_against_the_table() {
        let dir = scratch("assemble");
        let root = dir.join("tree");
        let built = dir.join("built");
        let out = dir.join("dist");
        std::fs::create_dir_all(root.join("crates/zu-capi/include")).expect("writes");
        std::fs::write(root.join("crates/zu-capi/include/zu.h"), "#define ZU 1\n").expect("writes");
        std::fs::create_dir_all(built.join("libzu-x86_64-apple-darwin")).expect("writes");
        std::fs::write(
            built.join("libzu-x86_64-apple-darwin/libzu.dylib"),
            vec![9u8; 4096],
        )
        .expect("writes");

        let targets = ["x86_64-apple-darwin".to_string()];
        let made = table()
            .assemble(&root, &built, &out, "0.5.0", &targets)
            .expect("assembles");
        assert_eq!(made.len(), 2, "{made:?}");
        assert_eq!(made[0].name, "libzu-x86_64-apple-darwin.tar.zst");
        assert_eq!(made[1].kind, "file");

        let (shipped, faults) = table().verify(&out, "0.5.0", &targets).expect("verifies");
        assert_eq!(faults, [] as [Fault; 0]);
        assert_eq!(shipped.len(), 2);
        assert!(shipped.iter().all(|s| s.bytes > 0));

        // The platform archive holds what the build uploaded, under the
        // name the release publishes it as.
        let archive = std::fs::read(out.join("libzu-x86_64-apple-darwin.tar.zst")).expect("reads");
        let tar = zstd::bulk::decompress(&archive, 1 << 20).expect("unpacks");
        let names: Vec<String> = tarball::entries(&tar).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["libzu-x86_64-apple-darwin/libzu.dylib"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_missing_an_artifact_says_which_one() {
        let dir = scratch("missing");
        std::fs::write(dir.join("zu.h"), "#define ZU 1\n").expect("writes");
        let targets = ["x86_64-apple-darwin".to_string()];
        let (_, faults) = table().verify(&dir, "0.5.0", &targets).expect("verifies");
        assert_eq!(
            faults,
            [Fault::Missing {
                name: "libzu-x86_64-apple-darwin.tar.zst".to_string()
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_nothing_promised_is_reported_and_so_is_an_empty_one() {
        let dir = scratch("stranger");
        std::fs::write(dir.join("zu.h"), "").expect("writes");
        std::fs::write(dir.join("secrets.env"), "x=1\n").expect("writes");
        let (_, faults) = table().verify(&dir, "0.5.0", &[]).expect("verifies");
        assert!(
            faults.contains(&Fault::Empty {
                name: "zu.h".to_string()
            }),
            "{faults:?}"
        );
        assert!(
            faults.contains(&Fault::Stranger {
                file: "secrets.env".to_string()
            }),
            "{faults:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_platform_that_built_nothing_stops_the_release() {
        let dir = scratch("nothing");
        let built = dir.join("built");
        std::fs::create_dir_all(built.join("libzu-x86_64-apple-darwin")).expect("writes");
        std::fs::create_dir_all(dir.join("tree/crates/zu-capi/include")).expect("writes");
        std::fs::write(dir.join("tree/crates/zu-capi/include/zu.h"), "x").expect("writes");
        let error = table()
            .assemble(
                &dir.join("tree"),
                &built,
                &dir.join("dist"),
                "0.5.0",
                &["x86_64-apple-darwin".to_string()],
            )
            .expect_err("an empty build directory");
        assert!(error.contains("built nothing"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_that_would_become_a_path_is_refused() {
        let dir = scratch("version");
        for version in ["", "../etc", "1.0 rc1"] {
            assert!(
                table()
                    .assemble(&dir, &dir, &dir.join("dist"), version, &[])
                    .is_err(),
                "{version:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_committed_contract_is_what_this_tree_publishes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = Table::load(&root.join(PATH)).expect("the committed table loads");
        let notes = table.check(&root).expect("the check runs");
        assert!(notes.is_empty(), "{notes:#?}");

        // dx/14 section 6 lists what a release publishes, and it is
        // this list. A row that quietly appeared or went is the
        // contract changing without anybody saying so.
        let mut names: Vec<&str> = table.artifacts.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "cli.json",
                "conformance-<version>.tar.zst",
                "errors.json",
                "gql.json",
                "libzu-<target>.tar.zst",
                "model.json",
                "zu.h",
            ]
        );

        // The seven platform artifacts are the platform table's seven,
        // which is the whole reason this row is expanded rather than
        // written out.
        let targets = tier1(&root).expect("the platform table loads");
        assert_eq!(targets.len(), 7);
        assert_eq!(table.names("0.5.0", &targets).len(), 7 + 3);
    }
}
