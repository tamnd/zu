//! `zu conformance`: the engine's own statement of what it can do, and a
//! check that a conformance report does not contradict it
//! (Spec/2064g/gql/plan/07).
//!
//! The declaration used to live in Go, hard-coded in the gql-compat zu
//! adapter. That is the wrong repository for it. A capability is a fact
//! about this engine, it changes in the same commit that changes the
//! engine, and a reviewer of that commit is the person who knows whether
//! it moved. Keeping it here means a PR that teaches the loader to hold
//! floats also flips `float-values`, in the same diff, in front of the
//! same reviewer. Keeping it there meant the two drifted until somebody
//! noticed a skip that should have been a verdict.
//!
//! `--declare` prints the declaration. `conformance.toml` at the repo
//! root is that output, checked in, with a test that regenerates it and
//! fails on drift, the same arrangement as the GQLSTATUS table.
//!
//! `--verify` reads a gql-compat report and fails when the report and
//! the declaration disagree, in either direction. Over-claiming is the
//! obvious failure and it is not the interesting one. Declaring a
//! feature unsupported when every case for it passes is just as wrong
//! and much easier to leave lying around, because it costs nothing at
//! the time and quietly converts real passes into skips.

use std::process::ExitCode;

use zu_json::{self as json, Json};

/// One capability zu declares, with the reason attached.
///
/// The reason is not decoration. A `false` with no reason is
/// indistinguishable from a `false` nobody thought about, and the second
/// is a bug that reads exactly like a finding.
struct Declared {
    key: &'static str,
    supported: bool,
    why: &'static str,
}

/// What zu's storage can hold, in the names gql-compat's fixture package
/// uses. Order is that package's `AllCapabilities` order so a diff of
/// this file against the harness reads straight down.
const DATA: &[Declared] = &[
    Declared {
        key: "labels",
        supported: true,
        why: "a node table is a label",
    },
    Declared {
        key: "multi-label",
        supported: true,
        why: "a node row carries a word with a bit per label, and the table it lives in is the label every row of it carries",
    },
    Declared {
        key: "node-properties",
        supported: true,
        why: "node tables carry property columns",
    },
    Declared {
        key: "edge-properties",
        supported: true,
        why: "a rel table carries property columns, addressed by the edge ordinal the load order gives every edge",
    },
    Declared {
        key: "edge-types",
        supported: true,
        why: "a rel table is an edge type",
    },
    Declared {
        key: "multiple-edge-types",
        supported: true,
        why: "a graph holds several rel tables",
    },
    Declared {
        key: "multiple-node-labels",
        supported: true,
        why: "a rel table names the node table at each of its ends, and the two need not be the same one",
    },
    Declared {
        key: "temporal-values",
        supported: true,
        why: "dates, local times and durations ride their own lanes, declared across the sqlite staging hop",
    },
    Declared {
        key: "list-values",
        supported: true,
        why: "a list column holds one element type, staged as a JSON array in a text column",
    },
    Declared {
        key: "null-properties",
        supported: true,
        why: "a column is dense either way and carries validity words saying which of its rows hold a value",
    },
    Declared {
        key: "float-values",
        supported: true,
        why: "float columns ride the fixed width lane as their IEEE bits",
    },
    Declared {
        key: "boolean-values",
        supported: true,
        why: "boolean columns ride the lane, declared BOOLEAN across the sqlite staging hop",
    },
    Declared {
        key: "undirected-edges",
        supported: true,
        why: "a rel table says whether its edges have a direction, and both stored lists answer for one that has none",
    },
    Declared {
        key: "self-loops",
        supported: true,
        why: "the converter reads endpoints through, so an edge to itself survives",
    },
    Declared {
        key: "parallel-edges",
        supported: true,
        why: "a second edge over the same ordered pair survives",
    },
    Declared {
        key: "parallel-edge-properties",
        supported: true,
        why: "an edge property is addressed by the ordinal the match bound, not by searching the forward list for the destination, so two edges over one pair carry their own values",
    },
];

/// What zu's session and wire protocol can do, as opposed to what its
/// storage can hold. These are the harness's non-data capability flags.
const ENGINE: &[Declared] = &[
    Declared {
        key: "gqlstatus",
        supported: true,
        why: "every reply carries a code from the ISO conditions artifact",
    },
    Declared {
        key: "parameters",
        supported: true,
        why: "the shell takes a params object and prepare reports its names",
    },
    Declared {
        key: "transactions",
        supported: true,
        why: "START TRANSACTION, COMMIT and ROLLBACK run across statements on a session",
    },
    Declared {
        key: "multiple-statements",
        supported: true,
        why: "one shell process runs a case's setup list in order",
    },
    Declared {
        key: "isolated",
        supported: true,
        why: "a reset is a statement on the running session, so a case starts on a graph the case before it did not write to",
    },
];

