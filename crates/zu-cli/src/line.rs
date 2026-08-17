//! The line editor: what is typed, where the cursor is, and what the
//! terminal has to be told to show it.
//!
//! Every part of this is a function of state and a key, and the state
//! is a string and two numbers, so the whole editor is testable without
//! a terminal: feed it keys, read the buffer, ask where the cursor
//! would be drawn. The only thing that talks to a terminal is the
//! escape sequence [`Editor::render`] returns, and its geometry is
//! checked the same way.
//!
//! Statements are multi-line by default rather than by a continuation
//! character. Pressing return runs what is typed when it is finished
//! and adds a line when it is not, and "finished" means the brackets
//! and the quotes are closed, which is the rule a reader can apply
//! without knowing the grammar. A trailing semicolon ends a statement
//! too, for the muscle memory every other database shell has trained,
//! and `Ctrl-J` adds a line whatever the text looks like, which is the
//! escape hatch when the two rules disagree with the user.

use crate::keys::Key;

/// What a keystroke did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The buffer changed and the screen has to catch up.
    Redraw,
    /// Nothing changed, so nothing is drawn: a key with no binding, a
    /// motion at the end it was already at.
    Idle,
    /// Run this statement. The buffer is already empty and the
    /// statement is already in the history.
    Submit(String),
    /// `Ctrl-C`: the line is abandoned and the session is not.
    Cancel,
    /// `Ctrl-D` on an empty buffer.
    Quit,
    /// `Ctrl-L`: the screen is cleared and everything is drawn again.
    Clear,
    /// Tab: the caller holds the names, so it is the caller that
    /// completes. [`Editor::complete`] is the other half.
    Complete,
}

/// The editor's whole state.
#[derive(Default)]
pub(crate) struct Editor {
    text: String,
    /// A byte offset into `text`, always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    /// Where in the history the buffer came from, while the user is
    /// walking it. `None` means the buffer is their own typing.
    browsing: Option<usize>,
    /// What they had typed before they started walking, so that coming
    /// back down the list returns it rather than an empty line.
    stash: String,
    /// The geometry of the last render, which is what a redraw has to
    /// undo before it can draw.
    drawn_rows: usize,
    drawn_row: usize,
    /// Whether a redraw paints the statement. Off by default, which is
    /// what the tests want and what a terminal that has not said
    /// otherwise gets.
    colour: bool,
}

impl Editor {
    /// An empty editor that paints what it draws, or does not.
    pub(crate) fn new(colour: bool) -> Editor {
        Editor {
            colour,
            ..Editor::default()
        }
    }

