//! Distils a gql-compat report into a tally, and renders a set of tallies
//! into the conformance scoreboard in `docs/gql-conformance.md`.
//!
//! A full report is a megabyte of per-case timings, host details and a
//! wall clock, none of which is the same twice. Checking one in would
//! mean a diff on every run that says nothing, which is the fastest way
//! to teach a reviewer to stop reading a file. So a report goes through
//! `zu conformance --tally` first, which keeps the counts and the
//! provenance and throws away everything that moves on its own. A tally
//! is a few hundred bytes, it is stable across runs on the same code,
//! and a diff on one is always worth reading.
//!
//! The scoreboard is generated from the checked-in tallies rather than
//! from a live run, so the doc can carry engines this repository's CI
//! has no way to stand up. That is a real limit and the page says so on
//! its face: every column is dated and names the machine, because a
//! number measured somewhere else in another month is worth something
//! but not the same thing as the one measured this morning.
//!
//! Both halves read JSON, the full report and the tally alike, because
//! zu-cli carries one hand written parser and no room for a second
//! format. See the `zu-json` crate for why there is no serde here.

use std::process::ExitCode;
use zu_json::{self as json, Json};

/// The five kinds a case can have, in the order the scoreboard prints
/// them. Fixed here rather than taken from the report so a new kind
/// appearing upstream is a visible failure instead of a column that
/// silently goes missing.
pub(crate) const KINDS: [&str; 5] = [
    "mandatory",
    "optional",
    "condition",
    "grammar",
    "performance",
];

/// One engine's counts, everything the scoreboard prints and nothing
/// that changes between two runs of the same code.
pub(crate) struct Tally {
    pub(crate) engine: String,
    pub(crate) version: String,
    pub(crate) tool: String,
    pub(crate) host: String,
    pub(crate) taken: String,
    pub(crate) selector: String,
    pub(crate) cases: u64,
    pub(crate) pass: u64,
    pub(crate) fail: u64,
    pub(crate) skip: u64,
    pub(crate) error: u64,
    /// Per kind, in `KINDS` order: cases, pass, fail, skip, error.
    pub(crate) by_kind: Vec<[u64; 5]>,
    /// ISO features with at least one passing case, and the number of
    /// features the corpus touches at all.
    pub(crate) features_passing: u64,
    pub(crate) features_touched: u64,
    /// Distinct GQLSTATUS values the engine actually produced on a case
    /// that was graded on one.
    pub(crate) conditions_seen: u64,
    /// The codes of the features with a passing case, in code order.
    ///
    /// The counts above are what the scoreboard prints; this is what the
    /// conformance statement is made of, since Clause 24.3 makes every
    /// optional feature a claim of its own and a claim is a code. It is
    /// the one list here rather than a count because a statement that
    /// said "224 features" and named none of them would be unusable by
    /// the reader it is written for.
    pub(crate) features_claimed: Vec<String>,
    /// Features of the standard the corpus can write no portable case
    /// for, each with the harness's one word reason, in code order.
    ///
    /// These are the difference between the features the corpus touches
    /// and the 228 the standard defines. A statement that quietly
    /// dropped them would be claiming a denominator it had not measured.
    pub(crate) features_unwritable: Vec<(String, String)>,
    /// Every optional feature the standard defines, which is the
    /// denominator both of the two lists above are read against.
    pub(crate) features_total: u64,
    /// GQLSTATUS conditions with a passing case, and the number the
    /// standard defines.
    pub(crate) conditions_passing: u64,
    pub(crate) conditions_total: u64,
    /// Codes with no passing case that nothing this engine is sent can
    /// raise, because ISO names them for engines lacking a feature this
    /// one has, and codes with no passing case where the engine took what
    /// a limit case asked for.
    ///
    /// Both are the difference between the codes reached and the 68, and
    /// they are two counts rather than one because they close differently.
    /// An unreachable code closes by taking a feature out, which is not an
    /// improvement anybody should make to move a number. A measured one
    /// closes by a case asking for more, and whether asking for more is
    /// worth doing is a question about the standard's silence.
    pub(crate) conditions_unreachable: u64,
    pub(crate) conditions_measured: u64,
    /// Normative subclauses with a passing case, and the number the
    /// standard has. This pair and the next are corpus reach rather than
    /// engine behaviour: a subclause no case cites is one nobody has
    /// written a case for yet, not one the engine failed.
    pub(crate) subclauses_passing: u64,
    pub(crate) subclauses_total: u64,
    /// Subclauses the harness registers as uncitable, and clause headings
    /// no case cites and a passing case cites something inside. A heading
    /// specifies nothing on its own, so it is reached through what it
    /// holds rather than named directly.
    pub(crate) subclauses_registered: u64,
    pub(crate) subclauses_beneath: u64,
    /// Grammar productions with a passing case, and the number the
    /// published BNF holds.
    pub(crate) productions_passing: u64,
    pub(crate) productions_total: u64,
    /// Productions the harness registers as uncitable, each because the
    /// rule spells the name of a catalog object no GQL statement creates
    /// or hangs off a feature the standard leaves unspelled.
    pub(crate) productions_registered: u64,
}

impl Tally {
    /// The five counts for one kind of case, by name.
    ///
    /// By name rather than by index because `KINDS` is this file's
    /// private order and a reader of the statement next door has no way
    /// to know that mandatory happens to be first, nor any reason to
    /// break when it stops being.
    pub(crate) fn kind(&self, name: &str) -> [u64; 5] {
        KINDS
            .iter()
            .position(|k| *k == name)
            .and_then(|i| self.by_kind.get(i).copied())
            .unwrap_or([0; 5])
    }
}

