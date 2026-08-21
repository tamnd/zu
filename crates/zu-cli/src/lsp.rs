//! `zu lsp --stdio`: the language server, in the same binary.
//!
//! An editor that understands zuQL is the difference between a language
//! you can use and a language you have to remember, and dx/13 §1 asks
//! for one. The decision worth writing down is that it is this binary
//! and not another: a server shipped separately drifts from the engine
//! it is meant to describe, and the day it drifts is the day it tells
//! somebody a table exists that does not. Here the completion list comes
//! out of the same catalog the shell reads, the diagnostics come out of
//! the same front end that would refuse the statement, and the colours
//! come out of the same scanner the prompt paints with. There is one
//! answer to every question because there is one implementation of it.
//!
//! Five things are served, which are the five an editor is judged on.
//! Diagnostics say whether the statement is a statement, from the parser
//! alone when no database was named and from the binder as well when one
//! was, so a misspelled table is a squiggle rather than a surprise at
//! run time. Completion offers what the file has. Hover says what a name
//! is. Formatting lays the text out. Semantic tokens colour it. Nothing
//! here runs a statement: an editor that ran what was in the buffer as
//! the buffer was typed would be an editor that deleted a graph on the
//! way to writing `MATCH`.
//!
//! The transport is the standard one, headers and a JSON body over
//! standard input and output, written out by hand on [`zu_json`] the
//! way the argument parsing is written out by hand. T7 caps this binary
//! at 15 MiB and a JSON-RPC framework is not worth a megabyte of it.
//!
//! [`serve`] takes a reader and a writer rather than reaching for the
//! process's own, which is what lets the tests below drive a whole
//! session through a pair of buffers and read the answers back. A
//! protocol implementation nobody can test without an editor open is a
//! protocol implementation nobody tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;

use zu::session::Session;
use zu::{DiagnosticRecord, Severity};
use zu_json::Json;

use crate::complete::{self, Names, What};
use crate::highlight::{self, Kind};
use crate::{format, meta};

/// The token kinds this server colours with, in the order the protocol
/// numbers them: a token's type is its index in this list.
///
/// All eight are names the protocol already defines, so an editor's
/// theme colours them without being told anything. A server that
/// invented its own names would come out uncoloured everywhere except
/// where somebody had written a theme for it.
const TOKEN_TYPES: &[&str] = &[
    "keyword",
    "string",
    "number",
    "comment",
    "parameter",
    "variable",
    "type",
    "property",
];

/// How the client counts a column.
///
/// The protocol's own default is UTF-16, which is a number that means
/// something to an editor written in JavaScript and nothing to anybody
/// else. A client that says it can read UTF-8 gets UTF-8, because then
/// a column is a byte offset and no conversion happens at all, which on
/// a document being retyped a character at a time is the difference
/// between free and a walk over the text per keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16 => "utf-16",
            Encoding::Utf32 => "utf-32",
        }
    }

    /// How many units this character is, in whatever the client counts.
    fn units(self, c: char) -> usize {
        match self {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
            Encoding::Utf32 => 1,
        }
    }
}

/// `zu lsp --stdio`.
pub(crate) fn lsp_command(args: &[String]) -> ExitCode {
    let mut stdio = false;
    let mut db: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stdio" => {
                stdio = true;
                i += 1;
            }
            "--db" => match args.get(i + 1) {
                Some(path) if !path.starts_with('-') => {
                    db = Some(path);
                    i += 2;
                }
                _ => return crate::usage_error("lsp"),
            },
            _ => return crate::usage_error("lsp"),
        }
    }
    // The flag is required rather than assumed. There is one transport
    // today and there will be others, and a client that asked for a
    // socket and silently got a pipe would hang rather than fail.
    if !stdio {
        return crate::usage_error("lsp");
    }

    let mut server = Server::new();
    if let Some(path) = db {
        server.attach(path);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        &mut server,
        &mut BufReader::new(stdin.lock()),
        &mut stdout.lock(),
    )
}

/// One open document, and everything known about the file behind it.
struct Server {
    /// The database, read only, or nothing. Read only because an editor
    /// is a reader: a server that took the write side would lock the
    /// file the user is about to open a shell on, and it has nothing to
    /// write anyway.
    db: Option<Session>,
    /// What went wrong opening it, said once, in the first diagnostics
    /// this publishes. A server that cannot reach the database is still
    /// worth running, because the parser does not need it, so this is a
    /// note and not a reason to exit.
    trouble: Option<String>,
    names: Names,
    /// Hover text for every name in the catalog, by the spelling the
    /// file uses.
    docs: HashMap<String, String>,
    /// The text of every document the client has open, which is the
    /// only copy either side trusts: the file on disk is whatever was
    /// last saved and the buffer is what the user is looking at.
    open: HashMap<String, String>,
    encoding: Encoding,
    /// Whether the client asked to shut down before it said exit, which
    /// is the difference between a clean exit and a client that dropped
    /// the connection.
    farewell: bool,
}

impl Server {
    fn new() -> Server {
        Server {
            db: None,
            trouble: None,
            names: Names::default(),
            docs: HashMap::new(),
            open: HashMap::new(),
            encoding: Encoding::Utf16,
            farewell: false,
        }
    }

    /// Opens the database the client named, and reads the catalog out
    /// of it. A failure is kept rather than raised: the editor stays up
    /// and answers what it can.
    fn attach(&mut self, path: &str) {
        let opened = zu::zu1::file::Zu1File::open_read_only(std::path::Path::new(path))
            .and_then(Session::on);
        match opened {
            Ok(session) => {
                self.db = Some(session);
                self.trouble = None;
                self.reread();
            }
            Err(e) => {
                self.db = None;
                self.trouble = Some(format!("cannot read {path}: {e}"));
            }
        }
    }

    /// Reads the catalog again, which is what the names and the hovers
    /// are built from.
    ///
    /// Done on every save as well as on the open. Nothing the editor
    /// does changes the catalog, but the shell in the next window is
    /// free to, and a save is the moment a user is most likely to have
    /// just done it.
    fn reread(&mut self) {
        let Some(session) = self.db.as_mut() else {
            return;
        };
        self.names = crate::repl::names(session);
        self.docs = catalog_docs(session);
    }
}

