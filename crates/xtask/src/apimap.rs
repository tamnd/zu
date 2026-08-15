//! `api-map.toml`, and the check that keeps it honest.
//!
//! The model says what the API is. The map says what every binding
//! owes it. One of these files lives here, beside `model.json`, and
//! classifies the whole public surface; one lives in each binding
//! repository and gives each classified entity the name that binding
//! calls it by.
//!
//! The check runs in two directions, because either one alone leaves a
//! way for the surface and the bindings to drift apart quietly. Every
//! entity in the model has to be classified here, so a new public
//! symbol that nobody decided about fails this repository's CI rather
//! than shipping unbound in six languages. And every entity classified
//! tier 1 has to be named in each binding's map, so a binding that
//! quietly stopped exposing something fails the release rather than
//! the user's build.
//!
//! What is not checked, on purpose: whether the name a binding gives
//! an entity is any good, and whether the thing behind that name
//! works. The first is a review, the second is the conformance corpus.

use std::collections::{BTreeMap, BTreeSet};

use zu_json::Json;

use crate::toml::Doc;

/// The schema version of `api-map.toml`, which moves when the shape of
/// the file changes and not when the API it classifies does.
pub const SCHEMA: i64 = 1;

/// The map with `target = "rust"` is the one that classifies. It lives
/// here, and every other map is checked against it.
pub const LEDGER: &str = "rust";

/// The targets a map may be written for. A closed list, so that adding
/// a language is a deliberate change here and not a typo in a file
/// this repository never sees.
pub const TARGETS: [&str; 8] = ["rust", "c", "cpp", "python", "node", "go", "java", "dotnet"];

/// What a binding owes an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// `tier = 1`. Every tier-1 SDK exposes it, and a release where one
    /// does not is a release that fails.
    Bound,
    /// `tier = 2`. A binding may expose it. Nothing is gated either
    /// way, which is where anything genuinely optional belongs.
    Optional,
    /// `tier = 3`. No binding exposes it. A reason is required: a
    /// public symbol nobody binds is a decision, and a decision with
    /// no reason written down is one nobody can revisit.
    Internal,
}

impl Tier {
    pub fn number(self) -> i64 {
        match self {
            Tier::Bound => 1,
            Tier::Optional => 2,
            Tier::Internal => 3,
        }
    }

    fn from_number(n: i64) -> Option<Tier> {
        match n {
            1 => Some(Tier::Bound),
            2 => Some(Tier::Optional),
            3 => Some(Tier::Internal),
            _ => None,
        }
    }
}

/// A path prefix and the tier everything under it takes. Groups are
/// what make the file reviewable: `zu::zu1` is five hundred entities
/// and one decision, and writing it out five hundred times would hide
/// the twenty that are not that decision.
#[derive(Debug, Clone)]
pub struct Group {
    /// Covers the prefix itself and everything under `prefix::`, so
    /// `zu::zu1` does not reach `zu::zu1x`.
    pub prefix: String,
    pub tier: Tier,
    pub reason: Option<String>,
    pub line: usize,
}

/// One entity, named exactly. In the ledger this is the exception to a
/// group; in a binding map it is the whole file, because a name cannot
/// be derived from a prefix.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    /// Set in the ledger, never in a binding map, because a tier is
    /// declared once and read everywhere.
    pub tier: Option<Tier>,
    /// What this target calls it. Set in a binding map, never in the
    /// ledger, where the identifier already is the Rust name.
    pub name: Option<String>,
    pub reason: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Map {
    pub target: String,
    /// Sorted by prefix, and the parser refuses a file that is not,
    /// because a ledger nobody can read a diff of is a ledger nobody
    /// reviews.
    pub groups: Vec<Group>,
    /// Sorted by id, for the same reason.
    pub entries: Vec<Entry>,
}

/// What a check found, with the line it is about when the file is the
/// thing at fault.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    pub line: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

fn problem(line: Option<usize>, message: String) -> Problem {
    Problem { line, message }
}