pub(crate) fn u(v: Option<&Json>) -> u64 {
    v.and_then(Json::as_u64).unwrap_or(0)
}

pub(crate) fn s(v: Option<&Json>) -> String {
    v.and_then(Json::as_str).unwrap_or("unknown").to_string()
}

/// Reads a gql-compat report and keeps the parts that are about the
/// engine rather than about the afternoon.
///
/// `taken` is the one field here that does move on its own, and it is
/// kept on purpose: a tally with no date is a number nobody can weigh.
/// It is also the reason a tally is regenerated by a person or a nightly
/// and not by a per-PR job, since a date that changes every commit is
/// the churn this file exists to avoid.
fn distil(report: &Json) -> Result<Tally, String> {
    let engine = report.get("engine").ok_or("report has no engine")?;
    let totals = report.get("totals").ok_or("report has no totals")?;
    let run = report.get("run").ok_or("report has no run")?;
    let host = report.get("host").ok_or("report has no host")?;

    let by_kind_obj = totals.get("by_kind");
    let mut by_kind = Vec::with_capacity(KINDS.len());
    for kind in KINDS {
        let k = by_kind_obj.and_then(|b| b.get(kind));
        by_kind.push([
            u(k.and_then(|k| k.get("cases"))),
            u(k.and_then(|k| k.get("pass"))),
            u(k.and_then(|k| k.get("fail"))),
            u(k.and_then(|k| k.get("skip"))),
            u(k.and_then(|k| k.get("error"))),
        ]);
    }

    // A feature counts as reached when at least one of its cases passed.
    // Cases that passed for a feature the corpus claims but the engine
    // skipped are not reached, which is the whole distinction the
    // headline percentage blurs.
    let mut features_passing = 0;
    let mut features_touched = 0;
    let mut features_claimed: Vec<String> = Vec::new();
    if let Some(Json::Obj(fs)) = report.get("coverage").and_then(|c| c.get("features")) {
        for (code, f) in fs {
            features_touched += 1;
            if u(f.get("pass")) > 0 {
                features_passing += 1;
                features_claimed.push(code.clone());
            }
        }
    }
    features_claimed.sort();

    // The features the corpus itself cannot reach, which the harness
    // names rather than leaving to be inferred from a gap in the
    // numbers.
    let mut features_unwritable: Vec<(String, String)> = Vec::new();
    if let Some(Json::Arr(us)) = report.get("coverage").and_then(|c| c.get("unwritable")) {
        for entry in us {
            features_unwritable.push((s(entry.get("feature")), s(entry.get("reason"))));
        }
    }
    features_unwritable.sort();

    // Conditions, subclauses and productions are counted the same way
    // features are: one of them is reached when a case citing it passed.
    let reached = |name: &str| -> u64 {
        match report.get("coverage").and_then(|c| c.get(name)) {
            Some(Json::Obj(entries)) => {
                entries.iter().filter(|(_, e)| u(e.get("pass")) > 0).count() as u64
            }
            _ => 0,
        }
    };

    // The same three tables read the other way: an entry with no passing
    // case that carries a count saying why. A code with a passing case is
    // never counted here, whatever else its other cases did, because what
    // this measures is the difference between what was reached and the
    // standard's own total.
    let unpassed = |name: &str, field: &str| -> u64 {
        match report.get("coverage").and_then(|c| c.get(name)) {
            Some(Json::Obj(entries)) => entries
                .iter()
                .filter(|(_, e)| u(e.get("pass")) == 0 && u(e.get(field)) > 0)
                .count() as u64,
            _ => 0,
        }
    };

    // The registers, which are lists rather than tables: things ISO names
    // and no portable case can reach, each with a reason the harness
    // checks. Only the count travels, since the reasons are the harness's
    // to publish and a tally that carried them would grow by a page.
    let listed = |name: &str| -> u64 {
        match report.get("coverage").and_then(|c| c.get(name)) {
            Some(Json::Arr(entries)) => entries.len() as u64,
            _ => 0,
        }
    };

    // Distinct codes the engine produced, counted off the cases rather
    // than off a summary, because a code the engine emitted once is the
    // evidence that the machinery works at all.
    let mut codes: Vec<&str> = Vec::new();
    if let Some(Json::Arr(cases)) = report.get("cases") {
        for c in cases {
            if let Some(code) = c.get("got_gqlstatus").and_then(Json::as_str)
                && !code.is_empty()
                && !codes.contains(&code)
            {
                codes.push(code);
            }
        }
    }

    Ok(Tally {
        engine: s(engine.get("adapter")),
        version: s(engine.get("version")),
        tool: s(report.get("tool")),
        host: format!(
            "{} {}, {}",
            s(host.get("os")),
            s(host.get("arch")),
            s(host.get("cpu_model"))
        ),
        // Date only. The clock time would change a checked-in file for
        // no reason a reader cares about.
        taken: s(report.get("generated")).chars().take(10).collect(),
        selector: s(run.get("selector")),
        cases: u(totals.get("cases")),
        pass: u(totals.get("pass")),
        fail: u(totals.get("fail")),
        skip: u(totals.get("skip")),
        error: u(totals.get("error")),
        by_kind,
        features_passing,
        features_touched,
        conditions_seen: codes.len() as u64,
        features_claimed,
        features_unwritable,
        features_total: u(report.get("coverage").and_then(|c| c.get("features_total"))),
        conditions_passing: reached("conditions"),
        conditions_total: u(report
            .get("coverage")
            .and_then(|c| c.get("conditions_total"))),
        conditions_unreachable: unpassed("conditions", "unreachable"),
        conditions_measured: unpassed("conditions", "measured"),
        subclauses_passing: reached("subclauses"),
        subclauses_total: u(report
            .get("coverage")
            .and_then(|c| c.get("subclauses_total"))),
        subclauses_registered: listed("uncitable_subclauses"),
        subclauses_beneath: listed("subclauses_beneath"),
        productions_passing: reached("productions"),
        productions_total: u(report
            .get("coverage")
            .and_then(|c| c.get("productions_total"))),
        productions_registered: listed("uncitable"),
    })
}

