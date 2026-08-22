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
//! unfolded so a multi-line statement can travel on one line. The
//! fold carries the statement's structure and stops at the quotes: a
//! string, a delimited identifier and a byte string arrive as they
//! were written, so `'a\\b'` is the backslash the standard says it is
//! rather than a backspace. A
//! parameter is JSON and takes the value JSON says it is, except for
//! the two references a statement can be handed: `{"$graph": "/social"}`
//! is the graph at that path and `{"$table": {"columns": [], "rows": []}}`
//! is a binding table written out.
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
    // No file named, or `:memory:` named, is a database in memory:
    // one nothing has to be cleaned up after, which is what somebody
    // trying the language out wants and what a file in the working
    // directory they did not ask for is the opposite of.
    let memory = matches!(path, None | Some(":memory:"));
    let path = path.unwrap_or(":memory:");
    let opened = match memory {
        true => Session::memory(),
        false => Session::open(std::path::Path::new(path)),
    };
    let mut session = match opened {
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
            let params = match frame_params(session, &frame) {
                Ok(p) => p,
                Err(fault) => return (fault.line(), false),
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
    let params = match frame_params(session, frame) {
        Ok(p) => p,
        Err(fault) => return fault.line(),
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

/// What can go wrong while a frame's parameters are being read.
///
/// The two are answered differently and that is the whole reason the
/// enum is here. A `params` that is not an object is a protocol fault,
/// which has no GQLSTATUS because the standard defines no condition for
/// a client that sent the wrong shape. A `$graph` naming a graph the
/// catalog does not hold is a condition the standard does define,
/// `42002`, and the reply says so the same way it would if the
/// statement had named the graph in its own text.
#[derive(Debug)]
enum Fault {
    Protocol(String),
    Engine(zu::ZuError),
}

impl Fault {
    /// The one-line reply this fault is answered with.
    fn line(&self) -> String {
        match self {
            Fault::Protocol(message) => error_line(message),
            Fault::Engine(error) => failure_line(error),
        }
    }
}

/// Pulls the optional `params` object out of a frame and types each
/// value the way the query engine binds it.
fn frame_params(session: &mut Session, frame: &Json) -> Result<Vec<(String, Value)>, Fault> {
    let Some(params) = frame.get("params") else {
        return Ok(Vec::new());
    };
    let Json::Obj(fields) = params else {
        return Err(Fault::Protocol("params must be an object".to_string()));
    };
    let mut out = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        out.push((name.clone(), param_value(session, value)?));
    }
    Ok(out)
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
///
/// The two exceptions are the references, GE04 and GE05. A graph and a
/// binding table are values a statement may be handed, and neither has
/// a JSON shape of its own, so each is written as an object with one
/// member whose name begins with a dollar sign: `{"$graph": "/social"}`
/// and `{"$table": {"columns": [...], "rows": [[...]]}}`. A dollar sign
/// cannot begin an identifier, so no record a statement could write out
/// loses its spelling to this, and the word is the same word the
/// statement writes in front of the parameter.
fn param_value(session: &mut Session, v: &Json) -> Result<Value, Fault> {
    Ok(match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Str(s.clone()),
        Json::Arr(items) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                list.push(param_value(session, item)?);
            }
            Value::List(list)
        }
        Json::Obj(fields) => match reference(fields) {
            Some(("$graph", value)) => graph_param(session, value)?,
            Some(("$table", value)) => table_param(session, value)?,
            Some((word, _)) => {
                return Err(Fault::Protocol(format!(
                    "{word} is not a reference this wire knows, which is $graph or $table"
                )));
            }
            None => {
                let mut members = Vec::with_capacity(fields.len());
                for (field, item) in fields {
                    members.push((field.clone(), param_value(session, item)?));
                }
                Value::record(members)
            }
        },
    })
}

/// The one member of an object whose name begins with a dollar sign,
/// and `None` when the object is an ordinary record. An object that
/// mixes the two is a reference with the rest ignored rather than a
/// record with a strange field, because a client that wrote `$graph`
/// meant a graph.
fn reference(fields: &[(String, Json)]) -> Option<(&str, &Json)> {
    fields
        .iter()
        .find(|(name, _)| name.starts_with('$'))
        .map(|(name, value)| (name.as_str(), value))
}