/// Problems that point at a line come first, and then the rest by
/// message. A line is somewhere a reader can go and edit, and a map
/// that was never written reports every tier-1 entity at once, so
/// without this the one entry that is actually wrong sits underneath
/// three hundred lines saying the file is empty.
fn sort(problems: &mut [Problem]) {
    problems.sort_by(|a, b| {
        (a.line.is_none(), a.line, &a.message).cmp(&(b.line.is_none(), b.line, &b.message))
    });
}

impl Map {
    /// Reads and validates the file's own shape. Everything that can be
    /// decided without the model is decided here, so that a map is
    /// either well formed or refused before anything joins against it.
    pub fn parse(text: &str) -> Result<Map, String> {
        let doc = Doc::parse(text)?;

        match doc.root.int("schema") {
            Some(SCHEMA) => {}
            Some(other) => {
                return Err(format!(
                    "schema {other}, and this reader knows {SCHEMA}. \
                     A map from a newer schema is refused rather than read \
                     as if the fields it added were absent."
                ));
            }
            None => return Err("no `schema` at the top of the file".to_string()),
        }
        let target = doc
            .root
            .str("target")
            .ok_or("no `target` at the top of the file")?
            .to_string();
        if !TARGETS.contains(&target.as_str()) {
            return Err(format!(
                "target {target:?} is not one of {}",
                TARGETS.join(", ")
            ));
        }
        if let Some(key) = doc.root.unknown(&["schema", "target"]).first() {
            return Err(format!("no key {key:?} above the first header"));
        }
        if let Some(name) = doc.unknown_arrays(&["group", "entity"]).first() {
            return Err(format!("no `[[{name}]]` table in this schema"));
        }

        let ledger = target == LEDGER;
        let mut groups = Vec::new();
        for table in doc.array("group") {
            let line = table.line;
            if !ledger {
                return Err(format!(
                    "line {line}: `[[group]]` only in the {LEDGER} map. \
                     A binding map gives names, and a name cannot be \
                     derived from a path prefix."
                ));
            }
            if let Some(key) = table.unknown(&["prefix", "tier", "reason"]).first() {
                return Err(format!("line {line}: no key {key:?} in `[[group]]`"));
            }
            let prefix = table
                .str("prefix")
                .ok_or_else(|| format!("line {line}: `[[group]]` with no `prefix`"))?;
            let tier = read_tier(table.int("tier"), line, "group")?;
            let reason = table.str("reason").map(str::to_string);
            groups.push(Group {
                prefix: prefix.to_string(),
                tier,
                reason,
                line,
            });
        }

        let mut entries = Vec::new();
        for table in doc.array("entity") {
            let line = table.line;
            if let Some(key) = table.unknown(&["id", "tier", "name", "reason"]).first() {
                return Err(format!("line {line}: no key {key:?} in `[[entity]]`"));
            }
            let id = table
                .str("id")
                .ok_or_else(|| format!("line {line}: `[[entity]]` with no `id`"))?;
            let tier = match (ledger, table.int("tier")) {
                (true, n) => Some(read_tier(n, line, "entity")?),
                (false, None) => None,
                (false, Some(_)) => {
                    return Err(format!(
                        "line {line}: `tier` only in the {LEDGER} map. \
                         A tier is declared once so that two maps cannot \
                         disagree about what a binding owes."
                    ));
                }
            };
            let name = table.str("name").map(str::to_string);
            match (ledger, &name) {
                (true, Some(_)) => {
                    return Err(format!(
                        "line {line}: `name` only in a binding map. \
                         In the {LEDGER} map the identifier is the name."
                    ));
                }
                (false, None) => {
                    return Err(format!("line {line}: `[[entity]]` with no `name`"));
                }
                _ => {}
            }
            entries.push(Entry {
                id: id.to_string(),
                tier,
                name,
                reason: table.str("reason").map(str::to_string),
                line,
            });
        }

        sorted_and_unique(groups.iter().map(|g| (&g.prefix, g.line)), "prefix")?;
        sorted_and_unique(entries.iter().map(|e| (&e.id, e.line)), "id")?;

        // A tier 3 with no reason is the one shape of entry that
        // passes every other check and still tells a later reader
        // nothing, so it is refused here rather than reviewed for.
        for (line, tier, reason, what) in groups
            .iter()
            .map(|g| (g.line, Some(g.tier), &g.reason, g.prefix.as_str()))
            .chain(
                entries
                    .iter()
                    .map(|e| (e.line, e.tier, &e.reason, e.id.as_str())),
            )
        {
            if tier == Some(Tier::Internal) && reason.is_none() {
                return Err(format!(
                    "line {line}: {what} is tier 3 with no `reason`. \
                     Say why nothing binds it, because that is the part \
                     a later reader cannot work out from the code."
                ));
            }
        }

        Ok(Map {
            target,
            groups,
            entries,
        })
    }

