//! The client table, the scorecard it defines, and the page it renders.
//!
//! dx/01 section 5 asks for a scorecard per SDK and a named maintainer
//! on every one of them, because a tier is a promise and a promise with
//! nobody's name on it is a commitment nobody made. A user deciding
//! whether to depend on the Go client wants three things: who answers
//! for it, where it lives, and what tier 1 actually means. All three are
//! `clients.toml`, and this renders them to `/docs/clients/overview`.
//!
//! A scorecard has three axes. Surface is how much of the API model the
//! client names, which the map completeness check of dx/15 section 6
//! measures per release. Conformance is how much of the cross-client
//! corpus it passes, which the corpus runner measures per release.
//! Neither is a column in the table, because a number a person types is
//! a number nobody measured; both arrive through the reports of
//! `repos.toml` and are collected by the conductor.
//!
//! Practice is the third, and it is the one that can be checked today: a
//! list of things a client repository either has or does not, each with
//! a weight out of a hundred. A release does not change the answer, so
//! it lives here beside the maintainer, and `--gate` fails on a client
//! under its tier's threshold. Without `--gate` the standing is printed
//! rather than enforced, since every client is under its threshold until
//! the apparatus of DX3 through DX5 is built and a check that is red for
//! two milestones is a check somebody turns off.
//!
//! The table is held to `repos.toml` in both directions: a repository
//! that owes a scorecard and has no row here is a client nothing scores,
//! and a row here for a repository that owes no scorecard is a scorecard
//! nothing collects. The tier is held there too, since `repos.toml` is
//! already what the README publishes and three copies of one promise is
//! how a client ends up tier 1 on one page and tier 2 on another.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::repos;
use crate::toml::Doc;

/// The table's schema version, which moves when the shape of the file
/// changes and not when the clients do.
pub const SCHEMA: i64 = 1;

/// Where the table is.
pub const PATH: &str = "clients.toml";

/// The page it renders, which the site publishes at [`URL`].
pub const PAGE: &str = "docs/clients/overview.md";

/// Where that page is published.
pub const URL: &str = "/docs/clients/overview";

/// Where a handle resolves, which is the same place the repositories
/// are and not the same path: a maintainer is a person, not a
/// repository of this project.
pub const PROFILE: &str = "https://github.com/";

/// What every client repository is under. A row whose repository is
/// somewhere else is a client this project does not publish.
pub const HOST: &str = "https://github.com/tamnd/";

/// The report that makes a repository a client of this table. A
/// repository owing this and missing here is the hole this catches.
pub const REPORT: &str = "scorecard";

/// The weights of the practice axis are out of this, and the axes are
/// percentages of it, so a score has one meaning everywhere it is read.
pub const WHOLE: i64 = 100;

/// What a tier promises, on each of the three axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub level: i64,
    pub surface: i64,
    pub conformance: i64,
    pub practice: i64,
    pub doc: String,
    pub line: usize,
}

/// One item of the practice axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub weight: i64,
    /// The report in `repos.toml` that makes this item apply. An item
    /// with none applies to every client; an item with one is not
    /// counted against a client whose repository does not owe that
    /// report, which is how zu-c is not marked down for the api-map it
    /// is right not to have.
    pub report: Option<String>,
    pub doc: String,
    pub line: usize,
}

/// One client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The repository name, read off the URL rather than written twice.
    pub name: String,
    pub repository: String,
    pub language: String,
    pub package: String,
    pub registry: String,
    /// A person and a handle, which is what a named maintainer is.
    pub maintainer: String,
    pub tier: i64,
    pub holds: Vec<String>,
    pub doc: String,
    pub line: usize,
}

impl Client {
    /// The handle inside the maintainer, without its `@`.
    pub fn handle(&self) -> &str {
        self.maintainer
            .split_once("<@")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(handle, _)| handle)
            .unwrap_or_default()
    }

    /// The person, without the handle after it.
    pub fn person(&self) -> &str {
        self.maintainer
            .split_once(" <@")
            .map(|(person, _)| person)
            .unwrap_or(&self.maintainer)
    }
}

/// Where one client stands on the practice axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Score {
    /// Out of a hundred, of the weight that applies.
    pub practice: i64,
    /// What its tier asks for.
    pub want: i64,
    /// The items it does not hold, in table order.
    pub missing: Vec<String>,
}