/// Reads requests until the client stops sending them.
///
/// The exit code is the protocol's: a client that said shutdown and
/// then exit gets a clean one, and a connection that ended any other
/// way gets a one, because an editor that crashed and an editor that
/// left are different events and only the second is fine.
fn serve(server: &mut Server, input: &mut impl BufRead, out: &mut impl Write) -> ExitCode {
    while let Some(raw) = read_message(input) {
        let Ok(message) = zu_json::parse(&raw) else {
            // No id to answer against, since the id was in the message
            // that would not parse. The protocol's own answer to that
            // is a null id, which is the one place it is allowed.
            send(out, &error(Json::Null, -32700, "the request is not JSON"));
            continue;
        };
        let method = message.get("method").and_then(Json::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Json::Null);
        if method == "exit" {
            return match server.farewell {
                true => ExitCode::SUCCESS,
                false => ExitCode::from(1),
            };
        }
        match message.get("id").cloned() {
            Some(id) => match request(server, method, &params, out) {
                Ok(result) => send(out, &reply(id, result)),
                Err((code, message)) => send(out, &error(id, code, &message)),
            },
            None => notice(server, method, &params, out),
        }
    }
    // Standard input closing without an exit is the editor going away
    // in a way it did not plan for.
    match server.farewell {
        true => ExitCode::SUCCESS,
        false => ExitCode::from(1),
    }
}

/// Answers one request, as the result or as an error to report against
/// its id.
fn request(
    server: &mut Server,
    method: &str,
    params: &Json,
    out: &mut impl Write,
) -> Result<Json, (i64, String)> {
    match method {
        "initialize" => Ok(initialize(server, params)),
        "shutdown" => {
            server.farewell = true;
            Ok(Json::Null)
        }
        "textDocument/completion" => Ok(completion(server, params)),
        "textDocument/hover" => Ok(hover(server, params)),
        "textDocument/formatting" => Ok(formatting(server, params)),
        "textDocument/semanticTokens/full" => Ok(semantic_tokens(server, params)),
        _ => {
            let _ = out;
            Err((-32601, format!("no method {method}")))
        }
    }
}

/// Takes one notification. Nothing is answered and nothing may fail:
/// the client is not listening for a reply and has nowhere to put an
/// error, so an unknown notification is dropped the way the protocol
/// says to drop it.
fn notice(server: &mut Server, method: &str, params: &Json, out: &mut impl Write) {
    match method {
        "textDocument/didOpen" => {
            let doc = params.get("textDocument");
            let (Some(uri), Some(text)) = (
                doc.and_then(|d| d.get("uri")).and_then(Json::as_str),
                doc.and_then(|d| d.get("text")).and_then(Json::as_str),
            ) else {
                return;
            };
            server.open.insert(uri.to_string(), text.to_string());
            publish(server, uri, out);
        }
        "textDocument/didChange" => {
            let Some(uri) = uri_of(params) else {
                return;
            };
            // Full document sync, which is what the capabilities asked
            // for, so the last change in the list is the document. An
            // incremental sync would buy nothing here: everything this
            // server answers reads the whole text anyway.
            let Some(text) = params
                .get("contentChanges")
                .and_then(Json::as_arr)
                .and_then(|changes| changes.last())
                .and_then(|change| change.get("text"))
                .and_then(Json::as_str)
            else {
                return;
            };
            server.open.insert(uri.clone(), text.to_string());
            publish(server, &uri, out);
        }
        "textDocument/didSave" => {
            let Some(uri) = uri_of(params) else {
                return;
            };
            server.reread();
            publish(server, &uri, out);
        }
        "textDocument/didClose" => {
            let Some(uri) = uri_of(params) else {
                return;
            };
            server.open.remove(&uri);
            // An empty list is how a squiggle is taken off a file that
            // is no longer on the screen. Saying nothing leaves it
            // there, in the problems panel, on a file nobody has open.
            send(
                out,
                &notification(
                    "textDocument/publishDiagnostics",
                    obj(vec![
                        ("uri", Json::Str(uri)),
                        ("diagnostics", Json::Arr(Vec::new())),
                    ]),
                ),
            );
        }
        _ => {}
    }
}

/// What this server can do, and what it needs from the client.
fn initialize(server: &mut Server, params: &Json) -> Json {
    // The client lists what it can count columns in, in its order of
    // preference, and the first one both sides know wins. A client that
    // says nothing gets UTF-16, which is what the protocol says it
    // means by silence.
    server.encoding = params
        .get("general")
        .and_then(|g| g.get("positionEncodings"))
        .and_then(Json::as_arr)
        .and_then(|list| {
            list.iter().find_map(|e| match e.as_str() {
                Some("utf-8") => Some(Encoding::Utf8),
                Some("utf-16") => Some(Encoding::Utf16),
                Some("utf-32") => Some(Encoding::Utf32),
                _ => None,
            })
        })
        .unwrap_or(Encoding::Utf16);
    // A database on the command line wins, because it was typed. This
    // is for the extension, which knows the workspace and does not get
    // to write the command line.
    if server.db.is_none()
        && server.trouble.is_none()
        && let Some(path) = params
            .get("initializationOptions")
            .and_then(|o| o.get("database"))
            .and_then(Json::as_str)
    {
        server.attach(path);
    }
    obj(vec![
        (
            "capabilities",
            obj(vec![
                ("positionEncoding", Json::Str(server.encoding.name().into())),
                (
                    "textDocumentSync",
                    obj(vec![
                        ("openClose", Json::Bool(true)),
                        ("change", Json::Int(1)),
                        ("save", obj(vec![("includeText", Json::Bool(false))])),
                    ]),
                ),
                (
                    "completionProvider",
                    obj(vec![
                        (
                            "triggerCharacters",
                            Json::Arr(
                                [":", ".", "$"]
                                    .iter()
                                    .map(|c| Json::Str((*c).into()))
                                    .collect(),
                            ),
                        ),
                        ("resolveProvider", Json::Bool(false)),
                    ]),
                ),
                ("hoverProvider", Json::Bool(true)),
                ("documentFormattingProvider", Json::Bool(true)),
                (
                    "semanticTokensProvider",
                    obj(vec![
                        (
                            "legend",
                            obj(vec![
                                (
                                    "tokenTypes",
                                    Json::Arr(
                                        TOKEN_TYPES
                                            .iter()
                                            .map(|t| Json::Str((*t).into()))
                                            .collect(),
                                    ),
                                ),
                                ("tokenModifiers", Json::Arr(Vec::new())),
                            ]),
                        ),
                        ("full", Json::Bool(true)),
                    ]),
                ),
            ]),
        ),
        (
            "serverInfo",
            obj(vec![
                ("name", Json::Str("zu".into())),
                ("version", Json::Str(crate::VERSION.into())),
            ]),
        ),
    ])
}

