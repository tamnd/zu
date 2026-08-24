//! Distils the performance half of a gql-compat report into a set of
//! measurements, and renders a set of those into `docs/gql-performance.md`.
//!
//! This is the scoreboard next door with the other half of the report in
//! it, and it is a separate file for one reason: a tally of correctness
//! is the same number tomorrow on the same code and a timing is not. The
//! conformance tally gates pull requests because a fall in it is always
//! somebody's fault. Nothing here gates anything, because a fall here is
//! as likely to be another build compiling on the same laptop, and a
//! gate that cries wolf is a gate people learn to skip.
//!
//! What the page is for is the shape of the cost: what an engine spends
//! answering, what it spends ingesting, what it holds in memory while it
//! does either, and what it leaves on disk. Every one of those is
//! measured per case by the harness, and every one of them is worth more
//! than the single latency number a benchmark usually reduces to.
//!
//! The 10x claim itself is not made here. That belongs to graph-bench,
//! which runs both engines on a plane it controls. This route measures
//! three engines through three different transports, so the honest thing
//! it can say is which cases carry a comparison at all, and that is the
//! whole job of the floor rule below.

use std::process::ExitCode;
use zu_json::{self as json, Json};

use crate::scoreboard::{push_str_field, s, u};

/// The engine every ratio on the page is taken against.
///
/// Named rather than "the first file the shell handed us", so the page
/// does not change meaning because a glob sorted differently. An input
/// set without it still renders: the first engine given becomes the
/// subject and the page says which one it was.
const SUBJECT: &str = "zu";

/// How far above its own floor a rival's latency has to be before it can
/// carry a 10x claim.
///
/// Ten, so that at most a tenth of what is being divided is the rival's
/// transport rather than its engine. It is the same ten the harness uses
/// to decide whether a store is dense enough to divide, for the same
/// reason: a figure that is mostly a fixed cost is a measurement of the
/// fixed cost.
const FLOOR_MULTIPLE: u64 = 10;

/// One engine's performance run, everything the page prints.
pub(crate) struct Measured {
    pub(crate) engine: String,
    version: String,
    tool: String,
    host: String,
    taken: String,
    selector: String,
    /// What `RETURN 1 AS n` costs, measured once per run. Zero when the
    /// engine would not answer it.
    round_trip_ns: u64,
    /// What the engine's data directory weighs holding nothing. Zero
    /// when the store is not on this machine, which is the usual answer
    /// for a server.
    empty_store_bytes: u64,
    /// Every performance case, in case id order.
    cases: Vec<PerfCase>,
    /// Every fixture the performance cases loaded, in the order the run
    /// first loaded it.
    loads: Vec<LoadStat>,
}

/// One timed case.
struct PerfCase {
    id: String,
    fixture: String,
    /// pass, fail, skip or error. A timing on anything but a pass is a
    /// measurement of the wrong answer, so the page prints the word
    /// instead of the number.
    outcome: String,
    p50_ns: u64,
    p99_ns: u64,
    /// The worst resident set the harness sampled while the case ran.
    /// Zero when the engine is not a process this machine can see.
    rss_peak_bytes: u64,
}

/// One fixture's ingest, as the engine paid for it.
struct LoadStat {
    fixture: String,
    nodes: u64,
    edges: u64,
    /// The whole load, and the part of it the adapter charged to the
    /// engine. The rate below is over the second one, so a clumsy route
    /// in does not make the store look slow.
    wall_ns: u64,
    engine_wall_ns: u64,
    rss_peak_bytes: u64,
    /// The store after the load, and the part of it the engine says is
    /// the graph rather than the schema. The second is zero when the
    /// engine does not separate them.
    store_bytes: u64,
    graph_bytes: u64,
    /// Bits per edge in thousandths, and whether the harness was willing
    /// to compute it. A store that is mostly its own fixed cost divides
    /// to a number about the fixed cost, so the harness withholds it and
    /// says why.
    bits_per_edge_milli: u64,
    density_ok: bool,
    density_note: String,
}

/// Reads a float field as thousandths, so nothing downstream needs a
/// float. A missing field, a negative one and a non-number are all zero,
/// which is the same answer the `density_ok` flag beside it gives.
fn milli(v: Option<&Json>) -> u64 {
    match v {
        Some(Json::Float(f)) if *f > 0.0 => (*f * 1000.0).round() as u64,
        Some(Json::Int(i)) if *i > 0 => (*i as u64) * 1000,
        _ => 0,
    }
}

/// A string field that is allowed to be absent, unlike `s`, which turns
/// a missing field into the word unknown. A note that is not there is
/// not a note that says unknown.
fn text(v: Option<&Json>) -> String {
    v.and_then(Json::as_str).unwrap_or("").to_string()
}

