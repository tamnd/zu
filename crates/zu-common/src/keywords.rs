//! The words the grammar spells out, and which of them a name may be.
//!
//! ISO/IEC 39075:2024 subclause 21.3 says a `<regular identifier>` is
//! not a `<reserved word>`. The rule lives in the Syntax Rules prose
//! rather than in the productions, so the grammar artifact alone does
//! not enforce it and an engine has to; the lists here are the
//! artifact's own, generated from it, so what counts as reserved is
//! the standard's answer and not one this crate invented.
//!
//! # The pre-reserved forty
//!
//! `<reserved word>` has `<pre-reserved word>` as its first
//! alternative, forty words ISO has taken for a later edition and given
//! no meaning to. Read strictly, `ABSTRACT`, `DATA`, `NUMBER`, `QUERY`,
//! `UNIT`, `VALUES` and thirty-four others are therefore not names.
//!
//! zu admits them, and [`is_reserved`] answers `false` for them on
//! purpose. Refusing a word buys a query nothing until the word means
//! something, and it costs everybody who has a label called `Unit` or a
//! property called `data` a rewrite for a meaning that has not arrived.
//! This is a deviation and `docs/07-query-engine.md` records it as one.
//! [`is_pre_reserved`] is here so that a tool that wants to warn can.

mod generated;

/// Whether a word may not be spelled as a regular identifier.
///
/// The comparison is ASCII case-insensitive, which is how the standard
/// compares a keyword: `match` and `MATCH` are the same word.
pub fn is_reserved(word: &str) -> bool {
    lookup(generated::RESERVED, word)
}

/// Whether a word is one of the forty ISO has taken for a later edition.
/// zu admits these as names; see the module note.
pub fn is_pre_reserved(word: &str) -> bool {
    lookup(generated::PRE_RESERVED, word)
}

/// Whether a word is spelled like a keyword and admitted as a name
/// anyway, which is what `<non-reserved word>` is for.
pub fn is_non_reserved(word: &str) -> bool {
    lookup(generated::NON_RESERVED, word)
}

/// A word is at most a few characters longer than the longest keyword,
/// so the uppercasing is on the stack and the search is over a sorted
/// slice. Anything longer than the longest word in the list cannot be
/// in it and is answered without looking.
fn lookup(words: &[&str], word: &str) -> bool {
    const LONGEST: usize = 32;
    if word.len() > LONGEST || !word.is_ascii() {
        return false;
    }
    let mut upper = [0u8; LONGEST];
    upper[..word.len()].copy_from_slice(word.as_bytes());
    upper[..word.len()].make_ascii_uppercase();
    let upper = std::str::from_utf8(&upper[..word.len()]).expect("ascii stays utf-8");
    words.binary_search(&upper).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reserved_word_is_one_in_any_case() {
        assert!(is_reserved("MATCH"));
        assert!(is_reserved("match"));
        assert!(is_reserved("MaTcH"));
        assert!(is_reserved("VALUE"));
        assert!(!is_reserved("person"));
        assert!(!is_reserved(""));
    }

    /// The three lists are three answers and a word gets one of them.
    #[test]
    fn the_pre_reserved_and_the_non_reserved_are_names_here() {
        assert!(is_pre_reserved("UNIT"));
        assert!(!is_reserved("UNIT"));
        assert!(is_non_reserved("ACYCLIC"));
        assert!(!is_reserved("ACYCLIC"));
        assert!(!is_pre_reserved("ACYCLIC"));
    }

    /// A name longer than any keyword, and a name outside ASCII, are
    /// both answered without touching the table.
    #[test]
    fn a_name_no_keyword_could_be_is_answered_cheaply() {
        assert!(!is_reserved(&"x".repeat(200)));
        assert!(!is_reserved("матч"));
    }
}
