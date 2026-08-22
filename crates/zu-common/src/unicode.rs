//! Unicode normalization: the four normal forms of ISO 20.24 and 19.7.
//!
//! GQL's `NORMALIZE` and `IS NORMALIZED` both defer to Unicode Standard
//! Annex 15, which is one algorithm read four ways. Decompose the string,
//! sort the combining marks into canonical order, and for the two
//! composed forms put it back together again; whether the decomposition
//! is canonical or compatibility is the only difference between NFC and
//! NFKC, and between NFD and NFKD.
//!
//! The tables are the Unicode Character Database's, machine-written into
//! `generated.rs` from the artifacts checked in beside it, the same way
//! the GQLSTATUS table is written from the conditions artifact. They are
//! fully expanded, so a decomposition is one lookup rather than a
//! recursion: the standard guarantees the expansion terminates, and
//! doing it once at generation time means the runtime never asks.
//!
//! Hangul is not in the tables at all. Eleven thousand syllables
//! decompose and compose by arithmetic, which UAX 15 spells out, and a
//! table of them would be four fifths of the data for none of the
//! meaning.
//!
//! A dependency would have been the other way to get this. The reason it
//! is here instead is the reason `docs/10` gives for owning the line
//! editor: what arrives with a normalization crate is not one algorithm
//! but a set of opinions about Unicode versions, feature flags and table
//! layouts, and the algorithm itself is two hundred lines against a
//! database whose correctness on this point is checked by the Unicode
//! Consortium's own conformance file.

mod generated;

use generated::{CANONICAL, CCC, COMPATIBILITY, COMPOSITION, DECOMPOSED};

pub use generated::UNICODE_VERSION;

/// The four normal forms. `K` is for compatibility, which the standard
/// spells with a K because C was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormalForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

impl NormalForm {
    /// The spelling GQL uses, which is also the spelling UAX 15 uses.
    pub fn name(self) -> &'static str {
        match self {
            NormalForm::Nfc => "NFC",
            NormalForm::Nfd => "NFD",
            NormalForm::Nfkc => "NFKC",
            NormalForm::Nfkd => "NFKD",
        }
    }

    /// The form a word names, or `None` for a word that names none. The
    /// comparison is case-insensitive because the parser hands over what
    /// the statement wrote and GQL keywords are not case-sensitive.
    pub fn from_name(word: &str) -> Option<NormalForm> {
        match word.to_ascii_uppercase().as_str() {
            "NFC" => Some(NormalForm::Nfc),
            "NFD" => Some(NormalForm::Nfd),
            "NFKC" => Some(NormalForm::Nfkc),
            "NFKD" => Some(NormalForm::Nfkd),
            _ => None,
        }
    }

    /// Whether the form composes after decomposing.
    fn composes(self) -> bool {
        matches!(self, NormalForm::Nfc | NormalForm::Nfkc)
    }

    /// Whether the form decomposes by compatibility rather than only
    /// canonically.
    fn compatibility(self) -> bool {
        matches!(self, NormalForm::Nfkc | NormalForm::Nfkd)
    }
}

/// The first Hangul syllable, the first leading jamo, the first vowel
/// jamo, and the trailing jamo base, which sits one below the first
/// trailing jamo so that a zero index means no trailing jamo at all.
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of a character, zero for a starter.
fn ccc(c: char) -> u8 {
    match CCC.binary_search_by_key(&c, |&(k, _)| k) {
        Ok(i) => CCC[i].1,
        Err(_) => 0,
    }
}

/// The fully expanded decomposition of a character, or `None` for a
/// character that is its own decomposition.
fn decomposition(c: char, compatibility: bool) -> Option<&'static [char]> {
    let table = if compatibility {
        COMPATIBILITY
    } else {
        CANONICAL
    };
    let i = table.binary_search_by_key(&c, |&(k, _, _)| k).ok()?;
    let (_, at, len) = table[i];
    Some(&DECOMPOSED[at as usize..at as usize + len as usize])
}