/// Keeps the performance half of a report and drops the rest.
fn distil(report: &Json) -> Result<Measured, String> {
    let engine = report.get("engine").ok_or("report has no engine")?;
    let run = report.get("run").ok_or("report has no run")?;
    let host = report.get("host").ok_or("report has no host")?;

    let mut cases = Vec::new();
    let mut loads: Vec<LoadStat> = Vec::new();
    if let Some(Json::Arr(all)) = report.get("cases") {
        for c in all {
            if c.get("kind").and_then(Json::as_str) != Some("performance") {
                continue;
            }
            let stats = c.get("stats");
            let fixture = s(c.get("fixture"));
            cases.push(PerfCase {
                id: s(c.get("id")),
                fixture: fixture.clone(),
                outcome: s(c.get("outcome")),
                p50_ns: u(stats.and_then(|s| s.get("p50_ns"))),
                p99_ns: u(stats.and_then(|s| s.get("p99_ns"))),
                rss_peak_bytes: u(c.get("process").and_then(|p| p.get("rss_peak_bytes"))),
            });
            // One entry per fixture. The harness reloads a fixture for
            // every case that names it, and ten copies of the same
            // ingest would be ten chances to quote whichever came out
            // fastest.
            if let Some(l) = c.get("load")
                && !loads.iter().any(|e| e.fixture == fixture)
            {
                loads.push(LoadStat {
                    fixture,
                    nodes: u(l.get("nodes")),
                    edges: u(l.get("edges")),
                    wall_ns: u(l.get("wall_ns")),
                    engine_wall_ns: u(l.get("engine_wall_ns")),
                    rss_peak_bytes: u(l.get("process").and_then(|p| p.get("rss_peak_bytes"))),
                    store_bytes: u(l.get("disk").and_then(|d| d.get("bytes_after"))),
                    graph_bytes: u(l.get("graph_bytes")),
                    bits_per_edge_milli: milli(l.get("bits_per_edge")),
                    density_ok: l.get("density_ok").and_then(Json::as_bool).unwrap_or(false),
                    density_note: text(l.get("density_note")),
                });
            }
        }
    }
    cases.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Measured {
        engine: s(engine.get("adapter")),
        version: s(engine.get("version")),
        tool: s(report.get("tool")),
        host: format!(
            "{} {}, {}",
            s(host.get("os")),
            s(host.get("arch")),
            s(host.get("cpu_model"))
        ),
        taken: s(report.get("generated")).chars().take(10).collect(),
        selector: s(run.get("selector")),
        round_trip_ns: u(engine
            .get("round_trip")
            .and_then(|r| r.get("stats"))
            .and_then(|s| s.get("p50_ns"))),
        empty_store_bytes: u(engine.get("empty_store").and_then(|e| e.get("bytes"))),
        cases,
        loads,
    })
}

/// Renders a measurement set as the JSON that gets checked in.
fn render_measured(m: &Measured) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    push_str_field(&mut out, "engine", &m.engine, true);
    push_str_field(&mut out, "version", &m.version, true);
    push_str_field(&mut out, "tool", &m.tool, true);
    push_str_field(&mut out, "host", &m.host, true);
    push_str_field(&mut out, "taken", &m.taken, true);
    push_str_field(&mut out, "selector", &m.selector, true);
    out.push_str(&format!("  \"round_trip_ns\": {},\n", m.round_trip_ns));
    out.push_str(&format!(
        "  \"empty_store_bytes\": {},\n",
        m.empty_store_bytes
    ));
    out.push_str("  \"cases\": [\n");
    for (i, c) in m.cases.iter().enumerate() {
        let comma = if i + 1 == m.cases.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"id\": \"{}\", \"fixture\": \"{}\", \"outcome\": \"{}\", \"p50_ns\": {}, \"p99_ns\": {}, \"rss_peak_bytes\": {}}}{}\n",
            c.id, c.fixture, c.outcome, c.p50_ns, c.p99_ns, c.rss_peak_bytes, comma
        ));
    }
    out.push_str("  ],\n");
    out.push_str("  \"loads\": [\n");
    for (i, l) in m.loads.iter().enumerate() {
        let comma = if i + 1 == m.loads.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"fixture\": \"{}\", \"nodes\": {}, \"edges\": {}, \"wall_ns\": {}, \"engine_wall_ns\": {}, \"rss_peak_bytes\": {}, \"store_bytes\": {}, \"graph_bytes\": {}, \"bits_per_edge_milli\": {}, \"density_ok\": {}, ",
            l.fixture,
            l.nodes,
            l.edges,
            l.wall_ns,
            l.engine_wall_ns,
            l.rss_peak_bytes,
            l.store_bytes,
            l.graph_bytes,
            l.bits_per_edge_milli,
            l.density_ok
        ));
        out.push_str("\"density_note\": ");
        json::escape_into(&l.density_note, &mut out);
        out.push_str(&format!("}}{comma}\n"));
    }
    out.push_str("  ]\n}\n");
    out
}