/// Publishes what is wrong with a document, which is nothing most of
/// the time and is published anyway: an empty list is how the last
/// squiggle comes off.
fn publish(server: &mut Server, uri: &str, out: &mut impl Write) {
    let Some(text) = server.open.get(uri).cloned() else {
        return;
    };
    let mut list = Vec::new();
    if let Some(trouble) = server.trouble.clone() {
        // At the top of the file, since it is about the whole file and
        // not about anything in it.
        list.push(obj(vec![
            ("range", span(&text, 0, 0, server.encoding)),
            ("severity", Json::Int(2)),
            ("source", Json::Str("zu".into())),
            ("message", Json::Str(trouble)),
        ]));
    }
    list.extend(diagnostics(server, &text));
    send(
        out,
        &notification(
            "textDocument/publishDiagnostics",
            obj(vec![
                ("uri", Json::Str(uri.to_string())),
                ("diagnostics", Json::Arr(list)),
            ]),
        ),
    );
}

/// Every complaint the front end has about a document.
///
/// One statement at a time, because a document is a file of them and a
/// syntax error in the first is not a reason to say nothing about the
/// third. With a database open this is a prepare, which parses, binds
/// and plans without running, so an unknown table and an unknown
/// property are caught here. Without one it is a parse, which is
/// everything that can be known about text with no catalog behind it: a
/// server with no database that complained about `Person` would be
/// complaining about the user's file being somewhere else.
fn diagnostics(server: &mut Server, text: &str) -> Vec<Json> {
    let encoding = server.encoding;
    let mut out = Vec::new();
    for (at, statement) in meta::spans(text) {
        let failed = match server.db.as_mut() {
            Some(session) => match session.prepare(statement) {
                Ok((stmt, _)) => {
                    // The plan is not wanted, only the fact that there
                    // is one. Left open, a document being retyped would
                    // fill the cache with a statement per keystroke.
                    session.close_stmt(stmt);
                    None
                }
                Err(e) => Some(e),
            },
            None => zu::query::check(statement).err(),
        };
        let Some(e) = failed else {
            continue;
        };
        out.push(match e.diagnostic() {
            Some(record) => complaint(text, at, statement, record, encoding),
            // An error with no GQLSTATUS is the engine failing rather
            // than the statement being wrong, and it has no position,
            // so it goes under the whole statement.
            None => obj(vec![
                ("range", span(text, at, at + statement.len(), encoding)),
                ("severity", Json::Int(1)),
                ("source", Json::Str("zu".into())),
                ("message", Json::Str(e.to_string())),
            ]),
        });
    }
    out.extend(strangers(server, text));
    out
}

/// A warning for every label the file has never heard of.
///
/// The binder does not raise one. `MATCH (a:Persno) RETURN a` prepares,
/// plans and runs, and returns nothing at all, which is the answer a
/// misspelling deserves least: it looks exactly like a table that is
/// empty. So the front end says it instead, and says it as a warning,
/// because a label the catalog does not carry today is still a label a
/// statement is allowed to name.
///
/// A name that matches something in the catalog in any case at all is
/// left alone, since case is the engine's business and a warning about
/// it here would be a guess.
fn strangers(server: &Server, text: &str) -> Vec<Json> {
    if server.db.is_none() {
        return Vec::new();
    }
    labelled(text)
        .into_iter()
        .filter(|piece| !known(server, piece.text))
        .map(|piece| unknown(text, piece, server.encoding))
        .collect()
}

/// Every name the text uses as a label or an edge type, and where.
///
/// The test is the bracket the name is inside. A colon in `(a:P)` and a
/// colon in `[:P]` introduce a label, a colon in `{name: value}` is a
/// map key, and nothing else in the language puts a bare name after a
/// colon inside a pattern. A bar or an ampersand carries on from a label
/// that was just read, which is how `(a:P|Q)` names two.
fn labelled(text: &str) -> Vec<format::Piece<'_>> {
    let mut out = Vec::new();
    let mut brackets: Vec<&str> = Vec::new();
    let mut expect = false;
    let mut after = false;
    for piece in format::pieces(text) {
        match piece.text {
            "(" | "[" | "{" => {
                brackets.push(piece.text);
                (expect, after) = (false, false);
            }
            ")" | "]" | "}" => {
                brackets.pop();
                (expect, after) = (false, false);
            }
            ":" => {
                expect = matches!(brackets.last(), Some(&"(" | &"["));
                after = false;
            }
            "|" | "&" => (expect, after) = (after, false),
            _ if piece.word => {
                after = expect;
                if expect {
                    out.push(piece);
                }
                expect = false;
            }
            _ => (expect, after) = (false, false),
        }
    }
    out
}

/// Whether the catalog carries a name, ignoring case.
fn known(server: &Server, name: &str) -> bool {
    let same = |other: &String| other.eq_ignore_ascii_case(name);
    server.names.labels.iter().any(same) || server.names.tables.iter().any(same)
}

/// The warning itself, under the name and not under the pattern.
fn unknown(text: &str, piece: format::Piece<'_>, encoding: Encoding) -> Json {
    let status = zu::gqlstatus::codes::C01000;
    obj(vec![
        (
            "range",
            span(text, piece.at, piece.at + piece.text.len(), encoding),
        ),
        ("severity", Json::Int(2)),
        ("code", Json::Str(status.code().into())),
        (
            "codeDescription",
            obj(vec![("href", Json::Str(status.doc_url()))]),
        ),
        ("source", Json::Str("zu".into())),
        (
            "message",
            Json::Str(format!(
                "no table or label named '{}' in this database",
                piece.text
            )),
        ),
    ])
}

/// One diagnostic record, as the protocol spells it.
fn complaint(
    text: &str,
    at: usize,
    statement: &str,
    record: &DiagnosticRecord,
    encoding: Encoding,
) -> Json {
    // The position is an offset into the statement and the editor wants
    // one into the document, so the two are added. A statement that
    // failed at its very end points past its last byte, which lands on
    // the end of the range rather than outside it.
    let range = match record.position {
        Some(position) => {
            let start = (at + position.offset as usize).min(text.len());
            span(text, start, word_end(text, start), encoding)
        }
        None => span(text, at, at + statement.len(), encoding),
    };
    obj(vec![
        ("range", range),
        (
            "severity",
            Json::Int(match record.severity() {
                Severity::Exception => 1,
                Severity::Warning => 2,
                Severity::Informational | Severity::NoData => 3,
                Severity::Success => 4,
            }),
        ),
        ("code", Json::Str(record.status.code().into())),
        (
            "codeDescription",
            obj(vec![("href", Json::Str(record.doc_url()))]),
        ),
        ("source", Json::Str("zu".into())),
        ("message", Json::Str(record.detail.clone())),
    ])
}