/// `{"$graph": "/social"}`, a graph reference written the way a
/// statement writes one. That is either the path a graph is at, where
/// the last segment is the graph and what stands in front of it is the
/// schema, or one of the words that name a graph without naming it,
/// which is what a client that does not know the paths of the engine
/// it is talking to has to write.
fn graph_param(session: &mut Session, value: &Json) -> Result<Value, Fault> {
    let Json::Str(path) = value else {
        return Err(Fault::Protocol(
            "$graph takes a graph reference, which is a path or one of the graph words".to_string(),
        ));
    };
    if path.eq_ignore_ascii_case("CURRENT_GRAPH")
        || path.eq_ignore_ascii_case("CURRENT_PROPERTY_GRAPH")
    {
        return session.working_graph_ref().map_err(Fault::Engine);
    }
    if path.eq_ignore_ascii_case("HOME_GRAPH") || path.eq_ignore_ascii_case("HOME_PROPERTY_GRAPH") {
        return session.home_graph_ref().map_err(Fault::Engine);
    }
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let (schema, name) = match trimmed.rsplit_once('/') {
        Some((head, name)) => (format!("/{head}"), name),
        None => ("/".to_string(), trimmed),
    };
    session.graph_ref(&schema, name).map_err(Fault::Engine)
}

/// `{"$table": {"columns": [...], "rows": [[...]]}}`, a binding table
/// written out. The rows carry values and not element references,
/// which is the same limit the wire has everywhere else: a node is an
/// offset in a snapshot and nothing a client holds can name one.
fn table_param(session: &mut Session, value: &Json) -> Result<Value, Fault> {
    let shape = "$table takes an object with a columns array and a rows array of arrays";
    let (Some(Json::Arr(columns)), Some(Json::Arr(rows))) =
        (value.get("columns"), value.get("rows"))
    else {
        return Err(Fault::Protocol(shape.to_string()));
    };
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        match column {
            Json::Str(name) => names.push(name.clone()),
            _ => return Err(Fault::Protocol(shape.to_string())),
        }
    }
    let mut table = Vec::with_capacity(rows.len());
    for row in rows {
        let Json::Arr(cells) = row else {
            return Err(Fault::Protocol(shape.to_string()));
        };
        if cells.len() != names.len() {
            return Err(Fault::Protocol(format!(
                "a row of {} values in a table of {} columns",
                cells.len(),
                names.len()
            )));
        }
        let mut values = Vec::with_capacity(cells.len());
        for cell in cells {
            values.push(param_value(session, cell)?);
        }
        table.push(values);
    }
    Ok(session.binding_table(zu::query::QueryResult::new(names, table)))
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
///
/// The fold carries a statement's structure and none of a statement's
/// structure lives inside a string, so a quoted sequence is copied
/// through as it was written. `'a\\b'` is how ISO spells one backslash
/// between two letters, and a fold that reached into it would hand the
/// engine `'a\b'`, which is a backspace. Staying out means reading the
/// same lexical layer the engine reads: the three quotes, the quote
/// written twice to mean itself, the `@` form where a backslash is a
/// backslash, the byte string, and the two comments.
fn unfold(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut ix = 0;
    while ix < bytes.len() {
        let b = bytes[ix];
        // A word is taken whole so that the `x` that ends `max` cannot
        // open a byte string the way the `x` of `x'01'` does, which is
        // the rule the lexer reads them under.
        if b.is_ascii_alphanumeric() || b == b'_' {
            let word = ix;
            while ix < bytes.len() && (bytes[ix].is_ascii_alphanumeric() || bytes[ix] == b'_') {
                ix += 1;
            }
            out.push_str(&line[word..ix]);
            if matches!(&line[word..ix], "X" | "x") && bytes.get(ix) == Some(&b'\'') {
                ix = copy_byte_string(line, ix, &mut out);
            }
            continue;
        }
        match b {
            b'\'' | b'"' | b'`' => ix = copy_quoted(line, ix, true, &mut out),
            // GL11's `@` turns the escapes off inside the sequence it
            // opens, and a lone `@` is nothing at all.
            b'@' if matches!(bytes.get(ix + 1), Some(b'\'' | b'"' | b'`')) => {
                out.push('@');
                ix = copy_quoted(line, ix + 1, false, &mut out);
            }
            b'/' if bytes.get(ix + 1) == Some(&b'*') => ix = copy_block_comment(line, ix, &mut out),
            b'/' | b'-' if bytes.get(ix + 1) == Some(&b) => {
                out.push_str(&line[ix..ix + 2]);
                ix = unfold_line_comment(line, ix + 2, &mut out);
            }
            b'\\' => ix = unfold_escape(line, ix, &mut out),
            _ => {
                let ch = line[ix..].chars().next().expect("a char at a boundary");
                out.push(ch);
                ix += ch.len_utf8();
            }
        }
    }
    out
}