/// Reads a checked-in measurement set back.
fn load_measured(text_in: &str) -> Result<Measured, String> {
    let j = json::parse(text_in)?;
    let mut cases = Vec::new();
    if let Some(Json::Arr(all)) = j.get("cases") {
        for c in all {
            cases.push(PerfCase {
                id: s(c.get("id")),
                fixture: s(c.get("fixture")),
                outcome: s(c.get("outcome")),
                p50_ns: u(c.get("p50_ns")),
                p99_ns: u(c.get("p99_ns")),
                rss_peak_bytes: u(c.get("rss_peak_bytes")),
            });
        }
    }
    let mut loads = Vec::new();
    if let Some(Json::Arr(all)) = j.get("loads") {
        for l in all {
            loads.push(LoadStat {
                fixture: s(l.get("fixture")),
                nodes: u(l.get("nodes")),
                edges: u(l.get("edges")),
                wall_ns: u(l.get("wall_ns")),
                engine_wall_ns: u(l.get("engine_wall_ns")),
                rss_peak_bytes: u(l.get("rss_peak_bytes")),
                store_bytes: u(l.get("store_bytes")),
                graph_bytes: u(l.get("graph_bytes")),
                bits_per_edge_milli: u(l.get("bits_per_edge_milli")),
                density_ok: l.get("density_ok").and_then(Json::as_bool).unwrap_or(false),
                density_note: text(l.get("density_note")),
            });
        }
    }
    Ok(Measured {
        engine: s(j.get("engine")),
        version: s(j.get("version")),
        tool: s(j.get("tool")),
        host: s(j.get("host")),
        taken: s(j.get("taken")),
        selector: s(j.get("selector")),
        round_trip_ns: u(j.get("round_trip_ns")),
        empty_store_bytes: u(j.get("empty_store_bytes")),
        cases,
        loads,
    })
}

impl Measured {
    fn case(&self, id: &str) -> Option<&PerfCase> {
        self.cases.iter().find(|c| c.id == id)
    }

    fn load(&self, fixture: &str) -> Option<&LoadStat> {
        self.loads.iter().find(|l| l.fixture == fixture)
    }

    /// The cheapest thing this engine was seen to do in this run.
    ///
    /// The harness times `RETURN 1 AS n` for exactly this, and most of
    /// the time that is the answer. It is not always: on the run behind
    /// this page Neo4j answered four cases in well under what its own
    /// round trip cost, because the round trip is measured once, at the
    /// start, against a server that has not warmed up, while the cases
    /// run later against one that has. A floor a query gets under is not
    /// a floor, so the smaller of the two is taken and the page prints
    /// both so the reader can see which one it was.
    fn floor_ns(&self) -> u64 {
        let cheapest = self
            .cases
            .iter()
            .filter(|c| c.outcome == "pass" && c.p50_ns > 0)
            .map(|c| c.p50_ns)
            .min()
            .unwrap_or(0);
        match (self.round_trip_ns, cheapest) {
            (0, c) => c,
            (r, 0) => r,
            (r, c) => r.min(c),
        }
    }

    /// The lowest latency this engine's numbers can carry a 10x claim
    /// above. Zero when there is no floor to build it on, and then
    /// nothing this engine measured is usable.
    fn threshold_ns(&self) -> u64 {
        self.floor_ns().saturating_mul(FLOOR_MULTIPLE)
    }

    /// Whether a case of this engine's can stand on the other side of a
    /// 10x claim: it has to have passed, and it has to be far enough
    /// above this engine's own floor that the claim is about the engine.
    fn usable(&self, c: &PerfCase) -> bool {
        let t = self.threshold_ns();
        t > 0 && c.outcome == "pass" && c.p50_ns > t
    }
}

/// Nanoseconds as milliseconds, three places, rounded rather than cut.
fn ms(ns: u64) -> String {
    let us = (ns + 500) / 1_000;
    format!("{}.{:03} ms", us / 1_000, us % 1_000)
}

/// One number over another, two places. The x is on the number because
/// a bare 23.98 in a table is a quantity nobody can name.
fn times(over: u64, under: u64) -> String {
    if under == 0 {
        return "n/a".to_string();
    }
    let h = (over * 100 + under / 2) / under;
    format!("{}.{:02}x", h / 100, h % 100)
}

/// A count with a space every three digits. A seven digit ingest rate
/// run together is a number a reader has to count on their fingers.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Bytes in the largest unit that leaves a whole part, two places.
fn bytes(n: u64) -> String {
    const MIB: u64 = 1 << 20;
    const KIB: u64 = 1 << 10;
    let unit = |n: u64, div: u64, name: &str| {
        let h = (n * 100 + div / 2) / div;
        format!("{}.{:02} {}", h / 100, h % 100, name)
    };
    if n >= MIB {
        unit(n, MIB, "MiB")
    } else if n >= KIB {
        unit(n, KIB, "KiB")
    } else {
        format!("{n} B")
    }
}