impl Score {
    pub fn met(&self) -> bool {
        self.practice >= self.want
    }
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A repository that owes a scorecard and has no row here, which is
    /// a client the release collects a scorecard from and nothing
    /// defines one for.
    Unlisted { repo: String, line: usize },
    /// A row here for a repository the split does not have.
    Unknown { name: String, line: usize },
    /// A row here for a repository that owes no scorecard back, which
    /// is a scorecard nothing collects.
    Silent { name: String, line: usize },
    /// A tier promised here and another one there.
    Mistiered {
        name: String,
        line: usize,
        found: i64,
        want: String,
    },
    /// An item claimed by a client whose repository does not owe the
    /// report behind it.
    Unearned {
        name: String,
        line: usize,
        item: String,
        report: String,
    },
    /// A client under its tier's practice threshold. Reported only
    /// under the gate.
    Short {
        name: String,
        line: usize,
        tier: i64,
        found: i64,
        want: i64,
    },
    /// The published page is not what the table renders.
    Drift {
        line: usize,
        found: String,
        want: String,
    },
    /// The published page is not in the tree at all.
    Absent,
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Unlisted { repo, line } => write!(
                f,
                "{}:{line}: {repo} owes a {REPORT} and {PATH} has no row for it",
                repos::PATH
            ),
            Note::Unknown { name, line } => write!(
                f,
                "{PATH}:{line}: {name} is a client and {} does not have that repository",
                repos::PATH
            ),
            Note::Silent { name, line } => write!(
                f,
                "{PATH}:{line}: {name} has a scorecard here and reports no {REPORT} in {}",
                repos::PATH
            ),
            Note::Mistiered {
                name,
                line,
                found,
                want,
            } => write!(
                f,
                "{PATH}:{line}: {name} is tier {found} here and tier {want:?} in {}",
                repos::PATH
            ),
            Note::Unearned {
                name,
                line,
                item,
                report,
            } => write!(
                f,
                "{PATH}:{line}: {name} holds {item:?} and its repository owes no {report:?} in {}",
                repos::PATH
            ),
            Note::Short {
                name,
                line,
                tier,
                found,
                want,
            } => write!(
                f,
                "{PATH}:{line}: {name} is tier {tier} and scores {found} of {want} on practice"
            ),
            Note::Drift { line, found, want } => write!(
                f,
                "{PAGE}:{line}: is {found:?} and {PATH} renders {want:?}. \
                 Run `cargo xtask clients`."
            ),
            Note::Absent => write!(f, "{PAGE}: is not in this tree, so {URL} publishes nothing"),
        }
    }
}

/// The table.
#[derive(Debug, Clone)]
pub struct Table {
    pub doc: String,
    pub audited: String,
    pub tiers: Vec<Tier>,
    pub items: Vec<Item>,
    pub clients: Vec<Client>,
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
        if let Some(name) = doc.unknown_arrays(&["tier", "item", "client"]).first() {
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

        let tiers = tiers(&doc)?;
        let items = items(&doc)?;
        let clients = clients(&doc, &tiers, &items)?;
        Ok(Table {
            doc: doc_text,
            audited,
            tiers,
            items,
            clients,
        })
    }

