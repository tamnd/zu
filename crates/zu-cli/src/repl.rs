//! The interactive half of `zu shell`: a prompt, an editor, and one
//! open session to run what is typed.
//!
//! The pieces are apart on purpose. [`crate::term`] is the only code
//! that talks to a driver, [`crate::keys`] turns bytes into keys and
//! [`crate::line`] turns keys into a buffer and the escape sequence that
//! shows it, all three testable with no terminal anywhere. What is left
//! here is the loop that joins them, which is the part a test would have
//! to fake a terminal to reach and is therefore the part worth keeping
//! small.
//!
//! Raw mode is held while a statement is being typed and dropped while
//! it runs, so a query that takes a minute takes it with the driver
//! back to normal and `Ctrl-C` at that moment is the signal it has
//! always been. The session outlives every statement, which is the whole
//! reason to have a shell: the catalog, the statistics, the plan cache
//! and the decoded block caches are paid for once.

use std::io::Read;
use std::process::ExitCode;

use zu::session::Session;

use crate::complete::Names;
use crate::keys::{self, Decoded};
use crate::line::{Editor, Step};
use crate::meta::{self, Column, Command};
use crate::page::{self, Move, Pager};
use crate::term::{self, Raw};

/// The prompt, and the one continuation lines get. Both are four
/// columns because a statement's second line should start under its
/// first: a shorter continuation prompt saves a column and costs the
/// reader the shape of what they wrote.
const PROMPT: &str = "zu> ";
const MORE: &str = "..> ";

/// How a line ended.
enum Line {
    Run(String),
    /// `Ctrl-C`, which throws the line away and keeps the session.
    Cancel,
    /// `Ctrl-D` on an empty line, or standard input closing under us.
    Done,
}

/// Runs the interactive shell until the user leaves it.
pub(crate) fn run(session: &mut Session, path: &str) -> ExitCode {
    let version = crate::VERSION;
    println!(
        "zu {version}, {path}. Ctrl-J for a new line, \\? for the commands, quit or Ctrl-D to leave."
    );
    // From here on a `Ctrl-C` while a statement runs is a flag rather
    // than the end of the process. At the prompt it is neither: raw
    // mode has taken the driver's signal characters away and the editor
    // reads the byte.
    term::catch_interrupts();
    let mut editor = Editor::new(crate::highlight::wanted());
    // Off until asked for, because a time printed under every answer is
    // a line of noise to everybody who did not want to know.
    let mut timing = false;
    // Gathered here and again after every statement, since a statement
    // is how a table or a label comes into being and a completion that
    // did not know about the graph just created would be a completion
    // people stopped pressing.
    let mut names = names(session);
    loop {
        let line = match read(&mut editor, &names) {
            Ok(line) => line,
            // A terminal that cannot be put in raw mode, or one that
            // stopped answering. Neither is worth a second attempt, and
            // the operating system's words are the useful ones.
            Err(e) => {
                eprintln!("zu shell: {e}");
                return ExitCode::from(3);
            }
        };
        match line {
            Line::Cancel => {}
            Line::Done => return ExitCode::SUCCESS,
            Line::Run(statement) => {
                if matches!(statement.to_ascii_lowercase().as_str(), "quit" | "exit") {
                    return ExitCode::SUCCESS;
                }
                // A backslash line is answered here and never reaches
                // the engine, which is why it is asked about first.
                let text = match meta::parse(&statement) {
                    Some(Command::Quit) => return ExitCode::SUCCESS,
                    Some(command) => backslash(session, command, &mut timing),
                    None => timed(session, &statement, timing).0,
                };
                if let Err(e) = show(&text) {
                    eprintln!("zu shell: {e}");
                    return ExitCode::from(3);
                }
                // A statement is how a table comes into being and a
                // file read by `\i` is how a hundred do, so the names
                // are gathered again after either.
                names = self::names(session);
            }
        }
    }
}

/// The names the open file has, in the shapes completion offers them.
///
/// Labels come from the dictionary and from the table names, because a
/// node table's name is the label every one of its rows carries and a
/// rel table's name is what a pattern writes after the colon in
/// `-[r:KNOWS]->`. Properties come from the declared graph types, which
/// is every property a typed graph has; a graph with no type declared
/// for it has no property names to offer, and offering the ones from
/// another graph would be worse than offering none.
fn names(session: &Session) -> Names {
    let catalog = session.catalog();
    let mut names = Names::default();
    for table in catalog.node_tables() {
        names.tables.push(table.name.clone());
        names.labels.push(table.name.clone());
    }
    for rel in catalog.rel_tables() {
        names.tables.push(rel.name.clone());
        names.labels.push(rel.name.clone());
    }
    names.labels.extend(catalog.labels().iter().cloned());
    for graph in catalog.graphs() {
        names.tables.push(graph.name.clone());
    }
    for ty in catalog.graph_types() {
        for element in &ty.elements {
            names
                .properties
                .extend(element.properties.iter().map(|p| p.name.clone()));
        }
    }
    names.tidy();
    names
}