/// Renders a tally as the JSON that gets checked in.
fn render_tally(t: &Tally) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    push_str_field(&mut out, "engine", &t.engine, true);
    push_str_field(&mut out, "version", &t.version, true);
    push_str_field(&mut out, "tool", &t.tool, true);
    push_str_field(&mut out, "host", &t.host, true);
    push_str_field(&mut out, "taken", &t.taken, true);
    push_str_field(&mut out, "selector", &t.selector, true);
    out.push_str(&format!("  \"cases\": {},\n", t.cases));
    out.push_str(&format!("  \"pass\": {},\n", t.pass));
    out.push_str(&format!("  \"fail\": {},\n", t.fail));
    out.push_str(&format!("  \"skip\": {},\n", t.skip));
    out.push_str(&format!("  \"error\": {},\n", t.error));
    out.push_str(&format!(
        "  \"features_passing\": {},\n",
        t.features_passing
    ));
    out.push_str(&format!(
        "  \"features_touched\": {},\n",
        t.features_touched
    ));
    out.push_str(&format!("  \"conditions_seen\": {},\n", t.conditions_seen));
    out.push_str(&format!("  \"features_total\": {},\n", t.features_total));
    out.push_str(&format!(
        "  \"conditions_passing\": {},\n",
        t.conditions_passing
    ));
    out.push_str(&format!(
        "  \"conditions_total\": {},\n",
        t.conditions_total
    ));
    out.push_str(&format!(
        "  \"conditions_unreachable\": {},\n",
        t.conditions_unreachable
    ));
    out.push_str(&format!(
        "  \"conditions_measured\": {},\n",
        t.conditions_measured
    ));
    out.push_str(&format!(
        "  \"subclauses_passing\": {},\n",
        t.subclauses_passing
    ));
    out.push_str(&format!(
        "  \"subclauses_total\": {},\n",
        t.subclauses_total
    ));
    out.push_str(&format!(
        "  \"subclauses_registered\": {},\n",
        t.subclauses_registered
    ));
    out.push_str(&format!(
        "  \"subclauses_beneath\": {},\n",
        t.subclauses_beneath
    ));
    out.push_str(&format!(
        "  \"productions_passing\": {},\n",
        t.productions_passing
    ));
    out.push_str(&format!(
        "  \"productions_total\": {},\n",
        t.productions_total
    ));
    out.push_str(&format!(
        "  \"productions_registered\": {},\n",
        t.productions_registered
    ));
    out.push_str("  \"features_claimed\": [");
    for (i, code) in t.features_claimed.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("\"{code}\""));
    }
    out.push_str("],\n");
    out.push_str("  \"features_unwritable\": [");
    for (i, (code, reason)) in t.features_unwritable.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{{\"feature\": \"{code}\", \"reason\": \"{reason}\"}}"
        ));
    }
    out.push_str("],\n");
    out.push_str("  \"by_kind\": {\n");
    for (i, kind) in KINDS.iter().enumerate() {
        let k = &t.by_kind[i];
        let comma = if i + 1 == KINDS.len() { "" } else { "," };
        out.push_str(&format!(
            "    \"{}\": {{\"cases\": {}, \"pass\": {}, \"fail\": {}, \"skip\": {}, \"error\": {}}}{}\n",
            kind, k[0], k[1], k[2], k[3], k[4], comma
        ));
    }
    out.push_str("  }\n}\n");
    out
}

pub(crate) fn push_str_field(out: &mut String, key: &str, value: &str, comma: bool) {
    out.push_str(&format!("  \"{key}\": \""));
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    if comma {
        out.push(',');
    }
    out.push('\n');
}

