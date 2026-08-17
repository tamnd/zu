//! Paging: an answer taller than the window.
//!
//! A result that does not fit is shown a screen at a time, because the
//! alternative is a user watching the rows they wanted scroll off the
//! top while the shell prints the ones they did not. Space takes the
//! next screen, return takes one row, and `q`, `Ctrl-C` or `Ctrl-D`
//! stops printing.
//!
//! There is no going back, and that is on purpose. This pager writes
//! into the terminal's own scrollback rather than onto an alternate
//! screen, so what has already gone past is still where the user's
//! terminal keeps it, with the terminal's own scrolling and its own
//! search. A pager that took the screen away would have to give both of
//! those back, and the two hundred lines that would cost buy nothing a
//! terminal did not already have.
//!
//! Nor is `$PAGER` spawned. Handing the rows to another program means
//! handing a user's data to whatever that variable happens to name, and
//! it means leaving raw mode, forking, and hoping the child gives the
//! terminal back the way it found it. Twenty rows of arithmetic here is
//! the smaller promise.

use crate::keys::Key;
use crate::line::width_of;

/// What a key means to a pager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Move {
    /// One screen more.
    Screen,
    /// One row more.
    Row,
    /// Stop printing. The rows not shown are dropped, not queued.
    Stop,
    /// A key with no meaning here, which is most of them.
    Idle,
}

/// What a key does at the `--more--` prompt.
///
/// The bindings are `more`'s and `less`'s, which is the muscle memory a
/// terminal user brings with them: space and `f` for a screen, return
/// and `j` for a row, `q` to stop. `Ctrl-C` stops as well, since a user
/// who wants out of a listing reaches for it before they read the
/// prompt.
pub(crate) fn command(key: Key) -> Move {
    match key {
        Key::Char(' ' | 'f') => Move::Screen,
        Key::Enter | Key::Newline | Key::Down | Key::Char('j') => Move::Row,
        Key::Char('q') | Key::Interrupt | Key::Eof => Move::Stop,
        _ => Move::Idle,
    }
}

/// Text laid out as the rows a window this wide will show it in, and a
/// place in that list.
pub(crate) struct Pager {
    rows: Vec<String>,
    at: usize,
    /// How many rows one press of space is worth: the window less the
    /// row the `--more--` prompt sits on.
    screen: usize,
}

impl Pager {
    /// Lays `text` out for a window of this size.
    pub(crate) fn new(text: &str, width: usize, height: usize) -> Pager {
        Pager {
            rows: wrap(text, width),
            at: 0,
            screen: height.saturating_sub(1).max(1),
        }
    }

    /// Whether the whole thing fits, in which case there is nothing to
    /// page and the caller should print it the way it always did.
    pub(crate) fn fits(&self) -> bool {
        self.rows.len() <= self.screen
    }

    /// Whether there is anything left to show.
    pub(crate) fn more(&self) -> bool {
        self.at < self.rows.len()
    }

    /// The next `n` rows, as the terminal wants them: every row ends
    /// `\r\n`, because paging happens in raw mode and a bare newline
    /// steps down without stepping back.
    pub(crate) fn take(&mut self, n: usize) -> String {
        let end = (self.at + n).min(self.rows.len());
        let mut out = String::new();
        for row in &self.rows[self.at..end] {
            out.push_str(row);
            out.push_str("\r\n");
        }
        self.at = end;
        out
    }

    /// The next screenful.
    pub(crate) fn screen(&mut self) -> String {
        self.take(self.screen)
    }
}

/// The rows a window this wide shows `text` in.
///
/// A line longer than the window is cut where the terminal would wrap
/// it, counting columns rather than characters so that a table of
/// Japanese names is cut in the right place, and a character that would
/// straddle the edge starts the next row rather than being split. The
/// trailing empty row every text ending in a newline would otherwise
/// produce is dropped, since a blank row at the bottom of a screen is a
/// row of the answer nobody gets to see.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
        let mut row = String::new();
        let mut cols = 0;
        for c in line.chars() {
            let w = width_of(c);
            if cols + w > width {
                rows.push(std::mem::take(&mut row));
                cols = 0;
            }
            row.push(c);
            cols += w;
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_cut_where_the_terminal_would_wrap_it() {
        assert_eq!(wrap("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(wrap("one\ntwo", 80), ["one", "two"]);
        // The newline every answer ends with is not a row of its own.
        assert_eq!(wrap("one\n", 80), ["one"]);
        assert_eq!(wrap("", 80), [""]);
        // A blank row in the middle is part of the answer and stays.
        assert_eq!(wrap("one\n\ntwo\n", 80), ["one", "", "two"]);
    }

    #[test]
    fn a_wide_character_is_not_split_across_two_rows() {
        // Three columns of window and two columns per character: one
        // character per row, and the third column is left empty rather
        // than filled with half of the next one.
        assert_eq!(wrap("中中", 3), ["中", "中"]);
        assert_eq!(wrap("中中", 4), ["中中"]);
    }

    #[test]
    fn what_fits_is_not_paged() {
        // A window of ten rows shows nine and keeps one for the prompt.
        let short = Pager::new("a\nb\nc\n", 80, 10);
        assert!(short.fits());
        let long = Pager::new(&"x\n".repeat(20), 80, 10);
        assert!(!long.fits());
    }

    #[test]
    fn a_screen_is_the_window_less_the_prompt_row() {
        let mut p = Pager::new(
            &(1..=10).fold(String::new(), |s, i| s + &format!("{i}\n")),
            80,
            4,
        );
        assert_eq!(p.screen(), "1\r\n2\r\n3\r\n");
        assert_eq!(p.take(1), "4\r\n");
        assert!(p.more());
        assert_eq!(p.screen(), "5\r\n6\r\n7\r\n");
        assert_eq!(p.screen(), "8\r\n9\r\n10\r\n");
        assert!(!p.more());
        // Asking past the end is empty rather than out of bounds.
        assert_eq!(p.screen(), "");
    }

    #[test]
    fn a_window_of_one_row_still_moves() {
        let mut p = Pager::new("a\nb\n", 80, 1);
        assert_eq!(p.screen(), "a\r\n");
        assert!(p.more());
    }

    #[test]
    fn the_keys_are_the_ones_a_terminal_user_already_knows() {
        assert_eq!(command(Key::Char(' ')), Move::Screen);
        assert_eq!(command(Key::Char('f')), Move::Screen);
        assert_eq!(command(Key::Enter), Move::Row);
        assert_eq!(command(Key::Char('j')), Move::Row);
        assert_eq!(command(Key::Char('q')), Move::Stop);
        assert_eq!(command(Key::Interrupt), Move::Stop);
        assert_eq!(command(Key::Char('z')), Move::Idle);
    }
}