/// Writes what the escape at `at` stands for and answers the index
/// after it. The caller has seen the backslash. A pair the fold does
/// not use is written back as it came, which is what lets a statement
/// that was never folded through unchanged.
fn unfold_escape(line: &str, at: usize, out: &mut String) -> usize {
    match line[at + 1..].chars().next() {
        Some('n') => {
            out.push('\n');
            at + 2
        }
        Some('t') => {
            out.push('\t');
            at + 2
        }
        Some('\\') => {
            out.push('\\');
            at + 2
        }
        Some(other) => {
            out.push('\\');
            out.push(other);
            at + 1 + other.len_utf8()
        }
        None => {
            out.push('\\');
            at + 1
        }
    }
}

/// Copies the quoted sequence opening at `open` through as written and
/// answers the index after it. `escapes` is off for the `@` form, where
/// a backslash is one character and writing the quote twice is the only
/// way one gets in.
///
/// A sequence with no closing quote takes the rest of the line, which
/// leaves the engine to say it is unterminated. That is the right
/// answer and it is the same one an unfolded statement would have got.
fn copy_quoted(line: &str, open: usize, escapes: bool, out: &mut String) -> usize {
    let bytes = line.as_bytes();
    let quote = bytes[open];
    out.push(quote as char);
    let mut ix = open + 1;
    while ix < bytes.len() {
        if bytes[ix] == quote {
            let doubled = bytes.get(ix + 1) == Some(&quote);
            out.push_str(&line[ix..ix + 1 + usize::from(doubled)]);
            ix += 1 + usize::from(doubled);
            if !doubled {
                return ix;
            }
            continue;
        }
        // The escaped character is copied with its backslash, which is
        // what keeps `'it\'s'` from ending three characters early. A
        // backslash with nothing after it is one more character.
        let width = usize::from(escapes && bytes[ix] == b'\\' && ix + 1 < bytes.len());
        let ch = line[ix + width..]
            .chars()
            .next()
            .expect("a char at a boundary");
        out.push_str(&line[ix..ix + width + ch.len_utf8()]);
        ix += width + ch.len_utf8();
    }
    ix
}

/// Copies GL08's byte string through as written and answers the index
/// after it. The caller has seen the `X` and the quote at `open`.
///
/// It is not the same shape as the other three: there are no escapes,
/// and a quote written twice is an empty byte string followed by
/// another one rather than a quote standing for itself.
fn copy_byte_string(line: &str, open: usize, out: &mut String) -> usize {
    let bytes = line.as_bytes();
    out.push('\'');
    let mut ix = open + 1;
    while ix < bytes.len() {
        let ch = line[ix..].chars().next().expect("a char at a boundary");
        out.push(ch);
        ix += ch.len_utf8();
        if ch == '\'' {
            return ix;
        }
    }
    ix
}

/// Unfolds a comment running to the end of the line and answers the
/// index after it. The caller has seen the two solidi or the two minus
/// signs.
///
/// A comment is unfolded rather than copied, and it is the one place
/// where undoing the fold is what makes the statement work rather than
/// what breaks it: the newline the fold wrote as `\n` is the only thing
/// that ends the comment, and a comment left folded would swallow every
/// statement after it. Nothing else in here is worth touching, so the
/// rest goes through as written.
fn unfold_line_comment(line: &str, at: usize, out: &mut String) -> usize {
    let bytes = line.as_bytes();
    let mut ix = at;
    while ix < bytes.len() {
        if bytes[ix] == b'\n' {
            out.push('\n');
            return ix + 1;
        }
        if bytes[ix] == b'\\' && bytes.get(ix + 1) == Some(&b'n') {
            out.push('\n');
            return ix + 2;
        }
        let ch = line[ix..].chars().next().expect("a char at a boundary");
        out.push(ch);
        ix += ch.len_utf8();
    }
    ix
}