/// Reads a checked-in tally back.
fn load_tally(text: &str) -> Result<Tally, String> {
    let j = json::parse(text)?;
    let mut by_kind = Vec::with_capacity(KINDS.len());
    for kind in KINDS {
        let k = j.get("by_kind").and_then(|b| b.get(kind));
        by_kind.push([
            u(k.and_then(|k| k.get("cases"))),
            u(k.and_then(|k| k.get("pass"))),
            u(k.and_then(|k| k.get("fail"))),
            u(k.and_then(|k| k.get("skip"))),
            u(k.and_then(|k| k.get("error"))),
        ]);
    }
    Ok(Tally {
        engine: s(j.get("engine")),
        version: s(j.get("version")),
        tool: s(j.get("tool")),
        host: s(j.get("host")),
        taken: s(j.get("taken")),
        selector: s(j.get("selector")),
        cases: u(j.get("cases")),
        pass: u(j.get("pass")),
        fail: u(j.get("fail")),
        skip: u(j.get("skip")),
        error: u(j.get("error")),
        by_kind,
        features_passing: u(j.get("features_passing")),
        features_touched: u(j.get("features_touched")),
        conditions_seen: u(j.get("conditions_seen")),
        features_claimed: match j.get("features_claimed") {
            Some(Json::Arr(codes)) => codes.iter().map(|c| s(Some(c))).collect(),
            _ => Vec::new(),
        },
        features_unwritable: match j.get("features_unwritable") {
            Some(Json::Arr(entries)) => entries
                .iter()
                .map(|e| (s(e.get("feature")), s(e.get("reason"))))
                .collect(),
            _ => Vec::new(),
        },
        features_total: u(j.get("features_total")),
        conditions_passing: u(j.get("conditions_passing")),
        conditions_total: u(j.get("conditions_total")),
        conditions_unreachable: u(j.get("conditions_unreachable")),
        conditions_measured: u(j.get("conditions_measured")),
        subclauses_passing: u(j.get("subclauses_passing")),
        subclauses_total: u(j.get("subclauses_total")),
        subclauses_registered: u(j.get("subclauses_registered")),
        subclauses_beneath: u(j.get("subclauses_beneath")),
        productions_passing: u(j.get("productions_passing")),
        productions_total: u(j.get("productions_total")),
        productions_registered: u(j.get("productions_registered")),
    })
}

/// Reads a checked-in tally from a file, for the statement renderer next
/// door, which is made of one engine's tally rather than of a set.
pub(crate) fn tally_from_file(path: &str) -> Result<Tally, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    load_tally(&text)
}

