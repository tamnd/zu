//! What a keystroke is, once the bytes of one have arrived.
//!
//! A terminal in raw mode delivers a key as one byte, as a UTF-8
//! sequence, or as an escape sequence of up to half a dozen bytes that
//! can arrive split across reads. Decoding is therefore a function of a
//! buffer that answers "not yet" as a first-class result, and the
//! reader above it only has to keep filling the buffer until it stops
//! saying so. That is also what makes the whole decoder testable
//! without a terminal: every sequence a key can arrive as is a slice.
//!
//! The set of keys is what the editor acts on and nothing else. An
//! unrecognized escape sequence is consumed and dropped rather than
//! inserted, because a function key pressed by accident should not put
//! `[15~` in the middle of somebody's statement.

/// A key, as the editor thinks of it.
///
/// Control characters that mean a motion are decoded as that motion,
/// so `Ctrl-A` and `Home` arrive as the same thing: the editor has one
/// arm for the intent rather than two for the two ways of spelling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// Text, one character of it.
    Char(char),
    /// Run the statement, or continue it when it is not finished.
    Enter,
    /// A newline in the buffer whatever the statement looks like, which
    /// is `Ctrl-J`.
    Newline,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
    /// `Ctrl-K`: from the cursor to the end of the line.
    KillToEnd,
    /// `Ctrl-U`: from the start of the line to the cursor.
    KillToStart,
    /// `Ctrl-W`: the word before the cursor.
    KillWord,
    /// `Ctrl-L`: repaint, for a terminal something else has scribbled
    /// on.
    Refresh,
    /// `Ctrl-C`: abandon what is typed, keep the session.
    Interrupt,
    /// `Ctrl-D`: end of input, which quits only on an empty buffer.
    Eof,
}

/// What a buffer of bytes turned out to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decoded {
    /// A key, and how many bytes it took.
    Key(Key, usize),
    /// Bytes that are not a key and are being dropped: an escape
    /// sequence this editor does not act on, or a byte that is not
    /// valid UTF-8.
    Skip(usize),
    /// The start of something, whose rest has not arrived.
    Need,
}

/// Reads one key off the front of `bytes`.
///
/// Nothing here consumes more than it explains: on `Key` and `Skip` the
/// caller drops exactly that many bytes, and on `Need` it keeps them
/// all and reads more.
pub(crate) fn decode(bytes: &[u8]) -> Decoded {
    let Some(&first) = bytes.first() else {
        return Decoded::Need;
    };
    match first {
        0x1b => escape(bytes),
        // The control characters that mean a motion or an edit. The
        // ones with no meaning here fall through to being dropped,
        // because a control character inserted into a statement is a
        // character the parser will refuse in a message about syntax.
        0x01 => Decoded::Key(Key::Home, 1),
        0x02 => Decoded::Key(Key::Left, 1),
        0x03 => Decoded::Key(Key::Interrupt, 1),
        0x04 => Decoded::Key(Key::Eof, 1),
        0x05 => Decoded::Key(Key::End, 1),
        0x06 => Decoded::Key(Key::Right, 1),
        0x08 | 0x7f => Decoded::Key(Key::Backspace, 1),
        0x09 => Decoded::Key(Key::Tab, 1),
        0x0a => Decoded::Key(Key::Newline, 1),
        0x0b => Decoded::Key(Key::KillToEnd, 1),
        0x0c => Decoded::Key(Key::Refresh, 1),
        0x0d => Decoded::Key(Key::Enter, 1),
        0x0e => Decoded::Key(Key::Down, 1),
        0x10 => Decoded::Key(Key::Up, 1),
        0x15 => Decoded::Key(Key::KillToStart, 1),
        0x17 => Decoded::Key(Key::KillWord, 1),
        0..=0x1f => Decoded::Skip(1),
        _ => character(bytes),
    }
}

/// One character, however many bytes UTF-8 spells it in.
///
/// A byte that cannot start a character, or a sequence that turns out
/// not to be one, is dropped one byte at a time rather than resynced by
/// scanning, since the only way to get here is a terminal sending
/// something no keyboard produced.
fn character(bytes: &[u8]) -> Decoded {
    let len = match bytes[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return Decoded::Skip(1),
    };
    if bytes.len() < len {
        return Decoded::Need;
    }
    match std::str::from_utf8(&bytes[..len]) {
        Ok(text) => match text.chars().next() {
            Some(c) => Decoded::Key(Key::Char(c), len),
            None => Decoded::Skip(len),
        },
        Err(_) => Decoded::Skip(1),
    }
}

