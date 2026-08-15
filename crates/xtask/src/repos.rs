//! The repository table, and the three places that have to agree with
//! it.
//!
//! The split of dx/18 section 2 put eight repositories outside this one,
//! and a split is only a decision once: after it, the list of what was
//! split out is a thing every part of the project quotes. The release
//! train dispatches to it, the README publishes it, the artifact
//! contract names consumers from it. Three copies of one list is how a
//! repository ends up on the train and off the README, which is the
//! failure where a release publishes to a registry no page tells anybody
//! about.
//!
//! So the list is `repos.toml`, once, and this holds the other three to
//! it. The conductor's matrix in both directions, because a repository
//! nothing dispatches to is a repository off the train and a dispatch
//! to a repository the table does not have is a release driving
//! something nobody decided to drive. The README's client table in both
//! directions, including the tier, because the tier is the promise a
//! user reads. And the consumers in `artifacts.toml`, because a
//! consumer that is not a repository is a fetch nobody will make.
//!
//! What each repository reports back is here too, and it is the column
//! that makes the collect step of dx/14 section 6 checkable rather than
//! aspirational: a release collects scorecards, map completeness, perf
//! and sizes, and which repository owes which is written down before
//! any of them can owe nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::artifacts;
use crate::matrix::{self, Row};
use crate::toml::Doc;

/// The table's schema version, which moves when the shape of the file
/// changes and not when the repositories do.
pub const SCHEMA: i64 = 1;

/// Where the table is.
pub const PATH: &str = "repos.toml";

/// The workflow whose matrix has to be the table.
pub const WORKFLOW: &str = ".github/workflows/conductor.yml";

/// The README, which publishes the same list to everybody else.
pub const README: &str = "README.md";

/// This repository, which drives the train rather than riding it.
pub const ENGINE: &str = "zu";

/// What a repository can report back to the collect step. A word that
/// is not one of these is a gate nothing implements.
pub const REPORTS: [&str; 6] = ["scorecard", "api-map", "corpus", "perf", "sizes", "site"];

/// What a repository is to the train.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This one. It makes the artifacts and is not dispatched to.
    Engine,
    /// An SDK against the frozen C ABI, published to a registry.
    Binding,
    /// The binding kit, which ships the tools rather than an SDK.
    Kit,
    /// The documentation site, dispatched after the bindings report.
    Site,
}

impl Role {
    pub fn name(&self) -> &'static str {
        match self {
            Role::Engine => "engine",
            Role::Binding => "binding",
            Role::Kit => "kit",
            Role::Site => "site",
        }
    }
}

/// One repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub name: String,
    pub role: Role,
    /// The support tier, which the site has none of because a site is
    /// not something a program links against.
    pub tier: Option<i64>,
    /// The milestone the repository appears at, or `exists` for this
    /// one.
    pub created: String,
    /// The workflow the train dispatches there, which every repository
    /// but the engine has.
    pub workflow: Option<String>,
    pub reports: Vec<String>,
    pub doc: String,
    pub line: usize,
}