/// The primary composite of a starter and a following character, or
/// `None` when the pair does not compose. Hangul is arithmetic; the rest
/// is the table, which already has the composition exclusions taken out
/// of it, so a pair that is in it composes.
fn compose_pair(first: char, second: char) -> Option<char> {
    let (a, b) = (first as u32, second as u32);
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        let syllable = S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT;
        return char::from_u32(syllable);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return char::from_u32(a + (b - T_BASE));
    }
    COMPOSITION
        .binary_search_by_key(&(first, second), |&(x, y, _)| (x, y))
        .ok()
        .map(|i| COMPOSITION[i].2)
}

/// Writes the decomposition of one character, Hangul by arithmetic and
/// everything else by table.
fn decompose_char(out: &mut Vec<char>, c: char, compatibility: bool) {
    let u = c as u32;
    if (S_BASE..S_BASE + S_COUNT).contains(&u) {
        let index = u - S_BASE;
        let (l, v, t) = (
            L_BASE + index / N_COUNT,
            V_BASE + (index % N_COUNT) / T_COUNT,
            index % T_COUNT,
        );
        out.extend(char::from_u32(l));
        out.extend(char::from_u32(v));
        if t != 0 {
            out.extend(char::from_u32(T_BASE + t));
        }
        return;
    }
    match decomposition(c, compatibility) {
        Some(chars) => out.extend_from_slice(chars),
        None => out.push(c),
    }
}

/// Sorts each run of combining marks by combining class, stably, which
/// is what makes two strings that differ only in the order they wrote
/// their marks in normalize to the same string. A starter ends a run,
/// and marks of equal class keep their order, so the sort has to be an
/// insertion sort and cannot be a general one.
fn canonical_order(chars: &mut [char]) {
    for i in 1..chars.len() {
        let class = ccc(chars[i]);
        if class == 0 {
            continue;
        }
        let mut j = i;
        while j > 0 && ccc(chars[j - 1]) > class {
            chars.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// The canonical composition algorithm of UAX 15. Each character is
/// tried against the last starter, and a character is blocked from that
/// starter when something already stands between them whose combining
/// class is not lower than its own. A character that composes is folded
/// into the starter and does not stand between anything.
fn compose(chars: Vec<char>) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut starter: Option<usize> = None;
    let mut previous = 0u8;
    let mut anything_since_starter = false;
    for c in chars {
        let class = ccc(c);
        if let Some(at) = starter
            && !(anything_since_starter && previous >= class)
            && let Some(composed) = compose_pair(out[at], c)
        {
            out[at] = composed;
            continue;
        }
        if class == 0 {
            starter = Some(out.len());
            anything_since_starter = false;
        } else {
            anything_since_starter = true;
        }
        previous = class;
        out.push(c);
    }
    out
}

/// The string in the normal form asked for.
///
/// A string of ASCII is in every normal form already, and that is the
/// common case in a database, so it is the case that costs a scan and no
/// allocation beyond the answer.
pub fn normalize(s: &str, form: NormalForm) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut chars = Vec::with_capacity(s.len());
    for c in s.chars() {
        decompose_char(&mut chars, c, form.compatibility());
    }
    canonical_order(&mut chars);
    if form.composes() {
        chars = compose(chars);
    }
    chars.into_iter().collect()
}

/// Whether the string is already in the normal form asked for.
///
/// The standard defines the predicate as the string being equal to its
/// own normalization, and that is how it is answered here rather than
/// through the quick-check properties. A quick check is a fourth table
/// and a second algorithm to be wrong in, and it answers `maybe` often
/// enough that the normalization runs anyway.
pub fn is_normalized(s: &str, form: NormalForm) -> bool {
    if s.is_ascii() {
        return true;
    }
    normalize(s, form) == s
}

/// Whether a character may begin a regular identifier (ISO 21.3).
///
/// The standard writes the set as Unicode general category classes and
/// the underscore, and that is what the table holds, so this is a
/// lookup and not a judgement. ASCII is answered without touching the
/// table, because nearly every identifier anybody writes is ASCII and
/// the table is there for the ones that are not.
pub fn is_ident_start(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphabetic() || c == '_';
    }
    in_ranges(generated::IDENT_START, c)
}

/// Whether a character may continue a regular identifier (ISO 21.3):
/// every start, and the marks, digits, connectors and format
/// characters that go with them.
pub fn is_ident_extend(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    in_ranges(generated::IDENT_EXTEND, c)
}

