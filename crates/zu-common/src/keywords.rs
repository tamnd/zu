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
    lookup(generated::RESERVED, &RESERVED_SHAPES, word)
}

/// Whether a word is one of the forty ISO has taken for a later edition.
/// zu admits these as names; see the module note.
pub fn is_pre_reserved(word: &str) -> bool {
    lookup(generated::PRE_RESERVED, &PRE_RESERVED_SHAPES, word)
}

/// Whether a word is spelled like a keyword and admitted as a name
/// anyway, which is what `<non-reserved word>` is for.
pub fn is_non_reserved(word: &str) -> bool {
    lookup(generated::NON_RESERVED, &NON_RESERVED_SHAPES, word)
}

/// The longest word any list holds is `CURRENT_PROPERTY_GRAPH` at
/// twenty-two, and a shape is a bit per length, so thirty-two is both
/// the width of the mask and the size of the buffer the uppercasing
/// goes in.
const LONGEST: usize = 32;

/// One mask per opening letter, a bit set for every length a word
/// starting with that letter has.
type Shapes = [u32; 26];

/// Which lengths of word each opening letter has, so that a name of a
/// shape no keyword has is answered without reading the table at all.
///
/// This is the whole point of the mask. `is_reserved` runs on every
/// identifier of every statement, and almost every one of them is not a
/// keyword, so the answer that has to be cheap is no. A binary search
/// over two hundred pointers costs eight cache misses to say it; two
/// array reads and an and say it for the great majority of names, and
/// the search runs only for a name shaped like a word that is there.
const fn shapes_of(words: &[&str]) -> Shapes {
    let mut shapes = [0u32; 26];
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_bytes();
        // Every word in every list opens with an upper case letter; the
        // underscores and the digits are all further in.
        shapes[(word[0] - b'A') as usize] |= 1 << word.len();
        i += 1;
    }
    shapes
}

static RESERVED_SHAPES: Shapes = shapes_of(generated::RESERVED);
static PRE_RESERVED_SHAPES: Shapes = shapes_of(generated::PRE_RESERVED);
static NON_RESERVED_SHAPES: Shapes = shapes_of(generated::NON_RESERVED);

/// A word is at most a few characters longer than the longest keyword,
/// so the uppercasing is on the stack and the search is over a sorted
/// slice. A word of a length or an opening letter no keyword has is
/// answered from the mask, without looking.
fn lookup(words: &[&str], shapes: &Shapes, word: &str) -> bool {
    let bytes = word.as_bytes();
    let len = bytes.len();
    if len == 0 || len >= LONGEST {
        return false;
    }
    // Folding the case with a single bit is enough to pick the mask:
    // anything outside the two letter ranges falls out of the range
    // check below, and a word opening outside ASCII cannot be a keyword.
    let opener = bytes[0] | 0x20;
    if !opener.is_ascii_lowercase() {
        return false;
    }
    if shapes[(opener - b'a') as usize] & (1 << len) == 0 {
        return false;
    }
    let mut upper = [0u8; LONGEST];
    upper[..len].copy_from_slice(bytes);
    upper[..len].make_ascii_uppercase();
    // Compared as bytes rather than as text, which is the same order a
    // sorted list of ASCII words is in and skips validating a buffer
    // that was already valid.
    words
        .binary_search_by(|held| held.as_bytes().cmp(&upper[..len]))
        .is_ok()
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

    /// The mask says no for a shape and the table says no for a word,
    /// and the two answers have to agree with the table alone. This
    /// walks every word of every list and every near miss of each of
    /// them, which is where a mask that dropped a length would show.
    #[test]
    fn the_shape_mask_never_says_no_where_the_table_says_yes() {
        for (words, held) in [
            (generated::RESERVED, is_reserved as fn(&str) -> bool),
            (generated::PRE_RESERVED, is_pre_reserved as fn(&str) -> bool),
            (generated::NON_RESERVED, is_non_reserved as fn(&str) -> bool),
        ] {
            for word in words {
                assert!(held(word), "{word}");
                assert!(held(&word.to_lowercase()), "{word}");
                assert!(!held(&format!("{word}x")), "{word}x");
                // The same shape and not the same word, which is what
                // fails if the mask is ever the only thing consulted.
                let odd = format!("{}_", &word[..word.len() - 1]);
                assert!(!held(&odd), "{odd}");
            }
        }
    }
}