    /// The tier an identifier takes: its own entry if it has one, and
    /// otherwise the longest group prefix that covers it. Longest
    /// rather than first, so that a narrower group is an exception to
    /// a wider one and the order of the file carries no meaning.
    pub fn tier_of(&self, id: &str) -> Option<Tier> {
        match self.entry(id) {
            Some(entry) => entry.tier,
            None => self.group_of(id).map(|i| self.groups[i].tier),
        }
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.id.as_str().cmp(id))
            .ok()
            .map(|i| &self.entries[i])
    }

    /// The index of the group that covers `id`, or `None`.
    ///
    /// This walks the identifier's own prefixes rather than the file's
    /// groups. Both give the same answer, because a prefix only covers
    /// on a `::` boundary, but this one costs the depth of a path and
    /// a binary search rather than the length of the file. The
    /// difference does not show on twenty-five groups, and this check
    /// is going to run over nine of these files with a surface that
    /// only grows.
    pub fn group_of(&self, id: &str) -> Option<usize> {
        prefixes(id).find_map(|prefix| {
            self.groups
                .binary_search_by(|g| g.prefix.as_str().cmp(prefix))
                .ok()
        })
    }

    /// How many identifiers landed at each tier, for the line the
    /// command prints when it finds nothing wrong. A check that says
    /// only "ok" gives a reader no way to notice that six hundred
    /// entities just became tier 3.
    pub fn census(&self, ids: &[String]) -> BTreeMap<Tier, usize> {
        let mut census = BTreeMap::new();
        for id in ids {
            if let Some(tier) = self.tier_of(id) {
                *census.entry(tier).or_insert(0) += 1;
            }
        }
        census
    }
}

/// Every prefix of an identifier that a group could be written as,
/// longest first: the identifier itself, then it without its last
/// segment, and so on. `zu::a::B` gives `zu::a::B`, `zu::a`, `zu`.
///
/// A group covers the prefix itself and anything under `prefix::`, so
/// the prefix that covers an identifier is always one of these, and
/// nothing that merely starts with the same letters is.
fn prefixes(id: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(id), |s| s.rsplit_once("::").map(|(head, _)| head))
}

fn read_tier(n: Option<i64>, line: usize, what: &str) -> Result<Tier, String> {
    let n = n.ok_or_else(|| format!("line {line}: `[[{what}]]` with no `tier`"))?;
    Tier::from_number(n).ok_or_else(|| format!("line {line}: tier {n}, and the tiers are 1, 2, 3"))
}

fn sorted_and_unique<'a>(
    items: impl Iterator<Item = (&'a String, usize)>,
    what: &str,
) -> Result<(), String> {
    let mut previous: Option<&String> = None;
    for (value, line) in items {
        if let Some(before) = previous {
            if before == value {
                return Err(format!("line {line}: {value} appears twice"));
            }
            if before.as_str() > value.as_str() {
                return Err(format!(
                    "line {line}: {value} sorts before {before}, and this file is \
                     read in order. Sort by {what}, so that a diff of it is \
                     something a reviewer can follow."
                ));
            }
        }
        previous = Some(value);
    }
    Ok(())
}

/// A module is a namespace, and namespaces are the one part of a Rust
/// surface that no binding reproduces: Python flattens them, C prefixes
/// them, Go packages them by repository. The full path is on every
/// entity already, so mapping the module as well would be asking nine
/// repositories to classify something none of them can express.
pub fn mappable(kind: &str) -> bool {
    kind != "module"
}

