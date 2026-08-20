//! The string library as vector kernels (ISO 20.9, features GF04 to
//! GF07).
//!
//! The numeric kernels beside this one all answer a number the same
//! width as the register they wrote it in, so the only question they
//! had was whether a row has an answer. A string function asks a
//! second question the numeric ones never do: where do the answer's
//! bytes live. The two length functions are here first because they
//! are the ones that do not ask it. An answer that is a count is a
//! whole number, so the output is an ordinary integer vector and
//! nothing is written into the arena but numbers.
//!
//! That makes the lengths the place to settle the other half of the
//! string story, which is the encodings. A column of strings reaches a
//! kernel three ways: flat views, one constant standing for the chunk,
//! and dictionary codes over a shared table of entries. The numeric
//! kernels refuse the third and ask the caller to materialize first;
//! here it is the cheapest of the three, because the length of a code
//! is the length of its entry and a chunk of two thousand rows over a
//! table of a thousand entries costs a thousand counts and a gather
//! rather than two thousand counts. The narrower the table the wider
//! the gap, and a column of a few dozen distinct values is where the
//! saving is worth having.

use zu_common::{Result, ZuError};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::str::{NO_BUFFERS, StrView};
use crate::vector::{PhysType, ValueVector, VecEncoding};

/// Which length a call asked for.
///
/// The two are one kernel because they differ in a single line and
/// agree about everything else that is hard: the encodings, the nulls,
/// and the fact that neither can raise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrLen {
    /// CHAR_LENGTH, and CHARACTER_LENGTH under its long spelling. The
    /// count of characters, which over UTF-8 is the count of bytes
    /// that are not continuations of the one before them.
    Chars,
    /// OCTET_LENGTH. The count of bytes, which a view carries in its
    /// header, so this one never reads a payload at all.
    Octets,
}

impl StrLen {
    fn name(self) -> &'static str {
        match self {
            StrLen::Chars => "char_length",
            StrLen::Octets => "octet_length",
        }
    }

    /// The type the answers land in. A count is a whole number
    /// whatever it counted, and the argument has to be a string.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match arg {
            PhysType::Str => Some(PhysType::Int64),
            _ => None,
        }
    }

    /// Whether a row could have no answer. Neither of these can: every
    /// string has a length, and the count of anything a column holds
    /// is far inside the range of an integer.
    pub fn may_raise(self) -> bool {
        false
    }
}

/// The length of one string, in whichever unit was asked for.
///
/// The character count is written as a fold over the bytes rather than
/// as a decode, because a byte begins a character exactly when it is
/// not a continuation byte, and that is one comparison per byte with
/// nothing the compiler cannot lay out flat. It is the answer the row
/// engine gives, which counts what Rust calls chars.
#[inline(always)]
fn measure(op: StrLen, bytes: &[u8]) -> i64 {
    match op {
        StrLen::Octets => bytes.len() as i64,
        StrLen::Chars => bytes.iter().filter(|b| (**b as i8) >= -0x40).count() as i64,
    }
}