/// A percentage of judged cases, to one decimal place, without floats in
/// the output path.
///
/// Judged means passed or failed. Skips are out of the denominator
/// because a skip is not a wrong answer, and errors are out because a
/// case that never reached the engine did not measure it. Both are
/// printed separately, in full, right next to this number, since a
/// percentage over a denominator the reader cannot see is a number
/// anyone can move by changing what they count.
fn pct(pass: u64, judged: u64) -> String {
    if judged == 0 {
        return "n/a".to_string();
    }
    let tenths = (pass * 1000 + judged / 2) / judged;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// One surface of the standard, and how to read a tally's counts for it.
///
/// The four are listed once, here, so that adding a fifth is a line in
/// this table rather than a fifth arm in the renderer.
struct Surface {
    name: &'static str,
    /// Whether a heading of this kind can be reached through what it
    /// holds. Only subclauses nest, so only subclauses have a beneath.
    has_beneath: bool,
    of: fn(&Tally) -> Reach,
}

/// The seven counts of one row, before the last is worked out.
struct Reach {
    reached: u64,
    registered: u64,
    unreachable: u64,
    measured: u64,
    beneath: u64,
    total: u64,
}

impl Reach {
    /// What is left after everything accounted for is taken off the
    /// standard's own total.
    ///
    /// Saturating rather than wrapping: a tally whose accounted counts
    /// somehow exceed the total is a tally to fix, and a row reading zero
    /// is a better way to be told that than one reading four billion.
    fn open(&self) -> u64 {
        self.total
            .saturating_sub(self.reached)
            .saturating_sub(self.registered)
            .saturating_sub(self.unreachable)
            .saturating_sub(self.measured)
            .saturating_sub(self.beneath)
    }
}

const SURFACES: [Surface; 4] = [
    Surface {
        name: "optional features",
        has_beneath: false,
        of: |t| Reach {
            reached: t.features_passing,
            registered: t.features_unwritable.len() as u64,
            unreachable: 0,
            measured: 0,
            beneath: 0,
            total: t.features_total,
        },
    },
    Surface {
        name: "GQLSTATUS codes",
        has_beneath: false,
        of: |t| Reach {
            reached: t.conditions_passing,
            registered: 0,
            unreachable: t.conditions_unreachable,
            measured: t.conditions_measured,
            beneath: 0,
            total: t.conditions_total,
        },
    },
    Surface {
        name: "grammar productions",
        has_beneath: false,
        of: |t| Reach {
            reached: t.productions_passing,
            registered: t.productions_registered,
            unreachable: 0,
            measured: 0,
            beneath: 0,
            total: t.productions_total,
        },
    },
    Surface {
        name: "normative subclauses",
        has_beneath: true,
        of: |t| Reach {
            reached: t.subclauses_passing,
            registered: t.subclauses_registered,
            unreachable: 0,
            measured: 0,
            beneath: t.subclauses_beneath,
            total: t.subclauses_total,
        },
    },
];

/// Whether every column of the page came out of one sitting.
///
/// Four things have to agree for a set of columns to be a comparison
/// rather than a collection: the day, the machine, the harness build and
/// the slice of the corpus that was run. A single column is trivially
/// all of those and no column at all makes no claim either way, so both
/// count as taken together.
fn together(tallies: &[Tally]) -> bool {
    let Some(first) = tallies.first() else {
        return true;
    };
    tallies.iter().all(|t| {
        t.taken == first.taken
            && t.host == first.host
            && t.tool == first.tool
            && t.selector == first.selector
    })
}

/// Renders the scoreboard page.
fn render_scoreboard(tallies: &[Tally]) -> String {
    let mut out = String::new();
    out.push_str("# GQL conformance\n\n");
    out.push_str(
        "Generated by `zu conformance --scoreboard docs/conformance/*.json`. Do not edit by hand: a test regenerates it and fails on drift.\n\n",
    );
    out.push_str(
        "Every column is a run of the [gql-compat](https://github.com/tamnd/gql-compat) corpus against one engine, distilled to counts by `zu conformance --tally`. The columns are dated and they name the machine, because a number taken somewhere else in another month is worth something, just not the same thing as the one taken this morning. What the table is for is the shape of the gap: which kinds of case an engine answers, which it refuses, and which it never gets to.\n\n",
    );
    // Whether the columns were taken together is the first thing a
    // reader of a comparison wants to know and the last thing they can
    // work out for themselves, so the page says it rather than leaving
    // it to be inferred from four rows that happen to match.
    if tallies.len() < 2 {
        // One column is not a comparison and no column is not a page,
        // so there is nothing here to say either way.
    } else if together(tallies) {
        out.push_str(
            "These columns were taken together: same corpus, same harness build, same machine, same day, one engine after another. That is what makes them comparable to each other, and it is a property of this set rather than of the page, which will say so or not say so depending on the tallies it was given.\n\n",
        );
    } else {
        out.push_str(
            "These columns were not all taken together, so they are not a race. Read the four provenance rows first, since a column from another day, another machine, another harness build or another slice of the corpus says something about that engine and less than it looks about the difference between it and the one beside it. How much less depends on how far apart they are, which the rows say and this sentence deliberately does not guess.\n\n",
        );
    }

    out.push_str("## Where each engine stands\n\n");
    out.push_str("| |");
    for t in tallies {
        out.push_str(&format!(" {} |", t.engine));
    }
    out.push_str("\n|---|");
    for _ in tallies {
        out.push_str("---|");
    }
    out.push('\n');
    row(&mut out, "version", tallies, |t| t.version.clone());
    row(&mut out, "measured", tallies, |t| t.taken.clone());
    row(&mut out, "on", tallies, |t| t.host.clone());
    row(&mut out, "harness", tallies, |t| t.tool.clone());
    row(&mut out, "corpus", tallies, |t| t.selector.clone());
    row(&mut out, "cases", tallies, |t| t.cases.to_string());
    row(&mut out, "judged (pass + fail)", tallies, |t| {
        (t.pass + t.fail).to_string()
    });
    row(&mut out, "**passed**", tallies, |t| {
        format!("**{}** ({})", t.pass, pct(t.pass, t.pass + t.fail))
    });
    row(&mut out, "failed", tallies, |t| t.fail.to_string());
    row(&mut out, "skipped, cannot hold the fixture", tallies, |t| {
        t.skip.to_string()
    });
    row(&mut out, "never reached a verdict", tallies, |t| {
        t.error.to_string()
    });
    row(&mut out, "ISO features with a passing case", tallies, |t| {
        format!("{} of {}", t.features_passing, t.features_touched)
    });
    row(
        &mut out,
        "distinct GQLSTATUS values produced",
        tallies,
        |t| t.conditions_seen.to_string(),
    );
    out.push('\n');

    out.push_str("## By kind of case\n\n");
    out.push_str("Pass over judged, then the two exclusions. Read the exclusions first. An engine with a high percentage and a large skip column has answered a small and easy corner of the corpus, and the percentage on its own hides that.\n\n");
    out.push_str("| kind | engine | cases | pass | fail | skip | error | pass of judged |\n");
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for (i, kind) in KINDS.iter().enumerate() {
        for t in tallies {
            let k = &t.by_kind[i];
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                kind,
                t.engine,
                k[0],
                k[1],
                k[2],
                k[3],
                k[4],
                pct(k[1], k[1] + k[2])
            ));
        }
    }
    out.push('\n');

    out.push_str("## What each engine reached of the standard\n\n");
    out.push_str("Four surfaces, four denominators, and every one of them from ISO 39075 rather than from the corpus. A row adds up: what a run reached, plus what the harness registers as unreachable by any portable case, plus what nothing sent to this engine can raise, plus what the engine took instead of refusing, plus the headings reached through what they hold, leaves what is still open. Open is the column to read. It is the work left, and it is the only one of the seven that a case can move.\n\n");
    out.push_str("| surface | engine | reached | registered | unreachable | measured | beneath | open | ISO total |\n");
    out.push_str("|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for surface in SURFACES {
        for t in tallies {
            let r = (surface.of)(t);
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                surface.name,
                t.engine,
                r.reached,
                r.registered,
                r.unreachable,
                r.measured,
                if surface.has_beneath {
                    r.beneath.to_string()
                } else {
                    "-".to_string()
                },
                r.open(),
                r.total
            ));
        }
    }
    out.push('\n');
    out.push_str("A registered item is one the harness holds a written reason for, checked against the standard's own text, so the count is an argument rather than a rounding. An unreachable code is one ISO defines for engines lacking a feature this engine has, which means the only way to raise it is to take the feature out. A measured one is a limit the standard leaves to the implementation, where the case asked for a number and the engine simply took it, so what was learned is that the limit is higher than the question.\n\n");
    out.push_str("A tally taken before the harness carried these registers reads zero in the middle columns and puts the whole difference in open, which is the honest reading of a file that never counted them.\n\n");

    out.push_str("## What the numbers do not say\n\n");
    out.push_str("A skip is not a pass. It means the engine declared it cannot hold the fixture the case needs, so the case never ran, and for zu almost every one of those is a limit of the loader rather than of the evaluator. The declaration behind those skips is in `conformance.toml` at the root of this repository, with a reason on every line.\n\n");
    out.push_str("The performance rows are pass and fail on a correctness check, not a speed comparison. Nothing in this table is a timing. What each of those cases cost, in latency, in ingest rate, in memory held and in bytes left on disk, is in `docs/gql-performance.md`, generated the same way from the same runs.\n\n");
    out.push_str("An engine missing from the table is missing because nobody has taken a tally for it on a machine this repository can point at, not because it was tried and refused. Drop a `docs/conformance/<engine>.json` in and it appears in the next regeneration.\n");
    out
}