/// The names worth offering where the cursor is.
fn completion(server: &Server, params: &Json) -> Json {
    let Some((text, at)) = cursor(server, params) else {
        return Json::Null;
    };
    let start = complete::replaced(text, at);
    let range = span(text, start, at, server.encoding);
    let items = complete::candidates(text, at, &server.names)
        .into_iter()
        .map(|candidate| {
            let mut fields = vec![
                ("label", Json::Str(candidate.name.clone())),
                (
                    "kind",
                    Json::Int(match candidate.what {
                        // The protocol's numbers. A label is a class, a
                        // property is a field, a table is a struct, and
                        // a keyword is a keyword, which is as close as
                        // a fixed list gets to a graph language.
                        What::Label => 7,
                        What::Property => 5,
                        What::Table => 22,
                        What::Keyword => 14,
                    }),
                ),
                (
                    "detail",
                    Json::Str(
                        match candidate.what {
                            What::Label => "label",
                            What::Property => "property",
                            What::Table => "table",
                            What::Keyword => "keyword",
                        }
                        .into(),
                    ),
                ),
                (
                    // An explicit edit rather than a bare label,
                    // because the client's idea of where a word starts
                    // is its own and `$p` and `a.b` are the two places
                    // it differs from ours.
                    "textEdit",
                    obj(vec![
                        ("range", range.clone()),
                        ("newText", Json::Str(candidate.name.clone())),
                    ]),
                ),
            ];
            if let Some(doc) = server.docs.get(&candidate.name) {
                fields.push((
                    "documentation",
                    obj(vec![
                        ("kind", Json::Str("markdown".into())),
                        ("value", Json::Str(doc.clone())),
                    ]),
                ));
            }
            obj(fields)
        })
        .collect();
    obj(vec![
        ("isIncomplete", Json::Bool(false)),
        ("items", Json::Arr(items)),
    ])
}

/// What the name under the cursor is.
fn hover(server: &Server, params: &Json) -> Json {
    let Some((text, at)) = cursor(server, params) else {
        return Json::Null;
    };
    // A hover asks about the character the pointer is over, and the
    // cursor is in front of it, so the word is the one that reaches
    // across the cursor rather than the one that ends at it.
    let start = complete::replaced(text, at);
    let end = word_end(text, at);
    if start == end {
        return Json::Null;
    }
    if matches!(highlight::kind_at(text, end), Kind::Text | Kind::Comment) {
        return Json::Null;
    }
    let word = &text[start..end];
    let before = text[..start].chars().next_back();
    let doc = if before == Some('$') {
        format!(
            "`${word}`\n\nA parameter. Whoever runs the statement binds it, \
             and nothing in the file has to be called this."
        )
    } else if let Some(doc) = server.docs.get(word) {
        doc.clone()
    } else if let Some(doc) = keyword_doc(word) {
        doc
    } else {
        return Json::Null;
    };
    obj(vec![
        (
            "contents",
            obj(vec![
                ("kind", Json::Str("markdown".into())),
                ("value", Json::Str(doc)),
            ]),
        ),
        ("range", span(text, start, end, server.encoding)),
    ])
}

/// The document, laid out. One edit over the whole of it, and no edit
/// at all when it is already laid out that way, which is what keeps a
/// format on save from marking a clean file dirty.
fn formatting(server: &Server, params: &Json) -> Json {
    let Some(uri) = uri_of(params) else {
        return Json::Null;
    };
    let Some(text) = server.open.get(&uri) else {
        return Json::Null;
    };
    let laid_out = format::format(text);
    if laid_out == *text {
        return Json::Arr(Vec::new());
    }
    Json::Arr(vec![obj(vec![
        ("range", span(text, 0, text.len(), server.encoding)),
        ("newText", Json::Str(laid_out)),
    ])])
}

/// The document's colours, in the protocol's five numbers per token.
fn semantic_tokens(server: &Server, params: &Json) -> Json {
    let Some(uri) = uri_of(params) else {
        return Json::Null;
    };
    let Some(text) = server.open.get(&uri) else {
        return Json::Null;
    };
    let mut data = Vec::new();
    let (mut line, mut column) = (0u32, 0u32);
    for (start, end, kind) in coloured(text) {
        // A token that crosses a line is written as one token per line,
        // because the protocol's numbers cannot say otherwise: a length
        // is a length along one line. A block comment and a string are
        // the two that cross.
        for (from, to) in per_line(text, start, end) {
            let (at_line, at_column) = place(text, from, server.encoding);
            let width: usize = text[from..to]
                .chars()
                .map(|c| server.encoding.units(c))
                .sum();
            if width == 0 {
                continue;
            }
            data.push(Json::Int(i64::from(at_line - line)));
            data.push(Json::Int(i64::from(match at_line == line {
                true => at_column - column,
                false => at_column,
            })));
            data.push(Json::Int(width as i64));
            data.push(Json::Int(i64::from(kind)));
            data.push(Json::Int(0));
            line = at_line;
            column = at_column;
        }
    }
    obj(vec![("data", Json::Arr(data))])
}

/// Every coloured run of the document, as byte ranges and token types.
///
/// The scanner gives five kinds and a sixth that means everything else,
/// and everything else is where the identifiers are. They are worth
/// separating, because a label, a property and a variable are three
/// different things to a reader and the character in front of the word
/// says which: a colon makes a type, a dot makes a property, and
/// nothing in particular makes a variable. That is the same rule the
/// completion uses, and it has to be, or a word would be offered as one
/// thing and coloured as another.
fn coloured(text: &str) -> Vec<(usize, usize, u32)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (run, kind) in highlight::scan(text) {
        let start = at;
        at += run.len();
        let ty = match kind {
            Kind::Keyword => 0,
            Kind::Text => 1,
            Kind::Number => 2,
            Kind::Comment => 3,
            Kind::Param => 4,
            Kind::Plain => {
                for (from, to) in words(run) {
                    let ty = match preceding(text, start + from) {
                        Some(':') => 6,
                        Some('.') => 7,
                        _ => 5,
                    };
                    out.push((start + from, start + to, ty));
                }
                continue;
            }
        };
        out.push((start, at, ty));
    }
    out
}