    /// Forgets the buffer without touching the history, which is what
    /// happens after a statement runs and after `Ctrl-C`.
    ///
    /// What is on the screen is not this method's business, since the
    /// caller has to move the cursor off the editor before it prints
    /// anything anyway; [`Editor::tail`] and [`Editor::drawn_nothing`]
    /// are that pair.
    pub(crate) fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
        self.stash.clear();
    }

    /// Applies one key.
    pub(crate) fn step(&mut self, key: Key) -> Step {
        match key {
            Key::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.browsing = None;
                Step::Redraw
            }
            Key::Newline => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                Step::Redraw
            }
            Key::Enter => {
                if self.text.trim().is_empty() {
                    // An empty return is a blank line and not an empty
                    // statement, because the engine's answer to one
                    // would be a syntax error a user did not ask for.
                    self.text.clear();
                    self.cursor = 0;
                    return Step::Redraw;
                }
                if !finished(&self.text) {
                    self.text.insert(self.cursor, '\n');
                    self.cursor += 1;
                    return Step::Redraw;
                }
                let statement = self.text.trim().trim_end_matches(';').trim().to_string();
                // The history keeps what was typed, semicolon and all,
                // since walking back to a statement should return the
                // line as it was written.
                let typed = std::mem::take(&mut self.text);
                if self.history.last().map(String::as_str) != Some(typed.trim()) {
                    self.history.push(typed.trim().to_string());
                }
                self.reset();
                Step::Submit(statement)
            }
            Key::Backspace => match self.prev_boundary(self.cursor) {
                Some(at) => {
                    self.text.replace_range(at..self.cursor, "");
                    self.cursor = at;
                    Step::Redraw
                }
                None => Step::Idle,
            },
            Key::Delete => match self.next_boundary(self.cursor) {
                Some(at) => {
                    self.text.replace_range(self.cursor..at, "");
                    Step::Redraw
                }
                None => Step::Idle,
            },
            Key::Left => match self.prev_boundary(self.cursor) {
                Some(at) => {
                    self.cursor = at;
                    Step::Redraw
                }
                None => Step::Idle,
            },
            Key::Right => match self.next_boundary(self.cursor) {
                Some(at) => {
                    self.cursor = at;
                    Step::Redraw
                }
                None => Step::Idle,
            },
            Key::WordLeft => {
                let at = word_left(&self.text, self.cursor);
                self.move_to(at)
            }
            Key::WordRight => {
                let at = word_right(&self.text, self.cursor);
                self.move_to(at)
            }
            Key::Home => {
                let at = line_start(&self.text, self.cursor);
                self.move_to(at)
            }
            Key::End => {
                let at = line_end(&self.text, self.cursor);
                self.move_to(at)
            }
            // Up and down walk the lines of a statement first and the
            // history second, so that editing the middle of a
            // three-line MATCH does not throw it away, and so that the
            // history is still one key from a single-line buffer.
            Key::Up => {
                if line_start(&self.text, self.cursor) > 0 {
                    let at = vertical(&self.text, self.cursor, true);
                    return self.move_to(at);
                }
                self.walk_history(-1)
            }
            Key::Down => {
                if line_end(&self.text, self.cursor) < self.text.len() {
                    let at = vertical(&self.text, self.cursor, false);
                    return self.move_to(at);
                }
                self.walk_history(1)
            }
            Key::KillToEnd => {
                let at = line_end(&self.text, self.cursor);
                if at == self.cursor {
                    return Step::Idle;
                }
                self.text.replace_range(self.cursor..at, "");
                Step::Redraw
            }
            Key::KillToStart => {
                let at = line_start(&self.text, self.cursor);
                if at == self.cursor {
                    return Step::Idle;
                }
                self.text.replace_range(at..self.cursor, "");
                self.cursor = at;
                Step::Redraw
            }
            Key::KillWord => {
                let at = word_left(&self.text, self.cursor);
                if at == self.cursor {
                    return Step::Idle;
                }
                self.text.replace_range(at..self.cursor, "");
                self.cursor = at;
                Step::Redraw
            }
            Key::Refresh => Step::Clear,
            Key::Interrupt => Step::Cancel,
            // On text, end of input is the end of the statement rather
            // than the end of the session, which is the rule every
            // shell has: a user who meant to quit presses it again on
            // the empty line they are left with.
            Key::Eof => {
                if self.text.is_empty() {
                    Step::Quit
                } else {
                    Step::Idle
                }
            }
            // A tab is never a character in a statement, so it is the
            // one key whose whole meaning is the completion the caller
            // does with the names it has.
            Key::Tab => Step::Complete,
        }
    }

    /// Completes the word the cursor is in, and hands back what is
    /// worth showing under the prompt.
    ///
    /// The insert happens here because the buffer is here; the list
    /// goes back because printing is the caller's, and because a list
    /// printed under the prompt is a thing the screen has to be told
    /// about before the next redraw.
    pub(crate) fn complete(&mut self, names: &crate::complete::Names) -> Vec<String> {
        let done = crate::complete::complete(&self.text, self.cursor, names);
        if !done.insert.is_empty() {
            self.text.insert_str(self.cursor, &done.insert);
            self.cursor += done.insert.len();
            self.browsing = None;
        }
        done.list
    }

    /// Moves the cursor, and says whether that was a move.
    fn move_to(&mut self, at: usize) -> Step {
        if at == self.cursor {
            return Step::Idle;
        }
        self.cursor = at;
        Step::Redraw
    }

    /// Walks the history one entry back or forward.
    ///
    /// Leaving the list at the bottom hands back what the user had
    /// typed, which is why it was stashed: a history walk is a look
    /// around, not a decision to throw the line away.
    fn walk_history(&mut self, by: i32) -> Step {
        if self.history.is_empty() {
            return Step::Idle;
        }
        let last = self.history.len() - 1;
        let next = match (self.browsing, by) {
            (None, -1) => {
                self.stash = self.text.clone();
                Some(last)
            }
            (None, _) => return Step::Idle,
            (Some(0), -1) => Some(0),
            (Some(at), -1) => Some(at - 1),
            (Some(at), _) if at == last => None,
            (Some(at), _) => Some(at + 1),
        };
        match next {
            Some(at) => {
                if self.browsing == Some(at) {
                    return Step::Idle;
                }
                self.browsing = Some(at);
                self.text = self.history[at].clone();
            }
            None => {
                self.browsing = None;
                self.text = std::mem::take(&mut self.stash);
            }
        }
        self.cursor = self.text.len();
        Step::Redraw
    }

    /// The character boundary before `at`, or `None` at the start.
    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.text[..at]
            .chars()
            .next_back()
            .map(|c| at - c.len_utf8())
    }

    /// The character boundary after `at`, or `None` at the end.
    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.text[at..].chars().next().map(|c| at + c.len_utf8())
    }

    /// Where the cursor lands on screen, and how many rows the buffer
    /// takes, for a window this wide and a prompt this many columns
    /// across. The continuation prompt is the same width as the first
    /// one, which is what keeps a statement's lines in one column.
    ///
    /// Rows are counted as `columns / width + 1` for every line
    /// including a full one, which is the geometry [`Editor::render`]
    /// then forces by ending a full line with a newline of its own. A
    /// terminal leaves the cursor in the last column of a line it has
    /// filled rather than at the start of the next one, and an editor
    /// that assumes otherwise draws its second line over its first.
    pub(crate) fn place(&self, width: usize, prompt: usize) -> (usize, usize, usize) {
        let width = width.max(1);
        let mut rows = 0;
        let mut cursor = (0, 0);
        let mut at = 0;
        for line in self.text.split('\n') {
            if (at..=at + line.len()).contains(&self.cursor) {
                let cols = prompt + columns(&line[..self.cursor - at]);
                cursor = (rows + cols / width, cols % width);
            }
            rows += (prompt + columns(line)) / width + 1;
            at += line.len() + 1;
        }
        (rows, cursor.0, cursor.1)
    }

    /// The escape sequence that turns what the terminal is showing into
    /// what the buffer says, cursor included.
    ///
    /// One string for one write, because a redraw split across writes
    /// is a flicker on anything slower than a local terminal.
    pub(crate) fn render(&mut self, width: usize, prompt: &str, more: &str) -> String {
        let width = width.max(1);
        let (rows, row, col) = self.place(width, columns(prompt));
        let mut out = String::with_capacity(self.text.len() + 32);
        // Back to the top left of what was drawn last time, then clear
        // everything below it. Clearing rather than overwriting is what
        // makes a buffer that got shorter get shorter on screen.
        out.push('\r');
        if self.drawn_row > 0 {
            out.push_str(&format!("\x1b[{}A", self.drawn_row));
        }
        out.push_str("\x1b[J");
        // The colour goes on the text that is written and never on the
        // text the geometry is measured from, because an escape
        // sequence occupies no column and a row count that included one
        // would wrap the statement in the wrong place.
        let painted = crate::highlight::lines(&self.text, self.colour);
        let mut lines = self.text.split('\n').zip(painted).enumerate().peekable();
        while let Some((i, (line, painted))) = lines.next() {
            out.push_str(if i == 0 { prompt } else { more });
            out.push_str(&painted);
            let cols = columns(if i == 0 { prompt } else { more }) + columns(line);
            if lines.peek().is_some() || (cols > 0 && cols.is_multiple_of(width)) {
                out.push_str("\r\n");
            }
        }
        // Where the cursor ended up after all of that, which is the end
        // of the last row unless that row was full and the newline
        // above pushed it onto a fresh one.
        let end_row = rows - 1;
        if end_row > row {
            out.push_str(&format!("\x1b[{}A", end_row - row));
        }
        out.push('\r');
        if col > 0 {
            out.push_str(&format!("\x1b[{col}C"));
        }
        self.drawn_rows = rows;
        self.drawn_row = row;
        out
    }

    /// Moves the cursor off the editor and onto a line of its own, for
    /// a caller about to print what a statement returned.
    ///
    /// The cursor sits wherever the last redraw left it, which for a
    /// multi-line statement is usually not the last row, so this steps
    /// down past the rest of the buffer before it steps onto the next
    /// line. Printing without it would overwrite the statement the user
    /// just typed with the answer to it.
    pub(crate) fn tail(&self) -> String {
        let below = self
            .drawn_rows
            .saturating_sub(1)
            .saturating_sub(self.drawn_row);
        if below > 0 {
            format!("\x1b[{below}B\r\n")
        } else {
            "\r\n".to_string()
        }
    }

    /// Tells the editor that the screen no longer holds what it drew,
    /// which is true after [`Editor::tail`] and after `Ctrl-L` has
    /// cleared the screen.
    pub(crate) fn drawn_nothing(&mut self) {
        self.drawn_rows = 0;
        self.drawn_row = 0;
    }
}