/// Whatever an engine did per second of the time charged to it.
fn per_sec(count: u64, ns: u64) -> String {
    if ns == 0 {
        return "n/a".to_string();
    }
    grouped(count.saturating_mul(1_000_000_000) / ns)
}

/// Whether every column of the page came out of one sitting. The same
/// four things have to agree as on the conformance scoreboard, and a
/// timing needs it more than a count does.
fn together(all: &[Measured]) -> bool {
    let Some(first) = all.first() else {
        return true;
    };
    all.iter().all(|m| {
        m.taken == first.taken
            && m.host == first.host
            && m.tool == first.tool
            && m.selector == first.selector
    })
}

/// The subject first, then the rivals in the order they were given.
fn ordered(all: &[Measured]) -> Vec<&Measured> {
    let mut out: Vec<&Measured> = all.iter().filter(|m| m.engine == SUBJECT).collect();
    out.extend(all.iter().filter(|m| m.engine != SUBJECT));
    out
}

/// Renders the performance page.
fn render_matrix(all: &[Measured]) -> String {
    let engines = ordered(all);
    let mut out = String::new();
    out.push_str("# GQL performance\n\n");
    out.push_str(
        "Generated by `zu conformance --matrix docs/performance/*.json`. Do not edit by hand: a test regenerates it and fails on drift.\n\n",
    );
    out.push_str(
        "Every column is the performance half of a [gql-compat](https://github.com/tamnd/gql-compat) run against one engine, over the same fixtures through the same harness. The conformance scoreboard next door counts answers; this page measures what the answers cost, in latency, in ingest rate, in memory held and in bytes left on disk.\n\n",
    );
    out.push_str(
        "Nothing here gates a pull request. A count of passing cases is the same number tomorrow on the same code and a timing is not, so these files are regenerated by a person or by a nightly, and a fall in one is a question rather than a verdict. Read the date and the machine before reading anything else.\n\n",
    );
    if engines.len() >= 2 {
        if together(all) {
            out.push_str(
                "These columns were taken together: same corpus, same harness build, same machine, same day, one engine after another. Nothing else on this page would mean anything without that.\n\n",
            );
        } else {
            out.push_str(
                "These columns were not all taken together, so they are not a race. Two timings measured months apart on different hardware say something about each engine and nothing about the difference between them.\n\n",
            );
        }
    }
    let subject = match engines.first() {
        Some(m) => *m,
        None => {
            out.push_str("No measurements are checked in. Drop a `docs/performance/<engine>.json` in and it appears in the next regeneration.\n");
            return out;
        }
    };
    if subject.engine != SUBJECT {
        out.push_str(&format!(
            "There is no {SUBJECT} column in this set, so every ratio below is taken against {} instead.\n\n",
            subject.engine
        ));
    }

    floors(&mut out, &engines);
    latency(&mut out, &engines, subject);
    watchlist(&mut out, &engines, subject);
    ingest(&mut out, &engines);
    footprint(&mut out, &engines);

    out.push_str("## What the numbers do not say\n\n");
    out.push_str("The three engines are reached three different ways, and the difference between the routes is larger than the difference between some of the engines. zu answers in a process on a pipe, Neo4j answers over Bolt on a loopback socket into a container, and Ladybug answers one process per exchange. The floor rule above exists because of that and it is the only reason any row on this page is excluded.\n\n");
    out.push_str("The container is worth naming rather than leaving in a footnote, because it is the largest single thing the Neo4j column carries that is not Neo4j. Its ingest reads about half what the same version read reached directly, and its resident memory does not read at all, since nothing outside a container can see a process inside one the way the harness measures. Neither changes a verdict here: the margins on this page are two and three orders of magnitude and the container is a factor of two. But a reader comparing this column against one taken without a container should expect it to be the slower of the two for that reason and not for a reason about the engine.\n\n");
    out.push_str("A timing on a case that did not pass is the cost of the wrong answer, so the table prints the outcome instead of the number. A case an engine skipped never ran at all.\n\n");
    out.push_str("Bits per edge is withheld, with the harness's reason printed beside it, whenever the store is small enough that most of what is being divided is the engine's own fixed cost rather than the graph. Every fixture in this corpus is small on purpose, because the corpus is there to catch a feature that lands slow rather than to size a store, so most of that column is a note. The density figures that carry weight are in `bench/budgets.toml`, taken on LiveJournal.\n\n");
    out.push_str("None of this is the 10x claim. That is graph-bench's matrix, run on a plane it controls, and this page never stands in for it.\n");
    out
}