/// Copies a block comment through as written and answers the index
/// after it. The caller has seen the `/*`.
///
/// Nothing inside one is read by anybody, so a newline the fold wrote
/// as `\n` can stay written that way; what matters is finding the `*/`
/// without mistaking a quote or an apostrophe in the prose for the
/// start of a string.
fn copy_block_comment(line: &str, at: usize, out: &mut String) -> usize {
    let bytes = line.as_bytes();
    out.push_str("/*");
    let mut ix = at + 2;
    while ix < bytes.len() {
        if bytes[ix] == b'*' && bytes.get(ix + 1) == Some(&b'/') {
            out.push_str("*/");
            return ix + 2;
        }
        let ch = line[ix..].chars().next().expect("a char at a boundary");
        out.push(ch);
        ix += ch.len_utf8();
    }
    ix
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

    /// A string is the statement's data and the fold carries the
    /// statement's structure, so the fold does not reach into one. Each
    /// of these was wrong before: `'a\\b'` came out as `'a\b'`, which
    /// the engine reads as a backspace, and `'\\'` came out as `'\'`,
    /// which it reads as a string nothing closes.
    #[test]
    fn a_quoted_sequence_keeps_the_backslashes_it_was_written_with() {
        for written in [
            r"RETURN 'a\\b' AS v",
            r"RETURN '\\' AS v",
            r"RETURN 'a\nb' AS v",
            r"RETURN 'a\tb' AS v",
            r#"RETURN "a\\b" AS v"#,
            r"RETURN 1 AS `a\\b`",
            r"RETURN 'it''s\\' AS v",
            r"RETURN @'a\\b' AS v",
            r"RETURN X'01AF' AS v",
        ] {
            assert_eq!(unfold(written), written, "{written}");
        }
    }

    /// The fold picks up again after the sequence ends, which is what
    /// says the scanner found the end rather than giving up on the
    /// line. The third of these is the reason the escapes are read at
    /// all: `\'` does not close the string, so a scanner that ignored
    /// it would call the rest of the statement string and leave the
    /// newline folded.
    #[test]
    fn the_fold_resumes_after_a_quoted_sequence() {
        assert_eq!(
            unfold(r"RETURN 'a\\b' AS v\nRETURN 2"),
            "RETURN 'a\\\\b' AS v\nRETURN 2"
        );
        assert_eq!(
            unfold(r"RETURN 'a''b' AS v\nRETURN 2"),
            "RETURN 'a''b' AS v\nRETURN 2"
        );
        assert_eq!(
            unfold(r"RETURN 'it\'s' AS v\nRETURN 2"),
            "RETURN 'it\\'s' AS v\nRETURN 2"
        );
        // The `@` form has no escapes, so the backslash is a character
        // and the quote after it still ends the sequence.
        assert_eq!(
            unfold(r"RETURN @'a\b' AS v\nRETURN 2"),
            "RETURN @'a\\b' AS v\nRETURN 2"
        );
        // An empty byte string is two quotes that do not mean a quote,
        // and reading them as one would swallow the fold after them.
        assert_eq!(
            unfold(r"RETURN X'' AS v\nRETURN 2"),
            "RETURN X'' AS v\nRETURN 2"
        );
        // The `x` that ends a word does not open one.
        assert_eq!(
            unfold(r"RETURN max'a\\b'\nRETURN 2"),
            "RETURN max'a\\\\b'\nRETURN 2"
        );
    }

    /// A comment is the one place the fold has to be undone rather than
    /// left alone, because the newline it wrote as `\n` is the only
    /// thing that ends a comment to the end of the line. The
    /// apostrophes are there on purpose: a scanner that read them as
    /// quotes would call the rest of the line a string and lose the
    /// newline, and the comment would swallow the statement.
    #[test]
    fn a_comment_is_unfolded_rather_than_copied() {
        assert_eq!(unfold(r"// it's fine\nRETURN 1"), "// it's fine\nRETURN 1");
        assert_eq!(unfold(r"-- it's fine\nRETURN 1"), "-- it's fine\nRETURN 1");
        assert_eq!(
            unfold(r"/* it's fine */ RETURN 'a\\b'"),
            r"/* it's fine */ RETURN 'a\\b'"
        );
        assert_eq!(
            unfold(r"RETURN 1 // x\nRETURN 2"),
            "RETURN 1 // x\nRETURN 2"
        );
        // A real newline ends one as well, since a client is free to
        // send a statement that was never folded.
        assert_eq!(unfold("// x\nRETURN 1"), "// x\nRETURN 1");
    }

    /// The ends of the line, which is where a scanner that assumed a
    /// closing character would panic rather than answer.
    #[test]
    fn an_unfinished_line_is_still_a_line() {
        for written in [
            r"RETURN 'a",
            r"RETURN 'a\",
            r"RETURN @'a",
            r"RETURN X'0",
            r"RETURN 1 /* a",
            r"RETURN 1 //",
            r"RETURN 1 \",
            r"RETURN '",
            "@",
            "",
        ] {
            unfold(written);
        }
    }

    /// A session on nothing, which every parameter test binds against
    /// because a reference parameter is a value only a session can
    /// hand out.
    fn session() -> Session {
        Session::memory().expect("a session on nothing")
    }

    #[test]
    fn frame_params_keep_their_types() {
        let mut session = session();
        let frame =
            json::parse(r#"{"params":{"a":1,"b":2.5,"c":"x","d":true,"e":null}}"#).expect("frame");
        let params = frame_params(&mut session, &frame).expect("params");
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
        assert_eq!(frame_params(&mut session, &none).expect("empty"), []);
    }

    #[test]
    fn a_list_parameter_arrives_as_a_list_and_an_object_as_a_record() {
        let mut session = session();
        let frame = json::parse(r#"{"params":{"a":[1,[2,"x"]],"b":{"y":2,"x":1}}}"#).expect("f");
        let params = frame_params(&mut session, &frame).expect("params");
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

    /// GE04 and GE05 over the wire. A graph and a binding table are
    /// the two values a client can be holding that JSON has no shape
    /// for, and each is written as an object with one dollar named
    /// member.
    #[test]
    fn the_two_references_have_a_spelling_of_their_own() {
        let mut session = session();
        let frame = json::parse(
            r#"{"params":{"g":{"$graph":"/home"},"c":{"$graph":"CURRENT_PROPERTY_GRAPH"},
                "t":{"$table":{"columns":["id","name"],"rows":[[1,"a"],[2,"b"]]}}}}"#,
        )
        .expect("frame");
        let params = frame_params(&mut session, &frame).expect("params");
        let by = |want: &str| {
            params
                .iter()
                .find(|(name, _)| name == want)
                .map(|(_, value)| value.clone())
                .expect("a parameter of that name")
        };
        // The path and the word name one graph here, because nothing
        // has moved the session out of the graph it started in.
        assert_eq!(by("g"), by("c"));
        assert!(matches!(by("g"), Value::Graph(_)));
        match by("t") {
            Value::BindingTable(table) => {
                assert_eq!(table.columns(), ["id".to_string(), "name".to_string()]);
                assert_eq!(table.rows().len(), 2);
                assert_eq!(table.rows()[1][1], Value::Str("b".into()));
            }
            other => panic!("a binding table, not {other:?}"),
        }
    }

    /// A graph the catalog does not hold is `42002`, the condition the
    /// statement would raise if it had named the graph itself, and a
    /// malformed reference is a protocol fault with no condition at
    /// all.
    #[test]
    fn a_reference_that_names_nothing_is_answered_the_way_the_statement_would_be() {
        let mut session = session();
        let missing = json::parse(r#"{"params":{"g":{"$graph":"/nowhere"}}}"#).expect("frame");
        match frame_params(&mut session, &missing) {
            Err(Fault::Engine(error)) => assert_eq!(
                error.diagnostic().expect("a condition").status.code(),
                "42002"
            ),
            Err(Fault::Protocol(message)) => panic!("a condition, not {message}"),
            Ok(_) => panic!("a graph that is not there"),
        }

        let shapeless = json::parse(r#"{"params":{"t":{"$table":[1,2]}}}"#).expect("frame");
        assert!(matches!(
            frame_params(&mut session, &shapeless),
            Err(Fault::Protocol(_))
        ));
        let unknown = json::parse(r#"{"params":{"x":{"$node":1}}}"#).expect("frame");
        assert!(matches!(
            frame_params(&mut session, &unknown),
            Err(Fault::Protocol(_))
        ));
    }
}
