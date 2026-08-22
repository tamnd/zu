//! Drives the real `zu shell` binary over pipes, the way a harness
//! keeps it alive: one request line in, one JSON line out, across a
//! mix of bare statements, frames, and errors, on a single process.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn seeded(path: &std::path::Path) {
    let mut db = zu::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    edges.sort_unstable();
    edges.dedup();
    zu::zu1::graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
}

#[test]
fn one_process_serves_statements_frames_and_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shell.zu1");
    seeded(&path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg(&path)
        .arg("--format")
        .arg("jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut lines = stdout.lines();
    // The greeting comes before anything is asked, which is how a
    // client learns whether it speaks this version at all.
    let hello = lines.next().expect("a greeting").expect("read");
    assert!(
        hello.starts_with("{\"protocol\":1,\"zu\":\""),
        "got {hello}"
    );
    assert!(hello.contains("\"c_abi\":"), "got {hello}");
    assert!(hello.contains("\"features\":["), "got {hello}");
    // The name only: a path is spelled with backslashes on Windows and
    // JSON escapes those, so the whole string is not the same string.
    assert!(hello.contains("\"file\":\""), "got {hello}");
    assert!(hello.contains("shell.zu1"), "got {hello}");
    let mut ask = |line: &str| -> String {
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
        lines.next().expect("a response line").expect("read")
    };

    // And asking for it again gives the same object, for the client
    // that attached to a session somebody else started.
    assert_eq!(ask(r#"{"op":"hello"}"#), hello);

    // A bare statement, folded onto one line the way a client sends it.
    let r = ask(r"MATCH (a:person {id: 3})-[:follows]->(b)\nRETURN count(b) AS n");
    assert!(
        r.starts_with("{\"gqlstatus\":\"00000\",\"columns\":[\"n\"],\"rows\":[["),
        "got {r}"
    );

    // A frame with parameters; same session, same process.
    let r = ask(
        r#"{"op":"query","q":"MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n","params":{"src":3}}"#,
    );
    assert!(
        r.starts_with("{\"gqlstatus\":\"00000\",\"columns\":[\"n\"],\"rows\":[["),
        "got {r}"
    );

    // Prepare once, execute with different bindings, close.
    let r = ask(r#"{"op":"prepare","q":"MATCH (a:person {id: $src}) RETURN a.id AS id"}"#);
    assert_eq!(r, "{\"stmt\":1,\"params\":[\"src\"]}");
    let r = ask(r#"{"op":"execute","stmt":1,"params":{"src":5}}"#);
    assert_eq!(
        r,
        "{\"gqlstatus\":\"00000\",\"columns\":[\"id\"],\"rows\":[[5]]}"
    );
    let r = ask(r#"{"op":"execute","stmt":1,"params":{"src":9}}"#);
    assert_eq!(
        r,
        "{\"gqlstatus\":\"00000\",\"columns\":[\"id\"],\"rows\":[[9]]}"
    );

    // A missing binding is an error line, and the loop survives it.
    let r = ask(r#"{"op":"execute","stmt":1}"#);
    assert!(r.contains("\"error\""), "got {r}");
    assert!(r.contains("missing parameter"), "got {r}");
    let r = ask(r#"{"op":"close_stmt","stmt":1}"#);
    assert_eq!(r, "{\"closed\":true}");
    let r = ask(r#"{"op":"close_stmt","stmt":1}"#);
    assert_eq!(r, "{\"closed\":false}");

    // An explain frame answers with the plan and no rows. The listing
    // names operators and carries no counters: a caller recording a
    // plan beside a latency it measured separately must not be paying
    // for an execution to get it, and a counter in here would be proof
    // that it did.
    let r = ask(r#"{"op":"explain","q":"MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n"}"#);
    assert!(r.starts_with("{\"text\":\""), "got {r}");
    assert!(r.contains("Aggregate"), "got {r}");
    assert!(r.contains("ScanNodes"), "got {r}");
    assert!(
        !r.contains("rows"),
        "the plan was measured, not compiled: {r}"
    );
    assert!(!r.contains("\"columns\""), "got {r}");

    // The same statement through explain_analyze does run, and says so
    // with the counters this shell's other explain refuses to invent.
    let r = ask(
        r#"{"op":"explain_analyze","q":"MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n"}"#,
    );
    assert!(r.contains("rows"), "got {r}");

    // A statement that does not compile fails the explain the same way
    // it would fail a query, with a code rather than a bare message.
    let r = ask(r#"{"op":"explain","q":"MATCH (a:person"}"#);
    assert!(r.contains("\"failure\""), "got {r}");
    assert!(r.contains("42001"), "got {r}");

    // A failure carries the whole diagnostic record ISO 23.2 asks for
    // and not just the code, because a client that has to find the
    // offending name inside an English sentence is parsing prose.
    let r = ask(r#"{"op":"query","q":"MATCH (a:person) RETURN b.id AS id"}"#);
    assert!(r.contains(r#""subject_kind":"variable""#), "got {r}");
    assert!(r.contains(r#""subject":"b""#), "got {r}");
    assert!(r.contains(r#""graph":"home""#), "got {r}");
    assert!(r.contains(r#""schema":"/""#), "got {r}");

    // A condition raised at a token says where, counted the three ways
    // a caller needs, and quotes the line it happened on.
    let r = ask(r#"{"op":"query","q":"MATCH (a:person) RETURN a.id AS id ORDR BY a.id"}"#);
    assert!(r.contains(r#""line":1,"column":36,"offset":35"#), "got {r}");
    assert!(r.contains(r#""excerpt":"MATCH"#), "got {r}");

    // A condition about nothing named writes no empty field for it,
    // since a null subject reads as a condition about nothing rather
    // than as a record with no opinion.
    let r = ask(r#"{"op":"query","q":"RETURN 1 / 0 AS v"}"#);
    assert!(r.contains("22012"), "got {r}");
    assert!(!r.contains("\"subject\""), "got {r}");
    assert!(!r.contains("\"line\""), "got {r}");

    // Broken input gets an error line too, then real work continues.
    let r = ask(r#"{"op":"query","q":"#);
    assert!(r.contains("bad frame"), "got {r}");
    let r = ask("MATCH (a:person) RETURN count(a) AS n");
    assert_eq!(
        r,
        "{\"gqlstatus\":\"00000\",\"columns\":[\"n\"],\"rows\":[[97]]}"
    );

    // A list parameter goes in as a list, which is what makes the
    // corpus's IN predicates expressible over the wire.
    let r = ask(
        r#"{"op":"query","q":"MATCH (a:person) WHERE a.id IN $ids RETURN count(a) AS n","params":{"ids":[1,2,3]}}"#,
    );
    assert_eq!(
        r,
        "{\"gqlstatus\":\"00000\",\"columns\":[\"n\"],\"rows\":[[3]]}"
    );

    // An unknown op is a protocol fault and says so without inventing a
    // condition code for it.
    let r = ask(r#"{"op":"sing"}"#);
    assert!(r.contains("unknown op"), "got {r}");
    assert!(!r.contains("gqlstatus"), "got {r}");

    let r = ask(r#"{"op":"quit"}"#);
    assert_eq!(r, "{\"bye\":true}");
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

/// The verb with no file is a session on a database in memory, which
/// is what somebody trying the language out wants: nothing to make
/// first and nothing left in the working directory after.
#[test]
fn bare_shell_opens_a_database_in_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg("--format")
        .arg("jsonl")
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\":memory:\""), "got {stdout}");
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read the directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(left.is_empty(), "nothing is left behind, found {left:?}");

    let help = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("--help")
        .output()
        .expect("help");
    assert!(String::from_utf8_lossy(&help.stdout).contains("shell"));
}

/// A flag nobody has is still a usage line rather than a session. A
/// mode probe runs the bare verb and takes "unknown command" to mean
/// the verb does not exist, so a usage line must not read as that.
#[test]
fn a_flag_nobody_has_prints_usage_not_unknown_command() {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg("--nope")
        .output()
        .expect("run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage:"), "got {stderr}");
    assert!(!stderr.contains("unknown command"), "got {stderr}");
}

/// And the name every embedded database spells it with means the same
/// thing as leaving it out.
#[test]
fn the_memory_name_is_the_same_as_no_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg(":memory:")
        .arg("--format")
        .arg("jsonl")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    lines.next().expect("greeting").expect("greeting");
    writeln!(stdin, "INSERT (p:person {{uid: 1, name: 'ada'}})").expect("write");
    lines.next().expect("insert").expect("insert");
    writeln!(stdin, "MATCH (p:person) RETURN p.name AS name").expect("write");
    let read = lines.next().expect("read").expect("read");
    assert!(read.contains("ada"), "got {read}");
    drop(stdin);
    assert!(child.wait().expect("wait").success());
    assert!(
        std::fs::read_dir(dir.path())
            .expect("read the directory")
            .next()
            .is_none(),
        "no file called :memory: is left behind"
    );
}

/// A bare statement goes through the fold and a frame does not, so the
/// two paths have to answer a string the same way. They did not: the
/// fold ran over the whole line including inside the quotes, so
/// `'a\\b'` reached the engine as `'a\b'` and came back as a backspace,
/// and `'\\'` reached it as `'\'` and came back as a string nothing
/// closes. The frame path was right the whole time, which is why this
/// asks both and compares them rather than pinning one.
#[test]
fn a_string_reads_the_same_bare_as_it_does_in_a_frame() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .arg("--format")
        .arg("jsonl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    lines.next().expect("greeting").expect("greeting");
    let mut ask = |line: &str| -> String {
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
        lines.next().expect("a response line").expect("read")
    };

    for (bare, framed) in [
        (
            r"RETURN 'a\\b' AS v",
            r#"{"op":"query","q":"RETURN 'a\\\\b' AS v"}"#,
        ),
        (
            r"RETURN '\\' AS v",
            r#"{"op":"query","q":"RETURN '\\\\' AS v"}"#,
        ),
        (
            r"RETURN 'a\nb' AS v",
            r#"{"op":"query","q":"RETURN 'a\\nb' AS v"}"#,
        ),
        (
            r"RETURN @'a\b' AS v",
            r#"{"op":"query","q":"RETURN @'a\\b' AS v"}"#,
        ),
        (
            r"RETURN 'it''s' AS v",
            r#"{"op":"query","q":"RETURN 'it''s' AS v"}"#,
        ),
        (
            r"RETURN X'01AF' AS v",
            r#"{"op":"query","q":"RETURN X'01AF' AS v"}"#,
        ),
    ] {
        let from_bare = ask(bare);
        let from_frame = ask(framed);
        assert_eq!(from_bare, from_frame, "{bare} against {framed}");
        assert!(
            from_bare.contains("\"rows\""),
            "{bare} answered {from_bare}"
        );
    }

    // And the fold still does its job around them, which is the half a
    // fix that simply stopped folding would have broken.
    let folded = ask(r"RETURN\n'a\\b' AS v");
    assert!(folded.contains(r#"[["a\\b"]]"#), "got {folded}");
    // A comment folded onto the line ends where the fold says it does,
    // apostrophe and all, rather than swallowing the statement after
    // it.
    assert_eq!(ask(r"// it's folded\nRETURN 'a\\b' AS v"), folded);
    assert_eq!(ask(r"-- it's folded\nRETURN 'a\\b' AS v"), folded);

    drop(stdin);
    assert!(child.wait().expect("wait").success());
}