/// The engine header and the floor rule, which every exclusion below
/// refers back to.
fn floors(out: &mut String, engines: &[&Measured]) {
    out.push_str("## What it costs to say hello\n\n");
    out.push_str("| |");
    for m in engines {
        out.push_str(&format!(" {} |", m.engine));
    }
    out.push_str("\n|---|");
    for _ in engines {
        out.push_str("---|");
    }
    out.push('\n');
    row(out, "version", engines, |m| m.version.clone());
    row(out, "measured", engines, |m| m.taken.clone());
    row(out, "on", engines, |m| m.host.clone());
    row(out, "harness", engines, |m| m.tool.clone());
    row(out, "corpus", engines, |m| m.selector.clone());
    row(out, "round trip, `RETURN 1 AS n`", engines, |m| {
        if m.round_trip_ns == 0 {
            "not answered".to_string()
        } else {
            ms(m.round_trip_ns)
        }
    });
    row(out, "cheapest case it passed", engines, |m| {
        match m
            .cases
            .iter()
            .filter(|c| c.outcome == "pass" && c.p50_ns > 0)
            .map(|c| c.p50_ns)
            .min()
        {
            Some(n) => ms(n),
            None => "none".to_string(),
        }
    });
    row(out, "**floor taken**", engines, |m| {
        format!("**{}**", ms(m.floor_ns()))
    });
    row(out, "usable above", engines, |m| ms(m.threshold_ns()));
    row(out, "empty store on disk", engines, |m| {
        if m.empty_store_bytes == 0 {
            "not on this machine".to_string()
        } else {
            bytes(m.empty_store_bytes)
        }
    });
    out.push('\n');
    out.push_str("An engine's floor is what the cheapest thing it can do costs, and it is why a fast query and a fast engine are not the same claim. The harness measures it directly, once per run, by timing a statement whose answer is already known. That measurement is taken before the run warms up, and on this route it is not always the smallest thing seen: a floor that a real query gets under is not a floor. So the smaller of the two is taken, and both are printed so a reader can see which one it was.\n\n");
    out.push_str(&format!("A rival's latency can carry a ten times claim only above {FLOOR_MULTIPLE} times that rival's own floor. Under that line most of what is being compared is the transport, and a comparison of transports is not a statement about an engine. Rows the rule excludes are marked and left out of the count rather than quietly kept.\n\n"));
}

/// The per case table, and the count of what it settles.
fn latency(out: &mut String, engines: &[&Measured], subject: &Measured) {
    out.push_str("## Latency, per case\n\n");
    out.push_str("The target is the best usable rival p50 divided by ten, best meaning the lowest, because beating the strongest rival is the claim worth making. A rival marked with a dagger passed the case but sits under its own floor threshold, so its number is printed and not counted.\n\n");
    out.push_str("| case |");
    for m in engines {
        out.push_str(&format!(" {} p50 |", m.engine));
    }
    out.push_str(&format!(
        " target | {} faster by | verdict |\n|---|",
        subject.engine
    ));
    for _ in engines {
        out.push_str("---:|");
    }
    out.push_str("---:|---:|---|\n");

    let (mut met, mut missed, mut nocmp) = (0u64, 0u64, 0u64);
    for c in &subject.cases {
        let short = c.id.strip_prefix("performance/").unwrap_or(&c.id);
        out.push_str(&format!("| `{short}` |"));
        let mut best = 0u64;
        for m in engines {
            let cell = match m.case(&c.id) {
                None => "not run".to_string(),
                Some(r) if r.outcome != "pass" => r.outcome.clone(),
                Some(r) if std::ptr::eq(*m, subject) => ms(r.p50_ns),
                Some(r) if m.usable(r) => {
                    if best == 0 || r.p50_ns < best {
                        best = r.p50_ns;
                    }
                    ms(r.p50_ns)
                }
                Some(r) => format!("{} †", ms(r.p50_ns)),
            };
            out.push_str(&format!(" {cell} |"));
        }
        let mine = c.outcome == "pass";
        let (target, gain, verdict) = if !mine {
            nocmp += 1;
            (
                "n/a".to_string(),
                "n/a".to_string(),
                subject.engine.clone() + " did not pass",
            )
        } else if best == 0 {
            nocmp += 1;
            (
                "n/a".to_string(),
                "n/a".to_string(),
                "no usable rival".to_string(),
            )
        } else {
            let ok = c.p50_ns.saturating_mul(FLOOR_MULTIPLE) <= best;
            if ok {
                met += 1;
            } else {
                missed += 1;
            }
            (
                ms(best / FLOOR_MULTIPLE),
                times(best, c.p50_ns),
                if ok { "met" } else { "**missed**" }.to_string(),
            )
        };
        out.push_str(&format!(" {target} | {gain} | {verdict} |\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "{} timed cases. A usable rival number on {} of them, of which {} meet the ten times target and {} do not. On the other {} the page states nothing either way: either {} did not pass the case, or every rival that did sits under its own floor.\n\n",
        subject.cases.len(),
        met + missed,
        met,
        missed,
        nocmp,
        subject.engine
    ));
}

/// Every case where a rival that passed came within ten times, whether
/// or not its number was usable.
///
/// The floor rule takes rows out of the contract and it is not allowed
/// to take them out of sight. A rival under its own floor is still a
/// rival that answered, and the case where it answered nearly as fast is
/// exactly the case worth looking at next.
fn watchlist(out: &mut String, engines: &[&Measured], subject: &Measured) {
    let mut rows: Vec<(String, String, u64, u64)> = Vec::new();
    for c in &subject.cases {
        if c.outcome != "pass" || c.p50_ns == 0 {
            continue;
        }
        for m in engines {
            if std::ptr::eq(*m, subject) {
                continue;
            }
            if let Some(r) = m.case(&c.id)
                && r.outcome == "pass"
                && r.p50_ns < c.p50_ns.saturating_mul(FLOOR_MULTIPLE)
            {
                rows.push((c.id.clone(), m.engine.clone(), c.p50_ns, r.p50_ns));
            }
        }
    }
    out.push_str("## Where a rival came within ten times\n\n");
    if rows.is_empty() {
        out.push_str("Nowhere. Every rival that passed a case the subject passed took at least ten times as long over it.\n\n");
        return;
    }
    out.push_str("These are the cases to look at next. Most of them are excluded from the table above because the rival's number is under its own floor, which keeps them out of the contract and is not a reason to keep them out of sight. A ratio under one means the rival was the faster of the two.\n\n");
    out.push_str("| case | rival | subject p50 | rival p50 | ratio |\n|---|---|---:|---:|---:|\n");
    for (id, rival, mine, theirs) in &rows {
        let short = id.strip_prefix("performance/").unwrap_or(id);
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            short,
            rival,
            ms(*mine),
            ms(*theirs),
            times(*theirs, *mine)
        ));
    }
    out.push('\n');
}