    /// The client of this name.
    pub fn client(&self, name: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.name == name)
    }

    /// What a tier promises.
    pub fn tier(&self, level: i64) -> Option<&Tier> {
        self.tiers.iter().find(|t| t.level == level)
    }

    /// The items that apply to a client, which is every item without a
    /// report and every item whose report its repository owes. A client
    /// with no row in the split is scored against all of them, since the
    /// missing row is already a note of its own.
    pub fn applies<'a>(&'a self, client: &Client, repos: &repos::Table) -> Vec<&'a Item> {
        let owed = repos.repo(&client.name).map(|repo| &repo.reports);
        self.items
            .iter()
            .filter(|item| match (&item.report, owed) {
                (None, _) => true,
                (Some(_), None) => true,
                (Some(report), Some(owed)) => owed.contains(report),
            })
            .collect()
    }

    /// Where a client stands on the practice axis.
    pub fn score(&self, client: &Client, repos: &repos::Table) -> Score {
        let applies = self.applies(client, repos);
        let of: i64 = applies.iter().map(|item| item.weight).sum();
        let held: i64 = applies
            .iter()
            .filter(|item| client.holds.contains(&item.name))
            .map(|item| item.weight)
            .sum();
        let missing = applies
            .iter()
            .filter(|item| !client.holds.contains(&item.name))
            .map(|item| item.name.clone())
            .collect();
        // Out of the weight that applies rather than out of a hundred,
        // so a client is scored on what it owes. Rounded to the nearest
        // whole point, because a threshold written as 90 is read as 90.
        let practice = if of == 0 {
            WHOLE
        } else {
            (held * WHOLE + of / 2) / of
        };
        Score {
            practice,
            want: self.tier(client.tier).map(|t| t.practice).unwrap_or(WHOLE),
            missing,
        }
    }

    /// Checks the tree under `root` against the table.
    pub fn check(&self, root: &Path, gate: bool) -> Result<Vec<Note>, String> {
        let repos = repos::Table::load(&root.join(repos::PATH))?;
        let page = std::fs::read_to_string(root.join(PAGE)).ok();
        Ok(self.hold(&repos, page.as_deref(), gate))
    }

    /// The checks, against what they read rather than against where it
    /// was: the split in both directions, then the page, then the
    /// thresholds if the caller is gating on them.
    pub fn hold(&self, repos: &repos::Table, page: Option<&str>, gate: bool) -> Vec<Note> {
        let mut notes = self.split(repos);
        notes.extend(self.published(repos, page));
        if gate {
            notes.extend(self.thresholds(repos));
        }
        notes
    }

    /// The check against `repos.toml`, both ways.
    fn split(&self, repos: &repos::Table) -> Vec<Note> {
        let mut notes = Vec::new();
        let mut listed: BTreeSet<&str> = BTreeSet::new();
        for client in &self.clients {
            listed.insert(client.name.as_str());
            let Some(repo) = repos.repo(&client.name) else {
                notes.push(Note::Unknown {
                    name: client.name.clone(),
                    line: client.line,
                });
                continue;
            };
            if !repo.reports.iter().any(|r| r == REPORT) {
                notes.push(Note::Silent {
                    name: client.name.clone(),
                    line: client.line,
                });
            }
            let want = repo.tier.map(|t| t.to_string()).unwrap_or_default();
            if want != client.tier.to_string() {
                notes.push(Note::Mistiered {
                    name: client.name.clone(),
                    line: client.line,
                    found: client.tier,
                    want,
                });
            }
            for item in &self.items {
                let Some(report) = &item.report else { continue };
                if client.holds.contains(&item.name) && !repo.reports.contains(report) {
                    notes.push(Note::Unearned {
                        name: client.name.clone(),
                        line: client.line,
                        item: item.name.clone(),
                        report: report.clone(),
                    });
                }
            }
        }
        for repo in &repos.repos {
            if repo.reports.iter().any(|r| r == REPORT) && !listed.contains(repo.name.as_str()) {
                notes.push(Note::Unlisted {
                    repo: repo.name.clone(),
                    line: repo.line,
                });
            }
        }
        notes
    }

    /// The check against the published page.
    fn published(&self, repos: &repos::Table, page: Option<&str>) -> Vec<Note> {
        let want = self.render(repos);
        match page {
            None => vec![Note::Absent],
            Some(found) => drift(found, &want).into_iter().collect(),
        }
    }

    /// The check the release runs: every client at its tier.
    fn thresholds(&self, repos: &repos::Table) -> Vec<Note> {
        self.clients
            .iter()
            .filter_map(|client| {
                let score = self.score(client, repos);
                (!score.met()).then(|| Note::Short {
                    name: client.name.clone(),
                    line: client.line,
                    tier: client.tier,
                    found: score.practice,
                    want: score.want,
                })
            })
            .collect()
    }

    /// The page, rendered.
    pub fn render(&self, repos: &repos::Table) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "<!-- Generated by `cargo xtask clients` from {PATH}. Edit that, not this. -->\n\n\
             # The clients\n\n\
             {}\n\n\
             Every client is a repository of its own, with a person who answers for it and a tier \
             that says what it promises. The tier is the same one the engine's README publishes, \
             because a client cannot be tier 1 on one page and tier 2 on another. Audited {}.\n\n",
            self.doc, self.audited
        ));

        out.push_str("| Client | Language | Package | Registry | Maintainer | Tier |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for client in &self.clients {
            out.push_str(&format!(
                "| [{}]({}) | {} | `{}` | {} | {} ([@{}]({PROFILE}{})) | {} |\n",
                client.name,
                client.repository,
                client.language,
                client.package,
                client.registry,
                client.person(),
                client.handle(),
                client.handle(),
                client.tier,
            ));
        }

        out.push_str(
            "\n## What a tier promises\n\n\
             A scorecard has three axes. Surface is how much of the API model the client names, \
             out of what its tier owes. Conformance is how much of the cross-client corpus it \
             passes. Practice is the apparatus below. The first two are measured by each release \
             and collected from the client repositories, so they are numbers nobody types; the \
             third is scored from the table this page is rendered from.\n\n\
             | Tier | Surface | Conformance | Practice | What it is |\n|---|---|---|---|---|\n",
        );
        for tier in &self.tiers {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                tier.level, tier.surface, tier.conformance, tier.practice, tier.doc
            ));
        }

        out.push_str(
            "\n## What practice counts\n\n\
             Each item is a thing a client repository either has or does not, and the weights are \
             out of a hundred. An item that applies only to some clients says so: a client that \
             owes no api-map is scored out of what it does owe rather than marked down for a file \
             it is right not to have.\n\n\
             | Item | Weight | Applies to | What it is |\n|---|---|---|---|\n",
        );
        for item in &self.items {
            let applies = match &item.report {
                None => "every client".to_string(),
                Some(report) => format!("clients that report `{report}`"),
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                item.name, item.weight, applies, item.doc
            ));
        }

        out.push_str(
            "\n## Where each client stands\n\n\
             The practice score, today, out of what the client's tier asks for. What is missing is \
             named rather than counted, because a number nobody can act on is a number nobody \
             reads.\n\n\
             | Client | Tier | Practice | Needs | Missing |\n|---|---|---|---|---|\n",
        );
        for client in &self.clients {
            let score = self.score(client, repos);
            let missing = if score.missing.is_empty() {
                "nothing".to_string()
            } else {
                score
                    .missing
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!(
                "| [{}]({}) | {} | {} | {} | {} |\n",
                client.name, client.repository, client.tier, score.practice, score.want, missing
            ));
        }

        out.push_str(&format!(
            "\nA client under its tier is a client with work left rather than a client that broke: \
             the apparatus arrives one milestone at a time, and this page is what says which \
             milestone is owed what. `cargo xtask clients --gate` is the run that fails on a row \
             below its threshold, and it is the release's, which is where a promise has to hold.\n\n\
             This page is generated from `{PATH}` in the engine repository. A client's tier, its \
             maintainer and what it holds change there, in a pull request, and reach this page and \
             the release gate together.\n"
        ));
        out
    }
}