/// Whether a statement is finished, which is the rule return obeys.
///
/// Balanced and unquoted, or ended with a semicolon. Comments are
/// skipped rather than scanned, because a `--` to the end of a line and
/// a `/* */` block are both places a bracket means nothing, and a shell
/// that asked for another line because a comment mentioned a
/// parenthesis would be a shell people stopped writing comments in.
pub(crate) fn finished(text: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if let Some(q) = quote {
            // A doubled quote is the standard's escape for a quote
            // inside a string, so it closes nothing.
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
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '-' if chars.peek().map(|(_, n)| *n) == Some('-') => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().map(|(_, n)| *n) == Some('*') => {
                chars.next();
                let mut prev = ' ';
                let mut closed = false;
                for (_, n) in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        closed = true;
                        break;
                    }
                    prev = n;
                }
                // A block comment nobody closed is as unfinished as a
                // bracket nobody closed, and for the same reason: the
                // next line is part of it.
                if !closed {
                    return false;
                }
            }
            _ => {}
        }
    }
    // A negative depth is a bracket closed that was never opened, which
    // is a statement the parser should refuse rather than a statement
    // the shell should keep waiting for.
    quote.is_none() && depth <= 0
}

/// How many columns a string occupies.
pub(crate) fn columns(text: &str) -> usize {
    text.chars().map(width_of).sum()
}