/// The identifier runs of a stretch of plain text, as offsets into it.
fn words(run: &str) -> Vec<(usize, usize)> {
    let bytes = run.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !word_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && word_byte(bytes[at]) {
            at += 1;
        }
        // A run of digits inside plain text is not an identifier, and
        // the scanner would have called it a number if it stood alone.
        if !bytes[start].is_ascii_digit() {
            out.push((start, at));
        }
    }
    out
}

/// The last character before `at` that is not a space, which is what
/// says whether a word is a label, a property or a variable.
fn preceding(text: &str, at: usize) -> Option<char> {
    text[..at].chars().rev().find(|c| !c.is_whitespace())
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A byte range cut at the newlines in it, so that no piece crosses a
/// line.
fn per_line(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = start;
    for (offset, _) in text[start..end].match_indices('\n') {
        out.push((from, start + offset));
        from = start + offset + 1;
    }
    out.push((from, end));
    out.into_iter().filter(|(a, b)| a < b).collect()
}

/// The end of the word `at` is in the middle or at the start of.
fn word_end(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = at;
    while end < bytes.len() && word_byte(bytes[end]) {
        end += 1;
    }
    end
}

/// The document and the byte offset a request is about.
fn cursor<'a>(server: &'a Server, params: &Json) -> Option<(&'a str, usize)> {
    let uri = uri_of(params)?;
    let text = server.open.get(&uri)?;
    let position = params.get("position")?;
    let line = position.get("line").and_then(Json::as_u64)? as u32;
    let character = position.get("character").and_then(Json::as_u64)? as u32;
    Some((text, offset(text, line, character, server.encoding)))
}

fn uri_of(params: &Json) -> Option<String> {
    params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
        .map(str::to_string)
}

/// Where a byte offset is, as the line and column the client counts in.
fn place(text: &str, at: usize, encoding: Encoding) -> (u32, u32) {
    let at = at.min(text.len());
    let before = &text[..at];
    let line = before.matches('\n').count() as u32;
    let start = before.rfind('\n').map_or(0, |n| n + 1);
    let column: usize = text[start..at].chars().map(|c| encoding.units(c)).sum();
    (line, column as u32)
}

/// A range, as two of those.
fn span(text: &str, start: usize, end: usize, encoding: Encoding) -> Json {
    let point = |at: usize| {
        let (line, character) = place(text, at, encoding);
        obj(vec![
            ("line", Json::Int(i64::from(line))),
            ("character", Json::Int(i64::from(character))),
        ])
    };
    obj(vec![
        ("start", point(start)),
        ("end", point(end.max(start))),
    ])
}

/// The byte offset of a line and a column, which is the way back.
///
/// A column past the end of its line lands at the end of it, and a line
/// past the end of the document lands at the end of the document. An
/// editor sends those: a cursor at the end of a line the user just
/// deleted arrives before the change that deleted it does.
fn offset(text: &str, line: u32, character: u32, encoding: Encoding) -> usize {
    let mut at = 0;
    for _ in 0..line {
        match text[at..].find('\n') {
            Some(n) => at += n + 1,
            None => return text.len(),
        }
    }
    let end = text[at..].find('\n').map_or(text.len(), |n| at + n);
    let mut counted = 0usize;
    for c in text[at..end].chars() {
        if counted >= character as usize {
            break;
        }
        counted += encoding.units(c);
        at += c.len_utf8();
    }
    at
}

/// The hover text for every name the catalog knows.
///
/// Built once from the catalog rather than looked up per hover, because
/// a hover happens whenever a pointer stops moving and a catalog walk
/// per pointer movement is a walk too many. The whole catalog of a
/// large file is a few hundred names.
fn catalog_docs(session: &mut Session) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let catalog = session.catalog();
    let mut entries: Vec<(String, String)> = Vec::new();
    for table in catalog.node_tables() {
        entries.push((
            table.name.clone(),
            format!(
                "**{}**, a node table\n\n{} {}",
                table.name,
                table.node_count,
                plural(table.node_count, "node")
            ),
        ));
    }
    for rel in catalog.rel_tables() {
        let from = catalog
            .node_by_id(rel.from)
            .map_or("?", |t| t.name.as_str())
            .to_string();
        let to = catalog
            .node_by_id(rel.to)
            .map_or("?", |t| t.name.as_str())
            .to_string();
        entries.push((
            rel.name.clone(),
            format!(
                "**{}**, {} edge table\n\n`{}` to `{}`, {} {}",
                rel.name,
                match rel.undirected {
                    true => "an undirected",
                    false => "a directed",
                },
                from,
                to,
                rel.edge_count,
                plural(rel.edge_count, "edge")
            ),
        ));
    }
    // The property directory rather than the declared type, for the
    // reason `\d` reads it: a table loaded from a CSV has columns and
    // no declaration, and a hover built from the declarations would
    // tell that user their table is empty.
    for (name, doc) in &mut entries {
        let columns = crate::repl::columns(session, name);
        if columns.is_empty() {
            continue;
        }
        doc.push_str("\n\n");
        for column in columns {
            doc.push_str(&format!(
                "- `{}` {}{}\n",
                column.name,
                column.ty,
                match column.nullable {
                    true => "",
                    false => ", not null",
                }
            ));
        }
    }
    for (name, doc) in entries {
        out.insert(name, doc);
    }
    out
}

fn plural(n: u64, what: &str) -> String {
    match n {
        1 => what.to_string(),
        _ => format!("{what}s"),
    }
}

/// What a reserved word does, for the ones where knowing is the
/// difference between writing the statement and looking it up.
///
/// Not every word in the language, because a hover that said `AS` is a
/// reserved word and nothing else would be a hover that trained people
/// to ignore hovers. The clauses are here, and the words that are easy
/// to get subtly wrong, and everything else answers with the one fact
/// that is always true and is worth knowing on its own: it is reserved,
/// so it is not available as a name.
fn keyword_doc(word: &str) -> Option<String> {
    let upper = word.to_ascii_uppercase();
    if let Some((_, doc)) = KEYWORD_DOCS.iter().find(|(name, _)| *name == upper) {
        return Some(format!("**{upper}**\n\n{doc}"));
    }
    if highlight::KEYWORDS.contains(&upper.as_str()) {
        return Some(format!(
            "**{upper}**\n\nA reserved word, so it cannot be used as a name."
        ));
    }
    None
}