/// Ingest, per fixture and per engine.
fn ingest(out: &mut String, engines: &[&Measured]) {
    out.push_str("## Ingest\n\n");
    out.push_str("The rate is over the time the adapter charged to the engine rather than over the whole load, so an engine reached through a clumsier route in is not made to look slow for it. Where an adapter could not separate the two, the two columns are the same number.\n\n");
    out.push_str("Both rates are printed because neither one describes every fixture. A path of a hundred thousand nodes is an ingest of nodes and a clique of five hundred is an ingest of a quarter of a million edges, and reading the first rate off the second fixture says the engine is slow when what it is doing is different work.\n\n");
    out.push_str("| fixture | engine | nodes | edges | whole load | engine's part | nodes per second | edges per second |\n|---|---|---:|---:|---:|---:|---:|---:|\n");
    for fixture in fixtures(engines) {
        for m in engines {
            let Some(l) = m.load(&fixture) else { continue };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                fixture,
                m.engine,
                grouped(l.nodes),
                grouped(l.edges),
                ms(l.wall_ns),
                ms(l.engine_wall_ns),
                per_sec(l.nodes, l.engine_wall_ns),
                per_sec(l.edges, l.engine_wall_ns)
            ));
        }
    }
    out.push('\n');
}

/// Memory and disk, per fixture and per engine.
fn footprint(out: &mut String, engines: &[&Measured]) {
    out.push_str("## Memory and disk\n\n");
    out.push_str("Peak resident set is the worst the harness sampled, once over the load and once over the cases that fixture serves. An engine the harness cannot see as a process on this machine has no memory column and no store column, which is the honest answer for a server and not a zero.\n\n");
    out.push_str("| fixture | engine | peak RSS loading | peak RSS answering | store after load | bits per edge |\n|---|---|---:|---:|---:|---:|\n");
    let mut notes: Vec<String> = Vec::new();
    for fixture in fixtures(engines) {
        for m in engines {
            let Some(l) = m.load(&fixture) else { continue };
            let worst = m
                .cases
                .iter()
                .filter(|c| c.fixture == fixture)
                .map(|c| c.rss_peak_bytes)
                .max()
                .unwrap_or(0);
            let density = if l.density_ok {
                let b = l.bits_per_edge_milli;
                format!("{}.{:03}", b / 1000, b % 1000)
            } else {
                if !l.density_note.is_empty() && !notes.contains(&l.density_note) {
                    notes.push(l.density_note.clone());
                }
                "withheld".to_string()
            };
            let seen = |n: u64| {
                if n == 0 {
                    "not visible".to_string()
                } else {
                    bytes(n)
                }
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} |\n",
                fixture,
                m.engine,
                seen(l.rss_peak_bytes),
                seen(worst),
                seen(l.store_bytes),
                density
            ));
        }
    }
    out.push('\n');
    if !notes.is_empty() {
        out.push_str("Why a density is withheld, in the harness's words:\n\n");
        for n in &notes {
            out.push_str(&format!("- {n}\n"));
        }
        out.push('\n');
    }
}