/// How many columns one character occupies.
///
/// Three answers, and the ranges are the ones that matter rather than
/// the whole of Unicode: a combining mark hangs off the character
/// before it and takes none, the CJK and emoji blocks take two, and
/// everything else takes one. A table with every range in it would be
/// larger than this file and would still be a guess about what a
/// terminal does, since terminals disagree; being right about the
/// scripts people write statements in is the promise here.
fn width_of(c: char) -> usize {
    let c = c as u32;
    match c {
        0x0300..=0x036f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

/// The start of the line the cursor is on.
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The end of the line the cursor is on.
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

/// The same column one line up or down, or the end of that line when it
/// is shorter.
fn vertical(text: &str, at: usize, up: bool) -> usize {
    let start = line_start(text, at);
    let column = columns(&text[start..at]);
    let (target_start, target_end) = if up {
        let end = start - 1;
        (line_start(text, end), end)
    } else {
        let start = line_end(text, at) + 1;
        (start, line_end(text, start))
    };
    let mut walked = 0;
    for (i, c) in text[target_start..target_end].char_indices() {
        if walked >= column {
            return target_start + i;
        }
        walked += width_of(c);
    }
    target_end
}

/// The start of the word before the cursor.
///
/// A word is a run of letters, digits and underscores, which is what an
/// identifier is in every statement this shell will see. Separators
/// before the cursor are stepped over first, so pressing it twice at
/// the end of `MATCH (n)` lands before `n` and then before `MATCH`.
fn word_left(text: &str, at: usize) -> usize {
    let mut i = at;
    while let Some(c) = text[..i].chars().next_back() {
        if is_word(c) {
            break;
        }
        i -= c.len_utf8();
    }
    while let Some(c) = text[..i].chars().next_back() {
        if !is_word(c) {
            break;
        }
        i -= c.len_utf8();
    }
    i
}

/// The end of the word after the cursor.
fn word_right(text: &str, at: usize) -> usize {
    let mut i = at;
    while let Some(c) = text[i..].chars().next() {
        if is_word(c) {
            break;
        }
        i += c.len_utf8();
    }
    while let Some(c) = text[i..].chars().next() {
        if !is_word(c) {
            break;
        }
        i += c.len_utf8();
    }
    i
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Types a string into an editor, one key per character.
    fn type_in(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.step(Key::Char(c));
        }
    }

    #[test]
    fn typing_and_backspace_move_the_cursor_with_the_text() {
        let mut e = Editor::default();
        type_in(&mut e, "MATCH");
        assert_eq!(e.text, "MATCH");
        assert_eq!(e.step(Key::Backspace), Step::Redraw);
        assert_eq!(e.text, "MATC");
        e.step(Key::Home);
        assert_eq!(e.step(Key::Backspace), Step::Idle);
        assert_eq!(e.step(Key::Char('x')), Step::Redraw);
        assert_eq!(e.text, "xMATC");
    }

    #[test]
    fn a_character_is_one_key_however_many_bytes_it_is() {
        let mut e = Editor::default();
        type_in(&mut e, "n中é");
        e.step(Key::Left);
        e.step(Key::Backspace);
        assert_eq!(e.text, "né");
        // The cursor is between the two, so delete takes the one in
        // front of it whatever it is spelled in.
        e.step(Key::Delete);
        assert_eq!(e.text, "n");
    }

    #[test]
    fn return_runs_a_finished_statement_and_continues_an_unfinished_one() {
        let mut e = Editor::default();
        type_in(&mut e, "MATCH (n");
        assert_eq!(e.step(Key::Enter), Step::Redraw);
        assert_eq!(e.text, "MATCH (n\n");
        type_in(&mut e, ") RETURN n");
        assert_eq!(
            e.step(Key::Enter),
            Step::Submit("MATCH (n\n) RETURN n".to_string())
        );
        assert_eq!(e.text, "");
    }

    #[test]
    fn a_semicolon_ends_a_statement_and_does_not_travel_with_it() {
        let mut e = Editor::default();
        type_in(&mut e, "RETURN 1;");
        assert_eq!(e.step(Key::Enter), Step::Submit("RETURN 1".to_string()));
        assert_eq!(e.history, ["RETURN 1;"]);
    }

    #[test]
    fn control_j_adds_a_line_whatever_the_statement_looks_like() {
        let mut e = Editor::default();
        type_in(&mut e, "RETURN 1");
        assert_eq!(e.step(Key::Newline), Step::Redraw);
        type_in(&mut e, "+ 1");
        assert_eq!(
            e.step(Key::Enter),
            Step::Submit("RETURN 1\n+ 1".to_string())
        );
    }

    #[test]
    fn an_empty_return_is_a_blank_line_and_not_a_statement() {
        let mut e = Editor::default();
        assert_eq!(e.step(Key::Enter), Step::Redraw);
        type_in(&mut e, "   ");
        assert_eq!(e.step(Key::Enter), Step::Redraw);
        assert_eq!(e.text, "");
        assert!(e.history.is_empty());
    }

    #[test]
    fn an_unclosed_quote_is_unfinished_and_a_comment_does_not_count() {
        assert!(finished("RETURN 1"));
        assert!(!finished("RETURN 'a"));
        assert!(finished("RETURN 'a'"));
        assert!(finished("RETURN 'it''s'"));
        assert!(!finished("MATCH (n {a: 1}"));
        assert!(finished("MATCH (n {a: 1})"));
        assert!(finished("RETURN 1 -- a ( comment"));
        assert!(finished("RETURN 1 /* a ( comment */"));
        assert!(!finished("RETURN 1 /* unclosed ("));
        // A bracket closed that was never opened is the parser's to
        // refuse, not the shell's to wait on.
        assert!(finished("RETURN 1)"));
    }

    #[test]
    fn control_d_quits_on_an_empty_line_and_does_nothing_on_a_full_one() {
        let mut e = Editor::default();
        assert_eq!(e.step(Key::Eof), Step::Quit);
        type_in(&mut e, "R");
        assert_eq!(e.step(Key::Eof), Step::Idle);
    }

    #[test]
    fn control_c_abandons_the_line_and_the_caller_keeps_the_session() {
        let mut e = Editor::default();
        type_in(&mut e, "RETURN 1");
        assert_eq!(e.step(Key::Interrupt), Step::Cancel);
    }

    #[test]
    fn the_word_motions_step_over_identifiers() {
        let mut e = Editor::default();
        type_in(&mut e, "MATCH (n:Person)");
        // One press steps over the closing paren and the label both,
        // since the separators before an identifier belong to the move.
        e.step(Key::WordLeft);
        assert_eq!(&e.text[e.cursor..], "Person)");
        e.step(Key::KillWord);
        assert_eq!(e.text, "MATCH (Person)");
        e.step(Key::Home);
        e.step(Key::WordRight);
        assert_eq!(&e.text[e.cursor..], " (Person)");
    }

    #[test]
    fn kill_to_end_and_to_start_take_the_line_the_cursor_is_on() {
        let mut e = Editor::default();
        type_in(&mut e, "one");
        e.step(Key::Newline);
        type_in(&mut e, "two three");
        e.step(Key::Home);
        e.step(Key::WordRight);
        assert_eq!(e.step(Key::KillToEnd), Step::Redraw);
        assert_eq!(e.text, "one\ntwo");
        assert_eq!(e.step(Key::KillToStart), Step::Redraw);
        assert_eq!(e.text, "one\n");
    }

    #[test]
    fn up_walks_the_lines_of_a_statement_before_the_history() {
        let mut e = Editor::default();
        type_in(&mut e, "RETURN 1");
        e.step(Key::Enter);
        type_in(&mut e, "MATCH (n)");
        e.step(Key::Newline);
        type_in(&mut e, "RETURN n");
        // On the second line, up is a move inside the buffer, to the
        // same column of the line above.
        assert_eq!(e.step(Key::Up), Step::Redraw);
        assert_eq!(&e.text[..e.cursor], "MATCH (n");
        // On the first line, up is the history, and the buffer that was
        // being edited comes back on the way down.
        assert_eq!(e.step(Key::Up), Step::Redraw);
        assert_eq!(e.text, "RETURN 1");
        assert_eq!(e.step(Key::Down), Step::Redraw);
        assert_eq!(e.text, "MATCH (n)\nRETURN n");
    }

    #[test]
    fn the_history_keeps_one_copy_of_a_statement_repeated() {
        let mut e = Editor::default();
        for _ in 0..3 {
            type_in(&mut e, "RETURN 1");
            e.step(Key::Enter);
        }
        assert_eq!(e.history, ["RETURN 1"]);
    }

    #[test]
    fn the_cursor_is_placed_where_the_text_puts_it() {
        let mut e = Editor::default();
        type_in(&mut e, "RETURN 1");
        // Prompt of four columns, a window of eighty: one row, and the
        // cursor after the twelfth column.
        assert_eq!(e.place(80, 4), (1, 0, 12));
        // A window narrow enough to wrap it: the same text is two rows.
        assert_eq!(e.place(8, 4), (2, 1, 4));
        e.step(Key::Home);
        assert_eq!(e.place(8, 4), (2, 0, 4));
    }

    #[test]
    fn a_line_that_exactly_fills_the_window_gets_a_row_of_its_own() {
        let mut e = Editor::default();
        type_in(&mut e, "12345678");
        // Eight columns of text in an eight column window: the cursor
        // is on the next row, because the render forces the wrap.
        assert_eq!(e.place(8, 0), (2, 1, 0));
    }

    #[test]
    fn a_wide_character_takes_two_columns() {
        let mut e = Editor::default();
        type_in(&mut e, "中中");
        assert_eq!(e.place(80, 0), (1, 0, 4));
        assert_eq!(columns("中a"), 3);
        // A combining mark hangs off the character before it.
        assert_eq!(columns("e\u{0301}"), 1);
    }

    #[test]
    fn a_multi_line_buffer_is_as_many_rows_as_it_looks() {
        let mut e = Editor::default();
        type_in(&mut e, "one");
        e.step(Key::Newline);
        type_in(&mut e, "two");
        assert_eq!(e.place(80, 4), (2, 1, 7));
    }

    #[test]
    fn the_cursor_steps_off_the_editor_before_output_is_printed() {
        let mut e = Editor::default();
        type_in(&mut e, "one");
        e.step(Key::Newline);
        type_in(&mut e, "two");
        e.render(80, "zu> ", "..> ");
        // The cursor is on the last row already.
        assert_eq!(e.tail(), "\r\n");
        e.step(Key::Up);
        e.render(80, "zu> ", "..> ");
        // A row of statement below the cursor to step past first.
        assert_eq!(e.tail(), "\x1b[1B\r\n");
    }

    #[test]
    fn a_painted_redraw_writes_colour_and_wraps_in_the_same_place() {
        let mut plain = Editor::default();
        let mut painted = Editor::new(true);
        type_in(&mut plain, "MATCH (a)");
        type_in(&mut painted, "MATCH (a)");
        let drawn = painted.render(12, "zu> ", "..> ");
        assert!(drawn.contains("\x1b[1;34mMATCH\x1b[0m"), "{drawn:?}");
        // The escapes take no columns, so the two agree on the geometry
        // of the same statement in the same window.
        plain.render(12, "zu> ", "..> ");
        assert_eq!(painted.place(12, 4), plain.place(12, 4));
        assert_eq!(painted.drawn_rows, plain.drawn_rows);
        assert_eq!(painted.drawn_row, plain.drawn_row);
    }

    #[test]
    fn tab_completes_the_word_under_the_cursor() {
        let mut names = crate::complete::Names {
            labels: vec!["Person".to_string()],
            ..crate::complete::Names::default()
        };
        names.tidy();
        let mut e = Editor::default();
        type_in(&mut e, "MATCH (a:Per");
        assert_eq!(e.step(Key::Tab), Step::Complete);
        assert!(e.complete(&names).is_empty());
        assert_eq!(e.text, "MATCH (a:Person");
        assert_eq!(e.cursor, e.text.len());
        // A tab with nothing to say leaves the buffer alone, so a tab is
        // never a character in a statement.
        e.step(Key::Tab);
        assert!(e.complete(&names).is_empty());
        assert_eq!(e.text, "MATCH (a:Person");
    }

    #[test]
    fn a_redraw_moves_up_by_what_it_drew_and_clears_below() {
        let mut e = Editor::default();
        type_in(&mut e, "one");
        let first = e.render(80, "zu> ", "  > ");
        assert!(first.starts_with("\r\x1b[J"), "{first:?}");
        assert!(first.contains("zu> one"), "{first:?}");
        e.step(Key::Newline);
        type_in(&mut e, "two");
        let second = e.render(80, "zu> ", "  > ");
        assert!(second.contains("  > two"), "{second:?}");
        // The cursor was left on the second row, so the next redraw
        // has to climb one before it clears.
        let third = e.render(80, "zu> ", "  > ");
        assert!(third.starts_with("\r\x1b[1A\x1b[J"), "{third:?}");
        assert_eq!(e.drawn_rows, 2);
    }
}