/// The clause words and the ones worth a sentence, kept as prose rather
/// than as a link, because a hover a reader has to follow is a hover
/// that did not answer.
const KEYWORD_DOCS: &[(&str, &str)] = &[
    (
        "MATCH",
        "Finds the parts of the graph that fit a pattern, and binds one row per way of fitting it. \
         `MATCH (p:Person)-[:KNOWS]->(f)` binds `p` and `f` once per edge.",
    ),
    (
        "OPTIONAL",
        "In front of `MATCH`, keeps the rows that fit nothing and fills their new variables with \
         null, rather than dropping them.",
    ),
    (
        "WHERE",
        "Keeps the rows a condition is true of. It reads the variables the clause in front of it \
         bound, so it goes after the `MATCH` and not inside it.",
    ),
    (
        "RETURN",
        "Says what the statement answers with, and names the columns. `RETURN p.name AS name` is \
         one column called `name`.",
    ),
    (
        "WITH",
        "Ends one part of a statement and starts the next with only the columns it names. It is \
         how a statement aggregates and then filters on what it aggregated.",
    ),
    (
        "UNWIND",
        "Turns one row holding a list into one row per element of it.",
    ),
    (
        "ORDER",
        "With `BY`, sorts the rows. `ASC` is the default and `DESC` reverses it.",
    ),
    (
        "LIMIT",
        "Keeps the first rows and stops. It is the whole statement's limit, so it goes after \
         `ORDER BY` and not before it.",
    ),
    (
        "SKIP",
        "Drops the first rows before `LIMIT` counts. Two statements that skip different amounts \
         only agree with each other if the order is total.",
    ),
    (
        "DISTINCT",
        "Keeps one of each row that repeats, comparing every returned column.",
    ),
    (
        "AS",
        "Names something: a returned column, or a variable a `WITH` carries forward. A column with \
         no name is called whatever expression made it.",
    ),
    (
        "INSERT",
        "Adds nodes and edges. The pattern is read as a description of what should exist rather \
         than as something to find.",
    ),
    (
        "SET",
        "Writes a property, or several. `SET p.age = 36` on a row that has no `p` writes nothing.",
    ),
    (
        "DELETE",
        "Removes nodes and edges. A node with edges still on it is refused, which is what `DETACH \
         DELETE` overrides.",
    ),
    (
        "DETACH",
        "In front of `DELETE`, removes the edges on a node along with the node.",
    ),
    (
        "CALL",
        "Runs a subquery, in braces, once per row it is given, and joins what it answers back on.",
    ),
    (
        "UNION",
        "Puts the rows of two statements together. Both have to answer the same columns, and \
         `UNION ALL` keeps the repeats that `UNION` drops.",
    ),
    (
        "COUNT",
        "Counts rows. `count(*)` counts every row and `count(x)` counts the rows where `x` is not \
         null, which is the difference that catches people out.",
    ),
    (
        "IS",
        "With `NULL` or `NOT NULL`, tests for a missing value. `= null` is not that test and is \
         never true.",
    ),
    ("IN", "Tests whether a value is an element of a list."),
    (
        "CASE",
        "Picks between expressions. `CASE WHEN c THEN a ELSE b END`, and a `CASE` with no `ELSE` \
         answers null when nothing matched.",
    ),
    (
        "USE",
        "Says which graph the statement runs against, for a file that holds more than one.",
    ),
    (
        "FINISH",
        "Ends a statement that answers no rows, which is how a write says it is finished rather \
         than returning something nobody asked for.",
    ),
];

/// One JSON object, from pairs, which is the shape nearly everything
/// this module builds.
fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::Obj(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn reply(id: Json, result: Json) -> Json {
    obj(vec![
        ("jsonrpc", Json::Str("2.0".into())),
        ("id", id),
        ("result", result),
    ])
}

fn error(id: Json, code: i64, message: &str) -> Json {
    obj(vec![
        ("jsonrpc", Json::Str("2.0".into())),
        ("id", id),
        (
            "error",
            obj(vec![
                ("code", Json::Int(code)),
                ("message", Json::Str(message.to_string())),
            ]),
        ),
    ])
}

fn notification(method: &str, params: Json) -> Json {
    obj(vec![
        ("jsonrpc", Json::Str("2.0".into())),
        ("method", Json::Str(method.to_string())),
        ("params", params),
    ])
}

/// One message, headers and all.
///
/// `Content-Length` is the only header that matters and the only one
/// that is read. The rest are skipped rather than refused, since a
/// client is free to send more and a server that fell over on an
/// unfamiliar header would be a server that broke on the next version
/// of the protocol.
fn read_message(input: &mut impl BufRead) -> Option<String> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; length?];
    std::io::Read::read_exact(input, &mut body).ok()?;
    String::from_utf8(body).ok()
}