impl Repo {
    /// Whether the train dispatches to it, which is every repository
    /// the split created and not this one.
    pub fn dispatched(&self) -> bool {
        self.role != Role::Engine
    }
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A repository the train is supposed to drive and the conductor
    /// does not dispatch to.
    Undriven { repo: String, line: usize },
    /// A dispatch to a repository the table does not have.
    Stranger { repo: String, line: usize },
    /// A dispatch to the repository the conductor is running in, which
    /// is a release triggering itself.
    Driver { line: usize },
    /// One repository dispatched twice, which is two runs of one
    /// release in one repository.
    Twice { repo: String, line: usize },
    /// A dispatch that drives the right repository the wrong way.
    Disagrees {
        repo: String,
        line: usize,
        key: String,
        found: String,
        want: String,
    },
    /// A repository the README does not list. Everything a user reaches
    /// this project through is in that table.
    Unlisted { repo: String, line: usize },
    /// A README row for a repository the table does not have.
    Unknown { repo: String },
    /// A README row promising a tier the table does not.
    Mistiered {
        repo: String,
        found: String,
        want: String,
    },
    /// A repository the split created that fetches nothing a release
    /// publishes. Either the contract forgot it or it should not have
    /// been split out.
    Idle { repo: String },
    /// An artifact consumer that is not one of the repositories.
    Foreign {
        name: String,
        consumer: String,
        line: usize,
    },
    /// The engine listed as a consumer of its own release.
    Maker { name: String, line: usize },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Undriven { repo, line } => write!(
                f,
                "{PATH}:{line}: the train drives {repo} and {WORKFLOW} does not dispatch to it"
            ),
            Note::Stranger { repo, line } => write!(
                f,
                "{WORKFLOW}:{line}: {repo} is dispatched to and {PATH} does not have it"
            ),
            Note::Driver { line } => write!(
                f,
                "{WORKFLOW}:{line}: {ENGINE} is dispatched to, and it is the repository the train \
                 is running in"
            ),
            Note::Twice { repo, line } => {
                write!(f, "{WORKFLOW}:{line}: {repo} is dispatched to twice")
            }
            Note::Disagrees {
                repo,
                line,
                key,
                found,
                want,
            } => write!(
                f,
                "{WORKFLOW}:{line}: {repo} has {key} {found:?} and {PATH} says {want:?}"
            ),
            Note::Unlisted { repo, line } => write!(
                f,
                "{PATH}:{line}: {repo} is a repository of this project and {README} does not list it"
            ),
            Note::Unknown { repo } => write!(
                f,
                "{README}: {repo} is listed as a client and {PATH} does not have it"
            ),
            Note::Mistiered { repo, found, want } => write!(
                f,
                "{README}: {repo} is tier {found:?} there and tier {want:?} in {PATH}"
            ),
            Note::Idle { repo } => write!(
                f,
                "{}: {repo} is one of the repositories and consumes nothing a release publishes",
                artifacts::PATH
            ),
            Note::Foreign {
                name,
                consumer,
                line,
            } => write!(
                f,
                "{}:{line}: {name} is fetched by {consumer:?}, which is not a repository in {PATH}",
                artifacts::PATH
            ),
            Note::Maker { name, line } => write!(
                f,
                "{}:{line}: {ENGINE} makes {name} and is not a consumer of it",
                artifacts::PATH
            ),
        }
    }
}

