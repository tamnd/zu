//! `zu shell`: a persistent query loop over one file, speaking one
//! JSON object per line on stdout. This is the process a harness or an
//! editor keeps alive so the catalog, stats, plan cache, and decoded
//! block caches pay their cost once instead of once per query; the
//! one-shot `zu query` path stays for humans and scripts.
//!
//! Input is line-oriented and comes in two spellings. A line starting
//! with `{` is a frame: `{"op":"query","q":"...","params":{...}}`,
//! plus `prepare`, `execute`, `close_stmt`, `explain_analyze`, and
//! `quit`. Any other non-empty line is a bare statement run with no
//! parameters, with `\n`, `\t`, and `\\` unfolded so a multi-line
//! statement can travel on one line. Responses are `{"columns":...,
//! "rows":...}` for results and `{"error":"..."}` for failures, and an
//! error never kills the loop: the session and its caches survive a
//! bad statement.
//!
//! Both response shapes carry GQLSTATUS when there is one
//! (Spec/2064g/gql/plan/07). A result that raised a condition it
//! survived grows a `"notices"` array; a failure the engine raised grows
//! a `"failure"` object with the code, the standard's text and the
//! severity. A failure the *protocol* raised, a malformed frame or an
//! unknown op, has no code and does not pretend to: those are not
//! conditions the standard defines.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use zu::query::Value;
use zu::session::Session;

use crate::json::{self, Json};

const SHELL_USAGE: &str = "zu shell <file.zu1> [--format jsonl]";

pub(crate) fn shell_command(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // jsonl is the only wire format; the flag exists so a
            // caller can say what it expects and fail loudly here if
            // a future default ever changes.
            "--format" | "-f" => match args.get(i + 1).map(String::as_str) {
                Some("jsonl") => i += 2,
                _ => return crate::usage_error(SHELL_USAGE),
            },
            arg if arg.starts_with('-') => return crate::usage_error(SHELL_USAGE),
            arg if path.is_none() => {
                path = Some(arg);
                i += 1;
            }
            _ => return crate::usage_error(SHELL_USAGE),
        }
    }
    let Some(path) = path else {
        return crate::usage_error(SHELL_USAGE);
    };
    let mut session = match Session::open(std::path::Path::new(path)) {
        Ok(s) => s,
        Err(e) => return crate::command_error("shell", &e),
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (response, quit) = respond(&mut session, line);
        if out.write_all(response.as_bytes()).is_err() || out.flush().is_err() {
            // The reader hung up; nothing left to serve.
            break;
        }
        if quit {
            break;
        }
    }
    ExitCode::SUCCESS
}

/// Handles one input line and returns the one-line response plus
/// whether the loop should end. Every path returns a string ending in
/// exactly one newline, because the reader on the other side counts
/// lines, not braces.
fn respond(session: &mut Session, line: &str) -> (String, bool) {
    if !line.starts_with('{') {
        let reply = match session.run(&unfold(line), &[]) {
            Ok(r) => crate::render_json(&r),
            Err(e) => failure_line(&e),
        };
        return (reply, false);
    }
    let frame = match json::parse(line) {
        Ok(f) => f,
        Err(e) => return (error_line(&format!("bad frame: {e}")), false),
    };
    match frame.get("op").and_then(Json::as_str) {
        Some("query") => (run_frame(session, &frame, false), false),
        Some("explain_analyze") => (run_frame(session, &frame, true), false),
        Some("prepare") => {
            let Some(q) = frame.get("q").and_then(Json::as_str) else {
                return (error_line("prepare needs a string q"), false);
            };
            let reply = match session.prepare(q) {
                Ok((id, names)) => {
                    let mut s = format!("{{\"stmt\":{id},\"params\":[");
                    for (i, name) in names.iter().enumerate() {
                        if i > 0 {
                            s.push(',');
                        }
                        crate::write_json_str(&mut s, name);
                    }
                    s.push_str("]}\n");
                    s
                }
                Err(e) => failure_line(&e),
            };
            (reply, false)
        }
        Some("execute") => {
            let Some(id) = frame.get("stmt").and_then(Json::as_u64) else {
                return (error_line("execute needs an integer stmt"), false);
            };
            let params = match frame_params(&frame) {
                Ok(p) => p,
                Err(e) => return (error_line(&e), false),
            };
            let borrowed: Vec<(&str, Value)> = params
                .iter()
                .map(|(n, v)| (n.as_str(), v.clone()))
                .collect();
            let reply = match session.execute(id, &borrowed) {
                Ok(r) => crate::render_json(&r),
                Err(e) => failure_line(&e),
            };
            (reply, false)
        }
        Some("close_stmt") => match frame.get("stmt").and_then(Json::as_u64) {
            Some(id) => {
                let closed = session.close_stmt(id);
                (format!("{{\"closed\":{closed}}}\n"), false)
            }
            None => (error_line("close_stmt needs an integer stmt"), false),
        },
        Some("quit") => ("{\"bye\":true}\n".to_string(), true),
        Some(op) => (error_line(&format!("unknown op {op:?}")), false),
        None => (error_line("frame needs a string op"), false),
    }
}

