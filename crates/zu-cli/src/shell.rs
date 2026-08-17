//! `zu shell`: a persistent query loop over one file. This is the
//! process a harness or an editor keeps alive so the catalog, stats,
//! plan cache, and decoded block caches pay their cost once instead of
//! once per query; the one-shot `zu query` path stays for scripts.
//!
//! It speaks to whoever is there. A person at a terminal gets the
//! editor in [`crate::repl`], with a prompt, history and multi-line
//! statements; a program gets one JSON object per line, which is the
//! rest of this file. Nothing has to be passed to choose, because the
//! question "is standard input a terminal" already has the answer, and
//! `--format jsonl` is there for the harness that wants to say so
//! anyway, or that runs zu under a pty and would otherwise be handed a
//! prompt it never asked for.
//!
//! Input is line-oriented and comes in two spellings. A line starting
//! with `{` is a frame: `{"op":"query","q":"...","params":{...}}`,
//! plus `prepare`, `execute`, `close_stmt`, `explain`,
//! `explain_analyze`, and `quit`. Any other non-empty line is a bare
//! statement run with no parameters, with `\n`, `\t`, and `\\`
//! unfolded so a multi-line statement can travel on one line.
//! Responses are `{"gqlstatus":...,"columns":...,"rows":...}` for
//! results, `{"text":"..."}` for the two explain frames, and
//! `{"error":"..."}` for failures, and an error never kills the loop:
//! the session and its caches survive a bad statement.
//!
//! The first line out is the greeting, before any line has come in:
//! `{"protocol":1,...}` with the build's versions and the file that was
//! opened. It is what makes this a protocol rather than an output
//! format, because a client reads one line and knows whether it is
//! talking to something it understands; `{"op":"hello"}` asks for the
//! same object again, for the client that attached to a session already
//! running. The number moves when a frame or a reply changes meaning,
//! never when one is added, so a reader that ignores fields it does not
//! know keeps working across a release.
//!
//! Both response shapes carry GQLSTATUS when there is one
//! (Spec/2064g/gql/plan/07). A successful result leads with the
//! completion condition, `00000` or `00001` when the statement had no
//! projection, and grows a `"notices"` array if it raised something it
//! survived. A failure the engine raised grows a `"failure"` object with
//! the code, the standard's text and the severity. A failure the
//! *protocol* raised, a malformed frame or an unknown op, has no code and
//! does not pretend to: those are not conditions the standard defines,
//! and a reader can tell the two apart by whether `"failure"` is there.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use zu::query::Value;
use zu::session::Session;

use zu_json::{self as json, Json};

/// The version of the wire this file speaks, `dx/12` §5.
///
/// It moves when a frame or a reply changes meaning and not when one is
/// added, because a client that reads the fields it knows and ignores
/// the rest is a client a release cannot break, and a number that moved
/// for every addition would make every client demand an exact match.
const PROTOCOL: u32 = 1;

/// The first line of a session, and the answer to `{"op":"hello"}`.
///
/// The build facts are spelled the way `zu version --format json`
/// spells them, so a caller reading both learns one vocabulary rather
/// than two, and the file is here because a client that opened a
/// session through a wrapper script may not know which one it got.
fn greeting(path: &str) -> String {
    let strings =
        |items: &[&str]| Json::Arr(items.iter().map(|s| Json::Str((*s).into())).collect());
    let features = crate::features();
    let mut line = Json::Obj(vec![
        ("protocol".into(), Json::Int(i64::from(PROTOCOL))),
        ("zu".into(), Json::Str(crate::VERSION.into())),
        ("c_abi".into(), Json::Str(zu::C_ABI_VERSION.into())),
        (
            "format_version".into(),
            Json::Int(i64::from(zu::zu1::FORMAT_VERSION)),
        ),
        ("features".into(), strings(&features)),
        ("file".into(), Json::Str(path.into())),
    ])
    .to_compact();
    line.push('\n');
    line
}