/// Notes the report prints verbatim next to zu's numbers. Anything a
/// reader needs in order to interpret a result and could not work out
/// from the numbers goes here.
const NOTES: &[&str] = &[
    "driven through `zu shell --format jsonl`, one long-lived process per session",
    "loaded through `zu convert`, which reads a SQLite database in zu's schema",
    "the evaluator is MATCH WHERE FILTER LET FOR CALL UNWIND WITH RETURN, plus INSERT of node patterns and \
     of edges between the elements a statement has in scope, SET and REMOVE of properties and \
     of labels, and DELETE and DETACH DELETE of elements, so a case that writes anything else \
     is answered with an error rather than a skip",
    "several statements chain with NEXT, and what one hands the next is the result it returned \
     and nothing else it had in hand, so a variable the statement before it matched is out of \
     scope behind the NEXT; the chain runs as one pipeline rather than materialising a table \
     between statements",
    "two result tables meet with UNION, EXCEPT or INTERSECT in both their forms, ALL keeping \
     every copy and DISTINCT keeping one, and leaving the quantifier out means DISTINCT; the \
     conjunctions are all at one level and fold to the left, so there is no precedence between \
     UNION and INTERSECT, and EXCEPT and INTERSECT hold whichever operand the planner estimates \
     fewer rows for and stream the other past it",
    "OTHERWISE answers the left operand, and runs the right one only when the left answered no \
     rows at all",
    "the operands of a conjunction have to have the same columns, in the same order and under \
     the same names, and neither of them may write, because how many times a write ran would \
     otherwise depend on which operand the planner chose to hold",
    "a FILTER keeps the rows its condition holds for and has no pattern under it, so it reads \
     what the statement already has, including what a NEXT handed it, and the WHERE the \
     standard allows after the word is optional and says nothing more",
    "a LET names values and takes no name away, which is what makes it a LET rather than a \
     WITH, and the definitions read left to right so a later one may use a name an earlier one \
     in the same statement gave; the name is a variable, so LET of a property is refused with \
     the statement that does change a property named",
    "a FOR makes a row of every element of a list, which is the same statement UNWIND is and \
     the spelling the standard gives it, and WITH ORDINALITY or WITH OFFSET numbers those rows, \
     from one and from zero; the number counts the elements of the list rather than the rows \
     the statement has answered, so a FOR under a match starts again at each row that reaches \
     it",
    "an INSERT runs once for every row the clauses before it answered, and the clauses after \
     it read the rows it wrote rather than the store, so a MATCH followed by an INSERT writes \
     one element per row the match answered",
    "an edge carries properties the way an element does, so a written edge holds one value for \
     every column its table stores, and an edge written into a table that stores none on its \
     edges is refused by name rather than dropping what it carried",
    "a SET changes what an element an earlier clause found holds, one row at a time, and the \
     clauses after it read the new value; a property of a node, a property of an edge and the \
     whole record of either are all reachable, and the record form empties every property it \
     does not name",
    "SET and REMOVE of a label change the bit the label is in the row's label word, so a \
     pattern naming that label finds the row afterwards; a label the row's table has not \
     declared is declared by the SET that puts it on, published with the rows the statement \
     changed and undone with them, while the name of the table is the label every row of it \
     carries rather than one a statement puts on or takes off",
    "a REMOVE is the assignment of a null the standard says it is, so it and SET of a null are \
     one write and a column holds the absence as a clear validity bit",
    "a DELETE takes away the element an earlier clause found, a DETACH DELETE takes its edges \
     with it, and a plain DELETE of an element that still has edges on it is refused with the \
     code the standard gives that rather than leaving a dangling edge",
    "a delete item is a variable an earlier clause bound or a query answering the element, \
     written VALUE and the query in braces, and the query runs on its own against the same \
     graph, so it reads the store rather than the variables around it and has to answer one \
     row of one column because one item takes away one element",
    "an element is created in the node table whose own name is the label the pattern wrote, \
     and a label no node table is named by makes one, out of the properties the pattern \
     writes and under the savepoint the statement holds, so a property written as a value \
     that has to be worked out first is refused rather than guessed at",
    "an edge type no rel table is named by makes one as well, between the node tables the two \
     ends of the step are in, and an end nothing gives a label to is refused rather than \
     guessed at because a rel table has to have both of its ends",
    "a graph with a closed graph type is checked at the write rather than at the read, so a \
     label change that would leave an element carrying a label set no element type of that \
     graph type describes is refused with the code the standard gives that, naming the row \
     and the set it would have carried, and a label that names no node table makes no table \
     in such a graph because the type already says what the graph holds",
    "the limits a write can reach are declared and finite rather than absent, so a statement \
     that asks for more than one of them is told the standard's answer for hitting that limit \
     rather than a general failure: a node carries between one label and 64 of them, an edge \
     carries the one label its rel table is named by, and an element or an edge carries up to \
     4096 properties",
    "assigning to the same property of one element twice in one SET is refused, because an \
     element holds one value per property and the clause has not said which of the two it \
     wants, while two SET clauses in a row stay last wins",
    "an element an earlier clause found and a DELETE in the same statement took away is gone \
     for the clauses after it, so reading a property off it and writing an edge onto it are \
     both refused by name rather than reading what the row used to hold",
    "a protocol fault, a malformed frame or an unknown op, reports no GQLSTATUS on purpose \
     and is scored on its message",
];

