//! The backslash commands: the questions the shell answers itself.
//!
//! `\d` and the rest are not zuQL and never reach the engine. They are
//! here because the questions they answer, what is in this file and
//! what columns does that table have, are the ones a person asks
//! between statements, and asking them in the language would mean
//! knowing a catalog view the language does not have. The spellings are
//! psql's, because a database user arrives with those in their fingers
//! and a shell that renamed them would be teaching a second set for
//! nothing.
//!
//! Parsing is apart from answering. [`parse`] turns a line into a
//! [`Command`] and touches nothing else, [`tables`], [`graphs`] and
//! [`describe`] turn a catalog and a column list into text, and the
//! loop in [`crate::repl`] is the one piece that holds the session and
//! joins the two. Everything here is testable without a terminal, and
//! everything but the gathering without a file.

use zu::session::Pin;
use zu::zu1::catalog::{Catalog, GraphTypeOf};

/// What a backslash line asked for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    /// The list of these commands.
    Help,
    /// Every table in the file.
    Tables,
    /// One table's columns.
    Describe(&'a str),
    /// Every graph in the file.
    Graphs,
    /// What this session is set to: schema, graph, time zone and the
    /// parameters it holds.
    SessionState,
    /// Turn timing on, off, or over.
    Timing(Option<bool>),
    /// Run the statements in a file.
    Include(&'a str),
    /// Leave, the same way `quit` does.
    Quit,
    /// A line the table has no entry for, or one whose argument is
    /// missing. The string is the sentence to print, because the parser
    /// is the only place that knows which of the two went wrong.
    Wrong(String),
}

/// One backslash command as a reader meets it in `\?`.
///
/// The synopsis is held to the parser by a test, brackets included: a
/// command documented here that nobody wrote, or one whose argument the
/// brackets call optional and the parser insists on, is a test failure
/// rather than a line a user types once and gives up on.
pub(crate) struct Entry {
    /// What is printed, with the argument in the brackets when it may
    /// be left out and bare when it may not.
    pub(crate) synopsis: &'static str,
    /// One line, lowercase, no full stop, printed in a column beside
    /// the others.
    pub(crate) summary: &'static str,
}

/// Every backslash command this shell answers, in the order a user
/// meets them: what is here, what is in it, then the two that do
/// something.
pub(crate) const COMMANDS: &[Entry] = &[
    Entry {
        synopsis: "\\?",
        summary: "this list",
    },
    Entry {
        synopsis: "\\d [NAME]",
        summary: "the tables in the file, or one table's columns",
    },
    Entry {
        synopsis: "\\l",
        summary: "the graphs in the file",
    },
    Entry {
        synopsis: "\\session",
        summary: "the schema, graph, time zone and parameters of this session",
    },
    Entry {
        synopsis: "\\i FILE",
        summary: "run the statements in a file",
    },
    Entry {
        synopsis: "\\timing [on|off]",
        summary: "print how long each statement took",
    },
    Entry {
        synopsis: "\\q",
        summary: "leave the shell",
    },
];

/// What this line asked for, or `None` when it is not a backslash line
/// and belongs to the engine.
///
/// The whole rest of the line is the argument, unsplit and unquoted, so
/// `\i /tmp/two words.gql` reads the file it names. A backslash command
/// takes one argument or none, which is what makes that affordable, and
/// a shell that made people quote a path would be a shell that made
/// them quote every path.
pub(crate) fn parse(line: &str) -> Option<Command<'_>> {
    let rest = line.trim().strip_prefix('\\')?;
    let (name, arg) = match rest.find(char::is_whitespace) {
        Some(at) => (&rest[..at], rest[at..].trim()),
        None => (rest, ""),
    };
    Some(match name {
        "?" | "h" => Command::Help,
        "d" if arg.is_empty() => Command::Tables,
        "d" => Command::Describe(arg),
        "l" => Command::Graphs,
        "session" => Command::SessionState,
        "i" if arg.is_empty() => Command::Wrong("\\i wants a file to read".into()),
        "i" => Command::Include(arg),
        "timing" => match arg {
            "" => Command::Timing(None),
            "on" => Command::Timing(Some(true)),
            "off" => Command::Timing(Some(false)),
            other => Command::Wrong(format!("\\timing takes on or off, not {other}")),
        },
        "q" => Command::Quit,
        // The empty name is a lone backslash, which is worth its own
        // sentence: a user who typed one meant to type a command.
        "" => Command::Wrong("a backslash on its own is not a command. \\? lists them".into()),
        other => Command::Wrong(format!(
            "no backslash command called \\{other}. \\? lists them"
        )),
    })
}