/// Answers a backslash command, as the text that goes under the prompt.
///
/// The catalog is what all but two of them read, and the two that do
/// not are a toggle and a file. Nothing here can fail in a way that
/// should end the shell: a file that will not open is a sentence, the
/// same as a statement that will not parse.
fn backslash(session: &mut Session, command: Command<'_>, timing: &mut bool) -> String {
    let catalog = session.catalog();
    match command {
        Command::Help => meta::help(),
        Command::Tables => meta::tables(catalog),
        Command::Graphs => meta::graphs(catalog),
        Command::Describe(name) => {
            let columns = columns(session, name);
            meta::describe(session.catalog(), name, &columns)
        }
        Command::Timing(set) => {
            *timing = set.unwrap_or(!*timing);
            format!("timing is {}\n", if *timing { "on" } else { "off" })
        }
        Command::Include(path) => include(session, path, *timing),
        // Handled by the caller, which is the one that can leave.
        Command::Quit => String::new(),
        Command::Wrong(message) => format!("{message}\n"),
    }
}

/// The columns of one table, as the file stores them.
///
/// The property directory rather than the graph type, because the
/// directory is what the file has: a table loaded from a CSV has
/// columns and no declared type, and a `\d` that answered from the
/// declarations alone would tell that user their table is empty. A
/// table with no properties, and one whose directory will not read,
/// both come back with nothing, since the shape printed above the
/// columns is worth having either way.
fn columns(session: &mut Session, name: &str) -> Vec<Column> {
    let catalog = session.catalog();
    let node = catalog.node_by_name(name).map(|t| t.id);
    let rel = catalog.rel_by_name(name).map(|t| t.id);
    let Ok(db) = session.file_mut() else {
        return Vec::new();
    };
    let directory = match (node, rel) {
        (Some(id), _) => zu::zu1::props::load_props(db, id),
        (_, Some(id)) => zu::zu1::props::load_rel_props(db, id),
        _ => return Vec::new(),
    };
    let Ok(Some(directory)) = directory else {
        return Vec::new();
    };
    directory
        .columns
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            ty: c.ty.to_string(),
            nullable: c.validity.is_some(),
        })
        .collect()
}

/// Runs the statements in a file and gathers what they answered.
///
/// It stops at the first failure and says which statement that was,
/// because a file is a program: the statement after a failed one was
/// written expecting it to have worked, and running it anyway turns one
/// error into a page of them. A statement at the prompt is not a
/// program and does not stop anything.
fn include(session: &mut Session, path: &str, timing: bool) -> String {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return format!("cannot read {path}: {e}\n"),
    };
    let statements = meta::statements(&text);
    let total = statements.len();
    let mut out = String::new();
    for (i, statement) in statements.iter().enumerate() {
        let (answered, ok) = timed(session, statement, timing);
        out.push_str(&answered);
        if !ok {
            let _ = std::fmt::Write::write_fmt(
                &mut out,
                format_args!("stopped at statement {} of {total}\n", i + 1),
            );
            return out;
        }
    }
    out.push_str(&meta::count(total, "statement"));
    out
}

/// Runs one statement, with the time it took underneath when timing is
/// on, and says whether it worked.
fn timed(session: &mut Session, statement: &str, timing: bool) -> (String, bool) {
    let start = std::time::Instant::now();
    let (mut text, ok) = watched(session, statement);
    if timing {
        text.push_str(&meta::took(start.elapsed()));
    }
    (text, ok)
}

/// How often the watcher wakes: often enough that a `Ctrl-C` is
/// answered while the finger is still on the key, rare enough that
/// waiting on a statement costs nothing measurable. Two hundred wakes a
/// second against an executor that checks its flag every chunk puts the
/// whole round trip well inside the tenth of a second a person reads as
/// immediate, and a wake that finds nothing to do is a compare and a
/// sleep.
///
/// A press pays this twice: once for the wake that reads the flag and
/// once for the wake that finds the statement over, since the loop only
/// looks between two sleeps. Ten milliseconds of budget for a press
/// measured at forty when this was twenty, against a budget of fifty.
/// The progress line is unaffected either way, because it is written
/// only when the tenth of a second or the row count it prints has
/// changed.
const TICK: std::time::Duration = std::time::Duration::from_millis(5);

