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
//! Five kinds of row, because there are five ways an artifact comes to
//! exist here. A `file` is a path the tree already holds. A `corpus` is
//! packed by the packer beside this. A `platform` row is one artifact
//! per tier 1 target, expanded against `platforms.toml` so the seven
//! move with that table rather than with this one. A `sums` row is the
//! digests of everything else, written last because it is over the rest
//! of the release and read first by anything that installs from it. And
//! a `later` row is an artifact the contract names that nothing makes
//! yet, carrying the milestone that will make it: naming it early is
//! the point, since a consumer needs to know what a release will
//! eventually carry, and the alternative is eight repositories each
//! guessing.

use std::collections::BTreeMap;
use std::path::Path;

use crate::toml::Doc;
use crate::{corpus, platforms, sha256, tarball};

/// The table's schema version, which moves when the shape of the file
/// changes and not when the artifacts do.
pub const SCHEMA: i64 = 1;

/// Where the table is.
pub const PATH: &str = "artifacts.toml";

/// The workflow that has to assemble from it.
pub const WORKFLOW: &str = ".github/workflows/release.yml";

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
    /// The digests of everything else, written last because it is over
    /// the rest of the release.
    Sums,
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
            Made::Sums => "sums",
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
    /// A published file the digest list does not carry, which is a file
    /// an installer cannot check and therefore will not install.
    Unsummed { name: String },
    /// A published file whose digest is not the one recorded beside it.
    Mismatch { name: String },
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Missing { name } => write!(f, "{name} is in {PATH} and not in the release"),
            Fault::Empty { name } => write!(f, "{name} is in the release and is empty"),
            Fault::Stranger { file } => {
                write!(f, "{file} is in the release and not in {PATH}")
            }
            Fault::Unsummed { name } => {
                write!(f, "{name} is in the release and has no digest beside it")
            }
            Fault::Mismatch { name } => write!(
                f,
                "{name} is not the file the digest list says it is, so either it was rebuilt \
                 after the digests were written or it was replaced"
            ),
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
                Some("sums") => Made::Sums,
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
                         platform, sums and later"
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
            // Which repositories those are is `repos.toml`'s business
            // and checked there, because the list of repositories is
            // one list and this is not where it lives.
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
        // One digest file, because two of them is a user asking which
        // one to trust and an installer picking whichever it was
        // written to read.
        if artifacts.iter().filter(|a| a.made == Made::Sums).count() > 1 {
            return Err("two rows write the digests of the release".to_string());
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
    /// `root` and the platform builds under `built`, packing the
    /// platform archives at `level`.
    ///
    /// What is assembled is the table and nothing else, which is what
    /// makes the table the list. A row nothing makes yet is skipped and
    /// reported rather than silently absent, since "the release has six
    /// files" should be a sentence somebody can check against seven
    /// rows.
    ///
    /// The level is the caller's because one caller is not publishing:
    /// the install check assembles a release of one platform, installs
    /// from it and deletes it, and [`tarball::LEVEL`] over an install
    /// prefix costs two minutes for a download that never happens.
    pub fn assemble(
        &self,
        root: &Path,
        built: &Path,
        out: &Path,
        version: &str,
        targets: &[String],
        level: i32,
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
                        let archive = pack(&built.join(&prefix), &prefix, level)?;
                        made.push(write(out, &name, &archive, "platform")?);
                    }
                }
                // Written after this loop, because it is over what the
                // loop produced.
                Made::Sums => {}
                Made::Later { .. } => unreachable!("published rows only"),
            }
        }
        if let Some(artifact) = self.artifacts.iter().find(|a| a.made == Made::Sums) {
            let name = artifact.expand(version, "");
            made.sort_by(|a, b| a.name.cmp(&b.name));
            let mut lines = String::new();
            for shipped in &made {
                let path = out.join(&shipped.name);
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                // The format `sha256sum -c` reads: the digest, two
                // spaces, the name. An installer on a machine with no
                // coreutils reads the same line by hand, which is why
                // the whole file is one shape and not a header and a
                // table.
                lines.push_str(&format!("{}  {}\n", sha256::hex(&bytes), shipped.name));
            }
            made.push(write(out, &name, lines.as_bytes(), "sums")?);
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

        // The digests are checked here rather than trusted, because the
        // one thing a release cannot ship is a digest list that does not
        // describe the release: an installer that verifies a download
        // against it would refuse every install, and the failure would
        // arrive as a user's bug report rather than as this line.
        if let Some(artifact) = self.artifacts.iter().find(|a| a.made == Made::Sums) {
            let list = artifact.expand(version, "");
            let text = std::fs::read_to_string(dir.join(&list)).unwrap_or_default();
            let recorded: BTreeMap<&str, &str> = text
                .lines()
                .filter_map(|line| line.split_once("  "))
                .map(|(digest, name)| (name, digest))
                .collect();
            for entry in shipped.iter().filter(|s| s.name != list) {
                let path = dir.join(&entry.name);
                let bytes =
                    std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                match recorded.get(entry.name.as_str()) {
                    None => faults.push(Fault::Unsummed {
                        name: entry.name.clone(),
                    }),
                    Some(digest) if *digest != sha256::hex(&bytes) => {
                        faults.push(Fault::Mismatch {
                            name: entry.name.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
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

/// Narrows the tier 1 targets to the ones asked for by name.
///
/// A release is every platform, and the only other thing anybody wants
/// here is one of them: the job that built a platform can assemble that
/// platform's release out of what it has in hand, which is what the
/// install check installs from. An empty ask is the whole list rather
/// than none of it, since that is the release.
///
/// A name that is not a tier 1 target is refused rather than dropped,
/// because a triple with a typo in it would otherwise assemble a release
/// of no platforms and say nothing about why.
pub fn only(tier1: &[String], wanted: &[String]) -> Result<Vec<String>, String> {
    if wanted.is_empty() {
        return Ok(tier1.to_vec());
    }
    for target in wanted {
        if !tier1.contains(target) {
            return Err(format!(
                "{target} is not a tier 1 platform of {}, which has {}",
                platforms::PATH,
                tier1.join(", ")
            ));
        }
    }
    Ok(tier1
        .iter()
        .filter(|target| wanted.contains(target))
        .cloned()
        .collect())
}

/// Whether this is a milestone of the two programs, which is what a row
/// nothing makes yet has to wait for, and what a repository that does
/// not exist yet is created at.
pub fn is_milestone(name: &str) -> bool {
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
fn pack(dir: &Path, prefix: &str, level: i32) -> Result<Vec<u8>, String> {
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
    tarball::compress_at(&tarball::tar(&under)?, level)
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

    use crate::scratch::Scratch;

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

    /// The same contract with the digest list on it, which is its own
    /// constant so the tests above keep counting the rows they were
    /// written to count.
    const SUMMED: &str = concat!(
        "[[artifact]]\n",
        "name = \"SHA256SUMS\"\n",
        "made = \"sums\"\n",
        "consumers = [\"zu-c\"]\n",
        "doc = \"The digests of the rest.\"\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    fn summed() -> Table {
        Table::parse(&format!("{TABLE}\n{SUMMED}")).expect("the table parses")
    }

    /// A release of the `summed` table, assembled into a fresh
    /// directory, which is what the digest tests all start from.
    fn released(name: &str) -> (Scratch, PathBuf, Vec<String>) {
        let dir = scratch(name);
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
        let targets = vec!["x86_64-apple-darwin".to_string()];
        summed()
            .assemble(&root, &built, &out, "0.5.0", &targets, tarball::LEVEL)
            .expect("assembles");
        (dir, out, targets)
    }

    /// A scratch tree, so a test can assemble into something.
    fn scratch(name: &str) -> Scratch {
        Scratch::new(&format!("artifacts-{name}"))
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
    fn an_artifact_nobody_fetches_is_refused() {
        let text = TABLE.replace("consumers = [\"zu-web\"]", "consumers = []");
        let error = Table::parse(&text).expect_err("a row for nobody");
        assert!(error.contains("fetched by nobody"), "{error}");
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
            .assemble(&root, &built, &out, "0.5.0", &targets, tarball::LEVEL)
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
    }

    /// The install check assembles the release of the one platform its
    /// job built, since that is the only platform whose package is on
    /// that machine. Nothing else about the release changes: the same
    /// rows, the same packer, and a digest list over what came out.
    #[test]
    fn one_platform_is_a_release_of_that_platform_and_the_rest_of_the_rows() {
        let tier1: Vec<String> = ["x86_64-apple-darwin", "x86_64-unknown-linux-musl"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(only(&tier1, &[]).expect("all of them"), tier1);
        assert_eq!(
            only(&tier1, &["x86_64-apple-darwin".to_string()]).expect("one of them"),
            ["x86_64-apple-darwin"]
        );

        // A triple with a typo in it would otherwise be a release of
        // everything but the platform somebody asked for.
        let err = only(&tier1, &["x86_64-apple-darwn".to_string()]).expect_err("refused");
        assert!(err.contains("x86_64-apple-darwn"), "{err}");
        assert!(err.contains("x86_64-apple-darwin"), "it says which: {err}");
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
                tarball::LEVEL,
            )
            .expect_err("an empty build directory");
        assert!(error.contains("built nothing"), "{error}");
    }

    #[test]
    fn a_version_that_would_become_a_path_is_refused() {
        let dir = scratch("version");
        for version in ["", "../etc", "1.0 rc1"] {
            assert!(
                table()
                    .assemble(&dir, &dir, &dir.join("dist"), version, &[], tarball::LEVEL)
                    .is_err(),
                "{version:?}"
            );
        }
    }

    /// The digest list is the file an installer reads before it reads
    /// anything else, so it has to describe the release it shipped with
    /// exactly: a line per file, in the format `sha256sum -c` reads, and
    /// no line for itself, which nothing could check.
    #[test]
    fn the_release_carries_the_digest_of_everything_it_publishes() {
        // Bound and not dropped: the scratch owns the tree `out` is
        // in, and `_` rather than `_dir` would take it away before
        // the check runs.
        let (_dir, out, targets) = released("sums");
        let text = std::fs::read_to_string(out.join("SHA256SUMS")).expect("reads");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        for (line, name) in lines
            .iter()
            .zip(["libzu-x86_64-apple-darwin.tar.zst", "zu.h"])
        {
            let (digest, file) = line.split_once("  ").expect("a sha256sum line");
            assert_eq!(file, name);
            assert_eq!(digest.len(), 64, "{line}");
            let bytes = std::fs::read(out.join(file)).expect("reads");
            assert_eq!(digest, sha256::hex(&bytes), "{file}");
        }
        assert!(!text.contains("SHA256SUMS"), "a list cannot check itself");

        let (shipped, faults) = summed().verify(&out, "0.5.0", &targets).expect("verifies");
        assert_eq!(faults, [] as [Fault; 0]);
        assert_eq!(shipped.len(), 3);
    }

    /// The failure this check exists for: an artifact rebuilt after the
    /// digests were written, which an installer would refuse and a
    /// release should catch here instead.
    #[test]
    fn a_file_that_is_not_what_its_digest_says_stops_the_release() {
        // Bound and not dropped: the scratch owns the tree `out` is
        // in, and `_` rather than `_dir` would take it away before
        // the check runs.
        let (_dir, out, targets) = released("mismatch");
        std::fs::write(out.join("zu.h"), "#define ZU 2\n").expect("writes");
        let (_, faults) = summed().verify(&out, "0.5.0", &targets).expect("verifies");
        assert_eq!(
            faults,
            [Fault::Mismatch {
                name: "zu.h".to_string()
            }]
        );

        // And a published file with no line at all, which is the same
        // failure from the other side: an installer that cannot check
        // it will not install it.
        let text = std::fs::read_to_string(out.join("SHA256SUMS")).expect("reads");
        let kept: String = text
            .lines()
            .filter(|l| !l.ends_with("zu.h"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(out.join("SHA256SUMS"), kept).expect("writes");
        let (_, faults) = summed().verify(&out, "0.5.0", &targets).expect("verifies");
        assert_eq!(
            faults,
            [Fault::Unsummed {
                name: "zu.h".to_string()
            }]
        );
    }

    #[test]
    fn two_rows_of_digests_are_refused() {
        let text = format!(
            "{TABLE}\n{SUMMED}\n{}",
            SUMMED.replace("SHA256SUMS", "sums")
        );
        let error = Table::parse(&text).expect_err("two digest lists");
        assert!(error.contains("two rows write the digests"), "{error}");
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
                "SHA256SUMS",
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
        assert_eq!(table.names("0.5.0", &targets).len(), 7 + 4);
    }
}