/// The command list, as `\?` prints it.
pub(crate) fn help() -> String {
    let rows: Vec<Vec<String>> = COMMANDS
        .iter()
        .map(|c| vec![c.synopsis.to_string(), c.summary.to_string()])
        .collect();
    crate::aligned(&["command", "does"], &rows)
}

/// Every table in the file, node tables before rel tables.
///
/// The count column is rows for a node table and edges for a rel table,
/// which are the same thing counted in the two places, and the detail
/// column is where a rel table says what it joins. A file with no
/// tables says so in a sentence rather than printing a header over
/// nothing.
pub(crate) fn tables(catalog: &Catalog) -> String {
    let graph = |id| graph_name(catalog, id);
    let node = |id| {
        catalog
            .node_by_id(id)
            .map_or("?".to_string(), |t| t.name.clone())
    };
    let mut rows: Vec<Vec<String>> = catalog
        .node_tables()
        .iter()
        .map(|t| {
            vec![
                "node".to_string(),
                t.name.clone(),
                graph(t.graph),
                t.node_count.to_string(),
                String::new(),
            ]
        })
        .collect();
    rows.extend(catalog.rel_tables().iter().map(|t| {
        vec![
            "rel".to_string(),
            t.name.clone(),
            graph(t.graph),
            t.edge_count.to_string(),
            format!(
                "{} {} {}",
                node(t.from),
                if t.undirected { "--" } else { "->" },
                node(t.to)
            ),
        ]
    }));
    if rows.is_empty() {
        return "no tables in this file\n".to_string();
    }
    let n = rows.len();
    let mut out = crate::aligned(&["kind", "name", "graph", "count", "joins"], &rows);
    out.push_str(&count(n, "table"));
    out
}

/// Every graph in the file, with the schema it lives in and the type it
/// was created with.
pub(crate) fn graphs(catalog: &Catalog) -> String {
    let rows: Vec<Vec<String>> = catalog
        .graphs()
        .iter()
        .map(|g| {
            vec![
                g.name.clone(),
                g.schema.clone(),
                match &g.graph_type {
                    GraphTypeOf::Open => "open".to_string(),
                    GraphTypeOf::Named(name) => name.clone(),
                    GraphTypeOf::Inline(_) => "inline".to_string(),
                },
            ]
        })
        .collect();
    let n = rows.len();
    let mut out = crate::aligned(&["name", "schema", "type"], &rows);
    out.push_str(&count(n, "graph"));
    out
}