/// The table.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub audited: String,
    pub repos: Vec<Repo>,
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
        if let Some(name) = doc.unknown_arrays(&["repo"]).first() {
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

        let mut repos = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for table in doc.array("repo") {
            let line = table.line;
            if let Some(key) = table
                .unknown(&[
                    "name", "role", "tier", "created", "workflow", "reports", "doc",
                ])
                .first()
            {
                return Err(format!("line {line}: a repository has no key {key:?}"));
            }
            let name = table
                .str("name")
                .ok_or_else(|| format!("line {line}: a repository with no name"))?
                .to_string();
            if name != ENGINE && !name.starts_with(&format!("{ENGINE}-")) {
                return Err(format!(
                    "line {line}: {name} is a repository of this project, and they are all named \
                     {ENGINE} or {ENGINE}-something"
                ));
            }
            let role = match table.str("role") {
                Some("engine") => Role::Engine,
                Some("binding") => Role::Binding,
                Some("kit") => Role::Kit,
                Some("site") => Role::Site,
                found => {
                    return Err(format!(
                        "line {line}: {name} is a {found:?}, and the roles are engine, binding, \
                         kit and site"
                    ));
                }
            };
            let tier = match (role, table.int("tier")) {
                (Role::Engine | Role::Site, None) => None,
                (Role::Engine | Role::Site, Some(tier)) => {
                    return Err(format!(
                        "line {line}: {name} is the {} and is tier {tier}, which is a promise \
                         about an SDK",
                        role.name()
                    ));
                }
                (_, Some(tier @ 1..=3)) => Some(tier),
                (_, found) => {
                    return Err(format!(
                        "line {line}: {name} is tier {found:?}, and there are three"
                    ));
                }
            };
            let created = table
                .str("created")
                .ok_or_else(|| format!("line {line}: {name} is created by nobody"))?
                .to_string();
            if created != "exists" && !artifacts::is_milestone(&created) {
                return Err(format!(
                    "line {line}: {name} is created at {created:?}, and that is a milestone or \
                     `exists`"
                ));
            }
            if (role == Role::Engine) != (created == "exists") {
                return Err(format!(
                    "line {line}: {name} is the {} and is created at {created:?}",
                    role.name()
                ));
            }
            let workflow = table.str("workflow").map(str::to_string);
            let reports: Vec<String> = table
                .list("reports")
                .unwrap_or_default()
                .iter()
                .map(|r| r.to_string())
                .collect();
            if role == Role::Engine {
                if workflow.is_some() || !reports.is_empty() {
                    return Err(format!(
                        "line {line}: {name} makes the release and is not dispatched to, so it \
                         has no workflow and reports nothing"
                    ));
                }
            } else {
                if workflow.is_none() {
                    return Err(format!(
                        "line {line}: the train dispatches to {name} and no workflow says which"
                    ));
                }
                if reports.is_empty() {
                    return Err(format!(
                        "line {line}: {name} is on the train and reports nothing back, which is a \
                         dispatch nothing gates on"
                    ));
                }
            }
            for (at, report) in reports.iter().enumerate() {
                if !REPORTS.contains(&report.as_str()) {
                    return Err(format!(
                        "line {line}: {name} reports {report:?}, and a release collects {}",
                        REPORTS.join(", ")
                    ));
                }
                if reports[..at].contains(report) {
                    return Err(format!("line {line}: {name} reports {report:?} twice"));
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
            repos.push(Repo {
                name,
                role,
                tier,
                created,
                workflow,
                reports,
                doc,
                line,
            });
        }

        let engines: Vec<&Repo> = repos.iter().filter(|r| r.role == Role::Engine).collect();
        match engines.as_slice() {
            [engine] if engine.name == ENGINE => {}
            [engine] => {
                return Err(format!(
                    "line {}: {} is the engine and this reader is in {ENGINE}",
                    engine.line, engine.name
                ));
            }
            found => {
                return Err(format!(
                    "a table with {} engines in it, and there is one",
                    found.len()
                ));
            }
        }

        Ok(Table {
            doc: doc_text,
            audited,
            repos,
        })
    }

    /// The repository of this name.
    pub fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name == name)
    }

    /// The repositories the train dispatches to, in table order, which
    /// is the order dx/14 section 6 dispatches in.
    pub fn dispatched(&self) -> impl Iterator<Item = &Repo> {
        self.repos.iter().filter(|r| r.dispatched())
    }

    /// Checks the tree under `root` against the table.
    pub fn check(&self, root: &Path) -> Result<Vec<Note>, String> {
        let read = |name: &str| {
            let path = root.join(name);
            std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
        };
        let contract = artifacts::Table::load(&root.join(artifacts::PATH))?;
        self.hold(&read(WORKFLOW)?, &read(README)?, &contract)
    }

    /// The three checks, against what they read rather than against
    /// where it was, in the order a person would read them.
    pub fn hold(
        &self,
        workflow: &str,
        readme: &str,
        contract: &artifacts::Table,
    ) -> Result<Vec<Note>, String> {
        let mut notes = self.dispatches(&matrix::rows(workflow, WORKFLOW)?);
        notes.extend(self.listed(readme));
        notes.extend(self.consumed(contract));
        Ok(notes)
    }

    /// The repositories by name, which is how the three checks look one
    /// up. A scan per row is nine comparisons today and a check nobody
    /// runs once the tier 3 bindings of dx/11 have their own rows.
    fn index(&self) -> BTreeMap<&str, &Repo> {
        self.repos.iter().map(|r| (r.name.as_str(), r)).collect()
    }

    /// The check against the conductor, against a matrix already read.
    fn dispatches(&self, rows: &[Row]) -> Vec<Note> {
        let known = self.index();
        let mut notes = Vec::new();
        let mut driven: BTreeMap<&str, usize> = BTreeMap::new();
        for row in rows {
            let name = row.get("repo").unwrap_or_default();
            if let Some(before) = driven.insert(name, row.line) {
                notes.push(Note::Twice {
                    repo: name.to_string(),
                    line: row.line.max(before),
                });
                continue;
            }
            let repo = match known.get(name).copied() {
                Some(repo) if repo.dispatched() => repo,
                Some(_) => {
                    notes.push(Note::Driver { line: row.line });
                    continue;
                }
                None => {
                    notes.push(Note::Stranger {
                        repo: name.to_string(),
                        line: row.line,
                    });
                    continue;
                }
            };
            // The reports are space separated in the matrix because a
            // shell loops over that and reads no JSON to do it, and the
            // collect step is a shell.
            for (key, want) in [
                ("workflow", repo.workflow.clone().unwrap_or_default()),
                ("reports", repo.reports.join(" ")),
            ] {
                let found = row.get(key).unwrap_or_default();
                if found != want {
                    notes.push(Note::Disagrees {
                        repo: repo.name.clone(),
                        line: row.line,
                        key: key.to_string(),
                        found: found.to_string(),
                        want,
                    });
                }
            }
        }
        for repo in self.dispatched() {
            if !driven.contains_key(repo.name.as_str()) {
                notes.push(Note::Undriven {
                    repo: repo.name.clone(),
                    line: repo.line,
                });
            }
        }
        notes
    }

    /// The check against the README's client table.
    ///
    /// The engine is not in that table and should not be: the page is
    /// what this repository is, and a link from it to itself is noise.
    /// Everything else is, with the tier it promises.
    fn listed(&self, readme: &str) -> Vec<Note> {
        let listed = clients(readme);
        let tiers: BTreeMap<&str, &str> = listed
            .iter()
            .map(|(name, tier)| (name.as_str(), tier.as_str()))
            .collect();
        let known = self.index();
        let mut notes = Vec::new();
        for repo in self.repos.iter().filter(|r| r.dispatched()) {
            match tiers.get(repo.name.as_str()) {
                None => notes.push(Note::Unlisted {
                    repo: repo.name.clone(),
                    line: repo.line,
                }),
                Some(tier) => {
                    let want = repo.tier.map(|t| t.to_string()).unwrap_or_default();
                    if **tier != want {
                        notes.push(Note::Mistiered {
                            repo: repo.name.clone(),
                            found: (*tier).to_string(),
                            want,
                        });
                    }
                }
            }
        }
        for (name, _) in &listed {
            if !known.contains_key(name.as_str()) {
                notes.push(Note::Unknown { repo: name.clone() });
            }
        }
        notes
    }

    /// The check against the artifact contract's consumers.
    fn consumed(&self, artifacts: &artifacts::Table) -> Vec<Note> {
        let known = self.index();
        let mut fetching: BTreeSet<&str> = BTreeSet::new();
        let mut notes = Vec::new();
        for artifact in &artifacts.artifacts {
            for consumer in &artifact.consumers {
                fetching.insert(consumer.as_str());
                if consumer == ENGINE {
                    notes.push(Note::Maker {
                        name: artifact.name.clone(),
                        line: artifact.line,
                    });
                } else if !known.contains_key(consumer.as_str()) {
                    notes.push(Note::Foreign {
                        name: artifact.name.clone(),
                        consumer: consumer.clone(),
                        line: artifact.line,
                    });
                }
            }
        }
        for repo in self.dispatched() {
            if !fetching.contains(repo.name.as_str()) {
                notes.push(Note::Idle {
                    repo: repo.name.clone(),
                });
            }
        }
        notes
    }
}