/// Renders the declaration as TOML.
///
/// Hand-rolled, like the JSON in `main.rs` and for the same reason: T7
/// caps the binary at 15 MiB and this is the only place that needs it.
/// Nothing here contains a quote or a backslash, and a test asserts that,
/// so there is no escaping to get wrong.
pub(crate) fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# What zu declares it can do, for the gql-compat harness.\n\
         #\n\
         # Generated by `zu conformance --declare`. Do not edit by hand: run\n\
         # `ZU_UPDATE_CONFORMANCE=1 cargo test -p zu-cli --test conformance_toml`.\n\
         #\n\
         # Every entry carries a reason, because a `false` with no reason is\n\
         # indistinguishable from a `false` nobody thought about, and the second\n\
         # is a bug that reads exactly like a finding.\n\n",
    );
    out.push_str("[engine]\n");
    out.push_str("name = \"zu\"\n");
    out.push_str(&format!("version = \"{}\"\n\n", crate::VERSION));

    out.push_str("# What the storage can hold.\n[data]\n");
    for d in DATA {
        out.push_str(&format!("{} = {}  # {}\n", d.key, d.supported, d.why));
    }
    out.push_str("\n# What the session and the wire protocol can do.\n[capabilities]\n");
    for d in ENGINE {
        out.push_str(&format!("{} = {}  # {}\n", d.key, d.supported, d.why));
    }
    out.push_str("\n# Printed verbatim beside zu's numbers in the report.\nnotes = [\n");
    for n in NOTES {
        out.push_str(&format!("  \"{n}\",\n"));
    }
    out.push_str("]\n");
    out
}

/// Everything `--verify` found wrong, as sentences a reader can act on.
///
/// It collects rather than returning on the first problem. A run that
/// drifted usually drifted in several places at once, and reporting one
/// at a time turns a single fix into five CI rounds.
fn verify_report(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();

    let Some(caps) = report.get("engine").and_then(|e| e.get("capabilities")) else {
        return vec!["the report has no engine.capabilities to check against".into()];
    };

    // The declaration and the adapter must agree exactly. This is the
    // check the whole file exists for: the two live in different
    // repositories and nothing else notices when they part company.
    let data = caps.get("Data");
    for d in DATA {
        match data.and_then(|m| m.get(d.key)) {
            Some(Json::Bool(reported)) if *reported == d.supported => {}
            Some(Json::Bool(reported)) => problems.push(format!(
                "data capability {}: zu declares {} but the harness reported {reported}",
                d.key, d.supported
            )),
            // Not declared is not "no". A capability the adapter never
            // mentioned reads as false in the report and is
            // indistinguishable from one it considered and rejected.
            _ => problems.push(format!(
                "data capability {}: zu declares {} and the harness reported nothing at all",
                d.key, d.supported
            )),
        }
    }

    // The harness spells its non-data flags in Go's field case, so the
    // mapping is written out rather than derived from the key.
    for (key, field) in [
        ("gqlstatus", "GQLStatus"),
        ("parameters", "Parameters"),
        ("transactions", "Transactions"),
        ("multiple-statements", "MultipleStatements"),
        ("isolated", "Isolated"),
    ] {
        let declared = ENGINE
            .iter()
            .find(|d| d.key == key)
            .expect("every mapped key is declared")
            .supported;
        match caps.get(field) {
            Some(Json::Bool(reported)) if *reported == declared => {}
            Some(Json::Bool(reported)) => problems.push(format!(
                "capability {key}: zu declares {declared} but the harness reported {reported}"
            )),
            _ => problems.push(format!(
                "capability {key}: zu declares {declared} and the harness reported nothing at all"
            )),
        }
    }

    problems.extend(verify_claims_are_not_empty(report));
    problems.extend(verify_nothing_was_contradicted(report));
    problems
}

