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

use crate::keys::{self, Decoded};
use crate::line::{Editor, Step};
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
    println!("zu {version}, {path}. Ctrl-J for a new line, quit or Ctrl-D to leave.");
    let mut editor = Editor::default();
    loop {
        let line = match read(&mut editor) {
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
                print!("{}", answer(session, &statement));
            }
        }
    }
}

/// Runs one statement and renders whatever came back, result or
/// failure, as the text that goes under the prompt.
///
/// A failure prints the condition's code next to its message, because
/// `42001` is the thing a user can look up and the sentence after it is
/// the thing they can act on, and neither is a substitute for the
/// other. Notices print the same way: a statement that succeeded while
/// raising something should not look like one that raised nothing.
fn answer(session: &mut Session, statement: &str) -> String {
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
            out
        }
        Err(e) => match e.diagnostic() {
            Some(d) => format!("error {} {}\n", d.status.code(), d.detail),
            None => format!("error {e}\n"),
        },
    }
}

/// Reads one statement, redrawing as it is typed.
///
/// The byte buffer outlives each read because an escape sequence
/// arrives split across reads often enough to be the normal case on a
/// slow link, and the decoder is written to say so rather than to guess.
fn read(editor: &mut Editor) -> std::io::Result<Line> {
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