/// The three tiers, which are three and in order because a tier is a
/// number a user compares.
fn tiers(doc: &Doc) -> Result<Vec<Tier>, String> {
    let mut tiers: Vec<Tier> = Vec::new();
    for table in doc.array("tier") {
        let line = table.line;
        if let Some(key) = table
            .unknown(&["level", "surface", "conformance", "practice", "doc"])
            .first()
        {
            return Err(format!("line {line}: a tier has no key {key:?}"));
        }
        let level = table
            .int("level")
            .ok_or_else(|| format!("line {line}: a tier with no level"))?;
        let mut axes = [0i64; 3];
        for (at, axis) in ["surface", "conformance", "practice"].iter().enumerate() {
            let found = table
                .int(axis)
                .ok_or_else(|| format!("line {line}: tier {level} promises no {axis}"))?;
            if !(0..=WHOLE).contains(&found) {
                return Err(format!(
                    "line {line}: tier {level} promises {found} on {axis}, and an axis is a \
                     percentage"
                ));
            }
            axes[at] = found;
        }
        let doc = table
            .str("doc")
            .ok_or_else(|| format!("line {line}: tier {level} has no doc"))?
            .to_string();
        if level != tiers.len() as i64 + 1 {
            return Err(format!(
                "line {line}: tier {level} is written after {} tiers, and they are 1, 2, 3",
                tiers.len()
            ));
        }
        if let Some(above) = tiers.last() {
            for (axis, found, was) in [
                ("surface", axes[0], above.surface),
                ("conformance", axes[1], above.conformance),
                ("practice", axes[2], above.practice),
            ] {
                if found > was {
                    return Err(format!(
                        "line {line}: tier {level} promises {found} on {axis} and tier {} \
                         promises {was}, so the lower tier promises more",
                        above.level
                    ));
                }
            }
        }
        tiers.push(Tier {
            level,
            surface: axes[0],
            conformance: axes[1],
            practice: axes[2],
            doc,
            line,
        });
    }
    if tiers.len() != 3 {
        return Err(format!(
            "a table with {} tiers in it, and there are three",
            tiers.len()
        ));
    }
    Ok(tiers)
}

