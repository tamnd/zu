//! `zu --help`, and everything that quotes it.
//!
//! dx/12 §7 asks for help that is generated, complete, and good: a
//! one-line summary, a synopsis, examples, and a see-also for every
//! subcommand, with `zu --help --format json` a supported versioned
//! output because the CLI reference pages are built from it. All of
//! that comes out of [`COMMANDS`], the usage line a command prints when
//! its arguments are wrong included, so the message a user gets when
//! they get it wrong is the message the documentation gave them.
//!
//! The table is a table rather than a `--help` arm per command because
//! the tests can then hold it to the dispatch in `main`: a command that
//! ships without an entry here, or an entry here for a command that
//! does not exist, is a test failure rather than a reference page a
//! reader finds stale.

use zu_json::Json;

/// One subcommand, as a reader meets it.
pub(crate) struct Command {
    /// What is typed after `zu`.
    pub name: &'static str,
    /// One line, lowercase, no full stop, because it is printed in a
    /// column beside twelve others.
    pub summary: &'static str,
    /// Every accepted form. More than one line where the forms differ
    /// in their arguments rather than in a flag, since a synopsis that
    /// tries to say `convert` in one line says it in none.
    pub synopsis: &'static [&'static str],
    /// Command lines that work, in the order a user meets them: the
    /// plain one first, then the one that shows the flag worth knowing.
    pub examples: &'static [&'static str],
    /// Where to go next, by command name. Held to the table by a test,
    /// so a renamed command cannot leave a dangling pointer here.
    pub see_also: &'static [&'static str],
}

