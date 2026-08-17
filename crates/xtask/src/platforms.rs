//! The platform table, and the check that the matrix builds it.
//!
//! Seven targets are tier 1, which means a prebuilt binary for every
//! SDK, a full test matrix per release, and a release that stops when
//! one of them fails. That promise is worth what CI proves of it, so
//! the list lives in `platforms.toml` and the workflow that builds
//! `libzu` is held to it: a target the table calls tier 1 and the
//! matrix does not name is the promise nothing keeps, and a matrix row
//! naming a target the table does not have is a platform being shipped
//! by nobody's decision.
//!
//! The rest of a row is what the workflow needs to build it: the runner
//! that has that machine, the container the two gnu targets build in so
//! that the glibc they link against is the floor rather than whatever
//! the runner happens to have, the names the three artifacts come out
//! under, and whether the runner can run what it built. Those are
//! checked too, because a row nothing reads is a row that drifts.
//!
//! The budgets are the other half of the file. A size ceiling per
//! artifact, gated on every platform, because binary size only ever
//! drifts upward and it is a real adoption factor for serverless and
//! mobile targets.

use std::collections::BTreeMap;
use std::path::Path;

use crate::matrix::{self, Row};
use crate::toml::Doc;

/// The table's schema version, which moves when the shape of the file
/// changes and not when the platforms do.
pub const SCHEMA: i64 = 2;

/// Where the table is.
pub const PATH: &str = "platforms.toml";

/// The workflow whose matrix has to be the table.
pub const WORKFLOW: &str = ".github/workflows/libzu.yml";

/// This repository's name in a budget's `repo`.
pub const REPO: &str = "zu";

/// The tier that blocks a release.
pub const TIER1: i64 = 1;

/// One platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub target: String,
    pub tier: i64,
    /// The runner that is this machine, or none when no hosted runner
    /// is, which is a row this repository does not build today.
    pub runner: Option<String>,
    /// The image the build runs in, which is how the gnu targets get
    /// the glibc they promise rather than the runner's own.
    pub container: Option<String>,
    pub lib: String,
    /// The static archive beside the shared library, which dx/09 C-5
    /// ships as well and which a binding that wants its users to
    /// install one file links instead.
    pub staticlib: String,
    pub exe: String,
    /// Whether the runner can execute what it built. A cross compiled
    /// row is measured and not exercised, and saying so is the
    /// difference between a matrix that tests seven platforms and one
    /// that claims to.
    pub smoke: bool,
    pub doc: String,
    pub line: usize,
}

/// One size ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub artifact: String,
    pub mib: i64,
    pub repo: String,
    pub doc: String,
    pub line: usize,
}

/// One built artifact against its ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Weight {
    pub artifact: String,
    pub file: String,
    pub bytes: u64,
    pub cap: u64,
}

impl Weight {
    pub fn over(&self) -> bool {
        self.bytes > self.cap
    }
}

impl std::fmt::Display for Weight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mib = |bytes: u64| bytes as f64 / 1024.0 / 1024.0;
        write!(
            f,
            "{:<12} {:6.2} MiB of {:2} ({}%)",
            self.file,
            mib(self.bytes),
            self.cap / 1024 / 1024,
            self.bytes * 100 / self.cap
        )
    }
}

/// The table.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub audited: String,
    pub platforms: Vec<Platform>,
    pub budgets: Vec<Budget>,
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A tier-1 target the matrix does not build. The release promise
    /// for it is a sentence nothing keeps.
    Missing { target: String, line: usize },
    /// A matrix row for a target the table does not have.
    Stranger { target: String, line: usize },
    /// One target built twice, which is two artifacts with one name.
    Twice { target: String, line: usize },
    /// A row that builds the right target the wrong way.
    Disagrees {
        target: String,
        line: usize,
        key: String,
        found: String,
        want: String,
    },
}