pub(crate) fn shell_command(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut jsonl = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // jsonl is the only wire format, and asking for it is how a
            // caller says it is a program even when it is holding a
            // terminal open.
            "--format" | "-f" => match args.get(i + 1).map(String::as_str) {
                Some("jsonl") => {
                    jsonl = true;
                    i += 2;
                }
                _ => return crate::usage_error("shell"),
            },
            arg if arg.starts_with('-') => return crate::usage_error("shell"),
            arg if path.is_none() => {
                path = Some(arg);
                i += 1;
            }
            _ => return crate::usage_error("shell"),
        }
    }
    let Some(path) = path else {
        return crate::usage_error("shell");
    };
    let mut session = match Session::open(std::path::Path::new(path)) {
        Ok(s) => s,
        Err(e) => return crate::command_error("shell", &e),
    };
    if !jsonl && crate::term::interactive() {
        return crate::repl::run(&mut session, path);
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    // Said before anything is asked, so a client that will not speak
    // this version finds out without having run a statement.
    if out.write_all(greeting(path).as_bytes()).is_err() || out.flush().is_err() {
        return ExitCode::SUCCESS;
    }
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (response, quit) = respond(&mut session, path, line);
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
fn respond(session: &mut Session, path: &str, line: &str) -> (String, bool) {
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
        Some("explain") => (explain_frame(session, &frame), false),
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
        Some("hello") => (greeting(path), false),
        Some("quit") => ("{\"bye\":true}\n".to_string(), true),
        Some(op) => (error_line(&format!("unknown op {op:?}")), false),
        None => (error_line("frame needs a string op"), false),
    }
}

/// Answers an `explain` frame with the plan and nothing else.
///
/// The frame carries `q` and no parameters. `explain_analyze` needs
/// them because it runs the statement; this does not run anything, and
/// zu's plan is a function of the text and the schema, so a `params`
/// object here would be a field with no effect and a reader would be
/// entitled to assume it had one.
///
/// The reply is `{"text":...}`, the same shape `explain_analyze`
/// answers with, because the two differ in what the listing contains
/// and not in what a caller has to do with it.
fn explain_frame(session: &mut Session, frame: &Json) -> String {
    let Some(q) = frame.get("q").and_then(Json::as_str) else {
        return error_line("explain needs a string q");
    };
    match session.explain(q) {
        Ok(text) => {
            let mut s = String::from("{\"text\":");
            crate::write_json_str(&mut s, &text);
            s.push_str("}\n");
            s
        }
        Err(e) => failure_line(&e),
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
    Ok(fields
        .iter()
        .map(|(name, v)| (name.clone(), param_value(v)))
        .collect())
}

/// One parameter, typed the way the engine binds it.
///
/// The five scalars are the same in both languages. A JSON array is a
/// list and a JSON object is a record, which is the mapping the data
/// model already implies: a record is named fields and an object is
/// named members, so a client sending the obvious thing gets the
/// obvious value, nested to whatever depth it wrote. What has no
/// spelling here is a temporal, because JSON has no date and a string
/// that looks like one is a string: a statement wanting one calls
/// `date($text)` on a string parameter, which says which calendar type
/// was meant instead of leaving the wire to guess.
fn param_value(v: &Json) -> Value {
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Str(s.clone()),
        Json::Arr(items) => Value::List(items.iter().map(param_value).collect()),
        Json::Obj(fields) => Value::record(
            fields
                .iter()
                .map(|(field, item)| (field.clone(), param_value(item)))
                .collect(),
        ),
    }
}

/// A failure with no GQLSTATUS: a malformed frame, an unknown op, a
/// `params` that is not an object. These are protocol faults, not GQL
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
    }

    #[test]
    fn a_list_parameter_arrives_as_a_list_and_an_object_as_a_record() {
        let frame = json::parse(r#"{"params":{"a":[1,[2,"x"]],"b":{"y":2,"x":1}}}"#).expect("f");
        let params = frame_params(&frame).expect("params");
        assert_eq!(
            params[0].1,
            Value::List(vec![
                Value::Int(1),
                Value::List(vec![Value::Int(2), Value::Str("x".into())]),
            ])
        );
        // A record's fields are in name order whatever order they were
        // written in, which is what makes two spellings one value.
        assert_eq!(
            params[1].1,
            Value::record(vec![
                ("x".to_string(), Value::Int(1)),
                ("y".to_string(), Value::Int(2)),
            ])
        );
    }
}