/// Runs a `query` or `explain_analyze` frame; both take `q` and
/// optional `params`.
fn run_frame(session: &mut Session, frame: &Json, explain: bool) -> String {
    let Some(q) = frame.get("q").and_then(Json::as_str) else {
        return error_line("query needs a string q");
    };
    let params = match frame_params(frame) {
        Ok(p) => p,
        Err(e) => return error_line(&e),
    };
    let borrowed: Vec<(&str, Value)> = params
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect();
    if explain {
        return match session.explain_analyze(q, &borrowed) {
            Ok(text) => {
                let mut s = String::from("{\"text\":");
                crate::write_json_str(&mut s, &text);
                s.push_str("}\n");
                s
            }
            Err(e) => failure_line(&e),
        };
    }
    match session.run(q, &borrowed) {
        Ok(r) => crate::render_json(&r),
        Err(e) => failure_line(&e),
    }
}

/// Pulls the optional `params` object out of a frame and types each
/// value the way the query engine binds it.
fn frame_params(frame: &Json) -> Result<Vec<(String, Value)>, String> {
    let Some(params) = frame.get("params") else {
        return Ok(Vec::new());
    };
    let Json::Obj(fields) = params else {
        return Err("params must be an object".to_string());
    };
    fields
        .iter()
        .map(|(name, v)| {
            let value = match v {
                Json::Null => Value::Null,
                Json::Bool(b) => Value::Bool(*b),
                Json::Int(i) => Value::Int(*i),
                Json::Float(f) => Value::Float(*f),
                Json::Str(s) => Value::Str(s.clone()),
                Json::Arr(_) | Json::Obj(_) => {
                    return Err(format!("param ${name} has an unsupported type"));
                }
            };
            Ok((name.clone(), value))
        })
        .collect()
}

/// A failure with no GQLSTATUS: a malformed frame, an unknown op, a
/// parameter the wire cannot type. These are protocol faults, not GQL
/// conditions, and giving them a made-up code would be worse than
/// leaving the field off.
fn error_line(message: &str) -> String {
    let mut s = String::from("{\"error\":");
    crate::write_json_str(&mut s, message);
    s.push_str("}\n");
    s
}

/// A failure the engine raised. When it carries a condition the frame
/// gets the code, the standard's text and the severity in fields of
/// their own, next to the same `error` string an older reader expects.
fn failure_line(err: &zu::ZuError) -> String {
    let Some(record) = err.diagnostic() else {
        return error_line(&err.to_string());
    };
    let mut s = String::from("{\"error\":");
    crate::write_json_str(&mut s, &record.detail);
    s.push_str(",\"failure\":");
    crate::write_json_diagnostic(&mut s, record);
    s.push_str("}\n");
    s
}

/// Undoes the one-line folding a client applies to a bare statement:
/// `\n` and `\t` become the real characters and `\\` a single
/// backslash. Any other backslash pair is left alone, so a statement
/// that was never folded still runs.
fn unfold(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_statements_unfold() {
        assert_eq!(unfold(r"MATCH (a)\nRETURN a"), "MATCH (a)\nRETURN a");
        assert_eq!(unfold(r"a\tb\\c"), "a\tb\\c");
        assert_eq!(unfold(r"plain"), "plain");
        assert_eq!(unfold(r"odd\q\"), "odd\\q\\");
    }

    #[test]
    fn frame_params_keep_their_types() {
        let frame =
            json::parse(r#"{"params":{"a":1,"b":2.5,"c":"x","d":true,"e":null}}"#).expect("frame");
        let params = frame_params(&frame).expect("params");
        assert_eq!(
            params,
            [
                ("a".to_string(), Value::Int(1)),
                ("b".to_string(), Value::Float(2.5)),
                ("c".to_string(), Value::Str("x".into())),
                ("d".to_string(), Value::Bool(true)),
                ("e".to_string(), Value::Null),
            ]
        );
        let none = json::parse(r#"{"op":"query"}"#).expect("frame");
        assert_eq!(frame_params(&none).expect("empty"), []);
        let bad = json::parse(r#"{"params":{"a":[1]}}"#).expect("frame");
        assert!(frame_params(&bad).is_err());
    }
}