/// One message out, framed the same way.
fn send(out: &mut impl Write, value: &Json) {
    let body = value.to_compact();
    // The length is in bytes and not in characters, which is the one
    // thing about this framing that is easy to get wrong and impossible
    // to notice until somebody writes a table name in Japanese.
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation, as the bytes a client would send.
    fn talk(server: &mut Server, messages: &[&str]) -> Vec<Json> {
        let mut input = Vec::new();
        for message in messages {
            input.extend_from_slice(
                format!("Content-Length: {}\r\n\r\n{message}", message.len()).as_bytes(),
            );
        }
        let mut out = Vec::new();
        serve(server, &mut std::io::Cursor::new(input), &mut out);
        let mut read = std::io::Cursor::new(out);
        let mut answers = Vec::new();
        while let Some(raw) = read_message(&mut read) {
            answers.push(zu_json::parse(&raw).expect("an answer that is JSON"));
        }
        answers
    }

    /// The handshake, and a document opened, which is what every case
    /// below needs in front of it.
    fn opened(text: &str) -> (Server, Vec<Json>) {
        let mut server = Server::new();
        let escaped = Json::Str(text.to_string()).to_compact();
        let answers = talk(
            &mut server,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                &format!(
                    r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///q.zuql","text":{escaped}}}}}}}"#
                ),
            ],
        );
        (server, answers)
    }

    /// Asks one more thing of a server that is already up, and answers
    /// with the result.
    fn ask(server: &mut Server, method: &str, params: &str) -> Json {
        let answers = talk(
            server,
            &[&format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"{method}","params":{params}}}"#
            )],
        );
        answers
            .into_iter()
            .find_map(|a| a.get("result").cloned())
            .expect("a result")
    }

    fn diagnostics_of(answers: &[Json]) -> Vec<Json> {
        answers
            .iter()
            .filter(|a| {
                a.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics")
            })
            .flat_map(|a| {
                a.get("params")
                    .and_then(|p| p.get("diagnostics"))
                    .and_then(Json::as_arr)
                    .unwrap_or(&[])
                    .to_vec()
            })
            .collect()
    }

    #[test]
    fn a_label_is_told_apart_from_a_map_key_by_the_bracket_it_is_in() {
        fn names(text: &str) -> Vec<&str> {
            labelled(text).iter().map(|p| p.text).collect()
        }

        // The two places a label is written, and the two more that a
        // pattern spells with a bar and an ampersand.
        assert_eq!(
            names("MATCH (a:Person)-[:FOLLOWS]->(b:Person)"),
            ["Person", "FOLLOWS", "Person"]
        );
        assert_eq!(
            names("MATCH (a:Person|Company&Listed) RETURN a"),
            ["Person", "Company", "Listed"]
        );

        // A map key is not one, whether the map stands alone or is
        // written inside a pattern, which is where it is easiest to
        // mistake for one.
        assert_eq!(names("RETURN {name: 'a', age: 1}"), Vec::<&str>::new());
        assert_eq!(names("INSERT (a:Person {name: 'a'})"), ["Person"]);

        // And neither is a name inside a string or a comment, which is
        // the reason to cut the text the way the formatter cuts it
        // rather than looking for colons.
        assert_eq!(
            names("RETURN 'MATCH (a:Person)' // (b:Company)"),
            Vec::<&str>::new()
        );

        // The offset is where an editor puts the squiggle, so it is the
        // name and not the colon in front of it.
        let found = labelled("MATCH (a:Person)");
        assert_eq!(found[0].at, 9);
    }

    #[test]
    fn the_handshake_says_what_the_server_can_do() {
        let (_, answers) = opened("RETURN 1 AS one");
        let capabilities = answers[0]
            .get("result")
            .and_then(|r| r.get("capabilities"))
            .expect("capabilities");
        assert_eq!(
            capabilities.get("positionEncoding").and_then(Json::as_str),
            Some("utf-16")
        );
        assert_eq!(
            capabilities.get("hoverProvider").and_then(Json::as_bool),
            Some(true)
        );
        assert!(capabilities.get("semanticTokensProvider").is_some());
        assert!(capabilities.get("documentFormattingProvider").is_some());
    }

    #[test]
    fn a_client_that_reads_utf8_is_answered_in_bytes() {
        let mut server = Server::new();
        talk(
            &mut server,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"general":{"positionEncodings":["utf-8","utf-16"]}}}"#,
            ],
        );
        assert_eq!(server.encoding, Encoding::Utf8);
    }

    #[test]
    fn a_statement_that_parses_has_nothing_wrong_with_it() {
        let (_, answers) = opened("RETURN 1 AS one");
        assert!(diagnostics_of(&answers).is_empty());
    }

    #[test]
    fn a_syntax_error_is_a_squiggle_where_the_parser_stopped() {
        let (_, answers) = opened("RETURN 1 AS one;\nMATCH (a) RETRUN a");
        let found = diagnostics_of(&answers);
        assert_eq!(found.len(), 1, "one bad statement out of two");
        let d = &found[0];
        assert_eq!(d.get("severity").and_then(Json::as_i64), Some(1));
        assert_eq!(d.get("source").and_then(Json::as_str), Some("zu"));
        assert_eq!(d.get("code").and_then(Json::as_str), Some("42001"));
        // The second statement is on the second line, so the squiggle
        // is where the parser stopped plus where the statement began,
        // and it covers the misspelled word rather than one character
        // of it.
        let range = d.get("range").expect("a range");
        let at = |end: &str, field: &str| {
            range
                .get(end)
                .and_then(|p| p.get(field))
                .and_then(Json::as_i64)
        };
        assert_eq!(at("start", "line"), Some(1));
        assert_eq!(at("start", "character"), Some(10));
        assert_eq!(at("end", "character"), Some(16));
    }

    #[test]
    fn a_table_that_is_not_there_is_only_a_complaint_with_a_file_open() {
        // Nothing is attached, so `Person` is a name this server has no
        // opinion about rather than a name it says is wrong.
        let (_, answers) = opened("MATCH (p:Person) RETURN p.id");
        assert!(diagnostics_of(&answers).is_empty());
    }

    #[test]
    fn a_closed_document_takes_its_squiggles_with_it() {
        let (mut server, _) = opened("MATCH (a) RETRUN a");
        let answers = talk(
            &mut server,
            &[
                r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///q.zuql"}}}"#,
            ],
        );
        assert!(diagnostics_of(&answers).is_empty());
        assert!(!server.open.contains_key("file:///q.zuql"));
    }

    #[test]
    fn a_change_is_the_whole_document_and_is_checked_again() {
        let (mut server, _) = opened("RETURN 1");
        let answers = talk(
            &mut server,
            &[
                r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///q.zuql"},"contentChanges":[{"text":"MATCH (a) RETRUN a"}]}}"#,
            ],
        );
        assert_eq!(diagnostics_of(&answers).len(), 1);
    }

    #[test]
    fn completion_offers_keywords_and_replaces_the_word_it_was_asked_about() {
        let (mut server, _) = opened("MATC");
        let result = ask(
            &mut server,
            "textDocument/completion",
            r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":4}}"#,
        );
        let items = result.get("items").and_then(Json::as_arr).expect("items");
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("label").and_then(Json::as_str))
            .collect();
        assert!(labels.contains(&"MATCH"), "got {labels:?}");
        let edit = items[0].get("textEdit").expect("an edit");
        let range = edit.get("range").expect("a range");
        assert_eq!(
            range
                .get("start")
                .and_then(|s| s.get("character"))
                .and_then(Json::as_i64),
            Some(0),
            "the whole word is replaced, not appended to"
        );
    }

    #[test]
    fn nothing_is_offered_inside_a_string() {
        let (mut server, _) = opened("RETURN 'MATC");
        let result = ask(
            &mut server,
            "textDocument/completion",
            r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":12}}"#,
        );
        assert_eq!(
            result
                .get("items")
                .and_then(Json::as_arr)
                .map(<[Json]>::len),
            Some(0)
        );
    }

    #[test]
    fn hover_says_what_a_clause_does_and_says_nothing_about_a_variable() {
        let (mut server, _) = opened("MATCH (a) RETURN a");
        let result = ask(
            &mut server,
            "textDocument/hover",
            r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":2}}"#,
        );
        let value = result
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(Json::as_str)
            .expect("hover text");
        assert!(value.starts_with("**MATCH**"), "got {value}");
        // `a` is the user's own variable and there is nothing to say
        // about it that the user does not already know.
        let nothing = ask(
            &mut server,
            "textDocument/hover",
            r#"{"textDocument":{"uri":"file:///q.zuql"},"position":{"line":0,"character":7}}"#,
        );
        assert_eq!(nothing, Json::Null);
    }

    #[test]
    fn formatting_answers_one_edit_over_the_whole_document_and_none_when_it_is_tidy() {
        let (mut server, _) = opened("match(a)return a");
        let edits = ask(
            &mut server,
            "textDocument/formatting",
            r#"{"textDocument":{"uri":"file:///q.zuql"}}"#,
        );
        let edits = edits.as_arr().expect("edits");
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].get("newText").and_then(Json::as_str),
            Some("match (a)\nreturn a\n")
        );

        let (mut tidy, _) = opened("match (a)\nreturn a\n");
        let none = ask(
            &mut tidy,
            "textDocument/formatting",
            r#"{"textDocument":{"uri":"file:///q.zuql"}}"#,
        );
        assert_eq!(none.as_arr().map(<[Json]>::len), Some(0));
    }

    #[test]
    fn semantic_tokens_colour_the_words_the_scanner_found() {
        let (mut server, _) = opened("MATCH (a:Person) RETURN a.name");
        let result = ask(
            &mut server,
            "textDocument/semanticTokens/full",
            r#"{"textDocument":{"uri":"file:///q.zuql"}}"#,
        );
        let data: Vec<i64> = result
            .get("data")
            .and_then(Json::as_arr)
            .expect("data")
            .iter()
            .filter_map(Json::as_i64)
            .collect();
        assert_eq!(data.len() % 5, 0, "five numbers to a token");
        // MATCH keyword, a variable, Person type, RETURN keyword, a
        // variable, name property.
        let types: Vec<i64> = data.chunks(5).map(|t| t[3]).collect();
        assert_eq!(types, [0, 5, 6, 0, 5, 7]);
        // The first token starts the document and the second is six
        // columns further along the same line.
        assert_eq!(&data[..5], [0, 0, 5, 0, 0]);
        assert_eq!(&data[5..10], [0, 7, 1, 5, 0]);
    }

    #[test]
    fn a_token_that_crosses_a_line_is_written_a_line_at_a_time() {
        let (mut server, _) = opened("RETURN /* one\ntwo */ 1");
        let result = ask(
            &mut server,
            "textDocument/semanticTokens/full",
            r#"{"textDocument":{"uri":"file:///q.zuql"}}"#,
        );
        let data: Vec<i64> = result
            .get("data")
            .and_then(Json::as_arr)
            .expect("data")
            .iter()
            .filter_map(Json::as_i64)
            .collect();
        let types: Vec<i64> = data.chunks(5).map(|t| t[3]).collect();
        assert_eq!(types, [0, 3, 3, 2], "the comment is two tokens");
        assert_eq!(data.chunks(5).nth(2).expect("the second half")[0], 1);
    }

    #[test]
    fn a_column_is_counted_the_way_the_client_asked_for() {
        // Four characters, each three bytes in UTF-8 and one unit in
        // UTF-16, so the two answers differ and neither is the other's
        // rounding error.
        let text = "RETURN '日本語'";
        assert_eq!(place(text, text.len(), Encoding::Utf16), (0, 12));
        assert_eq!(place(text, text.len(), Encoding::Utf8), (0, 18));
        assert_eq!(offset(text, 0, 12, Encoding::Utf16), text.len());
        assert_eq!(offset(text, 0, 18, Encoding::Utf8), text.len());
        // A column past the end of the line lands at the end of it.
        assert_eq!(offset(text, 0, 999, Encoding::Utf16), text.len());
        assert_eq!(offset(text, 9, 0, Encoding::Utf16), text.len());
    }

    #[test]
    fn an_unknown_request_is_refused_and_an_unknown_notification_is_not() {
        let mut server = Server::new();
        let answers = talk(
            &mut server,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/rename","params":{}}"#,
                r#"{"jsonrpc":"2.0","method":"$/setTrace","params":{"value":"off"}}"#,
            ],
        );
        assert_eq!(
            answers.len(),
            1,
            "the notification is answered with nothing"
        );
        assert_eq!(
            answers[0]
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Json::as_i64),
            Some(-32601)
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_refused_against_a_null_id() {
        let mut server = Server::new();
        let answers = talk(&mut server, &["{not json"]);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].get("id"), Some(&Json::Null));
        assert_eq!(
            answers[0]
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Json::as_i64),
            Some(-32700)
        );
    }

    #[test]
    fn shutdown_then_exit_is_a_clean_leaving_and_a_dropped_pipe_is_not() {
        let mut clean = Vec::new();
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#,
            r#"{"jsonrpc":"2.0","method":"exit"}"#,
        ] {
            clean.extend_from_slice(
                format!("Content-Length: {}\r\n\r\n{message}", message.len()).as_bytes(),
            );
        }
        let mut out = Vec::new();
        let code = serve(
            &mut Server::new(),
            &mut std::io::Cursor::new(clean),
            &mut out,
        );
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));

        let mut out = Vec::new();
        let code = serve(
            &mut Server::new(),
            &mut std::io::Cursor::new(Vec::new()),
            &mut out,
        );
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
    }

    #[test]
    fn the_framing_counts_bytes_and_not_characters() {
        let mut out = Vec::new();
        send(&mut out, &Json::Str("日本語".into()));
        let text = String::from_utf8(out).expect("utf-8");
        assert!(
            text.starts_with("Content-Length: 11\r\n\r\n"),
            "got {text:?}"
        );
    }

    #[test]
    fn a_message_with_headers_it_does_not_know_is_read_anyway() {
        let raw = "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
                   Content-Length: 2\r\n\r\n{}";
        let mut read = std::io::Cursor::new(raw.as_bytes().to_vec());
        assert_eq!(read_message(&mut read).as_deref(), Some("{}"));
        assert_eq!(read_message(&mut read), None);
    }
}