impl Note {
    pub fn line(&self) -> usize {
        match self {
            Note::Missing { line, .. }
            | Note::Stranger { line, .. }
            | Note::Twice { line, .. }
            | Note::Disagrees { line, .. } => *line,
        }
    }
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Missing { target, line } => write!(
                f,
                "{PATH}:{line}: {target} is tier {TIER1} and {WORKFLOW} does not build it"
            ),
            Note::Stranger { target, line } => write!(
                f,
                "{WORKFLOW}:{line}: {target} is built here and {PATH} does not have it"
            ),
            Note::Twice { target, line } => {
                write!(f, "{WORKFLOW}:{line}: {target} is built twice")
            }
            Note::Disagrees {
                target,
                line,
                key,
                found,
                want,
            } => write!(
                f,
                "{WORKFLOW}:{line}: {target} has {key} {found:?} and {PATH} says {want:?}"
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
    pub fn parse(text: &str) -> Result<Table, String> {
        let doc = Doc::parse(text)?;
        if let Some(key) = doc.root.unknown(&["schema", "doc", "audited"]).first() {
            return Err(format!("a table has no key {key:?}"));
        }
        if let Some(name) = doc.unknown_arrays(&["platform", "budget"]).first() {
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

        let mut platforms = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for table in doc.array("platform") {
            let line = table.line;
            if let Some(key) = table
                .unknown(&[
                    "target",
                    "tier",
                    "runner",
                    "container",
                    "lib",
                    "staticlib",
                    "exe",
                    "smoke",
                    "doc",
                ])
                .first()
            {
                return Err(format!("line {line}: a platform has no key {key:?}"));
            }
            let target = table
                .str("target")
                .ok_or_else(|| format!("line {line}: a platform with no target"))?
                .to_string();
            let tier = match table.int("tier") {
                Some(tier @ 1..=3) => tier,
                found => {
                    return Err(format!(
                        "line {line}: {target} is tier {found:?}, and there are three"
                    ));
                }
            };
            let runner = table.str("runner").map(str::to_string);
            let container = table.str("container").map(str::to_string);
            if container.is_some() && runner.is_none() {
                return Err(format!(
                    "line {line}: {target} builds in a container on no runner"
                ));
            }
            let smoke = table.bool("smoke").unwrap_or(false);
            if smoke && runner.is_none() {
                return Err(format!(
                    "line {line}: {target} is smoke tested by a runner it does not have"
                ));
            }
            let lib = table
                .str("lib")
                .ok_or_else(|| format!("line {line}: {target} builds libzu under no name"))?
                .to_string();
            let staticlib = table
                .str("staticlib")
                .ok_or_else(|| {
                    format!("line {line}: {target} builds the static libzu under no name")
                })?
                .to_string();
            let exe = table
                .str("exe")
                .ok_or_else(|| format!("line {line}: {target} builds the CLI under no name"))?
                .to_string();
            let doc = table
                .str("doc")
                .ok_or_else(|| format!("line {line}: {target} has no doc"))?
                .to_string();
            if let Some(before) = seen.insert(target.clone(), line) {
                return Err(format!(
                    "line {line}: {target} is written twice, and first on line {before}"
                ));
            }
            platforms.push(Platform {
                target,
                tier,
                runner,
                container,
                lib,
                staticlib,
                exe,
                smoke,
                doc,
                line,
            });
        }
        if !platforms.iter().any(|p| p.tier == TIER1) {
            return Err("a table with no tier 1 platform in it".to_string());
        }

        let mut budgets = Vec::new();
        let mut named: BTreeMap<String, usize> = BTreeMap::new();
        for table in doc.array("budget") {
            let line = table.line;
            if let Some(key) = table.unknown(&["artifact", "mib", "repo", "doc"]).first() {
                return Err(format!("line {line}: a budget has no key {key:?}"));
            }
            let artifact = table
                .str("artifact")
                .ok_or_else(|| format!("line {line}: a budget for no artifact"))?
                .to_string();
            let mib = match table.int("mib") {
                Some(mib) if mib > 0 => mib,
                found => {
                    return Err(format!(
                        "line {line}: {artifact} is capped at {found:?} MiB, which is not a size"
                    ));
                }
            };
            let repo = table
                .str("repo")
                .ok_or_else(|| format!("line {line}: {artifact} is built by no repository"))?
                .to_string();
            let doc = table
                .str("doc")
                .ok_or_else(|| format!("line {line}: {artifact} has no doc"))?
                .to_string();
            if let Some(before) = named.insert(artifact.clone(), line) {
                return Err(format!(
                    "line {line}: {artifact} is capped twice, and first on line {before}"
                ));
            }
            budgets.push(Budget {
                artifact,
                mib,
                repo,
                doc,
                line,
            });
        }

        Ok(Table {
            doc: doc_text,
            audited,
            platforms,
            budgets,
        })
    }

    /// The platform of this target.
    pub fn platform(&self, target: &str) -> Option<&Platform> {
        self.platforms.iter().find(|p| p.target == target)
    }

    /// The budget for this artifact.
    pub fn budget(&self, artifact: &str) -> Option<&Budget> {
        self.budgets.iter().find(|b| b.artifact == artifact)
    }

    /// What this repository's artifacts for `target` weigh, given the
    /// directory the build put them in.
    ///
    /// The two of them are the shared library and the CLI, and which
    /// file each is on a platform is that row's `lib` and `exe`, since
    /// the same build is `libzu.so`, `libzu.dylib` and `zu.dll`
    /// depending on who is asking. A file that is not there is an
    /// error rather than a zero, because the build not producing it is
    /// the failure this would otherwise report as a pass.
    pub fn weigh(&self, target: &str, dir: &Path) -> Result<Vec<Weight>, String> {
        let platform = self
            .platform(target)
            .ok_or_else(|| format!("{target} is not a platform {PATH} has"))?;
        let mut out = Vec::new();
        for budget in self.budgets.iter().filter(|b| b.repo == REPO) {
            let file = match budget.artifact.as_str() {
                "libzu" => &platform.lib,
                "zu" => &platform.exe,
                other => return Err(format!("{REPO} does not build {other}")),
            };
            let path = dir.join(file);
            let bytes = std::fs::metadata(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            out.push(Weight {
                artifact: budget.artifact.clone(),
                file: file.clone(),
                bytes,
                cap: budget.mib as u64 * 1024 * 1024,
            });
        }
        Ok(out)
    }

    /// Checks the workflow under `root` against the table.
    pub fn check(&self, root: &Path) -> Result<Vec<Note>, String> {
        let path = root.join(WORKFLOW);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok(self.against(&rows(&text)?))
    }

    /// The check itself, against a matrix already read, so that a test
    /// can hand it one without a file.
    fn against(&self, rows: &[Row]) -> Vec<Note> {
        let mut notes = Vec::new();
        let mut built: BTreeMap<&str, usize> = BTreeMap::new();
        for row in rows {
            let Some(target) = row.get("target") else {
                notes.push(Note::Stranger {
                    target: String::new(),
                    line: row.line,
                });
                continue;
            };
            if let Some(before) = built.insert(target, row.line) {
                notes.push(Note::Twice {
                    target: target.to_string(),
                    line: row.line.max(before),
                });
                continue;
            }
            let Some(platform) = self.platform(target) else {
                notes.push(Note::Stranger {
                    target: target.to_string(),
                    line: row.line,
                });
                continue;
            };
            // The empty string stands for a key the row leaves out,
            // which is what a matrix row without a container says and
            // what the workflow reads it as.
            for (key, want) in [
                ("runner", platform.runner.clone().unwrap_or_default()),
                ("container", platform.container.clone().unwrap_or_default()),
                ("lib", platform.lib.clone()),
                ("staticlib", platform.staticlib.clone()),
                ("exe", platform.exe.clone()),
                ("smoke", platform.smoke.to_string()),
            ] {
                let found = row.get(key).unwrap_or_default();
                if found != want {
                    notes.push(Note::Disagrees {
                        target: target.to_string(),
                        line: row.line,
                        key: key.to_string(),
                        found: found.to_string(),
                        want,
                    });
                }
            }
        }
        for platform in &self.platforms {
            if platform.tier == TIER1 && !built.contains_key(platform.target.as_str()) {
                notes.push(Note::Missing {
                    target: platform.target.clone(),
                    line: platform.line,
                });
            }
        }
        notes.sort_by_key(Note::line);
        notes
    }
}

/// The rows of the build workflow's matrix, read by the one reader.
fn rows(text: &str) -> Result<Vec<Row>, String> {
    matrix::rows(text, WORKFLOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = concat!(
        "schema = 2\n",
        "doc = \"The platforms zu builds for.\"\n",
        "audited = \"2026-08-15\"\n",
        "\n",
        "[[platform]]\n",
        "target = \"x86_64-unknown-linux-gnu\"\n",
        "tier = 1\n",
        "runner = \"ubuntu-latest\"\n",
        "container = \"quay.io/pypa/manylinux_2_28_x86_64:2026.08.15-1\"\n",
        "lib = \"libzu.so\"\n",
        "staticlib = \"libzu.a\"\n",
        "exe = \"zu\"\n",
        "smoke = true\n",
        "doc = \"The default Linux server.\"\n",
        "\n",
        "[[platform]]\n",
        "target = \"x86_64-apple-darwin\"\n",
        "tier = 1\n",
        "runner = \"macos-latest\"\n",
        "lib = \"libzu.dylib\"\n",
        "staticlib = \"libzu.a\"\n",
        "exe = \"zu\"\n",
        "smoke = false\n",
        "doc = \"Intel Macs, cross compiled.\"\n",
        "\n",
        "[[platform]]\n",
        "target = \"wasm32-wasip1\"\n",
        "tier = 2\n",
        "lib = \"zu.wasm\"\n",
        "staticlib = \"libzu.a\"\n",
        "exe = \"zu.wasm\"\n",
        "doc = \"WASI, built by the repositories that ship it.\"\n",
        "\n",
        "[[budget]]\n",
        "artifact = \"libzu\"\n",
        "mib = 14\n",
        "repo = \"zu\"\n",
        "doc = \"One shared library per platform.\"\n",
    );

    /// A matrix with both tier-1 rows of the table above, written the
    /// way the workflow writes them.
    const MATRIX: &str = concat!(
        "jobs:\n",
        "  build:\n",
        "    strategy:\n",
        "      matrix:\n",
        "        include:\n",
        "          - target: x86_64-unknown-linux-gnu\n",
        "            runner: ubuntu-latest\n",
        "            container: quay.io/pypa/manylinux_2_28_x86_64:2026.08.15-1\n",
        "            lib: libzu.so\n",
        "            staticlib: libzu.a\n",
        "            exe: zu\n",
        "            smoke: true\n",
        "          - target: x86_64-apple-darwin\n",
        "            runner: macos-latest\n",
        "            lib: libzu.dylib\n",
        "            staticlib: libzu.a\n",
        "            exe: zu\n",
        "            smoke: false\n",
        "    runs-on: ${{ matrix.runner }}\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    fn check(matrix: &str) -> Vec<String> {
        let rows = rows(matrix).expect("the matrix reads");
        table().against(&rows).iter().map(Note::to_string).collect()
    }

    #[test]
    fn a_matrix_that_is_the_table_has_nothing_to_say() {
        assert_eq!(check(MATRIX), [] as [String; 0]);
    }

    #[test]
    fn a_tier_1_target_the_matrix_drops_is_reported() {
        let at = MATRIX
            .find("          - target: x86_64-apple-darwin")
            .expect("the row is there");
        let end = MATRIX.find("    runs-on:").expect("the block ends");
        let matrix = format!("{}{}", &MATRIX[..at], &MATRIX[end..]);
        let notes = check(&matrix);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("does not build it"), "{}", notes[0]);
    }

    #[test]
    fn a_target_the_table_does_not_have_is_reported() {
        let matrix = MATRIX.replace("x86_64-apple-darwin", "x86_64-unknown-freebsd");
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("does not have it")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_row_built_on_the_wrong_runner_says_which_way() {
        let matrix = MATRIX.replace("runner: macos-latest", "runner: macos-13");
        let notes = check(&matrix);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("has runner \"macos-13\""), "{}", notes[0]);
    }

    #[test]
    fn a_row_that_lost_its_container_is_reported() {
        let matrix = MATRIX.replace(
            "            container: quay.io/pypa/manylinux_2_28_x86_64:2026.08.15-1\n",
            "",
        );
        let notes = check(&matrix);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("has container \"\""), "{}", notes[0]);
    }

    #[test]
    fn a_cross_compiled_row_that_claims_to_run_is_reported() {
        let matrix = MATRIX.replace("smoke: false", "smoke: true");
        let notes = check(&matrix);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("has smoke \"true\""), "{}", notes[0]);
    }

    #[test]
    fn one_target_built_twice_is_reported() {
        let matrix = MATRIX.replace("x86_64-apple-darwin", "x86_64-unknown-linux-gnu");
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("is built twice")),
            "{notes:?}"
        );
    }

    #[test]
    fn what_a_build_produced_is_weighed_against_its_ceiling() {
        let dir = std::env::temp_dir().join(format!("zu-platforms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch dir is writable");
        std::fs::write(dir.join("libzu.so"), vec![0u8; 3 << 20]).expect("writes");
        std::fs::write(dir.join("zu"), vec![0u8; 5 << 20]).expect("writes");
        let weights = table()
            .weigh("x86_64-unknown-linux-gnu", &dir)
            .expect("weighs");
        assert_eq!(weights.len(), 1, "{weights:?}");
        assert_eq!(weights[0].bytes, 3 << 20);
        assert_eq!(weights[0].cap, 14 << 20);
        assert!(!weights[0].over());
        assert!(weights[0].to_string().contains("21%"), "{}", weights[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_artifact_over_its_ceiling_says_so() {
        let dir = std::env::temp_dir().join(format!("zu-platforms-over-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch dir is writable");
        std::fs::write(dir.join("libzu.so"), vec![0u8; 15 << 20]).expect("writes");
        let weights = table()
            .weigh("x86_64-unknown-linux-gnu", &dir)
            .expect("weighs");
        assert!(weights[0].over(), "{:?}", weights[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_artifact_the_build_did_not_produce_is_an_error_and_not_a_zero() {
        let dir = std::env::temp_dir().join(format!("zu-platforms-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch dir is writable");
        let error = table()
            .weigh("x86_64-unknown-linux-gnu", &dir)
            .expect_err("nothing was built");
        assert!(error.contains("libzu.so"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quoted_value_is_the_same_value() {
        let matrix = MATRIX.replace(
            "target: x86_64-apple-darwin",
            "target: \"x86_64-apple-darwin\"",
        );
        assert_eq!(check(&matrix), [] as [String; 0]);
    }

    #[test]
    fn the_matrix_ends_where_its_indentation_does() {
        let rows = rows(MATRIX).expect("reads");
        assert_eq!(rows.len(), 2);
        assert!(
            rows[1].get("runner") == Some("macos-latest"),
            "{:?}",
            rows[1]
        );
        // `runs-on:` is outside the block and belongs to no row.
        assert!(rows.iter().all(|row| row.get("runs-on").is_none()));
    }

    #[test]
    fn a_matrix_key_before_any_row_is_refused() {
        let matrix = MATRIX.replace(
            "          - target: x86_64-unknown-linux-gnu",
            "          target: x86_64-unknown-linux-gnu",
        );
        let error = rows(&matrix).expect_err("a key with no row");
        assert!(error.contains("before any"), "{error}");
    }

    #[test]
    fn a_platform_smoke_tested_by_a_runner_it_lacks_is_refused() {
        let text = TABLE.replace(
            "target = \"wasm32-wasip1\"\ntier = 2\n",
            "target = \"wasm32-wasip1\"\ntier = 2\nsmoke = true\n",
        );
        let error = Table::parse(&text).expect_err("smoke without a runner");
        assert!(error.contains("runner it does not have"), "{error}");
    }

    #[test]
    fn a_platform_that_names_only_one_library_form_is_refused() {
        let text = TABLE.replace("staticlib = \"libzu.a\"\n", "");
        let error = Table::parse(&text).expect_err("a row with no static archive");
        assert!(error.contains("static libzu under no name"), "{error}");
    }

    #[test]
    fn a_row_that_forgot_the_static_archive_is_reported() {
        let matrix = MATRIX.replace("            staticlib: libzu.a\n", "");
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("has staticlib \"\"")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_tier_that_does_not_exist_is_refused() {
        let error = Table::parse(&TABLE.replace("tier = 2", "tier = 4")).expect_err("tier 4");
        assert!(error.contains("there are three"), "{error}");
    }

    #[test]
    fn a_platform_written_twice_is_refused() {
        let text = format!("{TABLE}\n{}", &TABLE[TABLE.find("[[platform]]").unwrap()..]);
        let error = Table::parse(&text).expect_err("two rows with one target");
        assert!(error.contains("is written twice"), "{error}");
    }

    #[test]
    fn a_schema_this_reader_does_not_read_is_refused() {
        let error = Table::parse(&TABLE.replace("schema = 2", "schema = 3")).expect_err("schema 3");
        assert!(error.contains("reads schema 2"), "{error}");
    }

    #[test]
    fn the_committed_table_is_what_the_committed_matrix_builds() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = Table::load(&root.join(PATH)).expect("the committed table loads");
        let notes = table.check(&root).expect("the workflow is readable");
        assert!(notes.is_empty(), "{notes:#?}");

        // The promise of tier 1 is seven platforms, and a table that
        // quietly grew or lost one is the promise changing without
        // anybody saying so.
        let tier1 = table.platforms.iter().filter(|p| p.tier == TIER1).count();
        assert_eq!(tier1, 7, "tier 1 is seven targets in dx/14 section 2");
        assert_eq!(table.budget("libzu").map(|b| b.mib), Some(14));
    }
}