/// Every fixture any engine loaded, in the order the first engine met
/// them, then whatever the others added.
fn fixtures(engines: &[&Measured]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in engines {
        for l in &m.loads {
            if !out.contains(&l.fixture) {
                out.push(l.fixture.clone());
            }
        }
    }
    out
}

fn row(out: &mut String, label: &str, engines: &[&Measured], f: impl Fn(&Measured) -> String) {
    out.push_str(&format!("| {label} |"));
    for m in engines {
        out.push_str(&format!(" {} |", f(m)));
    }
    out.push('\n');
}

/// `zu conformance --measured <report.json>`: distil one report.
pub(crate) fn measured_command(path: &str) -> ExitCode {
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
        Ok(m) => {
            if m.cases.is_empty() {
                eprintln!("zu conformance: {path} has no performance cases");
                return ExitCode::FAILURE;
            }
            print!("{}", render_measured(&m));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zu conformance: {path} is not a gql-compat report: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `zu conformance --matrix <measured.json>...`: render the page.
pub(crate) fn matrix_command(paths: &[String]) -> ExitCode {
    let mut all = Vec::with_capacity(paths.len());
    for p in paths {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("zu conformance: cannot read {p}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match load_measured(&text) {
            Ok(m) => all.push(m),
            Err(e) => {
                eprintln!("zu conformance: {p} is not a measurement set: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    print!("{}", render_matrix(&all));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with one fixture, one fast case and one that the rival
    /// answers just above its own floor.
    const REPORT: &str = r#"{
      "tool": "gql-compat devel",
      "generated": "2026-08-22T22:09:44.978+07:00",
      "engine": {"adapter": "zu", "version": "zu 0.0.1",
        "empty_store": {"bytes": 262144},
        "round_trip": {"stats": {"p50_ns": 81584}}},
      "host": {"os": "darwin", "arch": "arm64", "cpu_model": "Apple M4"},
      "run": {"selector": "the whole corpus apart from its large fixtures"},
      "cases": [
        {"id": "performance/sort/top-three", "kind": "performance",
         "fixture": "perf-path-100k", "outcome": "pass",
         "stats": {"p50_ns": 618000, "p99_ns": 700000},
         "process": {"rss_peak_bytes": 11190272},
         "load": {"wall_ns": 596482291, "engine_wall_ns": 78035542,
           "nodes": 100000, "edges": 99999,
           "process": {"rss_peak_bytes": 47480832},
           "disk": {"bytes_after": 4259840},
           "graph_bytes": 2424832, "bits_per_edge": 0,
           "density_ok": false, "density_note": "the graph is too small to divide"}},
        {"id": "performance/scan/count", "kind": "performance",
         "fixture": "perf-path-100k", "outcome": "pass",
         "stats": {"p50_ns": 40000, "p99_ns": 50000},
         "process": {"rss_peak_bytes": 8000000}},
        {"id": "performance/write/set", "kind": "performance",
         "fixture": "perf-path-100k", "outcome": "fail",
         "stats": {"p50_ns": 1238000}},
        {"id": "performance/join/hard", "kind": "performance",
         "fixture": "perf-path-100k", "outcome": "pass",
         "stats": {"p50_ns": 5000000, "p99_ns": 6000000},
         "process": {"rss_peak_bytes": 12000000}},
        {"id": "grammar/whatever", "kind": "grammar", "outcome": "pass"}]
    }"#;

    fn sample() -> Measured {
        distil(&json::parse(REPORT).expect("the sample parses")).expect("the sample distils")
    }

    /// A rival with a floor an order of magnitude above the subject's.
    fn rival() -> Measured {
        let mut m = sample();
        m.engine = "neo4j".to_string();
        m.version = "Neo4j 2025.10.1".to_string();
        m.round_trip_ns = 3_741_875;
        m.empty_store_bytes = 0;
        for c in &mut m.cases {
            c.p50_ns = match c.id.as_str() {
                "performance/sort/top-three" => 14_815_000,
                // Within ten times of the subject and under this
                // engine's own floor, which is the pair the watch list
                // exists for.
                "performance/scan/count" => 300_000,
                // Usable and inside ten times, which is a miss.
                "performance/join/hard" => 20_000_000,
                _ => 36_708_000,
            };
            c.outcome = "pass".to_string();
            c.rss_peak_bytes = 0;
        }
        m
    }

    #[test]
    fn only_performance_cases_are_kept_and_they_come_out_sorted() {
        let m = sample();
        assert_eq!(m.cases.len(), 4, "a grammar case reached the matrix");
        assert_eq!(m.cases[0].id, "performance/join/hard");
        assert_eq!(m.cases[3].id, "performance/write/set");
    }

    #[test]
    fn a_fixture_is_loaded_once_however_many_cases_name_it() {
        // Two cases share the fixture and only one carries a load
        // block, but even when both do the ingest is one measurement.
        // Ten copies of it would be ten chances to quote the fastest.
        let m = sample();
        assert_eq!(m.loads.len(), 1);
        assert_eq!(m.loads[0].nodes, 100_000);
        assert_eq!(
            per_sec(m.loads[0].nodes, m.loads[0].engine_wall_ns),
            "1 281 467"
        );
    }

    #[test]
    fn the_floor_is_the_smaller_of_the_round_trip_and_the_cheapest_pass() {
        // The subject's round trip is 81.584 µs and its cheapest pass is
        // 40 µs, so the pass wins. This is the case that matters: a
        // round trip measured cold that a real query beats is not a
        // floor, and taking it anyway would exclude honest rows.
        let m = sample();
        assert_eq!(m.floor_ns(), 40_000);
        assert_eq!(m.threshold_ns(), 400_000);
        // With no case at all the round trip is all there is.
        let mut bare = sample();
        bare.cases.clear();
        assert_eq!(bare.floor_ns(), 81_584);
    }

    #[test]
    fn a_rival_under_its_own_floor_is_not_a_comparison() {
        let r = rival();
        // 300 µs against a floor of 300 µs: the cheapest thing this
        // engine did, so it can never be ten times its own floor.
        let cheap = r.case("performance/scan/count").expect("the case");
        assert!(!r.usable(cheap));
        let sort = r.case("performance/sort/top-three").expect("the case");
        assert!(r.usable(sort), "14.8 ms is well over ten times 300 µs");
    }

    #[test]
    fn the_page_counts_a_met_case_a_missed_one_and_an_excluded_one() {
        let page = render_matrix(&[sample(), rival()]);
        // sort: 618 µs against 14.815 ms is 23.97x, met.
        assert!(page.contains("23.97x"), "{page}");
        // hard: 5 ms against 20 ms is 4x with a usable rival, missed.
        assert!(page.contains("**missed**"), "{page}");
        // count: the rival is on its own floor, so nothing is claimed.
        assert!(page.contains("no usable rival"), "{page}");
        // set: the subject failed it, so its timing states nothing.
        assert!(page.contains("zu did not pass"), "{page}");
        assert!(
            page.contains("A usable rival number on 2 of them, of which 1 meet the ten times target and 1 do not"),
            "{page}"
        );
    }

    #[test]
    fn an_excluded_rival_still_appears_on_the_watch_list() {
        // count is 40 µs against 300 µs, which is under ten times, and
        // the rival's number is not usable. The row has to show up
        // anyway: the floor rule is allowed to stop a claim, not to
        // hide a case.
        let page = render_matrix(&[sample(), rival()]);
        assert!(page.contains("Where a rival came within ten times"));
        assert!(page.contains("| `scan/count` | neo4j |"), "{page}");
    }

    #[test]
    fn a_measurement_set_survives_a_round_trip() {
        let before = sample();
        let after = load_measured(&render_measured(&before)).expect("reload");
        assert_eq!(after.engine, before.engine);
        assert_eq!(after.round_trip_ns, before.round_trip_ns);
        assert_eq!(after.cases.len(), before.cases.len());
        assert_eq!(after.cases[0].p50_ns, before.cases[0].p50_ns);
        assert_eq!(after.loads[0].store_bytes, before.loads[0].store_bytes);
        assert_eq!(after.loads[0].density_note, before.loads[0].density_note);
        assert_eq!(after.floor_ns(), before.floor_ns());
    }

    #[test]
    fn the_numbers_are_formatted_without_a_float_in_the_output_path() {
        assert_eq!(ms(618_000), "0.618 ms");
        assert_eq!(ms(14_815_000), "14.815 ms");
        // Rounds rather than cuts: 999.6 µs is a millisecond.
        assert_eq!(ms(999_600), "1.000 ms");
        assert_eq!(times(14_815_000, 618_000), "23.97x");
        assert_eq!(times(1, 0), "n/a");
        assert_eq!(grouped(1_281_467), "1 281 467");
        assert_eq!(grouped(999), "999");
        assert_eq!(bytes(2_434_793_472), "2322.00 MiB");
        assert_eq!(bytes(262_144), "256.00 KiB");
        assert_eq!(bytes(999), "999 B");
    }

    #[test]
    fn a_matrix_of_nothing_is_still_a_page() {
        let page = render_matrix(&[]);
        assert!(page.contains("# GQL performance"));
        assert!(page.contains("Drop a `docs/performance/"));
    }
}