/// Every command this build answers.
///
/// In reading order rather than alphabetical: the two a new user starts
/// with, then the ones that make and check a file, then the ones that
/// read a single fact out of one, then the two test harnesses. A
/// reference page generated from this inherits the order, which is the
/// other reason it is not the order the names happen to sort in.
pub(crate) const COMMANDS: &[Command] = &[
    Command {
        name: "shell",
        summary: "open an interactive session on a file, or on nothing",
        synopsis: &["zu shell [<file.zu1>] [--format jsonl]"],
        examples: &[
            "zu shell",
            "zu shell graph.zu1",
            "zu shell graph.zu1 --format jsonl < frames.jsonl",
        ],
        see_also: &["query", "stat"],
    },
    Command {
        name: "query",
        summary: "run one statement and print the result",
        synopsis: &["zu query <file.zu1> -c <zuQL> [--format table|json] [-p name=value ...]"],
        examples: &[
            "zu query graph.zu1 -c 'MATCH (n:node) RETURN count(n) AS n'",
            "zu query graph.zu1 -c 'MATCH (n:node {id: $id}) RETURN n.id' -p id=10 --format json",
        ],
        see_also: &["shell", "explain", "stat"],
    },
    Command {
        name: "lsp",
        summary: "speak the language server protocol, for an editor",
        synopsis: &["zu lsp --stdio [--db <file.zu1>]"],
        examples: &["zu lsp --stdio", "zu lsp --stdio --db graph.zu1"],
        see_also: &["shell", "query"],
    },
    Command {
        name: "copy",
        summary: "bulk load an edge list, or a whole dataset, into a new file",
        synopsis: &[
            "zu copy [--reorder degree|bfs|none] [--nodes <nodes.csv>] \
             <edges.txt|csv|parquet> <out.zu1> [--format text|json]",
            "zu copy --node <Table>=<nodes.csv> ... --rel <Table>=<From>:<To>:<rels.csv> ... \
             <out.zu1> [--format text|json]",
        ],
        examples: &[
            "zu copy edges.txt graph.zu1",
            "zu copy --nodes nodes.csv edges.txt graph.zu1",
            "zu copy --reorder degree edges.parquet graph.zu1 --format json",
            "zu copy --node Account=nodes/Account.csv --rel transfer=Account:Account:rels/transfer.csv fin.zu1",
        ],
        see_also: &["convert", "stat", "verify"],
    },
    Command {
        name: "convert",
        summary: "rewrite an edge list in another format, or a database in another engine",
        synopsis: &[
            "zu convert <in.txt|csv|parquet> <out.csv|parquet> [--format text|json]",
            "zu convert <in.zu1> <out.db> [--format text|json]",
            "zu convert <in.db> <out.zu1> [--format text|json]",
        ],
        examples: &[
            "zu convert edges.txt edges.parquet",
            "zu convert graph.zu1 graph.db --format json",
        ],
        see_also: &["copy", "verify"],
    },
    Command {
        name: "verify",
        summary: "walk every checksum and cross-check the structure",
        synopsis: &["zu verify <file.zu1> [--format text|json]"],
        examples: &["zu verify graph.zu1", "zu verify graph.zu1 --format json"],
        see_also: &["stat", "analyze"],
    },
    Command {
        name: "stat",
        summary: "print the size breakdown: schema, free space, and data",
        synopsis: &["zu stat <file.zu1> [--format text|json]"],
        examples: &["zu stat graph.zu1", "zu stat graph.zu1 --format json"],
        see_also: &["verify", "analyze"],
    },
    Command {
        name: "analyze",
        summary: "rebuild every rel table's optimizer summary",
        synopsis: &["zu analyze <file.zu1>"],
        examples: &["zu analyze graph.zu1"],
        see_also: &["stat", "query"],
    },
    Command {
        name: "neighbors",
        summary: "print one node's neighbor list, in either direction",
        synopsis: &["zu neighbors [--in] [--key] <file.zu1> <node>"],
        examples: &[
            "zu neighbors graph.zu1 42",
            "zu neighbors --in --key graph.zu1 42",
        ],
        see_also: &["edge", "lookup", "query"],
    },
    Command {
        name: "lookup",
        summary: "resolve an original id through the primary-key index",
        synopsis: &["zu lookup <file.zu1> <key>"],
        examples: &["zu lookup graph.zu1 42"],
        see_also: &["neighbors", "edge"],
    },
    Command {
        name: "edge",
        summary: "ask whether one edge exists, without decoding the list",
        synopsis: &["zu edge [--in] <file.zu1> <src> <dst>"],
        examples: &["zu edge graph.zu1 10 42", "zu edge --in graph.zu1 42 10"],
        see_also: &["neighbors", "lookup"],
    },
    Command {
        name: "conformance",
        summary: "declare, verify, tally, and score conformance reports",
        synopsis: &[
            "zu conformance --declare [--format toml|json]",
            "zu conformance --implementation-defined",
            "zu conformance --verify <report.json>",
            "zu conformance --tally <report.json>",
            "zu conformance --scoreboard <tally.json>...",
            "zu conformance --regressed <report.json> <baseline.json>",
        ],
        examples: &[
            "zu conformance --declare --format json > report.json",
            "zu conformance --implementation-defined > register.md",
            "zu conformance --regressed report.json baseline.json",
        ],
        see_also: &["corpus"],
    },
    Command {
        name: "corpus",
        summary: "run the shared corpus cases against this build",
        synopsis: &["zu corpus <dir> [--strict] [--quiet]"],
        examples: &["zu corpus corpus/", "zu corpus corpus/ --strict"],
        see_also: &["conformance", "query"],
    },
    Command {
        name: "version",
        summary: "print the version, the C ABI revision, and what this build supports",
        synopsis: &["zu version [--format text|json]"],
        examples: &["zu version", "zu version --format json"],
        see_also: &["stat"],
    },
];

/// The rest of the surface dx/12 §1 specifies, named so that `--help`
/// is complete about what the tool will be as well as about what it is.
///
/// A user who reads about `zu explain` elsewhere and finds nothing here
/// has to guess whether they typed it wrong or installed the wrong
/// build, and that guess costs more than one line of help. Each of
/// these is refused with the same message rather than ignored, and each
/// arrives with the milestone that gives it something to do.
pub(crate) const PLANNED: &[&str] = &[
    "exec",
    "explain",
    "profile",
    "bench",
    "s3",
    "serve",
    "mcp",
    "docs",
    "completions",
];

/// What the process exits with, fixed by dx/12 §7.
///
/// One table, because these are a contract with whatever script is
/// branching on them and a code invented at a call site is a contract
/// nobody wrote down. 130 arrives two ways that agree: a terminated
/// process reports the signal that killed it and a shell renders that
/// as 128 plus the number, which for `SIGINT` is already 130, and a
/// statement stopped through an interrupt returns a condition this maps
/// to the same number.
pub(crate) const EXIT_CODES: &[(u8, &str)] = &[
    (0, "success"),
    (1, "query error"),
    (2, "usage error"),
    (3, "file or io error"),
    (4, "corruption"),
    (5, "conflict"),
    (130, "interrupted"),
];