/// Whether a character falls in one of a sorted list of inclusive
/// ranges. The ranges do not touch or overlap, the generator having
/// merged the ones that did, so the search is over the starts alone.
fn in_ranges(ranges: &[(char, char)], c: char) -> bool {
    match ranges.binary_search_by(|(from, _)| from.cmp(&c)) {
        Ok(_) => true,
        Err(0) => false,
        Err(at) => ranges[at - 1].1 >= c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_already_in_every_form() {
        for form in [
            NormalForm::Nfc,
            NormalForm::Nfd,
            NormalForm::Nfkc,
            NormalForm::Nfkd,
        ] {
            assert_eq!(normalize("ab", form), "ab");
            assert!(is_normalized("ab", form));
        }
    }

    #[test]
    fn a_letter_and_its_accent_are_one_character_composed_and_two_decomposed() {
        assert_eq!(normalize("e\u{301}", NormalForm::Nfc), "\u{e9}");
        assert_eq!(normalize("\u{e9}", NormalForm::Nfd), "e\u{301}");
        assert!(is_normalized("\u{e9}", NormalForm::Nfc));
        assert!(!is_normalized("\u{e9}", NormalForm::Nfd));
    }

    #[test]
    fn compatibility_is_the_only_form_that_loses_the_formatting() {
        // U+FB01 is the fi ligature and U+00BD is one half. Neither has
        // a canonical decomposition, because a ligature is not the same
        // character as the letters it draws.
        assert_eq!(normalize("\u{fb01}", NormalForm::Nfc), "\u{fb01}");
        assert_eq!(normalize("\u{fb01}", NormalForm::Nfkc), "fi");
        assert_eq!(normalize("\u{bd}", NormalForm::Nfkd), "1\u{2044}2");
    }

    #[test]
    fn marks_sort_by_combining_class_and_keep_their_order_inside_a_class() {
        // U+0328 is ogonek, class 202, and U+0301 is acute, class 230.
        // Written the other way round they normalize to the same string.
        assert_eq!(
            normalize("a\u{301}\u{328}", NormalForm::Nfd),
            "a\u{328}\u{301}"
        );
        assert_eq!(
            normalize("a\u{301}\u{328}", NormalForm::Nfc),
            normalize("a\u{328}\u{301}", NormalForm::Nfc)
        );
    }

    #[test]
    fn a_hangul_syllable_is_arithmetic_in_both_directions() {
        assert_eq!(normalize("\u{ac00}", NormalForm::Nfd), "\u{1100}\u{1161}");
        assert_eq!(normalize("\u{1100}\u{1161}", NormalForm::Nfc), "\u{ac00}");
        assert_eq!(
            normalize("\u{d4db}", NormalForm::Nfd),
            "\u{1111}\u{1171}\u{11b6}"
        );
        assert_eq!(
            normalize("\u{1111}\u{1171}\u{11b6}", NormalForm::Nfc),
            "\u{d4db}"
        );
    }

    #[test]
    fn an_excluded_composition_stays_apart() {
        // U+0958 is in the composition exclusion list, so NFC leaves the
        // decomposed pair alone rather than putting it back together.
        assert_eq!(normalize("\u{958}", NormalForm::Nfc), "\u{915}\u{93c}");
        assert_eq!(
            normalize("\u{915}\u{93c}", NormalForm::Nfc),
            "\u{915}\u{93c}"
        );
    }

    #[test]
    fn a_blocked_mark_does_not_reach_the_starter() {
        // U+0344 decomposes to two marks of the same class, and the
        // second cannot compose with the a because the first blocks it.
        assert_eq!(
            normalize("a\u{308}\u{301}", NormalForm::Nfc),
            "\u{e4}\u{301}"
        );
        assert_eq!(
            normalize("a\u{328}\u{301}", NormalForm::Nfc),
            "\u{105}\u{301}"
        );
    }

    #[test]
    fn every_form_has_a_name_and_answers_to_it() {
        for form in [
            NormalForm::Nfc,
            NormalForm::Nfd,
            NormalForm::Nfkc,
            NormalForm::Nfkd,
        ] {
            assert_eq!(NormalForm::from_name(form.name()), Some(form));
            assert_eq!(
                NormalForm::from_name(&form.name().to_ascii_lowercase()),
                Some(form)
            );
        }
        assert_eq!(NormalForm::from_name("NFX"), None);
    }
}
