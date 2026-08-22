//! The CLI as a product: what a user sees, pinned character for
//! character.
//!
//! dx/12 §7 asks for a snapshot suite over every subcommand's output,
//! for the reason a command-line tool is judged on it. Help that has
//! gone stale, a column that lost its alignment, a JSON field that
//! quietly changed name: a unit test on the function behind any of
//! those passes, and a user notices all three. So these tests run the
//! built binary and compare the whole transcript, stderr and exit code
//! included, against a committed file.
//!
//! Hand-rolled rather than trycmd or insta because what those add over
//! the hundred lines here is a file format and a diff, and the thing
//! this suite actually needs is the scrubbing below, which is what
//! separates a snapshot from a clock. `ZU_UPDATE_SNAPSHOTS=1 cargo test
//! -p zu-cli --test snapshots` rewrites every file, and the diff on the
//! way into the commit is the review.
//!
//! Nothing here holds text an operating system wrote. A missing file
//! reads one way on linux and another on Windows, so those cases are
//! asserted on their exit code at the bottom instead, where dx/12 §7
//! defines what the code has to be.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The binary under test, built by cargo before this runs.
const ZU: &str = env!("CARGO_BIN_EXE_zu");

/// What a text or path substitution is: what a run happened to print,
/// and the name this suite knows it by.
type Scrub = Vec<(String, &'static str)>;

#[test]
fn the_overview_lists_every_command_with_its_summary() {
    let mut out = String::new();
    run(&mut out, &["--help"], &[]);
    snapshot("help.txt", &out);
}

/// The versioned document dx/12 §7 calls a supported output, because
/// the CLI reference pages are generated from it. Its own file, since
/// what a reader of a diff here wants is the JSON and not a transcript
/// wrapped around it.
#[test]
fn the_json_help_is_the_whole_table() {
    let out = Command::new(ZU)
        .args(["--help", "--format", "json"])
        .output()
        .expect("the binary runs");
    assert!(out.status.success());
    snapshot(
        "help.json",
        &clean(&String::from_utf8(out.stdout).expect("utf-8"), &[]),
    );
}

/// Every subcommand's page, in one file, because the thing worth
/// reviewing is all of them together: that is where a summary written
/// in a different voice or a missing example shows up.
#[test]
fn every_command_has_a_page() {
    let mut out = String::new();
    for name in commands() {
        run(&mut out, &[&name, "--help"], &[]);
    }
    snapshot("pages.txt", &out);
}

/// Every subcommand called wrongly, which is the output a user is most
/// likely to meet and the one nobody looks at. dx/12 §7 puts every one
/// of these on exit 2.
///
/// A flag no command has is the one wrong call that means the same thing
/// to all of them, and it is the mistake a user makes: a typo, or a flag
/// that belongs to a neighbouring subcommand. A command that opened it
/// as a filename instead would report the operating system's words for a
/// name beginning with two dashes, which is both a worse message and one
/// this suite refuses to hold.
#[test]
fn every_command_says_how_it_should_have_been_called() {
    let mut out = String::new();
    for name in commands() {
        run(&mut out, &[&name, "--no-such-flag"], &[]);
    }
    run(&mut out, &["no-such-command"], &[]);
    run(&mut out, &["explain"], &[]);
    snapshot("usage.txt", &out);
}

/// A whole session over one graph: load it, look at it, ask it
/// something, and check it, in both formats where a command has two.
/// One transcript rather than a file per command, because the sequence
/// is the thing a reader is checking and half of these commands only
/// mean anything after `copy` has run.
#[test]
fn a_session_over_one_graph_prints_what_it_always_printed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let edges = dir.path().join("edges.txt");
    let graph = dir.path().join("graph.zu1");
    let second = dir.path().join("second.zu1");
    let keyed = dir.path().join("keyed.zu1");
    let csv = dir.path().join("edges.csv");
    std::fs::write(&edges, "1 2\n1 3\n2 3\n3 1\n").expect("write edges");
    let scrub: Scrub = vec![
        (path(&edges), "<edges>"),
        (path(&graph), "<graph>"),
        (path(&second), "<second>"),
        (path(&keyed), "<keyed>"),
        (path(&csv), "<csv>"),
    ];

    let mut out = String::new();
    let mut zu = |args: &[&str]| run(&mut out, args, &scrub);
    zu(&["copy", &path(&edges), &path(&graph)]);
    // Its own output, because `copy` refuses to write over a file that is
    // already there and the interesting line here is the JSON one.
    zu(&["copy", "--format", "json", &path(&edges), &path(&second)]);
    zu(&[
        "copy",
        "--reorder",
        "degree",
        &path(&edges),
        &path(&keyed),
        "--format",
        "json",
    ]);
    zu(&["stat", &path(&graph)]);
    zu(&["stat", &path(&graph), "--format", "json"]);
    zu(&["verify", &path(&graph)]);
    zu(&["verify", &path(&graph), "--format", "json"]);
    zu(&[
        "query",
        &path(&graph),
        "-c",
        "MATCH (n:node) RETURN count(n) AS nodes",
    ]);
    zu(&[
        "query",
        &path(&graph),
        "-c",
        "MATCH (a:node)-[:edge]->(b:node) RETURN a.id AS src, b.id AS dst ORDER BY src, dst",
    ]);
    zu(&[
        "query",
        &path(&graph),
        "-c",
        "MATCH (n:node) RETURN count(n) AS nodes",
        "--format",
        "json",
    ]);
    zu(&["query", &path(&graph), "-c", "MATCH (n:nope) RETURN n"]);
    zu(&["neighbors", &path(&graph), "1"]);
    zu(&["neighbors", "--in", &path(&graph), "3"]);
    zu(&["lookup", &path(&keyed), "3"]);
    zu(&["edge", &path(&graph), "1", "2"]);
    zu(&["edge", &path(&graph), "2", "1"]);
    zu(&["analyze", &path(&graph)]);
    zu(&["convert", &path(&edges), &path(&csv)]);
    zu(&["convert", &path(&edges), &path(&csv), "--format", "json"]);
    zu(&["version"]);
    zu(&["version", "--format", "json"]);
    snapshot("session.txt", &out);
}