/// The repositories the README's client table links to, with the tier
/// each row promises, empty where the row leaves the column blank.
///
/// Read by the link rather than by the column, because the link is the
/// thing that has to be right: a row whose text says `zu-go` and whose
/// href is `zu-node` sends a user to the wrong repository, and the href
/// is what they follow.
fn clients(readme: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in readme.lines() {
        let line = line.trim();
        if !line.starts_with("| [") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        let Some(name) = cells
            .first()
            .and_then(|cell| cell.split_once("](https://github.com/tamnd/"))
            .and_then(|(_, href)| href.split_once(')'))
            .map(|(name, _)| name.to_string())
        else {
            continue;
        };
        let tier = cells.last().unwrap_or(&"").to_string();
        out.push((name, tier));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = concat!(
        "schema = 1\n",
        "doc = \"The repositories of the split.\"\n",
        "audited = \"2026-08-16\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu\"\n",
        "role = \"engine\"\n",
        "created = \"exists\"\n",
        "doc = \"The engine, and the tag that is a release.\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu-python\"\n",
        "role = \"binding\"\n",
        "tier = 1\n",
        "created = \"DX2\"\n",
        "workflow = \"release.yml\"\n",
        "reports = [\"scorecard\", \"corpus\"]\n",
        "doc = \"zudb on PyPI.\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu-web\"\n",
        "role = \"site\"\n",
        "created = \"DX0\"\n",
        "workflow = \"release.yml\"\n",
        "reports = [\"site\"]\n",
        "doc = \"The documentation site.\"\n",
    );

    /// A conductor dispatching to both rows of the table above, in the
    /// two stages the real one has.
    const MATRIX: &str = concat!(
        "jobs:\n",
        "  dispatch:\n",
        "    strategy:\n",
        "      matrix:\n",
        "        include:\n",
        "          - repo: zu-python\n",
        "            workflow: release.yml\n",
        "            reports: scorecard corpus\n",
        "  site:\n",
        "    strategy:\n",
        "      matrix:\n",
        "        include:\n",
        "          - repo: zu-web\n",
        "            workflow: release.yml\n",
        "            reports: site\n",
    );

    const CLIENTS: &str = concat!(
        "| Repository | What it is | Tier |\n",
        "|---|---|---|\n",
        "| [zu-python](https://github.com/tamnd/zu-python) | PyO3 wheels | 1 |\n",
        "| [zu-web](https://github.com/tamnd/zu-web) | The site |  |\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    fn check(matrix: &str) -> Vec<String> {
        let rows = matrix::rows(matrix, WORKFLOW).expect("the matrix reads");
        table()
            .dispatches(&rows)
            .iter()
            .map(Note::to_string)
            .collect()
    }

    fn listed(readme: &str) -> Vec<String> {
        table().listed(readme).iter().map(Note::to_string).collect()
    }

    #[test]
    fn a_row_is_read_as_written() {
        let table = table();
        let python = table.repo("zu-python").expect("a row");
        assert_eq!(python.role, Role::Binding);
        assert_eq!(python.tier, Some(1));
        assert_eq!(python.reports, ["scorecard", "corpus"]);
        assert!(python.dispatched());
        assert!(!table.repo("zu").expect("the engine").dispatched());
        assert_eq!(table.dispatched().count(), 2);
    }

    #[test]
    fn a_conductor_that_is_the_table_has_nothing_to_say() {
        assert_eq!(check(MATRIX), [] as [String; 0]);
    }

    #[test]
    fn a_repository_the_conductor_drops_is_reported() {
        let matrix = MATRIX.replace(
            "          - repo: zu-web\n",
            "          - repo: zu-python\n",
        );
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("does not dispatch to it")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("dispatched to twice")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_dispatch_to_a_repository_that_is_not_ours_is_reported() {
        let matrix = MATRIX.replace("zu-web", "zu-perl");
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("does not have it")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_dispatch_to_the_engine_is_reported() {
        let matrix = MATRIX.replace("repo: zu-web", "repo: zu");
        let notes = check(&matrix);
        assert!(
            notes.iter().any(|n| n.contains("zu is dispatched to")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_dispatch_of_the_wrong_workflow_says_which_way() {
        let matrix = MATRIX.replace(
            "          - repo: zu-web\n            workflow: release.yml\n",
            "          - repo: zu-web\n            workflow: nightly.yml\n",
        );
        let notes = check(&matrix);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("has workflow \"nightly.yml\""),
            "{}",
            notes[0]
        );
    }

    #[test]
    fn a_readme_that_is_the_table_has_nothing_to_say() {
        assert_eq!(listed(CLIENTS), [] as [String; 0]);
    }

    #[test]
    fn a_repository_the_readme_does_not_list_is_reported() {
        let readme = CLIENTS.replace(
            "| [zu-python](https://github.com/tamnd/zu-python) | PyO3 wheels | 1 |\n",
            "",
        );
        let notes = listed(&readme);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("does not list it"), "{}", notes[0]);
    }

    #[test]
    fn a_readme_row_for_a_repository_we_do_not_have_is_reported() {
        let readme = format!("{CLIENTS}| [zu-perl](https://github.com/tamnd/zu-perl) | No | 3 |\n");
        let notes = listed(&readme);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("does not have it"), "{}", notes[0]);
    }

    #[test]
    fn a_readme_promising_another_tier_is_reported() {
        let readme = CLIENTS.replace("| PyO3 wheels | 1 |", "| PyO3 wheels | 2 |");
        let notes = listed(&readme);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("tier \"2\" there"), "{}", notes[0]);
    }

    #[test]
    fn a_row_whose_link_goes_somewhere_else_is_read_by_the_link() {
        let readme = CLIENTS.replace(
            "[zu-python](https://github.com/tamnd/zu-python)",
            "[zu-python](https://github.com/tamnd/zu-node)",
        );
        let notes = listed(&readme);
        assert!(notes.iter().any(|n| n.contains("zu-node")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("zu-python")), "{notes:?}");
    }

    #[test]
    fn an_artifact_consumer_that_is_not_a_repository_is_reported() {
        let contract = concat!(
            "schema = 1\n",
            "doc = \"What a release publishes.\"\n",
            "audited = \"2026-08-16\"\n",
            "\n",
            "[[artifact]]\n",
            "name = \"zu.h\"\n",
            "made = \"file\"\n",
            "from = \"crates/zu-capi/include/zu.h\"\n",
            "consumers = [\"zu-perl\", \"zu\"]\n",
            "doc = \"The C ABI.\"\n",
        );
        let artifacts = artifacts::Table::parse(contract).expect("the contract parses");
        let notes: Vec<String> = table()
            .consumed(&artifacts)
            .iter()
            .map(Note::to_string)
            .collect();
        assert!(
            notes.iter().any(|n| n.contains("not a repository")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("is not a consumer of it")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("zu-python is one of the")),
            "a repository fetching nothing: {notes:?}"
        );
    }

    #[test]
    fn a_repository_the_train_drives_needs_a_workflow_and_a_report() {
        let text = TABLE.replace("workflow = \"release.yml\"\nreports = [\"site\"]\n", "");
        let error = Table::parse(&text).expect_err("a dispatch to nothing");
        assert!(error.contains("no workflow says which"), "{error}");

        let text = TABLE.replace("reports = [\"site\"]", "reports = []");
        let error = Table::parse(&text).expect_err("a dispatch nothing gates on");
        assert!(error.contains("reports nothing back"), "{error}");
    }

    #[test]
    fn a_report_nothing_collects_is_refused() {
        let text = TABLE.replace("reports = [\"site\"]", "reports = [\"vibes\"]");
        let error = Table::parse(&text).expect_err("a gate nothing implements");
        assert!(error.contains("a release collects"), "{error}");

        let text = TABLE.replace("[\"scorecard\", \"corpus\"]", "[\"corpus\", \"corpus\"]");
        let error = Table::parse(&text).expect_err("one report twice");
        assert!(error.contains("twice"), "{error}");
    }

    #[test]
    fn an_engine_that_is_dispatched_to_is_refused() {
        let text = TABLE.replace(
            "role = \"engine\"\ncreated = \"exists\"\n",
            "role = \"engine\"\ncreated = \"exists\"\nworkflow = \"release.yml\"\n",
        );
        let error = Table::parse(&text).expect_err("the engine on the train");
        assert!(error.contains("is not dispatched to"), "{error}");
    }

    #[test]
    fn a_table_with_no_engine_in_it_is_refused() {
        let first = TABLE.find("[[repo]]").expect("a row");
        let second = TABLE[first + 1..].find("[[repo]]").expect("a second row") + first + 1;
        let text = format!("{}{}", &TABLE[..first], &TABLE[second..]);
        let error = Table::parse(&text).expect_err("no engine");
        assert!(error.contains("0 engines"), "{error}");
    }

    #[test]
    fn a_site_with_a_tier_is_refused() {
        let text = TABLE.replace(
            "role = \"site\"\ncreated = \"DX0\"",
            "role = \"site\"\ntier = 1\ncreated = \"DX0\"",
        );
        let error = Table::parse(&text).expect_err("a site with a tier");
        assert!(error.contains("a promise about an SDK"), "{error}");
    }

    #[test]
    fn a_repository_created_by_no_milestone_is_refused() {
        let text = TABLE.replace("created = \"DX2\"", "created = \"soon\"");
        let error = Table::parse(&text).expect_err("a milestone that does not exist");
        assert!(error.contains("that is a milestone"), "{error}");

        let text = TABLE.replace("created = \"DX2\"", "created = \"exists\"");
        let error = Table::parse(&text).expect_err("a binding that always existed");
        assert!(error.contains("is the binding"), "{error}");
    }

    #[test]
    fn a_repository_that_is_not_one_of_ours_is_refused() {
        let text = TABLE.replace("name = \"zu-python\"", "name = \"zulu\"");
        let error = Table::parse(&text).expect_err("somebody else's repository");
        assert!(error.contains("they are all named"), "{error}");
    }

    #[test]
    fn a_repository_written_twice_is_refused() {
        let at = TABLE.rfind("[[repo]]").expect("a row");
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
    fn the_committed_table_is_what_this_project_is() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = Table::load(&root.join(PATH)).expect("the committed table loads");
        let notes = table.check(&root).expect("the tree is readable");
        assert!(notes.is_empty(), "{notes:#?}");

        // dx/18 section 2 is nine repositories, one of them this one,
        // and the split is a decision that was made once. A row that
        // quietly appeared or went is that decision changing without
        // anybody saying so.
        assert_eq!(table.repos.len(), 9);
        assert_eq!(table.dispatched().count(), 8);
        let tier1 = table
            .repos
            .iter()
            .filter(|r| r.tier == Some(1))
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tier1, ["zu-python", "zu-node", "zu-go", "zu-java", "zu-c"]);
    }
}