/// Evaluate `op(v)` into a new flat integer vector.
///
/// Nothing here can raise, so unlike the numeric kernels there is no
/// pass to decide whether the loop is allowed to run. A row outside
/// the selection and a row holding a null are measured anyway, since
/// measuring one costs less than the branch that would skip it and
/// neither answer is ever read.
pub fn length(arena: &mut MorselArena, op: StrLen, v: &ValueVector) -> Result<ValueVector> {
    if op.answer_type(v.phys).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?}",
            op.name(),
            v.phys
        )));
    }
    let len = v.len as usize;
    let bufs = v.str_buffers().unwrap_or(&NO_BUFFERS);
    let mut out = ValueVector::flat_uninit(arena, PhysType::Int64, len);
    match v.encoding {
        // One string standing for the chunk, so one measurement stands
        // for every row of it.
        VecEncoding::Constant => {
            let view = v.constant_value::<StrView>();
            let n = measure(op, view.bytes(bufs));
            out.values_mut::<i64>()[..len].fill(n);
        }
        // Codes over a shared table. The length of a code is the
        // length of its entry, so the table is measured once and the
        // rows are a gather over the answers.
        VecEncoding::Dict { .. } => {
            let dict = v.dictionary();
            let table: Vec<i64> = (0..dict.len())
                .map(|i| measure(op, dict.get(i as u32)))
                .collect();
            let codes = v.codes_u16();
            let dst = out.values_mut::<i64>();
            for (slot, code) in dst[..len].iter_mut().zip(codes) {
                *slot = table[*code as usize];
            }
        }
        VecEncoding::Flat => {
            let views = v.values::<StrView>();
            let dst = out.values_mut::<i64>();
            match op {
                // A view carries its byte count in its header, so the
                // count of bytes is a read of the header and the
                // payload is never touched at all.
                StrLen::Octets => {
                    for (slot, view) in dst[..len].iter_mut().zip(views) {
                        *slot = view.len() as i64;
                    }
                }
                StrLen::Chars => {
                    for (slot, view) in dst[..len].iter_mut().zip(views) {
                        *slot = measure(op, view.bytes(bufs));
                    }
                }
            }
        }
    }
    // NULL in, NULL out: the answer is null exactly where the argument
    // was, and the bitmap is copied only where the argument carries
    // one.
    out.validity = v.validity.as_ref().map(|valid| {
        let mut copy = Bitmap::new_in(arena, len, true);
        copy.and_with(valid);
        copy
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str::Dictionary;
    use crate::vector::{Aux, str_vector};
    use std::sync::Arc;

    /// A count of characters and a count of bytes are the same number
    /// only while the strings are plain, and they part company at the
    /// first one that is not.
    #[test]
    fn the_two_lengths_part_company_outside_ascii() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["", "abc", "héllo", "🙂", "a\tb"]);
        let chars = length(&mut arena, StrLen::Chars, &v).unwrap();
        assert_eq!(chars.phys, PhysType::Int64);
        assert_eq!(chars.values::<i64>(), &[0, 3, 5, 1, 3]);
        let octets = length(&mut arena, StrLen::Octets, &v).unwrap();
        assert_eq!(octets.values::<i64>(), &[0, 3, 6, 4, 3]);
    }

    /// A string longer than a view holds inline lives in a buffer, and
    /// the character count has to resolve through it rather than
    /// reading the header and stopping.
    #[test]
    fn a_string_too_long_to_sit_inline_is_measured_through_its_buffer() {
        let mut arena = MorselArena::new();
        let long = "a string well past the twelve bytes a view holds";
        let v = str_vector(&mut arena, &["short", long]);
        let chars = length(&mut arena, StrLen::Chars, &v).unwrap();
        assert_eq!(chars.values::<i64>(), &[5, long.chars().count() as i64]);
        let octets = length(&mut arena, StrLen::Octets, &v).unwrap();
        assert_eq!(octets.values::<i64>(), &[5, long.len() as i64]);
    }

    /// One string standing for the chunk is measured once, and the
    /// answer is the same for every row of it.
    #[test]
    fn a_constant_is_measured_once() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("héllo".as_bytes()),
            4,
        );
        v.aux = Aux::None;
        let out = length(&mut arena, StrLen::Chars, &v).unwrap();
        assert_eq!(out.values::<i64>(), &[5, 5, 5, 5]);
    }

    /// Codes over a shared table, which the numeric kernels refuse and
    /// this one answers faster than either of the other two encodings.
    /// The answer has to be what the same strings flat would have
    /// given.
    #[test]
    fn a_dictionary_is_measured_a_table_at_a_time() {
        let mut arena = MorselArena::new();
        let entries = ["", "a", "héllo", "🙂"];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [3u16, 0, 2, 1, 2];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let chars = length(&mut arena, StrLen::Chars, &v).unwrap();
        assert_eq!(chars.values::<i64>(), &[1, 0, 5, 1, 5]);
        let octets = length(&mut arena, StrLen::Octets, &v).unwrap();
        assert_eq!(octets.values::<i64>(), &[4, 0, 6, 1, 6]);
    }

    /// A null argument answers null rather than nought, which is the
    /// one thing a count must not quietly get wrong: nought is the
    /// length of the empty string and says something else entirely.
    #[test]
    fn a_null_argument_answers_null_and_not_nought() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["ab", "", "cde"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let out = length(&mut arena, StrLen::Chars, &v).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
        assert!(out.is_valid(0) && out.is_valid(2));
        assert_eq!(out.values::<i64>()[0], 2);
        assert_eq!(out.values::<i64>()[2], 3);
    }

    /// Neither of these has a row it cannot answer, which is what lets
    /// one of them stand in a projection and behind an OR where a root
    /// or a distance from nought cannot.
    #[test]
    fn a_length_cannot_raise_and_answers_a_whole_number() {
        assert!(!StrLen::Chars.may_raise());
        assert!(!StrLen::Octets.may_raise());
        assert_eq!(
            StrLen::Chars.answer_type(PhysType::Str),
            Some(PhysType::Int64)
        );
        assert_eq!(StrLen::Octets.answer_type(PhysType::Int64), None);
    }

    /// A number is not a string, and a kernel handed one says so
    /// rather than reading sixteen bytes of it as a view.
    #[test]
    fn a_number_is_not_a_string() {
        let mut arena = MorselArena::new();
        let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2]);
        let Err(err) = length(&mut arena, StrLen::Chars, &v) else {
            panic!("a number was measured as a string");
        };
        assert!(err.to_string().contains("char_length"));
    }
}