/// dx/12 §7 fixes the exit codes, and they are the half of the
/// interface a script reads. Asserted rather than snapshotted, because
/// three of these carry an operating system's own words for what went
/// wrong and those are not the same words on two platforms.
#[test]
fn the_exit_codes_are_the_ones_the_spec_defines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let edges = dir.path().join("edges.txt");
    let graph = dir.path().join("graph.zu1");
    let torn = dir.path().join("torn.zu1");
    std::fs::write(&edges, "1 2\n2 3\n").expect("write edges");
    assert_eq!(code(&["copy", &path(&edges), &path(&graph)]), 0, "success");
    assert_eq!(
        code(&["query", &path(&graph), "-c", "RETURN 1 / 0"]),
        1,
        "a statement the engine refused is a query error"
    );
    assert_eq!(code(&["stat"]), 2, "no file named is a usage error");
    assert_eq!(
        code(&["stat", &path(&graph), "--format", "yaml"]),
        2,
        "a format the command does not have is a usage error"
    );
    assert_eq!(
        code(&["stat", &path(&dir.path().join("missing.zu1"))]),
        3,
        "a file that is not there is an io error"
    );

    // One byte of the file header flipped, which is inside what its crc
    // covers, so this is the damage every read of this file starts by
    // finding rather than one a particular query would have to reach.
    let mut bytes = std::fs::read(&graph).expect("read back");
    bytes[40] ^= 0xff;
    std::fs::write(&torn, &bytes).expect("write the damaged copy");
    assert_eq!(
        code(&["verify", &path(&torn)]),
        4,
        "a file that does not check out is corruption"
    );
    assert_eq!(
        code(&["verify", &path(&torn), "--format", "json"]),
        4,
        "and the format it was asked for does not change that"
    );
}

/// The JSON form of `verify` answers on stdout whether the file passed
/// or not, unlike every other command here, because the verdict is what
/// was asked for rather than a diagnostic about the answer. An auditor
/// recording a fleet reads one stream and gets both.
#[test]
fn a_failed_verify_reports_its_verdict_as_json_rather_than_as_a_diagnostic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let torn = dir.path().join("torn.zu1");
    std::fs::write(&torn, vec![0u8; 4096]).expect("write");
    let out = Command::new(ZU)
        .args(["verify", &path(&torn), "--format", "json"])
        .output()
        .expect("the binary runs");
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(text.starts_with("{\"file\":"), "{text}");
    assert!(text.contains("\"ok\":false"), "{text}");
    assert!(text.contains("\"error\":"), "{text}");
    assert!(
        out.stderr.is_empty(),
        "the verdict went to stdout and a copy of it went to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The revisions the snapshots scrub, asserted here instead, because a
/// build that reports the wrong C ABI sends a caller to compile against
/// the wrong header and the snapshot above cannot say so with the number
/// taken out of it.
#[test]
fn version_reports_the_revisions_this_build_actually_publishes() {
    let out = Command::new(ZU)
        .args(["version", "--format", "json"])
        .output()
        .expect("the binary runs");
    let doc = zu_json::parse(&String::from_utf8(out.stdout).expect("utf-8")).expect("json");
    for (field, want) in [
        ("zu", env!("CARGO_PKG_VERSION")),
        ("c_abi", zu::C_ABI_VERSION),
    ] {
        assert_eq!(
            doc.get(field).and_then(zu_json::Json::as_str),
            Some(want),
            "{field}"
        );
    }
}

/// The two spellings of the feature list say the same thing.
///
/// Which features a build carries depends on how cargo was invoked, so
/// the snapshots take the list out and no fixed list can be asserted
/// here either. What can be asserted is that the text and the JSON
/// agree, which is the part a reader of either one is relying on.
#[test]
fn the_two_spellings_of_the_feature_list_agree() {
    let json = Command::new(ZU)
        .args(["version", "--format", "json"])
        .output()
        .expect("the binary runs");
    let doc = zu_json::parse(&String::from_utf8(json.stdout).expect("utf-8")).expect("json");
    let from_json: Vec<&str> = doc
        .get("features")
        .and_then(zu_json::Json::as_arr)
        .expect("an array of features")
        .iter()
        .map(|f| f.as_str().expect("a feature name"))
        .collect();

    let text = Command::new(ZU)
        .arg("version")
        .output()
        .expect("the binary runs");
    let text = String::from_utf8(text.stdout).expect("utf-8");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("features: "))
        .expect("a features line");

    let want = if from_json.is_empty() {
        "none".to_owned()
    } else {
        from_json.join(", ")
    };
    assert_eq!(line, want);
}