/// Checks a challenging run for a claim of absence the engine did not keep.
///
/// `gql-compat run -challenge` ignores the declaration, runs the cases it
/// would have excluded, and writes one entry per claim into
/// `declarations`. An entry marked `contradicted` is one where every
/// excluded case passed, which is the one outcome an engine that lacks the
/// thing cannot produce. An ordinary run writes no such array and this
/// check has nothing to say.
///
/// This is the half of `--verify` that could not be written before. The
/// comparison above catches a declaration the adapter reports differently,
/// which is drift between two files. This catches a declaration that is
/// simply wrong, and the only evidence for it is cases that ran.
fn verify_nothing_was_contradicted(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(Json::Arr(declarations)) = report.get("declarations") else {
        return problems;
    };
    for d in declarations {
        if d.get("contradicted") != Some(&Json::Bool(true)) {
            continue;
        }
        let claim = d.get("claim").and_then(Json::as_str).unwrap_or("(unnamed)");
        let cases = d.get("cases").and_then(Json::as_u64).unwrap_or(0);
        let ids = match d.get("passing") {
            Some(Json::Arr(items)) => items
                .iter()
                .filter_map(Json::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        problems.push(format!(
            "zu declares {claim} absent, and all {cases} case(s) it excluded passed: {ids}"
        ));
    }
    problems
}

/// Checks that a capability zu claims actually did something in the run.
///
/// This is the direction that rots quietly. Declaring a feature
/// unsupported when it works costs nothing at the time and silently
/// turns real passes into skips; claiming one that never fires costs
/// nothing either, and inflates the declaration table in the report
/// while the case row stays empty. Neither shows up as a failure
/// anywhere else, so it has to be asserted here.
fn verify_claims_are_not_empty(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    let claims_gqlstatus = ENGINE
        .iter()
        .find(|d| d.key == "gqlstatus")
        .expect("gqlstatus is declared")
        .supported;
    if !claims_gqlstatus {
        return problems;
    }
    let Some(Json::Arr(cases)) = report.get("cases") else {
        problems.push("the report has no cases, so no claim can be checked".into());
        return problems;
    };
    let graded = cases
        .iter()
        .filter(|c| {
            c.get("got_gqlstatus")
                .and_then(Json::as_str)
                .is_some_and(|s| !s.is_empty())
        })
        .count();
    if graded == 0 {
        problems.push(
            "zu declares gqlstatus but no case in the report was graded on a code, \
             so the claim did nothing"
                .into(),
        );
    }
    problems
}

/// The same declaration as JSON, for the harness rather than for a
/// reader.
///
/// The checked-in artifact is TOML because a person has to read it and
/// the reasons matter more than the flags. The harness is written in Go,
/// which has no TOML parser in its standard library, and adding a
/// dependency to that repository so it can read forty lines of key and
/// bool is a worse trade than emitting the same tables twice. Both come
/// from `DATA` and `ENGINE`, so they cannot disagree, and the drift test
/// pins the TOML.
///
/// The reasons are deliberately not here. They exist for a person
/// reading the declaration, and a harness that could read them would
/// sooner or later match on one.
fn render_json() -> String {
    let mut out = String::from("{\"engine\":{\"name\":\"zu\",\"version\":\"");
    out.push_str(crate::VERSION);
    out.push_str("\"},\"data\":{");
    for (i, d) in DATA.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\":{}", d.key, d.supported));
    }
    out.push_str("},\"capabilities\":{");
    for (i, d) in ENGINE.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\":{}", d.key, d.supported));
    }
    out.push_str("},\"notes\":[");
    for (i, n) in NOTES.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{n}\""));
    }
    out.push_str("]}\n");
    out
}

pub(crate) fn conformance_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("--declare") => match args.get(1).map(String::as_str) {
            None => {
                print!("{}", render());
                ExitCode::SUCCESS
            }
            Some("--format") => match args.get(2).map(String::as_str) {
                Some("toml") => {
                    print!("{}", render());
                    ExitCode::SUCCESS
                }
                Some("json") => {
                    print!("{}", render_json());
                    ExitCode::SUCCESS
                }
                _ => crate::usage_error("conformance"),
            },
            _ => crate::usage_error("conformance"),
        },
        Some("--verify") => match args.get(1) {
            Some(path) => verify(path),
            None => crate::usage_error("conformance"),
        },
        Some("--tally") => match args.get(1) {
            Some(path) => crate::scoreboard::tally_command(path),
            None => crate::usage_error("conformance"),
        },
        Some("--scoreboard") if args.len() > 1 => crate::scoreboard::scoreboard_command(&args[1..]),
        Some("--regressed") => match (args.get(1), args.get(2)) {
            (Some(report), Some(baseline)) => {
                crate::scoreboard::regressed_command(report, baseline)
            }
            _ => crate::usage_error("conformance"),
        },
        _ => crate::usage_error("conformance"),
    }
}

