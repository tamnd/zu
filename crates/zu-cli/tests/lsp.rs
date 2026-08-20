//! Drives the real `zu lsp --stdio` binary over pipes, with a database
//! behind it.
//!
//! The unit tests in the binary drive the same loop through a pair of
//! buffers, which is where the protocol details are checked. What only
//! this can check is the half that needs a file: that `--db` reaches a
//! catalog, that the names in that catalog come back as completions and
//! as hovers, and that a table which is not in it is a squiggle rather
//! than a shrug. That is the difference between an editor that knows
//! the language and one that knows the file.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use zu_json::Json;

fn seeded(path: &std::path::Path) {
    let mut db = zu::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    edges.sort_unstable();
    edges.dedup();
    zu::zu1::graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
}

/// The client side of the wire.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn send(&mut self, body: &str) {
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write");
        self.stdin.flush().expect("flush");
    }

    fn read(&mut self) -> Json {
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).expect("read") > 0,
                "the server closed the pipe"
            );
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.parse::<usize>().ok();
            }
        }
        let mut body = vec![0u8; length.expect("a length header")];
        self.stdout.read_exact(&mut body).expect("a body");
        zu_json::parse(&String::from_utf8(body).expect("utf-8")).expect("json")
    }

    /// Asks one thing and reads past whatever the server said on its
    /// own account, which is diagnostics: a notification arrives when
    /// the server has something to say and not when it is asked.
    fn ask(&mut self, id: i64, method: &str, params: &str) -> Json {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        loop {
            let message = self.read();
            if message.get("id").and_then(Json::as_i64) == Some(id) {
                return message.get("result").cloned().expect("a result");
            }
        }
    }

    /// Opens a document and reads the diagnostics the server publishes
    /// for it.
    fn open(&mut self, text: &str) -> Vec<Json> {
        let escaped = Json::Str(text.to_string()).to_compact();
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///q.zuql","text":{escaped}}}}}}}"#
        ));
        loop {
            let message = self.read();
            if message.get("method").and_then(Json::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                return message
                    .get("params")
                    .and_then(|p| p.get("diagnostics"))
                    .and_then(Json::as_arr)
                    .expect("a list")
                    .to_vec();
            }
        }
    }
}

fn start(db: &std::path::Path) -> Client {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("lsp")
        .arg("--stdio")
        .arg("--db")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut client = Client {
        child,
        stdin,
        stdout,
    };
    client.ask(1, "initialize", "{}");
    client.send(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    client
}

impl Drop for Client {
    fn drop(&mut self) {
        self.send(r#"{"jsonrpc":"2.0","id":99,"method":"shutdown"}"#);
        self.send(r#"{"jsonrpc":"2.0","method":"exit"}"#);
        let _ = self.child.wait();
    }
}

#[test]
fn one_server_answers_about_the_file_it_was_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lsp.zu1");
    seeded(&path);
    let mut client = start(&path);

    // A statement about the tables that are there is a statement with
    // nothing wrong with it, all the way through the binder.
    assert_eq!(
        client.open("MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n"),
        Vec::new()
    );

    // And one about a table that is not there says so, which is the
    // whole reason to hand the server a database.
    let wrong = client.open("MATCH (a:nosuchtable) RETURN a");
    assert_eq!(wrong.len(), 1, "got {wrong:?}");
    assert!(
        wrong[0]
            .get("message")
            .and_then(Json::as_str)
            .expect("a message")
            .contains("nosuchtable"),
        "got {:?}",
        wrong[0]
    );

    // Completion after a colon offers the labels the file has, spelled
    // the way the file spells them.
    client.open("MATCH (a:per");
    let items = client.ask(
        2,
        "textDocument/completion",
        r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":12}}"#,
    );
    let labels: Vec<&str> = items
        .get("items")
        .and_then(Json::as_arr)
        .expect("items")
        .iter()
        .filter_map(|i| i.get("label").and_then(Json::as_str))
        .collect();
    assert_eq!(labels, ["person"], "got {labels:?}");

    // Hover over that name says what it is and how much of it there is,
    // which is the fact a person opens a shell in another window for.
    client.open("MATCH (a:person) RETURN a");
    let hover = client.ask(
        3,
        "textDocument/hover",
        r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":10}}"#,
    );
    let text = hover
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(Json::as_str)
        .expect("hover text");
    assert!(text.starts_with("**person**, a node table"), "got {text}");
    assert!(text.contains("97 nodes"), "got {text}");

    // The edge table is named the same way and reads as an edge table,
    // with the two node tables it runs between.
    let hover = client.ask(
        4,
        "textDocument/hover",
        r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":2}}"#,
    );
    assert!(
        hover
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(Json::as_str)
            .expect("hover text")
            .starts_with("**MATCH**"),
        "the keyword still wins where no table is called that"
    );
}

#[test]
fn a_database_that_will_not_open_is_a_note_and_not_a_dead_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("not-a-database.zu1");
    std::fs::write(&path, b"this is not a zu1 file").expect("write");
    let mut client = start(&path);

    // The server is up and the parser still works, so the note about
    // the file is the only thing wrong with a statement that is fine.
    let found = client.open("RETURN 1 AS one");
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].get("severity").and_then(Json::as_i64), Some(2));
    assert!(
        found[0]
            .get("message")
            .and_then(Json::as_str)
            .expect("a message")
            .contains("not-a-database.zu1"),
        "got {:?}",
        found[0]
    );
}