/// Every command name, from the CLI itself, so a command that ships
/// without arriving in these snapshots is a failing test rather than a
/// gap nobody sees. The list is the `commands` array of the JSON help,
/// which the help module holds to the dispatch in `main`.
fn commands() -> Vec<String> {
    let out = Command::new(ZU)
        .args(["--help", "--format", "json"])
        .output()
        .expect("the binary runs");
    let doc = zu_json::parse(&String::from_utf8(out.stdout).expect("utf-8")).expect("json help");
    doc.get("commands")
        .and_then(zu_json::Json::as_arr)
        .expect("an array of commands")
        .iter()
        .map(|c| {
            c.get("name")
                .and_then(zu_json::Json::as_str)
                .expect("a name")
                .to_owned()
        })
        .collect()
}

/// Runs `zu` and appends the whole of what a user would have seen: the
/// command line, stdout, stderr marked so the two are told apart, and
/// the exit code whenever it is not zero.
fn run(out: &mut String, args: &[&str], scrub: &[(String, &'static str)]) {
    let output = Command::new(ZU)
        .args(args)
        .output()
        .expect("the binary runs");
    let mut line = String::from("$ zu");
    for arg in args {
        line.push(' ');
        // A quoted argument, for the two that carry spaces, so the
        // transcript is a line somebody could paste.
        if arg.contains(' ') {
            line.push_str(&format!("'{arg}'"));
        } else {
            line.push_str(arg);
        }
    }
    out.push_str(&clean(&line, scrub));
    out.push('\n');
    for (mark, stream) in [("", &output.stdout), ("! ", &output.stderr)] {
        let text = clean(&String::from_utf8_lossy(stream), scrub);
        for line in text.lines() {
            out.push_str(mark);
            out.push_str(line);
            out.push('\n');
        }
    }
    match output.status.code() {
        Some(0) => {}
        Some(code) => out.push_str(&format!("# exit {code}\n")),
        None => out.push_str("# killed\n"),
    }
    out.push('\n');
}

/// The exit code of one run, for a case whose text belongs to the
/// operating system.
fn code(args: &[&str]) -> i32 {
    Command::new(ZU)
        .args(args)
        .output()
        .expect("the binary runs")
        .status
        .code()
        .expect("a code rather than a signal")
}

/// The JSON spelling of the feature list, replaced by a placeholder.
///
/// The plain-text spelling is one line and is handled with the others.
/// This one is an array in the middle of a line, so it is found by its
/// key and cut at its bracket rather than by a rule that would have to
/// know what is in it.
fn scrub_features(text: &str) -> String {
    const KEY: &str = "\"features\":[";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        out.push_str(&rest[..at]);
        rest = &rest[at + KEY.len()..];
        match rest.find(']') {
            Some(end) => {
                out.push_str("\"features\":<features>");
                rest = &rest[end + 1..];
            }
            // An unterminated array is not this function's to repair.
            None => {
                out.push_str(KEY);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A path as an argument. Every path in these tests is under a temp
/// directory, so it is scrubbed out of the transcript again on the way
/// in; this is the one place that decides how it was spelled.
fn path(p: &Path) -> String {
    p.display().to_string()
}

/// Everything that differs between two runs of the same command, out.
///
/// The paths are the caller's, since only the caller knows which
/// temporary file was which. The rest is here: the version, which moves
/// every release, the uuid, which is new in every file, and every
/// decimal number.
///
/// A decimal in this tool's output is a measurement or a rounded size.
/// The measurements are a clock and belong in no snapshot; the rounded
/// sizes are derived from counts that the JSON forms print exactly and
/// this suite pins there. One rule covers all of them, because a rule
/// per line is a rule that misses the line somebody adds next month and
/// leaves a suite that fails once a week for no reason.
fn clean(text: &str, scrub: &[(String, &'static str)]) -> String {
    let mut text = text.to_owned();
    for (from, to) in scrub {
        text = text.replace(from.as_str(), to);
    }
    text = text.replace(env!("CARGO_PKG_VERSION"), "<version>");
    // The C ABI revision is a decimal that is not a measurement, so it is
    // named here rather than left to the pass at the bottom. Named in
    // both spellings it is printed in, rather than replaced everywhere it
    // occurs, because a plain substring rule would eat a timing that
    // happened to read 0.5 seconds.
    text = text.replace(
        &format!("\"c_abi\":\"{}\"", zu::C_ABI_VERSION),
        "\"c_abi\":\"<abi>\"",
    );
    // Which optional features the binary carries is a property of how
    // cargo was invoked, not of the code under test. A snapshot that
    // pinned it passed on the machine it was taken on and failed on
    // every machine that built the workspace a different way, which is
    // the one thing a snapshot suite must not do.
    text = scrub_features(&text);
    // `lines` drops the last newline, and a transcript that lost one is
    // a difference on every line after it.
    let ended = text.ends_with('\n');
    text = text
        .lines()
        .map(|line| {
            if line.starts_with("uuid:") {
                "uuid:            <uuid>".to_owned()
            } else if line.starts_with("features: ") {
                "features: <features>".to_owned()
            } else if line == format!("c abi {}", zu::C_ABI_VERSION) {
                "c abi <abi>".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if ended {
        text.push('\n');
    }
    decimals(&text)
}

/// Every decimal number, including the exponent form a very small float
/// prints as, replaced by `<n>`. A bare integer is left alone: those are
/// counts and byte totals, which are exactly what these snapshots are
/// for.
fn decimals(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        // Runs of non-digits move across whole, which is also what keeps
        // this safe on text that is not ascii: a digit byte never
        // appears inside a multi-byte character.
        if !bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // A point and its digits, an exponent, or both. Either one on
        // its own makes the run a measurement rather than a count,
        // which matters because the writer behind the JSON output
        // drops the point entirely once a number is small enough: a
        // parse that took nine hundredths of a millisecond prints as
        // 9e-5 and the digits in front of the e are all there is of it.
        let mut measured = false;
        if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            measured = true;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        if bytes.get(i) == Some(&b'e') {
            let mut at = i + 1;
            if matches!(bytes.get(at), Some(b'-' | b'+')) {
                at += 1;
            }
            let digits = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            // An exponent is digits and then something that is not a
            // letter or a digit. A revision hash reads as a number and
            // an e and more of itself, and it is not a measurement.
            if at > digits && !bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) {
                measured = true;
                i = at;
            }
        }
        if !measured {
            out.push_str(&text[start..i]);
            continue;
        }
        out.push_str("<n>");
    }
    out
}

/// Compares against the committed file, or writes it when
/// `ZU_UPDATE_SNAPSHOTS` is set.
fn snapshot(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed == actual {
        return;
    }
    if std::env::var_os("ZU_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a writable tests dir");
        std::fs::write(&path, actual).expect("a writable snapshot");
        return;
    }
    panic!(
        "{} is not what the CLI prints. Read the difference, and if the new output is the \
         intended one, `ZU_UPDATE_SNAPSHOTS=1 cargo test -p zu-cli --test snapshots` writes \
         it.\n\n--- committed\n{committed}\n--- printed\n{actual}",
        path.display()
    );
}

/// The scrubber is what makes these snapshots hold on a machine of a
/// different speed, so what it does and does not take out is worth a
/// test of its own. The exponent cases are the ones that took a build
/// down: a fast enough parse prints with no decimal point in it at all.
#[test]
fn a_measurement_is_scrubbed_and_a_count_is_not() {
    for (text, want) in [
        (
            "copied 4 edges, 4 nodes, 1 groups",
            "copied 4 edges, 4 nodes, 1 groups",
        ),
        ("2359296 bytes on disk", "2359296 bytes on disk"),
        ("total 0.0123s", "total <n>s"),
        ("\"parse_secs\":9e-5", "\"parse_secs\":<n>"),
        ("\"parse_secs\":9.5e-5", "\"parse_secs\":<n>"),
        ("\"edges_per_sec\":1.2e+7", "\"edges_per_sec\":<n>"),
        (
            "2 M edges/s and 4e2f1a is a revision",
            "2 M edges/s and 4e2f1a is a revision",
        ),
        ("version 1e", "version 1e"),
    ] {
        assert_eq!(decimals(text), want, "scrubbing {text}");
    }
}