/// An escape sequence, of which three shapes matter.
///
/// `ESC [` and `ESC O` introduce the cursor keys, in both the modern
/// and the application spellings a terminal may be in. `ESC` followed
/// by a letter is the meta key, which is how `Alt` arrives everywhere
/// but the Windows console. A lone `ESC` with nothing behind it is a
/// user pressing escape, which this editor has no use for, and is
/// dropped rather than waited on: waiting is where an editor gets its
/// reputation for feeling stuck.
fn escape(bytes: &[u8]) -> Decoded {
    match bytes.get(1) {
        None => Decoded::Need,
        Some(b'[') => bracket(bytes),
        Some(b'O') => match bytes.get(2) {
            None => Decoded::Need,
            Some(b'A') => Decoded::Key(Key::Up, 3),
            Some(b'B') => Decoded::Key(Key::Down, 3),
            Some(b'C') => Decoded::Key(Key::Right, 3),
            Some(b'D') => Decoded::Key(Key::Left, 3),
            Some(b'H') => Decoded::Key(Key::Home, 3),
            Some(b'F') => Decoded::Key(Key::End, 3),
            Some(_) => Decoded::Skip(3),
        },
        Some(b'b') => Decoded::Key(Key::WordLeft, 2),
        Some(b'f') => Decoded::Key(Key::WordRight, 2),
        Some(0x7f) => Decoded::Key(Key::KillWord, 2),
        Some(_) => Decoded::Skip(2),
    }
}