/// The practice axis. The weights are out of a hundred and they have to
/// be: a score out of a number that moves is not a score.
fn items(doc: &Doc) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for table in doc.array("item") {
        let line = table.line;
        if let Some(key) = table.unknown(&["name", "weight", "report", "doc"]).first() {
            return Err(format!("line {line}: an item has no key {key:?}"));
        }
        let name = table
            .str("name")
            .ok_or_else(|| format!("line {line}: an item with no name"))?
            .to_string();
        let weight = table
            .int("weight")
            .ok_or_else(|| format!("line {line}: {name} is worth nothing said"))?;
        if weight < 1 {
            return Err(format!(
                "line {line}: {name} is worth {weight}, and an item nobody is scored on is an item \
                 to delete"
            ));
        }
        let report = table.str("report").map(str::to_string);
        if let Some(report) = &report {
            if !repos::REPORTS.contains(&report.as_str()) {
                return Err(format!(
                    "line {line}: {name} applies where {report:?} is reported, and a release \
                     collects {}",
                    repos::REPORTS.join(", ")
                ));
            }
            if report == REPORT {
                return Err(format!(
                    "line {line}: {name} applies where the {REPORT} is reported, which is every \
                     client on this page"
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
        items.push(Item {
            name,
            weight,
            report,
            doc,
            line,
        });
    }
    let total: i64 = items.iter().map(|item| item.weight).sum();
    if total != WHOLE {
        return Err(format!(
            "the practice items weigh {total} together, and a score is out of {WHOLE}"
        ));
    }
    Ok(items)
}

/// The clients themselves.
fn clients(doc: &Doc, tiers: &[Tier], items: &[Item]) -> Result<Vec<Client>, String> {
    let known: BTreeSet<&str> = items.iter().map(|item| item.name.as_str()).collect();
    let mut clients = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for table in doc.array("client") {
        let line = table.line;
        if let Some(key) = table
            .unknown(&[
                "repository",
                "language",
                "package",
                "registry",
                "maintainer",
                "tier",
                "holds",
                "doc",
            ])
            .first()
        {
            return Err(format!("line {line}: a client has no key {key:?}"));
        }
        let repository = table
            .str("repository")
            .ok_or_else(|| format!("line {line}: a client that lives nowhere"))?
            .to_string();
        // The name is read off the URL rather than written beside it,
        // because the URL is what a reader follows and two fields that
        // have to agree eventually will not.
        let name = repository
            .strip_prefix(HOST)
            .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            .ok_or_else(|| format!("line {line}: {repository} is not one repository under {HOST}"))?
            .to_string();
        let text = |key: &str| -> Result<String, String> {
            table
                .str(key)
                .filter(|found| !found.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("line {line}: {name} says no {key}"))
        };
        let language = text("language")?;
        let package = text("package")?;
        let registry = text("registry")?;
        let maintainer = text("maintainer")?;
        let doc = text("doc")?;
        // A name and a handle. dx/01 section 5 asks for a named
        // maintainer, and a name with nobody reachable behind it is the
        // half that does not help: a user with a question needs somebody
        // to open an issue at.
        let handle = maintainer
            .split_once(" <@")
            .and_then(|(person, rest)| rest.strip_suffix('>').map(|handle| (person, handle)))
            .filter(|(person, handle)| {
                !person.trim().is_empty()
                    && !handle.is_empty()
                    && handle
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
            .map(|(_, handle)| handle);
        if handle.is_none() {
            return Err(format!(
                "line {line}: {name} is maintained by {maintainer:?}, and a maintainer is a person \
                 and a handle, written `Ada Lovelace <@ada>`"
            ));
        }
        let tier = table
            .int("tier")
            .ok_or_else(|| format!("line {line}: {name} promises no tier"))?;
        if !tiers.iter().any(|t| t.level == tier) {
            return Err(format!(
                "line {line}: {name} is tier {tier}, and the tiers are 1, 2, 3"
            ));
        }
        let holds: Vec<String> = table
            .list("holds")
            .ok_or_else(|| {
                format!(
                    "line {line}: {name} holds nothing said, and a client that holds none of it \
                     writes `holds = []`"
                )
            })?
            .to_vec();
        for (at, held) in holds.iter().enumerate() {
            if !known.contains(held.as_str()) {
                return Err(format!(
                    "line {line}: {name} holds {held:?}, and the practice items are {}",
                    items
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if holds[..at].contains(held) {
                return Err(format!("line {line}: {name} holds {held:?} twice"));
            }
        }
        if let Some(before) = seen.insert(name.clone(), line) {
            return Err(format!(
                "line {line}: {name} is written twice, and first on line {before}"
            ));
        }
        clients.push(Client {
            name,
            repository,
            language,
            package,
            registry,
            maintainer,
            tier,
            holds,
            doc,
            line,
        });
    }
    if clients.is_empty() {
        return Err("a table with no clients in it".to_string());
    }
    Ok(clients)
}

/// The first line the published page and the render disagree on.
fn drift(found: &str, want: &str) -> Option<Note> {
    if found == want {
        return None;
    }
    let mut theirs = found.lines();
    let mut ours = want.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (theirs.next(), ours.next()) {
            (None, None) => return None,
            (a, b) if a == b => continue,
            (a, b) => {
                return Some(Note::Drift {
                    line,
                    found: a.unwrap_or("<end of file>").to_string(),
                    want: b.unwrap_or("<end of file>").to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = concat!(
        "schema = 1\n",
        "doc = \"The clients of this engine.\"\n",
        "audited = \"2026-08-19\"\n",
        "\n",
        "[[tier]]\n",
        "level = 1\n",
        "surface = 100\n",
        "conformance = 100\n",
        "practice = 90\n",
        "doc = \"Everything, and the apparatus that keeps it.\"\n",
        "\n",
        "[[tier]]\n",
        "level = 2\n",
        "surface = 100\n",
        "conformance = 95\n",
        "practice = 75\n",
        "doc = \"The whole surface, and a corpus with declared skips.\"\n",
        "\n",
        "[[tier]]\n",
        "level = 3\n",
        "surface = 90\n",
        "conformance = 90\n",
        "practice = 50\n",
        "doc = \"Built with the kit.\"\n",
        "\n",
        "[[item]]\n",
        "name = \"quickstart\"\n",
        "weight = 60\n",
        "doc = \"The README's programs are run as printed.\"\n",
        "\n",
        "[[item]]\n",
        "name = \"misuse\"\n",
        "weight = 20\n",
        "doc = \"Wrong programs, and what each is told.\"\n",
        "\n",
        "[[item]]\n",
        "name = \"api-map\"\n",
        "weight = 20\n",
        "report = \"api-map\"\n",
        "doc = \"A map, checked both ways.\"\n",
        "\n",
        "[[client]]\n",
        "repository = \"https://github.com/tamnd/zu-python\"\n",
        "language = \"Python\"\n",
        "package = \"zudb\"\n",
        "registry = \"PyPI\"\n",
        "maintainer = \"Ada Lovelace <@ada>\"\n",
        "tier = 1\n",
        "holds = [\"quickstart\", \"misuse\"]\n",
        "doc = \"PyO3, three wheels per platform.\"\n",
        "\n",
        "[[client]]\n",
        "repository = \"https://github.com/tamnd/zu-c\"\n",
        "language = \"C and C++\"\n",
        "package = \"zu\"\n",
        "registry = \"vcpkg and Conan\"\n",
        "maintainer = \"Grace Hopper <@grace>\"\n",
        "tier = 1\n",
        "holds = [\"quickstart\"]\n",
        "doc = \"The developer kit for the ABI itself.\"\n",
    );

    /// A split holding exactly those two, one of them owing a map and
    /// one of them right not to.
    const SPLIT: &str = concat!(
        "schema = 1\n",
        "doc = \"The repositories of the split.\"\n",
        "audited = \"2026-08-19\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu\"\n",
        "role = \"engine\"\n",
        "created = \"exists\"\n",
        "doc = \"The engine.\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu-python\"\n",
        "role = \"binding\"\n",
        "tier = 1\n",
        "created = \"DX2\"\n",
        "workflow = \"release.yml\"\n",
        "reports = [\"scorecard\", \"api-map\"]\n",
        "doc = \"zudb on PyPI.\"\n",
        "\n",
        "[[repo]]\n",
        "name = \"zu-c\"\n",
        "role = \"binding\"\n",
        "tier = 1\n",
        "created = \"DX1\"\n",
        "workflow = \"release.yml\"\n",
        "reports = [\"scorecard\"]\n",
        "doc = \"The C developer kit.\"\n",
    );

    fn table() -> Table {
        Table::parse(TABLE).expect("the table parses")
    }

    fn split() -> repos::Table {
        repos::Table::parse(SPLIT).expect("the split parses")
    }

    fn notes(table: &Table, split: &repos::Table, gate: bool) -> Vec<String> {
        let page = table.render(split);
        table
            .hold(split, Some(&page), gate)
            .iter()
            .map(Note::to_string)
            .collect()
    }

    #[test]
    fn a_row_is_read_as_written() {
        let table = table();
        let python = table.client("zu-python").expect("a row");
        assert_eq!(python.name, "zu-python");
        assert_eq!(python.tier, 1);
        assert_eq!(python.person(), "Ada Lovelace");
        assert_eq!(python.handle(), "ada");
        assert_eq!(python.holds, ["quickstart", "misuse"]);
        assert_eq!(table.tier(2).expect("a tier").conformance, 95);
    }

    #[test]
    fn a_table_that_is_the_split_has_nothing_to_say() {
        assert_eq!(notes(&table(), &split(), false), [] as [String; 0]);
    }

    #[test]
    fn a_client_is_scored_out_of_what_it_owes() {
        let table = table();
        let split = split();

        // Both hold the quickstart, worth 60. Python holds the misuse
        // suite too and owes a map it does not have, so it is 80 of 100.
        let python = table.score(table.client("zu-python").expect("a row"), &split);
        assert_eq!(python.practice, 80);
        assert_eq!(python.missing, ["api-map"]);

        // The C kit owes no map, so the 20 that item is worth is not
        // counted against it: 60 of the 80 that apply, which is 75.
        let c = table.score(table.client("zu-c").expect("a row"), &split);
        assert_eq!(c.practice, 75);
        assert_eq!(c.missing, ["misuse"]);
    }

    #[test]
    fn a_client_under_its_tier_is_reported_by_the_gate_and_not_before() {
        let table = table();
        let split = split();
        assert_eq!(notes(&table, &split, false), [] as [String; 0]);
        let gated = notes(&table, &split, true);
        assert_eq!(gated.len(), 2, "{gated:?}");
        assert!(
            gated[0].contains("scores 80 of 90 on practice"),
            "{gated:?}"
        );
        assert!(
            gated[1].contains("scores 75 of 90 on practice"),
            "{gated:?}"
        );
    }

    #[test]
    fn a_client_that_holds_everything_that_applies_meets_its_tier() {
        let text = TABLE.replace(
            "holds = [\"quickstart\"]",
            "holds = [\"quickstart\", \"misuse\"]",
        );
        let table = Table::parse(&text).expect("the table parses");
        let split = split();
        let c = table.score(table.client("zu-c").expect("a row"), &split);
        assert_eq!(c.practice, 100);
        assert!(c.met());
        assert!(c.missing.is_empty(), "{:?}", c.missing);
    }

    #[test]
    fn a_repository_owing_a_scorecard_and_missing_here_is_reported() {
        let split = repos::Table::parse(&SPLIT.replace("name = \"zu-c\"", "name = \"zu-node\""))
            .expect("the split parses");
        let found = notes(&table(), &split, false);
        assert!(
            found.iter().any(|n| n.contains("zu-node owes a scorecard")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|n| n.contains("does not have that repository")),
            "{found:?}"
        );
    }

    #[test]
    fn a_client_whose_repository_reports_no_scorecard_is_reported() {
        let split = repos::Table::parse(&SPLIT.replace(
            "reports = [\"scorecard\"]\ndoc = \"The C developer kit.\"",
            "reports = [\"corpus\"]\ndoc = \"The C developer kit.\"",
        ))
        .expect("the split parses");
        let found = notes(&table(), &split, false);
        assert!(
            found.iter().any(|n| n.contains("reports no scorecard")),
            "{found:?}"
        );
    }

    #[test]
    fn a_tier_promised_twice_and_differently_is_reported() {
        let split = repos::Table::parse(
            &SPLIT.replace("tier = 1\ncreated = \"DX1\"", "tier = 2\ncreated = \"DX1\""),
        )
        .expect("the split parses");
        let found = notes(&table(), &split, false);
        assert!(
            found
                .iter()
                .any(|n| n.contains("tier 1 here and tier \"2\"")),
            "{found:?}"
        );
    }

    #[test]
    fn an_item_claimed_where_the_report_is_not_owed_is_reported() {
        let text = TABLE.replace(
            "holds = [\"quickstart\"]",
            "holds = [\"quickstart\", \"api-map\"]",
        );
        let table = Table::parse(&text).expect("the table parses");
        let found = notes(&table, &split(), false);
        assert!(
            found
                .iter()
                .any(|n| n.contains("holds \"api-map\" and its repository owes no")),
            "{found:?}"
        );
    }

    #[test]
    fn a_page_that_is_not_the_table_says_which_line() {
        let table = table();
        let split = split();
        let page = table.render(&split).replace("| 1 |", "| 3 |");
        let found: Vec<String> = table
            .hold(&split, Some(&page), false)
            .iter()
            .map(Note::to_string)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("cargo xtask clients"), "{}", found[0]);

        let found: Vec<String> = table
            .hold(&split, None, false)
            .iter()
            .map(Note::to_string)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("publishes nothing"), "{}", found[0]);
    }

    #[test]
    fn the_page_publishes_the_two_fields_the_milestone_asks_for() {
        let page = table().render(&split());
        assert!(
            page.contains("https://github.com/tamnd/zu-python"),
            "{page}"
        );
        assert!(
            page.contains("Ada Lovelace ([@ada](https://github.com/ada))"),
            "{page}"
        );
        assert!(page.contains("| Client | Language | Package | Registry | Maintainer | Tier |"));
        // The standing, and what is missing named rather than counted.
        assert!(page.contains("| 1 | 80 | 90 | `api-map` |"), "{page}");
    }

    #[test]
    fn a_maintainer_who_is_not_a_person_and_a_handle_is_refused() {
        for written in [
            "the community",
            "<@ada>",
            "Ada Lovelace",
            "Ada Lovelace <@>",
        ] {
            let text = TABLE.replace("Ada Lovelace <@ada>", written);
            let error = Table::parse(&text).expect_err("a maintainer nobody can reach");
            assert!(error.contains("a person and a handle"), "{error}");
        }
    }

    #[test]
    fn a_repository_somewhere_else_is_refused() {
        for written in [
            "https://gitlab.com/tamnd/zu-python",
            "https://github.com/tamnd/",
            "https://github.com/tamnd/zu-python/tree/main",
        ] {
            let text = TABLE.replace("https://github.com/tamnd/zu-python", written);
            let error = Table::parse(&text).expect_err("a repository that is not ours");
            assert!(error.contains("is not one repository under"), "{error}");
        }
    }

    #[test]
    fn a_practice_axis_that_does_not_weigh_a_hundred_is_refused() {
        let text = TABLE.replace("weight = 20\nreport", "weight = 25\nreport");
        let error = Table::parse(&text).expect_err("a score out of 105");
        assert!(error.contains("weigh 105 together"), "{error}");

        let text = TABLE.replace("weight = 20\nreport", "weight = 0\nreport");
        let error = Table::parse(&text).expect_err("an item nobody is scored on");
        assert!(error.contains("is worth 0"), "{error}");
    }

    #[test]
    fn an_item_nothing_reports_is_refused() {
        let text = TABLE.replace("report = \"api-map\"", "report = \"vibes\"");
        let error = Table::parse(&text).expect_err("a report nothing collects");
        assert!(error.contains("a release collects"), "{error}");

        let text = TABLE.replace("report = \"api-map\"", "report = \"scorecard\"");
        let error = Table::parse(&text).expect_err("an item that applies to everybody");
        assert!(error.contains("which is every client"), "{error}");
    }

    #[test]
    fn a_client_holding_something_that_is_not_an_item_is_refused() {
        let text = TABLE.replace("holds = [\"quickstart\"]", "holds = [\"vibes\"]");
        let error = Table::parse(&text).expect_err("an item nothing defines");
        assert!(error.contains("the practice items are"), "{error}");

        let text = TABLE.replace(
            "holds = [\"quickstart\", \"misuse\"]",
            "holds = [\"misuse\", \"misuse\"]",
        );
        let error = Table::parse(&text).expect_err("one item twice");
        assert!(error.contains("twice"), "{error}");
    }

    #[test]
    fn a_tier_a_lower_one_beats_is_refused() {
        let text = TABLE
            .replace("conformance = 95", "conformance = 100")
            .replace("practice = 75", "practice = 95");
        let error = Table::parse(&text).expect_err("tier 2 promising more than tier 1");
        assert!(error.contains("the lower tier promises more"), "{error}");
    }

    #[test]
    fn tiers_out_of_order_or_missing_are_refused() {
        let text = TABLE
            .replace("level = 2", "level = 3")
            .replace("level = 3\nsurface = 90", "level = 2\nsurface = 90");
        let error = Table::parse(&text).expect_err("tiers written out of order");
        assert!(error.contains("they are 1, 2, 3"), "{error}");

        let at = TABLE.rfind("[[tier]]").expect("a tier");
        let to = TABLE.find("[[item]]").expect("the items");
        let text = format!("{}{}", &TABLE[..at], &TABLE[to..]);
        let error = Table::parse(&text).expect_err("two tiers");
        assert!(error.contains("2 tiers in it"), "{error}");
    }

    #[test]
    fn an_axis_that_is_not_a_percentage_is_refused() {
        let text = TABLE.replace(
            "surface = 100\nconformance = 100",
            "surface = 120\nconformance = 100",
        );
        let error = Table::parse(&text).expect_err("120 out of 100");
        assert!(error.contains("an axis is a percentage"), "{error}");
    }

    #[test]
    fn a_client_written_twice_is_refused() {
        let at = TABLE.rfind("[[client]]").expect("a row");
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
    fn the_committed_table_is_what_this_project_promises() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let table = Table::load(&root.join(PATH)).expect("the committed table loads");
        let notes = table.check(&root, false).expect("the tree is readable");
        assert!(notes.is_empty(), "{notes:#?}");

        // Seven clients: the five tier 1 SDKs of DX4, the tier 2 one,
        // and the kit. Every one of them owes a scorecard back, which is
        // the list in repos.toml, and every one of them has a name on
        // it, which is what dx/01 section 5 asks for.
        assert_eq!(table.clients.len(), 7);
        let tier1: Vec<&str> = table
            .clients
            .iter()
            .filter(|c| c.tier == 1)
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(tier1, ["zu-python", "zu-node", "zu-go", "zu-java", "zu-c"]);
        for client in &table.clients {
            assert!(!client.handle().is_empty(), "{} has no handle", client.name);
        }
    }
}
