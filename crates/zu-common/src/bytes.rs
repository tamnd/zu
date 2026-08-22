//! Byte strings, GV35: how one is written down and how one is read
//! back.
//!
//! A byte string is a sequence of octets, and there is no encoding to
//! guess at: the value is the bytes. What there is, is a spelling, and
//! ISO 21.3 gives it as `X'00AB'`, two hexits to a byte. Writing and
//! reading that spelling is the whole of this module, and it is here
//! rather than beside the lexer because the same spelling comes back
//! out of a result column, so a value that went in as `X'00ab'` and
//! came back as `X'00AB'` is the same value and reads as one.

/// The hexits of a byte string, upper case, no quotes and no `X`.
///
/// Upper case because ISO writes the literal that way, and a reader
/// comparing two of these is comparing text: one case is one answer.
pub fn hexits(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).expect("a nibble is a hexit"));
        out.push(char::from_digit(u32::from(b & 0xf), 16).expect("a nibble is a hexit"));
    }
    out.to_ascii_uppercase()
}

/// The literal a byte string is written as, quotes and `X` and all.
pub fn literal(bytes: &[u8]) -> String {
    format!("X'{}'", hexits(bytes))
}

/// The bytes a run of hexits names, `None` for anything that is not a
/// run of hexits or names half a byte.
///
/// Space is allowed anywhere and dropped, which is what the standard's
/// production allows and what lets a long literal be written in groups.
pub fn from_hexits(text: &str) -> Option<Vec<u8>> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_whitespace() {
            continue;
        }
        nibbles.push(c.to_digit(16)? as u8);
    }
    if !nibbles.len().is_multiple_of(2) {
        return None;
    }
    Some(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_string_writes_two_hexits_a_byte() {
        assert_eq!(literal(&[0x00, 0xab, 0x00]), "X'00AB00'");
        assert_eq!(literal(&[]), "X''");
    }

    #[test]
    fn the_spelling_reads_back_as_what_was_written() {
        for case in [&[][..], &[0][..], &[0xde, 0xad, 0xbe, 0xef][..]] {
            assert_eq!(from_hexits(&hexits(case)).as_deref(), Some(case));
        }
    }

    #[test]
    fn either_case_of_hexit_reads_and_space_is_dropped() {
        assert_eq!(from_hexits("00ab00"), Some(vec![0x00, 0xab, 0x00]));
        assert_eq!(from_hexits("00 AB 00"), Some(vec![0x00, 0xab, 0x00]));
    }

    #[test]
    fn half_a_byte_and_a_digit_that_is_not_a_hexit_are_not_byte_strings() {
        assert_eq!(from_hexits("abc"), None);
        assert_eq!(from_hexits("zz"), None);
    }
}