fn row(out: &mut String, label: &str, tallies: &[Tally], f: impl Fn(&Tally) -> String) {
    out.push_str(&format!("| {label} |"));
    for t in tallies {
        out.push_str(&format!(" {} |", f(t)));
    }
    out.push('\n');
}

/// `zu conformance --tally <report.json>`: distil one report.
pub(crate) fn tally_command(path: &str) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zu conformance: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = match json::parse(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zu conformance: {path} is not JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    match distil(&report) {
        Ok(t) => {
            print!("{}", render_tally(&t));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zu conformance: {path} is not a gql-compat report: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `zu conformance --regressed <report.json> <baseline.json>`: fail when
/// a kind passes fewer cases than the checked-in tally says it did.
///
/// This is the guard the per-PR job runs, and the asymmetry is on
/// purpose. A fall is a failure, because a change that makes zu answer
/// fewer cases correctly needs someone to say out loud that it was worth
/// it. A rise is not: it prints and passes, and the tally in the
/// repository is updated by a person or by the nightly, so an
/// improvement never gets to update its own scoreboard in the same
/// commit that claims it.
///
/// Only kinds the new run actually attempted are compared. The per-PR
/// job runs a subset to keep the wall clock down, and a kind it did not
/// run has zero passes for a reason that has nothing to do with the
/// engine. Counting those would make every PR fail, which is the same
/// as having no guard at all.
pub(crate) fn regressed_command(report_path: &str, baseline_path: &str) -> ExitCode {
    let fresh = match read_report(report_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zu conformance: {report_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let base = match std::fs::read_to_string(baseline_path)
        .map_err(|e| e.to_string())
        .and_then(|t| load_tally(&t))
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zu conformance: {baseline_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut fell = Vec::new();
    let mut rose = Vec::new();
    let mut skipped = Vec::new();
    for (i, kind) in KINDS.iter().enumerate() {
        let now = fresh.by_kind[i];
        let was = base.by_kind[i];
        if now[0] == 0 {
            if was[0] > 0 {
                skipped.push(*kind);
            }
            continue;
        }
        if now[1] < was[1] {
            fell.push(format!(
                "{kind}: {} passing, was {} in {baseline_path}",
                now[1], was[1]
            ));
        } else if now[1] > was[1] {
            rose.push(format!("{kind}: {} passing, was {}", now[1], was[1]));
        }
        // A case that never reached a verdict is worse than one that
        // failed, because the run did not measure what it says it
        // measured. The harness treats any error as fatal on its own
        // exit status, which zu cannot use as a gate while it still has
        // errors in the baseline, so the gate is here: the count may go
        // down and may hold, and may not go up.
        if now[4] > was[4] {
            fell.push(format!(
                "{kind}: {} case(s) never reached a verdict, was {} in {baseline_path}",
                now[4], was[4]
            ));
        }
    }

    if !skipped.is_empty() {
        // Said out loud rather than left implicit. A guard that quietly
        // checks two kinds out of five reads in a green tick exactly
        // like one that checked all of them.
        println!(
            "zu conformance: not compared, this run had no cases of that kind: {}",
            skipped.join(", ")
        );
    }
    for r in &rose {
        println!("zu conformance: up, {r}");
    }
    if fell.is_empty() {
        println!("zu conformance: nothing regressed against {baseline_path}");
        if !rose.is_empty() {
            println!(
                "zu conformance: {} kind(s) improved; regenerate the tally with `zu conformance --tally` when you are ready to publish it",
                rose.len()
            );
        }
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "zu conformance: {} regression(s) against the baseline:",
        fell.len()
    );
    for f in &fell {
        eprintln!("  {f}");
    }
    eprintln!(
        "zu conformance: if the fall is intended, say why in the PR and update {baseline_path}"
    );
    ExitCode::FAILURE
}

fn read_report(path: &str) -> Result<Tally, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let report = json::parse(&text)?;
    distil(&report)
}

/// `zu conformance --scoreboard <tally.json>...`: render the page.
///
/// The engines come out in the order given so the caller decides which
/// column is first, and the drift test passes them sorted by filename
/// so the checked-in page does not depend on how a shell expanded a
/// glob.
pub(crate) fn scoreboard_command(paths: &[String]) -> ExitCode {
    let mut tallies = Vec::with_capacity(paths.len());
    for p in paths {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("zu conformance: cannot read {p}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match load_tally(&text) {
            Ok(t) => tallies.push(t),
            Err(e) => {
                eprintln!("zu conformance: {p} is not a tally: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    print!("{}", render_scoreboard(&tallies));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest report that exercises every field the tally reads.
    const REPORT: &str = r#"{
      "tool": "gql-compat devel",
      "generated": "2026-08-13T21:59:57.275+07:00",
      "engine": {"adapter": "zu", "version": "zu 0.0.1"},
      "host": {"os": "darwin", "arch": "arm64", "cpu_model": "Apple M4"},
      "run": {"selector": "the whole corpus apart from its large fixtures"},
      "totals": {"cases": 6, "pass": 2, "fail": 1, "skip": 2, "error": 1,
        "by_kind": {
          "mandatory": {"cases": 2, "pass": 1, "fail": 1, "skip": 0, "error": 0},
          "condition": {"cases": 2, "pass": 1, "fail": 0, "skip": 1, "error": 0},
          "optional": {"cases": 2, "pass": 0, "fail": 0, "skip": 1, "error": 1}}},
      "coverage": {
        "features": {
          "G004": {"cases": 1, "pass": 1, "fail": 0},
          "G010": {"cases": 1, "pass": 0, "fail": 1}},
        "features_total": 4,
        "conditions": {
          "22G03": {"cases": 1, "pass": 1},
          "22G04": {"cases": 1, "pass": 0, "unreachable": 1},
          "22G10": {"cases": 1, "pass": 0, "measured": 1},
          "42001": {"cases": 1, "pass": 0}},
        "conditions_total": 5,
        "productions": {"path element list step": {"cases": 1, "pass": 1}},
        "productions_total": 3,
        "subclauses": {"16.1": {"cases": 1, "pass": 1}},
        "subclauses_total": 5,
        "unwritable": [{"feature": "G099", "reason": "nothing portable asks for it"}],
        "uncitable": [{"rule": "a rule", "reason": "it spells a catalog object"}],
        "uncitable_subclauses": [{"subclause": "9.1", "reason": "no case can cite it"}],
        "subclauses_beneath": ["16"]},
      "cases": [
        {"id": "a", "got_gqlstatus": "22G03"},
        {"id": "b", "got_gqlstatus": "22G03"},
        {"id": "c", "got_gqlstatus": "42001"},
        {"id": "d", "got_gqlstatus": ""},
        {"id": "e"}]
    }"#;

    fn sample() -> Tally {
        distil(&json::parse(REPORT).expect("the sample parses")).expect("the sample distils")
    }

    #[test]
    fn a_tally_keeps_the_counts_and_drops_the_clock() {
        let t = sample();
        assert_eq!(t.engine, "zu");
        assert_eq!((t.cases, t.pass, t.fail, t.skip, t.error), (6, 2, 1, 2, 1));
        // Date only, no time. A checked-in file that changes because the
        // run started at a different minute is a file people stop reading.
        assert_eq!(t.taken, "2026-08-13");
        assert!(!t.taken.contains(':'), "the tally kept a clock time");
        assert_eq!(t.host, "darwin arm64, Apple M4");
    }

    #[test]
    fn a_kind_the_report_never_mentions_counts_as_zero_not_as_missing() {
        // The sample has no grammar or performance cases. Those rows
        // still have to appear, or a run that happened to skip a kind
        // would silently shorten the table.
        let t = sample();
        assert_eq!(t.by_kind.len(), KINDS.len());
        let grammar = KINDS.iter().position(|k| *k == "grammar").expect("grammar");
        assert_eq!(t.by_kind[grammar], [0, 0, 0, 0, 0]);
        let mandatory = KINDS.iter().position(|k| *k == "mandatory").expect("m");
        assert_eq!(t.by_kind[mandatory], [2, 1, 1, 0, 0]);
    }

    #[test]
    fn a_feature_counts_as_reached_only_when_a_case_for_it_passed() {
        let t = sample();
        assert_eq!(t.features_touched, 2);
        assert_eq!(t.features_passing, 1, "a failing feature was counted");
    }

    #[test]
    fn codes_are_counted_distinct_and_a_blank_is_not_a_code() {
        // Five cases, three with a code, one of them repeated, one blank
        // and one absent. The answer is two.
        let t = sample();
        assert_eq!(t.conditions_seen, 2);
    }

    #[test]
    fn a_tally_survives_a_round_trip() {
        // The checked-in file is read back by the scoreboard, so a field
        // that renders but does not load would show up as "unknown" in
        // the doc and nowhere else.
        let before = sample();
        let after = load_tally(&render_tally(&before)).expect("reload");
        assert_eq!(after.engine, before.engine);
        assert_eq!(after.version, before.version);
        assert_eq!(after.host, before.host);
        assert_eq!(after.taken, before.taken);
        assert_eq!(after.selector, before.selector);
        assert_eq!(after.cases, before.cases);
        assert_eq!(after.pass, before.pass);
        assert_eq!(after.by_kind, before.by_kind);
        assert_eq!(after.features_passing, before.features_passing);
        assert_eq!(after.conditions_seen, before.conditions_seen);
        assert_eq!(after.conditions_unreachable, before.conditions_unreachable);
        assert_eq!(after.conditions_measured, before.conditions_measured);
        assert_eq!(after.subclauses_registered, before.subclauses_registered);
        assert_eq!(after.subclauses_beneath, before.subclauses_beneath);
        assert_eq!(after.productions_registered, before.productions_registered);
    }

    #[test]
    fn a_code_with_no_passing_case_is_counted_by_the_reason_it_carries() {
        // Four codes: one passed, one is unreachable, one was measured
        // and one is simply open. The first is never counted as anything
        // but reached, since a code a case raised is reached whatever
        // else its other cases did.
        let t = sample();
        assert_eq!(t.conditions_passing, 1);
        assert_eq!(t.conditions_unreachable, 1);
        assert_eq!(t.conditions_measured, 1);
    }

    #[test]
    fn a_register_travels_as_its_count_and_not_as_its_reasons() {
        // The reasons belong to the harness's own report, which prints
        // them in full. What a tally needs is the number, so that the
        // arithmetic of a row can be checked without the page growing a
        // page of prose it did not measure.
        let t = sample();
        assert_eq!(t.features_unwritable.len(), 1);
        assert_eq!(t.productions_registered, 1);
        assert_eq!(t.subclauses_registered, 1);
        assert_eq!(t.subclauses_beneath, 1);
    }

    #[test]
    fn every_row_of_the_reach_table_accounts_for_its_own_denominator() {
        // The one property worth asserting about that table: each row
        // adds up to the standard's total, so a column that appears
        // without another column shrinking is a column that is wrong.
        let t = sample();
        let expected = [
            (1, 1, 0, 0, 0, 4),
            (1, 0, 1, 1, 0, 5),
            (1, 1, 0, 0, 0, 3),
            (1, 1, 0, 0, 1, 5),
        ];
        for (surface, want) in SURFACES.iter().zip(expected) {
            let r = (surface.of)(&t);
            assert_eq!(
                (
                    r.reached,
                    r.registered,
                    r.unreachable,
                    r.measured,
                    r.beneath,
                    r.total
                ),
                want,
                "{} read wrong",
                surface.name
            );
            assert_eq!(
                r.reached + r.registered + r.unreachable + r.measured + r.beneath + r.open(),
                r.total,
                "{} does not add up",
                surface.name
            );
        }
    }

    #[test]
    fn a_row_with_nothing_left_reads_open_zero() {
        // The shape a conformance claim is trying to reach. Nothing here
        // is rounded down to get there: what is not reached is named, and
        // when everything is named the remainder is zero.
        let r = Reach {
            reached: 63,
            registered: 0,
            unreachable: 3,
            measured: 2,
            beneath: 0,
            total: 68,
        };
        assert_eq!(r.open(), 0);
    }

    #[test]
    fn a_tally_older_than_the_registers_puts_the_whole_difference_in_open() {
        // A checked-in tally from before the harness counted registers
        // has zeros in the middle columns. The row still has to add up,
        // with the gap sitting in open where a reader can see it, rather
        // than the page quietly borrowing a number it was never given.
        let mut t = sample();
        t.conditions_unreachable = 0;
        t.conditions_measured = 0;
        let codes = SURFACES
            .iter()
            .find(|s| s.name == "GQLSTATUS codes")
            .expect("the codes row");
        let r = (codes.of)(&t);
        assert_eq!(r.open(), 4);
    }

    #[test]
    fn the_reach_table_names_every_surface_and_says_what_open_means() {
        let page = render_scoreboard(&[sample()]);
        for surface in SURFACES {
            assert!(page.contains(surface.name), "{} is missing", surface.name);
        }
        assert!(page.contains("Open is the column to read"));
        // Only subclauses nest, so the other three rows have to say so
        // with a dash rather than with a zero that reads as a measurement.
        assert!(page.contains("| - |"), "{page}");
    }

    #[test]
    fn the_percentage_is_over_judged_cases_and_says_so_when_there_are_none() {
        // 2 passed, 1 failed, 2 skipped, 1 errored. Over judged that is
        // 66.7%. Over cases it would be 33.3%, and picking the larger
        // number while calling it the same thing is the specific
        // dishonesty this test exists to prevent.
        assert_eq!(pct(2, 3), "66.7%");
        assert_eq!(pct(1, 3), "33.3%");
        assert_eq!(pct(0, 5), "0.0%");
        assert_eq!(pct(5, 5), "100.0%");
        assert_eq!(pct(0, 0), "n/a");
        // Rounds half up rather than truncating, so 1 of 8 is 12.5 and
        // not 12.4.
        assert_eq!(pct(1, 8), "12.5%");
    }

    #[test]
    fn the_scoreboard_names_every_engine_and_every_kind() {
        let mut second = sample();
        second.engine = "ladybug".to_string();
        second.pass = 72;
        let page = render_scoreboard(&[sample(), second]);
        assert!(page.contains("| zu |") || page.contains("zu | "), "{page}");
        assert!(page.contains("ladybug"), "{page}");
        for kind in KINDS {
            assert!(page.contains(kind), "the {kind} row is missing");
        }
        // The two exclusions have to be on the page in words, not just
        // as columns. A reader who takes the headline and stops should
        // still have been told what it leaves out.
        assert!(page.contains("A skip is not a pass"));
    }

    #[test]
    fn a_scoreboard_of_nothing_is_still_a_page() {
        // The glob can legitimately match nothing on a fresh checkout,
        // and a panic there would be a worse failure than an empty
        // table with the explanation still attached.
        let page = render_scoreboard(&[]);
        assert!(page.contains("# GQL conformance"));
        assert!(page.contains("Drop a `docs/conformance/"));
    }

    /// One column moving to another day is enough to stop the page
    /// calling itself a comparison.
    #[test]
    fn columns_are_taken_together_only_when_all_four_agree() {
        let a = sample();
        let mut b = sample();
        assert!(together(&[]), "no columns claim nothing either way");
        assert!(together(std::slice::from_ref(&a)));
        b.taken = "2026-01-01".to_string();
        let page = render_scoreboard(&[a, b]);
        assert!(
            page.contains("were not all taken together"),
            "a column from another day still read as a race"
        );
    }
}