/// A control sequence: `ESC [`, then digits and semicolons, then the
/// letter or tilde that says what it was.
///
/// The parameters are read rather than matched literally, because a
/// modified arrow key arrives as `1;5C` from one terminal and `5C`
/// from another, and this editor treats every modifier on an arrow as
/// the word motion: nothing here binds `Shift-Left` to something a
/// `Ctrl-Left` user would be surprised by.
fn bracket(bytes: &[u8]) -> Decoded {
    let mut at = 2;
    while let Some(&b) = bytes.get(at) {
        if b.is_ascii_digit() || b == b';' {
            at += 1;
            continue;
        }
        let len = at + 1;
        let modified = at > 2;
        return match b {
            b'A' => Decoded::Key(Key::Up, len),
            b'B' => Decoded::Key(Key::Down, len),
            b'C' => Decoded::Key(if modified { Key::WordRight } else { Key::Right }, len),
            b'D' => Decoded::Key(if modified { Key::WordLeft } else { Key::Left }, len),
            b'H' => Decoded::Key(Key::Home, len),
            b'F' => Decoded::Key(Key::End, len),
            b'~' => match &bytes[2..at] {
                b"1" | b"7" => Decoded::Key(Key::Home, len),
                b"4" | b"8" => Decoded::Key(Key::End, len),
                b"3" => Decoded::Key(Key::Delete, len),
                _ => Decoded::Skip(len),
            },
            _ => Decoded::Skip(len),
        };
    }
    Decoded::Need
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_arrives_as_characters_however_many_bytes_it_took() {
        assert_eq!(decode(b"a"), Decoded::Key(Key::Char('a'), 1));
        assert_eq!(decode("é".as_bytes()), Decoded::Key(Key::Char('é'), 2));
        assert_eq!(decode("中".as_bytes()), Decoded::Key(Key::Char('中'), 3));
        assert_eq!(decode("😀".as_bytes()), Decoded::Key(Key::Char('😀'), 4));
    }

    #[test]
    fn a_character_split_across_reads_waits_for_the_rest() {
        let bytes = "中".as_bytes();
        assert_eq!(decode(&bytes[..1]), Decoded::Need);
        assert_eq!(decode(&bytes[..2]), Decoded::Need);
        assert_eq!(decode(bytes), Decoded::Key(Key::Char('中'), 3));
    }

    #[test]
    fn a_byte_that_is_not_utf8_is_dropped_rather_than_inserted() {
        assert_eq!(decode(&[0xff]), Decoded::Skip(1));
        assert_eq!(decode(&[0xc3, 0x28]), Decoded::Skip(1));
    }

    #[test]
    fn the_control_keys_are_the_motions_they_mean() {
        assert_eq!(decode(b"\x01"), Decoded::Key(Key::Home, 1));
        assert_eq!(decode(b"\x05"), Decoded::Key(Key::End, 1));
        assert_eq!(decode(b"\x03"), Decoded::Key(Key::Interrupt, 1));
        assert_eq!(decode(b"\x04"), Decoded::Key(Key::Eof, 1));
        assert_eq!(decode(b"\x7f"), Decoded::Key(Key::Backspace, 1));
        assert_eq!(decode(b"\r"), Decoded::Key(Key::Enter, 1));
        assert_eq!(decode(b"\n"), Decoded::Key(Key::Newline, 1));
        // A control character with no binding here is dropped, not
        // inserted: 0x18 is Ctrl-X.
        assert_eq!(decode(b"\x18"), Decoded::Skip(1));
    }

    #[test]
    fn the_cursor_keys_arrive_in_both_spellings() {
        for (bytes, key) in [
            (&b"\x1b[A"[..], Key::Up),
            (&b"\x1b[B"[..], Key::Down),
            (&b"\x1b[C"[..], Key::Right),
            (&b"\x1b[D"[..], Key::Left),
            (&b"\x1bOA"[..], Key::Up),
            (&b"\x1bOD"[..], Key::Left),
        ] {
            assert_eq!(decode(bytes), Decoded::Key(key, bytes.len()), "{bytes:?}");
        }
    }

    #[test]
    fn home_end_and_delete_arrive_in_every_spelling_a_terminal_uses() {
        for (bytes, key) in [
            (&b"\x1b[H"[..], Key::Home),
            (&b"\x1b[F"[..], Key::End),
            (&b"\x1b[1~"[..], Key::Home),
            (&b"\x1b[7~"[..], Key::Home),
            (&b"\x1b[4~"[..], Key::End),
            (&b"\x1b[8~"[..], Key::End),
            (&b"\x1b[3~"[..], Key::Delete),
            (&b"\x1bOH"[..], Key::Home),
        ] {
            assert_eq!(decode(bytes), Decoded::Key(key, bytes.len()), "{bytes:?}");
        }
    }

    #[test]
    fn a_modified_arrow_is_a_word_motion_whichever_modifier_it_was() {
        assert_eq!(decode(b"\x1b[1;5C"), Decoded::Key(Key::WordRight, 6));
        assert_eq!(decode(b"\x1b[1;5D"), Decoded::Key(Key::WordLeft, 6));
        assert_eq!(decode(b"\x1b[1;3D"), Decoded::Key(Key::WordLeft, 6));
        assert_eq!(decode(b"\x1b[5D"), Decoded::Key(Key::WordLeft, 4));
        assert_eq!(decode(b"\x1bb"), Decoded::Key(Key::WordLeft, 2));
        assert_eq!(decode(b"\x1bf"), Decoded::Key(Key::WordRight, 2));
    }

    #[test]
    fn an_escape_sequence_split_across_reads_waits_and_then_decodes() {
        assert_eq!(decode(b"\x1b"), Decoded::Need);
        assert_eq!(decode(b"\x1b["), Decoded::Need);
        assert_eq!(decode(b"\x1b[1"), Decoded::Need);
        assert_eq!(decode(b"\x1b[1;"), Decoded::Need);
        assert_eq!(decode(b"\x1b[1;5"), Decoded::Need);
        assert_eq!(decode(b"\x1b[1;5C"), Decoded::Key(Key::WordRight, 6));
    }

    #[test]
    fn a_sequence_with_no_binding_is_dropped_whole() {
        // F5, which is a key this editor does nothing with. Dropping
        // the whole sequence is the point: leaving the tail behind
        // would put `[15~` in the buffer.
        assert_eq!(decode(b"\x1b[15~"), Decoded::Skip(5));
        assert_eq!(decode(b"\x1b[Z"), Decoded::Skip(3));
        assert_eq!(decode(b"\x1bx"), Decoded::Skip(2));
    }
}