/// What a session is set to, as `\session` prints it (ISO 7.1, GS01
/// through GS16).
///
/// Four lines and then a table, because the four are what every session
/// has and the table is what this one has been given. The zone is
/// written the way a statement writes it, `+07:00` rather than 420, so
/// that a reader who wants to put it back can copy it.
///
/// The epoch column is the pin. It is empty for a value parameter,
/// which pins nothing, and holds the epoch the reference was taken at
/// for a graph or a binding table, with a word beside it when that
/// epoch has gone stale. A pin holds no blocks: a binding table
/// parameter holds rows copied out of the snapshot, so an old one costs
/// the memory of its rows and nothing on the file.
pub(crate) fn session_state(
    schema: &str,
    graph: &str,
    zone: i16,
    epoch: u64,
    params: &[(&str, &'static str, String)],
    pins: &[Pin],
) -> String {
    let mut out = format!(
        "schema:    {schema}\ngraph:     {graph}\ntime zone: {}\nepoch:     {epoch}\n",
        zone_text(zone)
    );
    if params.is_empty() {
        out.push_str("no parameters set\n");
        return out;
    }
    let rows: Vec<Vec<String>> = params
        .iter()
        .map(|(name, kind, value)| {
            let pin = pins.iter().find(|p| p.name == *name);
            vec![
                format!("${name}"),
                kind.to_string(),
                value.clone(),
                match pin {
                    Some(pin) => match pin.stale {
                        zu::session::Stale::Fresh => pin.epoch.to_string(),
                        zu::session::Stale::Old => format!("{} (old)", pin.epoch),
                        zu::session::Stale::Gone => format!("{} (gone)", pin.epoch),
                    },
                    None => String::new(),
                },
            ]
        })
        .collect();
    let n = rows.len();
    out.push_str(&crate::aligned(
        &["name", "kind", "value", "pinned at"],
        &rows,
    ));
    out.push_str(&count(n, "parameter"));
    out
}

/// A time zone displacement as a statement writes it, minutes east of
/// UTC turned back into the sign, hours and minutes they came from.
fn zone_text(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// One column of a table, as the file stores it.
///
/// The caller gathers these, because the columns are in the property
/// directory rather than in the catalog and reading one wants the file.
/// What is left here is the part that is a function of its argument.
pub(crate) struct Column {
    pub(crate) name: String,
    /// The declared type, spelled the way the language spells it.
    pub(crate) ty: String,
    /// Whether any row leaves the property out.
    pub(crate) nullable: bool,
}

/// One table, its shape and its columns.
///
/// A name that is neither a node table nor a rel table gets a sentence
/// and a pointer at `\d`, since the likeliest reason to be here is a
/// spelling, and the list is the answer to a spelling.
pub(crate) fn describe(catalog: &Catalog, name: &str, columns: &[Column]) -> String {
    let mut out = String::new();
    if let Some(t) = catalog.node_by_name(name) {
        out.push_str(&format!(
            "node table {} in graph {}, {} rows\n",
            t.name,
            graph_name(catalog, t.graph),
            t.node_count
        ));
        // The table's own name is the first label and every row carries
        // it, so a table with one label has nothing to add here.
        let labels: Vec<&str> = t
            .labels
            .iter()
            .filter_map(|&id| catalog.label_name(id))
            .collect();
        if labels.len() > 1 {
            out.push_str(&format!("labels: {}\n", labels.join(", ")));
        }
    } else if let Some(t) = catalog.rel_by_name(name) {
        out.push_str(&format!(
            "rel table {} in graph {}, {} edges, {} {} {}\n",
            t.name,
            graph_name(catalog, t.graph),
            t.edge_count,
            endpoint(catalog, t.from),
            if t.undirected { "--" } else { "->" },
            endpoint(catalog, t.to)
        ));
    } else {
        return format!("no table called {name}. \\d lists them\n");
    }
    if columns.is_empty() {
        out.push_str("no columns\n");
        return out;
    }
    let rows: Vec<Vec<String>> = columns
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                c.ty.clone(),
                if c.nullable { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    out.push_str(&crate::aligned(&["column", "type", "null"], &rows));
    out.push_str(&count(columns.len(), "column"));
    out
}

/// The statements in a file.
///
/// A file is cut at the semicolons the statements end with, which is
/// the rule a file written for any database already obeys, and not at
/// the balance rule return obeys in the editor: a person typing gets a
/// statement when it looks whole because they are waiting, and a file
/// is not waiting for anything. Quotes and comments are scanned so that
/// a semicolon inside either is a character rather than a cut, and a
/// chunk that is only a comment or only space is dropped rather than
/// run.
pub(crate) fn statements(text: &str) -> Vec<&str> {
    spans(text).into_iter().map(|(_, stmt)| stmt).collect()
}

/// The same statements, each with where it starts in `text`.
///
/// The offset is what an editor needs and a file runner does not: a
/// parse error carries a position into the statement it was raised on,
/// and a squiggle goes under the third statement of a document rather
/// than under the third character of it. The offset is of the trimmed
/// text, so adding a position to it lands on the same byte the parser
/// was looking at.
pub(crate) fn spans(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if let Some(q) = quote {
            // A doubled quote is the standard's escape for a quote in a
            // string, so it closes nothing.
            if c == q {
                if chars.peek().map(|(_, n)| *n) == Some(q) {
                    chars.next();
                } else {
                    quote = None;
                }
            } else if c == '\\' {
                chars.next();
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '/' if chars.peek().map(|(_, n)| *n) == Some('/') => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().map(|(_, n)| *n) == Some('*') => {
                chars.next();
                let mut prev = ' ';
                for (_, n) in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            ';' => {
                push(&mut out, text, start, at);
                start = at + 1;
            }
            _ => {}
        }
    }
    // A file whose last statement has no semicolon is a file one line
    // short of a convention, not a file with a statement missing.
    push(&mut out, text, start, text.len());
    out
}

/// Keeps a chunk if there is a statement in it.
///
/// What is left after the comments and the space is what decides, so a
/// file ending in a comment does not run an empty statement and get a
/// syntax error for its trouble.
fn push<'a>(out: &mut Vec<(usize, &'a str)>, text: &'a str, start: usize, end: usize) {
    let chunk = &text[start..end];
    if !bare(chunk).is_empty() {
        out.push((start + chunk.len() - chunk.trim_start().len(), chunk.trim()));
    }
}

/// The chunk with its comments and its space taken out, which is only
/// ever asked whether it is empty.
fn bare(chunk: &str) -> String {
    let mut out = String::new();
    let mut chars = chunk.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// The name of the graph a table belongs to.
fn graph_name(catalog: &Catalog, id: u32) -> String {
    catalog
        .graph_by_id(id)
        .map_or("?".to_string(), |g| g.name.clone())
}

/// The name of the node table at one end of a rel table.
fn endpoint(catalog: &Catalog, id: u32) -> String {
    catalog
        .node_by_id(id)
        .map_or("?".to_string(), |t| t.name.clone())
}

/// The line under a listing, plural where English wants one.
pub(crate) fn count(n: usize, what: &str) -> String {
    format!("({n} {what}{})\n", if n == 1 { "" } else { "s" })
}

/// How long a statement took, as `\timing` prints it.
///
/// Always milliseconds and always three places after the point, however
/// long it was. A unit that changed with the number would make two runs
/// of the same statement print in two units and leave the reader
/// converting, and the reason to turn timing on is to compare.
pub(crate) fn took(elapsed: std::time::Duration) -> String {
    format!("time: {:.3} ms\n", elapsed.as_secs_f64() * 1000.0)
}

/// The line a statement that is still running writes over itself.
///
/// Seconds with one place, because this is a number a person watches
/// rather than compares, and a third decimal that changes ten times a
/// second is a flicker rather than a fact. The rows are the ones the
/// engine has read out of storage, which is the number that moves on
/// the statement worth watching: one that reads a hundred million rows
/// to answer with one. A statement that has read none of them prints
/// the time alone rather than a zero that never moves.
pub(crate) fn running(elapsed: std::time::Duration, rows: u64) -> String {
    let seconds = elapsed.as_secs_f64();
    if rows == 0 {
        return format!("running {seconds:.1} s, Ctrl-C stops it");
    }
    format!(
        "running {seconds:.1} s, {} rows read, Ctrl-C stops it",
        grouped(rows)
    )
}

/// What is printed instead of an answer when the user stopped it.
///
/// It says what was thrown away and that nothing else was, because the
/// question somebody asks after pressing `Ctrl-C` is whether they still
/// have their session.
pub(crate) fn stopped(elapsed: std::time::Duration, rows: u64) -> String {
    let seconds = elapsed.as_secs_f64();
    let read = if rows == 0 {
        String::new()
    } else {
        format!(" after reading {} rows", grouped(rows))
    };
    format!("interrupted at {seconds:.1} s{read}. the session is still open\n")
}

/// A count with a space every three digits, which is what makes eight
/// figures readable at a glance. A space rather than a comma or a point
/// because those two mean opposite things to different readers and this
/// number is read by both.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zu::zu1::catalog::Catalog;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        let person = catalog.upsert_node("Person", 3).unwrap();
        let place = catalog.upsert_node("Place", 1).unwrap();
        catalog.upsert_rel("KNOWS", person, person, 5).unwrap();
        catalog.upsert_rel("LIVES_IN", person, place, 2).unwrap();
        catalog
    }

    #[test]
    fn a_line_that_is_not_a_backslash_line_belongs_to_the_engine() {
        assert!(parse("MATCH (a) RETURN a").is_none());
        assert!(parse("   ").is_none());
        // A backslash inside a statement is a statement.
        assert!(parse("RETURN 'a\\tb'").is_none());
    }

    #[test]
    fn every_command_in_the_list_parses_the_way_its_brackets_say() {
        let wrong = |line: &str| matches!(parse(line), Some(Command::Wrong(_)));
        for entry in COMMANDS {
            let mut words = entry.synopsis.split(' ');
            let word = words.next().unwrap();
            let argument = words.next();
            assert!(parse(word).is_some(), "{word} is not a backslash line");
            match argument {
                // A bare argument is one the parser has to insist on,
                // and a bracketed one is one it has to do without.
                Some(argument) if !argument.starts_with('[') => {
                    assert!(
                        wrong(word),
                        "{word} takes {argument} and did not ask for it"
                    );
                    let with = format!("{word} zzz");
                    assert!(!wrong(&with), "{with} does not parse");
                }
                _ => assert!(!wrong(word), "{word} does not parse"),
            }
        }
    }

    #[test]
    fn the_argument_is_the_rest_of_the_line() {
        assert_eq!(parse("\\d"), Some(Command::Tables));
        assert_eq!(parse("  \\d Person  "), Some(Command::Describe("Person")));
        assert_eq!(
            parse("\\i /tmp/two words.gql"),
            Some(Command::Include("/tmp/two words.gql"))
        );
        assert_eq!(parse("\\timing"), Some(Command::Timing(None)));
        assert_eq!(parse("\\timing on"), Some(Command::Timing(Some(true))));
        assert_eq!(parse("\\timing off"), Some(Command::Timing(Some(false))));
    }

    #[test]
    fn a_command_nobody_wrote_says_so_and_says_where_the_list_is() {
        let wrong = |line: &str| match parse(line) {
            Some(Command::Wrong(message)) => message,
            other => panic!("{line} parsed as {other:?}"),
        };
        assert!(wrong("\\zz").contains("\\zz"));
        assert!(wrong("\\zz").contains("\\?"));
        assert!(wrong("\\i").contains("file"));
        assert!(wrong("\\timing sometimes").contains("on or off"));
        assert!(wrong("\\").contains("\\?"));
    }

    #[test]
    fn the_table_list_counts_rows_and_says_what_a_rel_table_joins() {
        let text = tables(&catalog());
        assert!(text.contains("node  Person"), "{text}");
        assert!(text.contains("rel   KNOWS"), "{text}");
        assert!(text.contains("Person -> Place"), "{text}");
        assert!(text.ends_with("(4 tables)\n"), "{text}");
        assert_eq!(tables(&Catalog::default()), "no tables in this file\n");
    }

    #[test]
    fn a_file_always_has_the_home_graph_to_list() {
        let text = graphs(&Catalog::default());
        assert!(text.contains("home"), "{text}");
        assert!(text.contains("open"), "{text}");
        assert!(text.ends_with("(1 graph)\n"), "{text}");
    }

    #[test]
    fn a_table_describes_its_shape_and_its_columns() {
        let catalog = catalog();
        let columns = [
            Column {
                name: "name".into(),
                ty: "STRING".into(),
                nullable: false,
            },
            Column {
                name: "age".into(),
                ty: "INT64".into(),
                nullable: true,
            },
        ];
        let text = describe(&catalog, "Person", &columns);
        assert!(
            text.starts_with("node table Person in graph home, 3 rows\n"),
            "{text}"
        );
        assert!(text.contains("name    STRING  no"), "{text}");
        assert!(text.contains("age     INT64   yes"), "{text}");
        assert!(text.ends_with("(2 columns)\n"), "{text}");
        let rel = describe(&catalog, "LIVES_IN", &[]);
        assert!(rel.contains("2 edges, Person -> Place"), "{rel}");
        assert!(rel.ends_with("no columns\n"), "{rel}");
    }

    #[test]
    fn a_name_no_table_has_points_at_the_list() {
        let text = describe(&catalog(), "Persno", &[]);
        assert_eq!(text, "no table called Persno. \\d lists them\n");
    }

    #[test]
    fn a_file_is_cut_at_the_semicolons_between_its_statements() {
        assert_eq!(
            statements("RETURN 1;\nRETURN 2;\n"),
            ["RETURN 1", "RETURN 2"]
        );
        // The last statement need not end in one.
        assert_eq!(statements("RETURN 1;\nRETURN 2"), ["RETURN 1", "RETURN 2"]);
        // A statement over several lines stays one statement.
        assert_eq!(
            statements("MATCH (a:Person)\nRETURN a;"),
            ["MATCH (a:Person)\nRETURN a"]
        );
    }

    #[test]
    fn a_semicolon_in_a_string_or_a_comment_is_not_a_cut() {
        assert_eq!(statements("RETURN 'a;b';"), ["RETURN 'a;b'"]);
        assert_eq!(statements("RETURN 1; // and; then\n"), ["RETURN 1"]);
        assert_eq!(statements("/* a; b */ RETURN 1;"), ["/* a; b */ RETURN 1"]);
    }

    #[test]
    fn a_time_is_milliseconds_however_long_it_was() {
        assert_eq!(
            took(std::time::Duration::from_micros(1234)),
            "time: 1.234 ms\n"
        );
        assert_eq!(
            took(std::time::Duration::from_secs(10)),
            "time: 10000.000 ms\n"
        );
        assert_eq!(took(std::time::Duration::ZERO), "time: 0.000 ms\n");
    }

    #[test]
    fn a_progress_line_says_what_moved_and_how_to_stop_it() {
        use std::time::Duration;
        assert_eq!(
            running(Duration::from_millis(1250), 4_300_000),
            "running 1.2 s, 4 300 000 rows read, Ctrl-C stops it"
        );
        // Nothing read yet is the first second of every statement, and
        // a zero there says less than the time does.
        assert_eq!(
            running(Duration::from_millis(400), 0),
            "running 0.4 s, Ctrl-C stops it"
        );
    }

    #[test]
    fn a_stopped_statement_says_the_session_survived_it() {
        use std::time::Duration;
        assert_eq!(
            stopped(Duration::from_millis(2400), 12_000),
            "interrupted at 2.4 s after reading 12 000 rows. the session is still open\n"
        );
        assert_eq!(
            stopped(Duration::from_millis(90), 0),
            "interrupted at 0.1 s. the session is still open\n"
        );
    }

    #[test]
    fn digits_are_grouped_in_threes_from_the_right() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1 000");
        assert_eq!(grouped(12_345_678), "12 345 678");
    }

    #[test]
    fn a_session_with_nothing_set_says_the_four_lines_and_stops() {
        let shown = session_state("/", "/social", 0, 3, &[], &[]);
        assert_eq!(
            shown,
            "schema:    /\ngraph:     /social\ntime zone: +00:00\nepoch:     3\nno parameters set\n"
        );
    }

    #[test]
    fn a_pinned_parameter_says_the_epoch_it_is_pinned_at() {
        let pins = vec![
            Pin {
                name: "g".to_string(),
                kind: "GRAPH",
                epoch: 2,
                what: "GRAPH /scratch".to_string(),
                stale: zu::session::Stale::Gone,
            },
            Pin {
                name: "t".to_string(),
                kind: "BINDING TABLE",
                epoch: 5,
                what: "BINDING TABLE #1 (1 column, 4 rows)".to_string(),
                stale: zu::session::Stale::Old,
            },
        ];
        let params = [
            ("cut", "VALUE", "35".to_string()),
            ("g", "GRAPH", "GRAPH /scratch".to_string()),
            ("t", "BINDING TABLE", "BINDING TABLE #1".to_string()),
        ];
        let shown = session_state("/", "/social", -330, 9, &params, &pins);
        assert!(shown.contains("time zone: -05:30"), "{shown}");
        // A value pins nothing, so its cell is empty, and the two that
        // do pin say what has become of what they name.
        assert!(shown.contains("$cut"), "{shown}");
        assert!(shown.contains("2 (gone)"), "{shown}");
        assert!(shown.contains("5 (old)"), "{shown}");
        assert!(shown.contains("3 parameters"), "{shown}");
    }

    #[test]
    fn a_file_of_nothing_runs_nothing() {
        assert!(statements("").is_empty());
        assert!(statements(";;\n\n").is_empty());
        assert!(statements("// a file of notes\n").is_empty());
        assert!(statements("/* nothing here */").is_empty());
    }
}