/// How long a statement runs before the shell says it is running.
///
/// Nothing a person waits less than this for wants a line about
/// itself, and every statement in a normal session finishes inside it,
/// so the usual case prints nothing at all.
const QUIET: std::time::Duration = std::time::Duration::from_millis(400);

/// Runs one statement on a thread of its own while this one watches for
/// `Ctrl-C` and says how far it has got.
///
/// The statement has to be somewhere other than here, because the
/// thread running a query is inside the executor and cannot also be
/// reading a flag. Where there is no terminal there is nobody to press
/// anything and nowhere to draw a progress line, so the statement runs
/// on this thread and a harness driving the shell down a pipe sees
/// exactly what it saw before.
///
/// A press stops the statement rather than the process: the session,
/// its plans and its warm readers are all still here afterwards, which
/// is the difference between this and the signal a shell without a
/// handler would take.
fn watched(session: &mut Session, statement: &str) -> (String, bool) {
    if !term::interactive() {
        return answer(session, statement);
    }
    let stop = session.interrupt();
    // Cleared before rather than after, so a press that arrived while
    // nothing was running cannot end what is about to.
    stop.clear();
    term::interrupted();
    let start = std::time::Instant::now();
    let mut drawn = String::new();
    let (text, ok) = std::thread::scope(|scope| {
        let running = scope.spawn(|| answer(session, statement));
        while !running.is_finished() {
            std::thread::sleep(TICK);
            if term::interrupted() {
                stop.stop();
            }
            if start.elapsed() >= QUIET {
                // Written only when it would say something different,
                // since the poll is faster than either the tenth of a
                // second or the row count can change and a terminal
                // handed the same line fifty times a second is fifty
                // writes buying nothing.
                let line = meta::running(start.elapsed(), stop.rows());
                if line != drawn {
                    // Carriage return, the line, then erase whatever
                    // the longer line before it left to the right.
                    let _ = term::emit(&format!("\r{line}\x1b[K"));
                    drawn = line;
                }
            }
        }
        running.join().unwrap_or_else(|_| {
            // A panic in the query path is a bug in the engine, and the
            // shell's job is to say so and stay open rather than to
            // unwind through a terminal it has left in raw mode.
            (
                "error the engine panicked running that statement\n".to_string(),
                false,
            )
        })
    });
    if !drawn.is_empty() {
        let _ = term::emit("\r\x1b[K");
    }
    if stop.stopped() {
        return (meta::stopped(start.elapsed(), stop.rows()), false);
    }
    (text, ok)
}

/// Runs one statement and renders whatever came back, result or
/// failure, as the text that goes under the prompt.
///
/// A failure prints the condition's code next to its message, because
/// `42001` is the thing a user can look up and the sentence after it is
/// the thing they can act on, and neither is a substitute for the
/// other. Notices print the same way: a statement that succeeded while
/// raising something should not look like one that raised nothing.
fn answer(session: &mut Session, statement: &str) -> (String, bool) {
    match session.run(statement, &[]) {
        Ok(r) => {
            let mut out = String::new();
            for notice in &r.notices {
                out.push_str(&format!(
                    "note {} {}\n",
                    notice.status.code(),
                    notice.detail
                ));
            }
            out.push_str(&crate::render_table(&r));
            (out, true)
        }
        Err(e) => match e.diagnostic() {
            Some(d) => (format!("error {} {}\n", d.status.code(), d.detail), false),
            None => (format!("error {e}\n"), false),
        },
    }
}