fn verify(path: &str) -> ExitCode {
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
    let problems = verify_report(&report);
    if problems.is_empty() {
        println!("zu conformance: the report agrees with conformance.toml");
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "zu conformance: the report contradicts conformance.toml in {} place(s):",
        problems.len()
    );
    for p in &problems {
        eprintln!("  {p}");
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declaration_carries_a_reason() {
        for d in DATA.iter().chain(ENGINE) {
            assert!(
                !d.why.trim().is_empty(),
                "{} declares {} with no reason",
                d.key,
                d.supported
            );
        }
    }

    #[test]
    fn nothing_needs_toml_escaping() {
        // The renderer does no escaping, which is fine only as long as
        // this holds. When it stops holding the renderer has to grow an
        // escape, not the string get quietly reworded.
        let all = DATA
            .iter()
            .chain(ENGINE)
            .flat_map(|d| [d.key, d.why])
            .chain(NOTES.iter().copied());
        for s in all {
            assert!(
                !s.contains('"') && !s.contains('\\') && !s.contains('\n'),
                "{s:?} needs escaping the renderer does not do"
            );
        }
    }

    #[test]
    fn the_declaration_matches_the_harnesss_capability_list() {
        // gql-compat's fixture.AllCapabilities, in its order. If the
        // harness grows a capability and zu says nothing about it, the
        // report prints "no" for something nobody decided, which is the
        // exact failure `Capabilities.Undeclared` exists to catch. This
        // is that check on our side of the wire.
        let expected = [
            "labels",
            "multi-label",
            "node-properties",
            "edge-properties",
            "edge-types",
            "multiple-edge-types",
            "multiple-node-labels",
            "temporal-values",
            "list-values",
            "null-properties",
            "float-values",
            "boolean-values",
            "undirected-edges",
            "self-loops",
            "parallel-edges",
            "parallel-edge-properties",
        ];
        let declared: Vec<&str> = DATA.iter().map(|d| d.key).collect();
        assert_eq!(
            declared, expected,
            "declaration is out of step with gql-compat"
        );
    }

    #[test]
    fn the_rendered_toml_says_what_the_tables_say() {
        let toml = render();
        assert!(toml.contains("name = \"zu\""));
        assert!(toml.contains("gqlstatus = true"));
        assert!(toml.contains("transactions = true"));
        assert!(toml.contains("float-values = true"));
        assert!(toml.contains("self-loops = true"));
        // The reason rides along on the same line as the value, so a
        // reader of the file never has to go looking for it.
        assert!(toml.contains(
            "float-values = true  # float columns ride the fixed width lane as their IEEE bits"
        ));
    }

    #[test]
    fn a_claim_the_harness_contradicted_fails_verification() {
        let report = json::parse(
            r#"{"declarations":[
                 {"claim":"float-values","skip_reason":"fixture-capability",
                  "cases":3,"pass":3,"contradicted":true,
                  "passing":["mandatory/return/float","optional/gv01/double"]}]}"#,
        )
        .expect("test report parses");
        let problems = verify_nothing_was_contradicted(&report);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("float-values"), "{}", problems[0]);
        // The case ids travel with the complaint. A contradiction nobody
        // can reproduce is a line in a log and not a bug report.
        assert!(
            problems[0].contains("mandatory/return/float"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn a_claim_the_harness_confirmed_passes_verification() {
        // Both shapes have to be quiet: a run that challenged the
        // declaration and found it honest, and an ordinary run, which
        // writes no declarations at all and is the common case.
        let challenged = json::parse(
            r#"{"declarations":[
                 {"claim":"GQ13","skip_reason":"required-feature",
                  "cases":2,"pass":0,"fail":2,"contradicted":false}]}"#,
        )
        .expect("test report parses");
        assert!(verify_nothing_was_contradicted(&challenged).is_empty());

        let ordinary = json::parse(r#"{"cases":[]}"#).expect("test report parses");
        assert!(verify_nothing_was_contradicted(&ordinary).is_empty());
    }
}