/// The version of the shape [`json`] emits, not of the CLI.
///
/// A reference-page generator reads that document and a change to how
/// it nests is a change that breaks the generator, which is the case
/// dx/12 §7 calls a "supported, versioned output". The CLI's own
/// version travels in the same document under `zu`, so a reader can
/// tell a build apart from a schema.
pub(crate) const HELP_VERSION: u32 = 1;

/// The entry for `name`, or `None` for a name that is not a command.
pub(crate) fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// `zu --help`: the whole surface on one screen, and where to go for
/// the detail. The summary column is padded to the widest name so the
/// summaries line up, computed rather than typed because a longer
/// command name would otherwise quietly ruin the column.
pub(crate) fn overview(version: &str) -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0) + 2;
    let mut out = format!("zu {version}: embedded property-graph database\n\n");
    out.push_str("usage: zu <command> [arguments]\n\n");
    for c in COMMANDS {
        out.push_str(&format!("  {:<width$}{}\n", c.name, c.summary));
    }
    out.push_str(&format!(
        "\nnot in this build yet, and refused rather than ignored: {}\n",
        PLANNED.join(", ")
    ));
    out.push_str(&format!(
        "\nexit codes: {}\n",
        EXIT_CODES
            .iter()
            .map(|(code, meaning)| format!("{code} {meaning}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str("\nrun `zu <command> --help` for the synopsis, examples, and related commands\n");
    out.push_str("or `zu --help --format json` for all of it at once\n");
    out
}

/// `zu <command> --help`: the four things dx/12 §7 asks for, in the
/// order a reader needs them.
pub(crate) fn page(c: &Command) -> String {
    let mut out = format!("zu {}: {}\n\n", c.name, c.summary);
    out.push_str(&usage(c));
    out.push_str("\nexamples:\n");
    for example in c.examples {
        out.push_str(&format!("  {example}\n"));
    }
    if !c.see_also.is_empty() {
        out.push_str(&format!("\nsee also: {}\n", c.see_also.join(", ")));
    }
    out
}

/// The synopsis block, which is also what a wrong command line prints.
/// The continuation lines are indented under the first so that a reader
/// sees one paragraph of forms rather than three sentences that each
/// look like the whole answer.
pub(crate) fn usage(c: &Command) -> String {
    let mut out = String::new();
    for (i, line) in c.synopsis.iter().enumerate() {
        out.push_str(if i == 0 { "usage: " } else { "       " });
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `zu --help --format json`: the same table, for the tool that builds
/// the reference pages.
///
/// Pretty rather than compact, unlike the `--format json` the other
/// commands print. Those are frames a pipe carries; this is a document
/// that gets committed and reviewed, and a diff of one long line tells
/// a reviewer nothing.
pub(crate) fn json(version: &str) -> String {
    let strings =
        |items: &[&str]| Json::Arr(items.iter().map(|s| Json::Str((*s).into())).collect());
    let commands = COMMANDS
        .iter()
        .map(|c| {
            Json::Obj(vec![
                ("name".into(), Json::Str(c.name.into())),
                ("summary".into(), Json::Str(c.summary.into())),
                ("synopsis".into(), strings(c.synopsis)),
                ("examples".into(), strings(c.examples)),
                ("see_also".into(), strings(c.see_also)),
            ])
        })
        .collect();
    let codes = EXIT_CODES
        .iter()
        .map(|(code, meaning)| {
            Json::Obj(vec![
                ("code".into(), Json::Int(i64::from(*code))),
                ("meaning".into(), Json::Str((*meaning).into())),
            ])
        })
        .collect();
    Json::Obj(vec![
        ("help_version".into(), Json::Int(i64::from(HELP_VERSION))),
        ("zu".into(), Json::Str(version.into())),
        ("commands".into(), Json::Arr(commands)),
        ("planned".into(), strings(PLANNED)),
        ("exit_codes".into(), Json::Arr(codes)),
    ])
    .to_pretty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the whole of what `main` dispatches, checked against
    /// `main` itself rather than against a second list that would drift
    /// the same way the first one would.
    ///
    /// The dispatch is one `match` on the first argument, so the command
    /// names are the string literals inside `Some("` in the body of
    /// `fn main`. Anything starting with a dash is a flag spelling of a
    /// command rather than a command, and `help` is the one name that
    /// documents rather than is documented.
    #[test]
    fn every_command_main_answers_has_a_page_and_the_reverse() {
        let source = include_str!("main.rs");
        let body = source
            .split_once("fn main() -> ExitCode {")
            .expect("main is where the dispatch is")
            .1
            .split_once("\n}\n")
            .expect("and it ends")
            .0;
        let mut dispatched: Vec<&str> = Vec::new();
        for at in body.match_indices("Some(\"").map(|(i, _)| i + 6) {
            let name = &body[at..][..body[at..].find('"').expect("a closed literal")];
            if !name.starts_with('-') && name != "help" {
                dispatched.push(name);
            }
        }
        for name in &dispatched {
            assert!(find(name).is_some(), "`zu {name}` runs and has no help");
        }
        for c in COMMANDS {
            assert!(
                dispatched.contains(&c.name),
                "`zu {}` has help and nothing dispatches it",
                c.name
            );
        }
    }

    /// A see-also that names a command this build does not have is a
    /// dead end, and one that names a planned command is a promise, so
    /// both are allowed and anything else is not.
    #[test]
    fn every_see_also_points_at_something() {
        for c in COMMANDS {
            for name in c.see_also {
                assert!(
                    find(name).is_some() || PLANNED.contains(name),
                    "`zu {}` sends a reader to `{name}`, which is nothing",
                    c.name
                );
                assert_ne!(*name, c.name, "`zu {}` sends a reader to itself", c.name);
            }
        }
    }

    /// dx/12 §7 asks for a summary, a synopsis, examples, and a see-also
    /// on every subcommand. An entry missing one of them is help that
    /// looks generated, which is the thing that section is against.
    #[test]
    fn every_page_is_complete() {
        for c in COMMANDS {
            assert!(!c.summary.is_empty(), "{} has no summary", c.name);
            assert!(!c.synopsis.is_empty(), "{} has no synopsis", c.name);
            assert!(!c.examples.is_empty(), "{} has no examples", c.name);
            assert!(!c.see_also.is_empty(), "{} has no see also", c.name);
            for line in c.synopsis {
                assert!(
                    line.starts_with(&format!("zu {}", c.name)),
                    "{}: the synopsis `{line}` is some other command",
                    c.name
                );
            }
            for example in c.examples {
                assert!(
                    example.starts_with(&format!("zu {}", c.name)),
                    "{}: the example `{example}` is some other command",
                    c.name
                );
            }
        }
    }

    /// The JSON is what the reference pages are built from, so its shape
    /// is a contract: a reader looks up `commands` and expects the same
    /// five fields on every entry.
    #[test]
    fn the_json_carries_every_command_with_every_field() {
        let doc = zu_json::parse(&json("9.9.9")).expect("what we write, we can read");
        assert_eq!(doc.get("help_version").and_then(Json::as_u64), Some(1));
        assert_eq!(doc.get("zu").and_then(Json::as_str), Some("9.9.9"));
        let commands = doc
            .get("commands")
            .and_then(Json::as_arr)
            .expect("an array");
        assert_eq!(commands.len(), COMMANDS.len());
        for (entry, c) in commands.iter().zip(COMMANDS) {
            assert_eq!(entry.get("name").and_then(Json::as_str), Some(c.name));
            for field in ["summary", "synopsis", "examples", "see_also"] {
                assert!(entry.get(field).is_some(), "{}: no {field}", c.name);
            }
        }
        let codes = doc
            .get("exit_codes")
            .and_then(Json::as_arr)
            .expect("an array");
        assert_eq!(codes.len(), EXIT_CODES.len());
        assert_eq!(codes[0].get("code").and_then(Json::as_u64), Some(0));
    }

    /// A planned command that quietly became a real one would go on
    /// being advertised as missing, which is worse than not mentioning
    /// it, because the help then contradicts the tool.
    #[test]
    fn nothing_is_both_shipped_and_planned() {
        for name in PLANNED {
            assert!(
                find(name).is_none(),
                "`zu {name}` both ships and is planned"
            );
        }
    }
}