/// Prints an answer, a screen at a time when it is taller than the
/// window.
///
/// What fits is printed the way it always was, so nothing about a
/// one-row result changes and nothing waits for a key that a user with
/// a full screen of answer has no reason to press. What does not fit
/// goes through the pager, which needs raw mode for the same reason the
/// editor does: a key at the `--more--` prompt is one key and not one
/// line. A terminal that will not enter raw mode gets the whole answer
/// rather than an error, since printing too much is a smaller failure
/// than printing nothing.
fn show(text: &str) -> std::io::Result<()> {
    let mut pager = Pager::new(text, term::width(), term::height());
    if pager.fits() {
        print!("{text}");
        return std::io::Write::flush(&mut std::io::stdout());
    }
    let Ok(_raw) = Raw::enter() else {
        print!("{text}");
        return std::io::Write::flush(&mut std::io::stdout());
    };
    // Reverse video says where the answer stopped and the prompt began
    // without spending a colour on it, and a terminal that asked for no
    // colour is a terminal that asked for no styling either.
    let prompt = if crate::highlight::wanted() {
        "\x1b[7m--more--\x1b[0m"
    } else {
        "--more--"
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut bytes: Vec<u8> = Vec::with_capacity(16);
    term::emit(&pager.screen())?;
    while pager.more() {
        term::emit(prompt)?;
        let moved = press(&mut input, &mut bytes)?;
        // The prompt goes whatever the key meant, so that the rows that
        // follow it are the answer and nothing else.
        term::emit("\r\x1b[K")?;
        match moved {
            Move::Screen => term::emit(&pager.screen())?,
            Move::Row => term::emit(&pager.take(1))?,
            Move::Stop => break,
            Move::Idle => {}
        }
    }
    Ok(())
}

/// Waits for a key the pager has a meaning for.
///
/// Keys with no meaning are read and dropped rather than ending the
/// wait, so that an arrow key, which arrives as an escape sequence of
/// three bytes, does not scroll three screens.
fn press(input: &mut impl Read, bytes: &mut Vec<u8>) -> std::io::Result<Move> {
    let mut chunk = [0u8; 16];
    loop {
        while !bytes.is_empty() {
            let (key, taken) = match keys::decode(bytes) {
                Decoded::Need => break,
                Decoded::Skip(n) => {
                    bytes.drain(..n);
                    continue;
                }
                Decoded::Key(key, n) => (key, n),
            };
            bytes.drain(..taken);
            match page::command(key) {
                Move::Idle => {}
                moved => return Ok(moved),
            }
        }
        let read = input.read(&mut chunk)?;
        if read == 0 {
            // The terminal went away mid-listing, which is not a reason
            // to keep printing at it.
            return Ok(Move::Stop);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// Reads one statement, redrawing as it is typed.
///
/// The byte buffer outlives each read because an escape sequence
/// arrives split across reads often enough to be the normal case on a
/// slow link, and the decoder is written to say so rather than to guess.
fn read(editor: &mut Editor, names: &Names) -> std::io::Result<Line> {
    let _raw = Raw::enter()?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut bytes: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 64];
    term::emit(&editor.render(term::width(), PROMPT, MORE))?;
    loop {
        while !bytes.is_empty() {
            let (key, taken) = match keys::decode(&bytes) {
                Decoded::Need => break,
                Decoded::Skip(n) => {
                    bytes.drain(..n);
                    continue;
                }
                Decoded::Key(key, n) => (key, n),
            };
            bytes.drain(..taken);
            match editor.step(key) {
                Step::Idle => {}
                Step::Redraw => term::emit(&editor.render(term::width(), PROMPT, MORE))?,
                Step::Clear => {
                    // Home, then erase everything: the buffer is drawn
                    // again from the top of a screen nothing else is on.
                    term::emit("\x1b[H\x1b[2J")?;
                    editor.drawn_nothing();
                    term::emit(&editor.render(term::width(), PROMPT, MORE))?;
                }
                Step::Complete => {
                    let list = editor.complete(names);
                    if !list.is_empty() {
                        // The candidates go above the prompt and stay
                        // there, the way a shell's do: the statement is
                        // drawn again underneath them, so the screen
                        // keeps both what was offered and what is being
                        // typed.
                        term::emit(&editor.tail())?;
                        editor.drawn_nothing();
                        term::emit(&crate::complete::grid(&list, term::width()))?;
                    }
                    term::emit(&editor.render(term::width(), PROMPT, MORE))?;
                }
                Step::Submit(statement) => {
                    term::emit(&editor.tail())?;
                    editor.drawn_nothing();
                    return Ok(Line::Run(statement));
                }
                Step::Cancel => {
                    term::emit(&editor.tail())?;
                    editor.drawn_nothing();
                    editor.reset();
                    return Ok(Line::Cancel);
                }
                Step::Quit => {
                    term::emit(&editor.tail())?;
                    editor.drawn_nothing();
                    return Ok(Line::Done);
                }
            }
        }
        let read = input.read(&mut chunk)?;
        if read == 0 {
            // Standard input closed while a terminal was attached,
            // which is the terminal going away rather than a user
            // leaving. Ending is the only thing left to do.
            term::emit("\r\n")?;
            return Ok(Line::Done);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}