/// The identifiers a map has to speak about, read out of a parsed
/// `model.json`: every entity except the modules, in the order the
/// model holds them, which is sorted.
pub fn mappable_ids(model: &Json) -> Result<Vec<String>, String> {
    let entities = model
        .get("entities")
        .and_then(Json::as_arr)
        .ok_or("the model has no `entities` array")?;
    entities
        .iter()
        .filter(|e| e.get("kind").and_then(Json::as_str).is_none_or(mappable))
        .map(|e| {
            e.get("id")
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| "an entity in the model has no `id`".to_string())
        })
        .collect()
}

/// The ledger against the model: everything public is classified, and
/// nothing classified has gone away.
///
/// The second half is what catches the slow failure. A prefix that
/// stopped matching anything is a decision about code that no longer
/// exists, and it will sit in the file looking like coverage until
/// somebody joins the two by hand.
pub fn check_surface(map: &Map, ids: &[String]) -> Vec<Problem> {
    let mut problems = Vec::new();
    // Both directions come out of one walk over the surface. A group
    // or an entry that nothing reached is a group or an entry about
    // code that is gone, which is the same question asked backwards.
    let mut group_used = vec![false; map.groups.len()];
    let mut entry_used = vec![false; map.entries.len()];

    for id in ids {
        match map
            .entries
            .binary_search_by(|e| e.id.as_str().cmp(id.as_str()))
        {
            Ok(i) => {
                entry_used[i] = true;
                continue;
            }
            Err(_) => {
                if let Some(i) = map.group_of(id) {
                    group_used[i] = true;
                    continue;
                }
            }
        }
        problems.push(problem(
            None,
            format!(
                "{id} is public and nothing maps it. Give it a `[[group]]` \
                 or an `[[entity]]` in the map, saying what a binding owes it."
            ),
        ));
    }

    // An exception under a group counts for the group as well, or
    // carving one identifier out of a module would report the module's
    // group as dead the moment it had nothing else under it.
    for (i, entry) in map.entries.iter().enumerate() {
        if entry_used[i]
            && let Some(g) = map.group_of(&entry.id)
        {
            group_used[g] = true;
        }
    }

    for (group, used) in map.groups.iter().zip(group_used) {
        if !used {
            problems.push(problem(
                Some(group.line),
                format!(
                    "nothing public is under {}, so this group is dead",
                    group.prefix
                ),
            ));
        }
    }
    for (entry, used) in map.entries.iter().zip(entry_used) {
        if !used {
            problems.push(problem(
                Some(entry.line),
                format!("{} is not in the model, so this entry is dead", entry.id),
            ));
        }
    }

    sort(&mut problems);
    problems
}

/// A binding map against the ledger: every tier-1 entity is named, and
/// nothing is named that the ledger says no binding exposes.
///
/// This is the direction a release runs, once per binding repository,
/// over the maps the conductor collected. It runs against the ledger
/// rather than against the model so that a binding never has to be
/// taught which of five hundred storage internals it was not supposed
/// to bind.
pub fn check_binding(binding: &Map, ledger: &Map, ids: &[String]) -> Vec<Problem> {
    let mut problems = Vec::new();

    for id in ids {
        if ledger.tier_of(id) == Some(Tier::Bound) && binding.entry(id).is_none() {
            problems.push(problem(
                None,
                format!("{id} is tier 1 and {} does not name it", binding.target),
            ));
        }
    }

    let known: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    for entry in &binding.entries {
        if !known.contains(entry.id.as_str()) {
            problems.push(problem(
                Some(entry.line),
                format!("{} is not in the model", entry.id),
            ));
            continue;
        }
        if ledger.tier_of(&entry.id) == Some(Tier::Internal) {
            problems.push(problem(
                Some(entry.line),
                format!(
                    "{} is tier 3, and {} names it. Either the map is wrong \
                     or the ledger is, and both are a change here.",
                    entry.id, binding.target
                ),
            ));
        }
    }

    sort(&mut problems);
    problems
}
