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
    let mut ask = |line: &str| -> String {
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
        lines.next().expect("a response line").expect("read")
    };

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

    // Broken input gets an error line too, then real work continues.
    let r = ask(r#"{"op":"query","q":"#);
    assert!(r.contains("bad frame"), "got {r}");
    let r = ask("MATCH (a:person) RETURN count(a) AS n");
    assert_eq!(
        r,
        "{\"gqlstatus\":\"00000\",\"columns\":[\"n\"],\"rows\":[[97]]}"
    );

    let r = ask(r#"{"op":"quit"}"#);
    assert_eq!(r, "{\"bye\":true}");
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn bare_shell_prints_usage_not_unknown_command() {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("shell")
        .output()
        .expect("run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    // A mode probe runs the bare verb and takes "unknown command" to
    // mean the verb does not exist; a usage line must not read as that.
    assert!(stderr.contains("usage:"), "got {stderr}");
    assert!(!stderr.contains("unknown command"), "got {stderr}");
    let help = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("--help")
        .output()
        .expect("help");
    assert!(String::from_utf8_lossy(&help.stdout).contains("shell"));
}
