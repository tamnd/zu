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
//!
//! The functions that answer a string sort themselves by how many
//! bytes they have to put somewhere. A trim puts none anywhere, since
//! what it answers is a part of what it was handed and those bytes are
//! already in a buffer somebody is holding, so its answers point back
//! into the argument. A fold writes as much as it was handed, an ASCII
//! fold never changing a byte's width. Anything that can change a
//! length, which is what a normal form does, has to write out an
//! amount it cannot know in advance, and that is the case the builder
//! exists for.

use std::sync::Arc;

use zu_common::{Result, ZuError};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::str::{INLINE_LEN, NO_BUFFERS, StrBuilder, StrView};
use crate::vector::{Aux, PhysType, ValueVector, VecEncoding};

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
    out.validity = carried(arena, v, len);
    Ok(out)
}

/// NULL in, NULL out: the answer is null exactly where the argument
/// was, and the bitmap is copied only where the argument carries one,
/// an absent one meaning every row is valid and costing nothing.
fn carried(arena: &mut MorselArena, v: &ValueVector, len: usize) -> Option<Bitmap> {
    v.validity.as_ref().map(|valid| {
        let mut copy = Bitmap::new_in(arena, len, true);
        copy.and_with(valid);
        copy
    })
}

/// Which fold a call asked for.
///
/// Both are the ASCII fold, which is the fold the row engine does and
/// the one the standard's default collation asks for: a letter of the
/// English alphabet changes case and every other byte is left as it
/// stands. It is worth saying what that buys, because it is the whole
/// reason these two are the string library's easy pair. An ASCII fold
/// never changes a byte's width, so the answer is exactly as long as
/// the argument, an answer that sat inside its view still sits inside
/// one, and a chunk's worth of answers needs no more room than the
/// chunk it was made from. A fold with a Unicode table behind it would
/// have none of that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrFold {
    /// UPPER.
    Upper,
    /// LOWER.
    Lower,
}

impl StrFold {
    fn name(self) -> &'static str {
        match self {
            StrFold::Upper => "upper",
            StrFold::Lower => "lower",
        }
    }

    /// The type the answers land in. A folded string is a string.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match arg {
            PhysType::Str => Some(PhysType::Str),
            _ => None,
        }
    }

    /// Whether a row could have no answer. Every string folds.
    pub fn may_raise(self) -> bool {
        false
    }
}

/// How many bytes of `v` live outside their views, which is how much
/// room a length preserving answer needs in the builder.
///
/// A short string is carried by the view itself and its answer will be
/// too, so it asks for nothing here. The dictionary case counts each
/// entry once, which is what the answers cost as well, the rows
/// pointing at bytes rather than holding them.
fn long_bytes(v: &ValueVector) -> usize {
    let outside = |n: usize| if n > INLINE_LEN { n } else { 0 };
    match v.encoding {
        VecEncoding::Constant => outside(v.constant_value::<StrView>().len()),
        VecEncoding::Dict { .. } => {
            let dict = v.dictionary();
            (0..dict.len())
                .map(|i| outside(dict.get(i as u32).len()))
                .sum()
        }
        VecEncoding::Flat => v
            .values::<StrView>()
            .iter()
            .map(|view| outside(view.len()))
            .sum(),
    }
}

/// Fold `src` into `dst`, which is the same length.
#[inline(always)]
fn fold_into(op: StrFold, src: &[u8], dst: &mut [u8]) {
    dst.copy_from_slice(src);
    match op {
        StrFold::Upper => dst.make_ascii_uppercase(),
        StrFold::Lower => dst.make_ascii_lowercase(),
    }
}

/// Evaluate `op(v)` into a new flat string vector.
///
/// The answers are flat views whatever the argument was, since a chunk
/// of computed strings is read back by row and the encodings are the
/// scan's business rather than a kernel's. What the encodings still
/// decide is how much folding happens: a constant folds once, and
/// codes over a table fold the table rather than the rows, so the
/// bytes of an entry are written into the answer once however many
/// rows point at it and the rest of the work is a gather over views.
/// Only a flat argument folds a string a row.
pub fn fold(arena: &mut MorselArena, op: StrFold, v: &ValueVector) -> Result<ValueVector> {
    if op.answer_type(v.phys).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?}",
            op.name(),
            v.phys
        )));
    }
    let len = v.len as usize;
    let bufs = v.str_buffers().unwrap_or(&NO_BUFFERS);
    let mut out = ValueVector::flat_uninit(arena, PhysType::Str, len);
    // What a fold answers is as long as what it was handed, so the room
    // the long answers need is the room the long arguments took and the
    // buffer is right the first time. Reading it is a walk of the view
    // headers, which the fold is about to walk anyway.
    let mut build = StrBuilder::with_capacity(long_bytes(v));
    match v.encoding {
        VecEncoding::Constant => {
            let view = v.constant_value::<StrView>();
            let bytes = view.bytes(bufs);
            let one = build.push_with(bytes.len(), |dst| fold_into(op, bytes, dst));
            out.values_mut::<StrView>()[..len].fill(one);
        }
        VecEncoding::Dict { .. } => {
            let dict = v.dictionary();
            let table: Vec<StrView> = (0..dict.len())
                .map(|i| {
                    let entry = dict.get(i as u32);
                    build.push_with(entry.len(), |dst| fold_into(op, entry, dst))
                })
                .collect();
            let codes = v.codes_u16();
            let dst = out.values_mut::<StrView>();
            for (slot, code) in dst[..len].iter_mut().zip(codes) {
                *slot = table[*code as usize];
            }
        }
        VecEncoding::Flat => {
            let views = v.values::<StrView>();
            let dst = out.values_mut::<StrView>();
            for (slot, view) in dst[..len].iter_mut().zip(views) {
                let bytes = view.bytes(bufs);
                *slot = build.push_with(bytes.len(), |room| fold_into(op, bytes, room));
            }
        }
    }
    out.aux = Aux::Str(Arc::new(build.finish()));
    out.validity = carried(arena, v, len);
    Ok(out)
}

/// Which ends a trim takes characters off.
///
/// ISO 20.24 spells six functions here and the kernel knows three,
/// because the whole of what the six disagree about is settled before a
/// plan is built. `TRIM` takes one character and raises `22027` when it
/// is handed more, and `BTRIM`, `LTRIM` and `RTRIM` take a set; that is
/// a question about what was written, not about what the loop does, so
/// the compiler asks it against the written set and this is left with
/// the only thing that varies a row at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrTrim {
    /// TRIM and BTRIM: both ends.
    Both,
    /// TRIM LEADING and LTRIM: the front.
    Leading,
    /// TRIM TRAILING and RTRIM: the back.
    Trailing,
}

impl StrTrim {
    fn name(self) -> &'static str {
        match self {
            StrTrim::Both => "trim",
            StrTrim::Leading => "ltrim",
            StrTrim::Trailing => "rtrim",
        }
    }

    fn front(self) -> bool {
        matches!(self, StrTrim::Both | StrTrim::Leading)
    }

    fn back(self) -> bool {
        matches!(self, StrTrim::Both | StrTrim::Trailing)
    }

    /// The type the answers land in. A trimmed string is a string.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match arg {
            PhysType::Str => Some(PhysType::Str),
            _ => None,
        }
    }

    /// Whether a row could have no answer. The one condition in the
    /// trim family is about the trim set rather than about a row, and
    /// it is settled at compile time, so nothing here can raise.
    pub fn may_raise(self) -> bool {
        false
    }
}

/// The characters a trim takes off, in the form the loop wants them.
///
/// The set is written once in a statement and asked about once a
/// character, so it is worth preparing. The plain characters become a
/// bitmask, one bit each, which is the whole test for the sets almost
/// every statement writes: a space, or a handful of punctuation. Any
/// character past ASCII is kept as the bytes it is written as, and
/// matching is then a comparison of bytes rather than a decode.
///
/// Bytes are enough because of what UTF-8 is. A byte below 128 is
/// always a whole character and never part of a longer one, and the
/// encoding of a character is never the front or the back of another
/// character's encoding, so a match on bytes is a match on characters
/// and no member of the set can be found halfway through something
/// else.
#[derive(Debug, Default)]
pub struct TrimSet {
    /// One bit a character for the members below 128.
    ascii: u128,
    /// The encodings of the members above it, which is usually none.
    wide: Vec<Box<[u8]>>,
}

impl TrimSet {
    /// The set a statement wrote, prepared.
    pub fn new(chars: &str) -> Self {
        let mut set = Self::default();
        for c in chars.chars() {
            if c.is_ascii() {
                set.ascii |= 1u128 << (c as u8);
            } else {
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes();
                if !set.wide.iter().any(|w| &**w == bytes) {
                    set.wide.push(bytes.into());
                }
            }
        }
        set
    }

    /// Whether the set has anything in it. An empty one trims nothing,
    /// which is the answer the row engine gives too.
    pub fn is_empty(&self) -> bool {
        self.ascii == 0 && self.wide.is_empty()
    }

    /// How many bytes the character at the front of `bytes` takes, when
    /// that character is a member. `bytes` is not empty.
    #[inline(always)]
    fn width_at(&self, bytes: &[u8]) -> Option<usize> {
        let head = bytes[0];
        if head < 0x80 {
            return ((self.ascii >> head) & 1 == 1).then_some(1);
        }
        self.wide
            .iter()
            .find(|w| bytes.starts_with(w))
            .map(|w| w.len())
    }

    /// The same for the character at the back of `bytes`.
    #[inline(always)]
    fn width_before(&self, bytes: &[u8]) -> Option<usize> {
        let last = bytes[bytes.len() - 1];
        if last < 0x80 {
            return ((self.ascii >> last) & 1 == 1).then_some(1);
        }
        self.wide
            .iter()
            .find(|w| bytes.ends_with(w))
            .map(|w| w.len())
    }
}

/// The part of `bytes` a trim leaves: where it starts and how long it
/// is.
///
/// Nothing is written here, which is the point. A trim of a string that
/// has nothing to take off walks one character at each end it trims and
/// answers the string it was given, and a trim that takes plenty off
/// still only walks what it takes.
#[inline(always)]
fn kept(ends: StrTrim, set: &TrimSet, bytes: &[u8]) -> (usize, usize) {
    let mut start = 0;
    let mut end = bytes.len();
    if ends.front() {
        while start < end {
            match set.width_at(&bytes[start..end]) {
                Some(n) => start += n,
                None => break,
            }
        }
    }
    if ends.back() {
        while start < end {
            match set.width_before(&bytes[start..end]) {
                Some(n) => end -= n,
                None => break,
            }
        }
    }
    (start, end - start)
}

/// Evaluate `ends(v, set)` into a new flat string vector.
///
/// This is the string kernel that fills no buffer. What a trim answers
/// is a part of what it was handed, and a part of a string is bytes
/// that are already somewhere, so a flat argument's answers are views
/// back into the argument's own buffers and the answer vector holds the
/// same buffers rather than a copy of them. A chunk of two thousand
/// trimmed strings costs two thousand views and not one byte more.
///
/// That is a saving in memory rather than in time. The walk still
/// tests the set once for every character it takes off, which on the
/// bench's fifteen byte strings costs about what the fold's copy of
/// the same string costs, so the two run level there.
///
/// A dictionary is the one shape that cannot do that, since the entries
/// of a table are not bytes a view can name, so the table's answers are
/// written out once an entry and the rows gather over them. That is
/// still a table's worth of copying rather than a chunk's.
pub fn trim(
    arena: &mut MorselArena,
    ends: StrTrim,
    set: &TrimSet,
    v: &ValueVector,
) -> Result<ValueVector> {
    if ends.answer_type(v.phys).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?}",
            ends.name(),
            v.phys
        )));
    }
    let len = v.len as usize;
    let bufs = v.str_buffers().unwrap_or(&NO_BUFFERS);
    let mut out = ValueVector::flat_uninit(arena, PhysType::Str, len);
    // Set only where the answers' bytes had to be written somewhere,
    // which is the dictionary and nothing else.
    let mut made = None;
    match v.encoding {
        VecEncoding::Constant => {
            let view = v.constant_value::<StrView>();
            let bytes = view.bytes(bufs);
            let (at, n) = kept(ends, set, bytes);
            out.values_mut::<StrView>()[..len].fill(view.sub(bytes, at, n));
        }
        VecEncoding::Dict { .. } => {
            let dict = v.dictionary();
            let mut build = StrBuilder::with_capacity(long_bytes(v));
            let table: Vec<StrView> = (0..dict.len())
                .map(|i| {
                    let entry = dict.get(i as u32);
                    let (at, n) = kept(ends, set, entry);
                    build.push(&entry[at..at + n])
                })
                .collect();
            let codes = v.codes_u16();
            let dst = out.values_mut::<StrView>();
            for (slot, code) in dst[..len].iter_mut().zip(codes) {
                *slot = table[*code as usize];
            }
            made = Some(build.finish());
        }
        VecEncoding::Flat => {
            let views = v.values::<StrView>();
            let dst = out.values_mut::<StrView>();
            for (slot, view) in dst[..len].iter_mut().zip(views) {
                let bytes = view.bytes(bufs);
                let (at, n) = kept(ends, set, bytes);
                *slot = view.sub(bytes, at, n);
            }
        }
    }
    out.aux = match made {
        Some(bufs) => Aux::Str(Arc::new(bufs)),
        // The argument's buffers, held rather than copied, because the
        // answers are pointing into them.
        None => match &v.aux {
            Aux::Str(bufs) => Aux::Str(Arc::clone(bufs)),
            _ => Aux::None,
        },
    };
    out.validity = carried(arena, v, len);
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

    /// Reads a string vector back the way a sink would, so a test can
    /// say what a fold answered without caring which side of the
    /// twelve byte line each answer fell.
    fn read(v: &ValueVector) -> Vec<String> {
        let bufs = v.str_buffers().unwrap_or(&NO_BUFFERS);
        v.values::<StrView>()
            .iter()
            .map(|view| String::from_utf8(view.bytes(bufs).to_vec()).unwrap())
            .collect()
    }

    /// The fold is the ASCII one, so the English letters change case
    /// and everything else is left exactly as it stands, an accented
    /// letter and an emoji included.
    #[test]
    fn a_fold_moves_the_english_letters_and_nothing_else() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["Ann", "bo", "", "héllo", "🙂 x9"]);
        let up = fold(&mut arena, StrFold::Upper, &v).unwrap();
        assert_eq!(up.phys, PhysType::Str);
        assert_eq!(read(&up), ["ANN", "BO", "", "HéLLO", "🙂 X9"]);
        let down = fold(&mut arena, StrFold::Lower, &v).unwrap();
        assert_eq!(read(&down), ["ann", "bo", "", "héllo", "🙂 x9"]);
    }

    /// An answer longer than a view holds has to land in the builder's
    /// buffer and read back through it, and an answer that fits stays
    /// inside its view, so a chunk holding both has to answer both.
    #[test]
    fn a_long_answer_lands_in_a_buffer_and_a_short_one_does_not() {
        let mut arena = MorselArena::new();
        let long = "a string well past the twelve bytes a view holds";
        let v = str_vector(&mut arena, &["short", long]);
        let up = fold(&mut arena, StrFold::Upper, &v).unwrap();
        let views = up.values::<StrView>();
        assert!(views[0].is_inline(), "a short answer stays in its view");
        assert!(!views[1].is_inline(), "a long one goes to the buffer");
        assert_eq!(read(&up), ["SHORT", &long.to_ascii_uppercase()]);
    }

    /// One string standing for the chunk is folded once, and every row
    /// reads the one answer back.
    #[test]
    fn a_constant_is_folded_once() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("héllo".as_bytes()),
            3,
        );
        v.aux = Aux::None;
        let up = fold(&mut arena, StrFold::Upper, &v).unwrap();
        assert_eq!(read(&up), ["HéLLO", "HéLLO", "HéLLO"]);
    }

    /// Codes over a table fold the table rather than the rows, and the
    /// answer still has to be what the same strings flat would have
    /// given, repeated codes and all.
    #[test]
    fn a_dictionary_is_folded_a_table_at_a_time() {
        let mut arena = MorselArena::new();
        let entries = ["Ann", "a longer name than a view holds", "bo"];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [2u16, 0, 1, 0, 1];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let up = fold(&mut arena, StrFold::Upper, &v).unwrap();
        let flat = str_vector(&mut arena, &codes.map(|c| entries[c as usize]));
        assert_eq!(
            read(&up),
            read(&fold(&mut arena, StrFold::Upper, &flat).unwrap())
        );
        assert_eq!(read(&up)[0], "BO");
        assert_eq!(read(&up)[2], entries[1].to_ascii_uppercase());
    }

    /// A null argument answers null, and the rows around it answer
    /// what they would have answered on their own.
    #[test]
    fn a_fold_of_a_null_is_null() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["Ann", "Bo", "Cy"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let up = fold(&mut arena, StrFold::Upper, &v).unwrap();
        assert!(!up.is_valid(1), "null in, null out");
        assert!(up.is_valid(0) && up.is_valid(2));
        assert_eq!(read(&up)[0], "ANN");
        assert_eq!(read(&up)[2], "CY");
    }

    /// Neither fold has a row it cannot answer, and a folded string is
    /// a string.
    #[test]
    fn a_fold_cannot_raise_and_answers_a_string() {
        assert!(!StrFold::Upper.may_raise());
        assert!(!StrFold::Lower.may_raise());
        assert_eq!(
            StrFold::Lower.answer_type(PhysType::Str),
            Some(PhysType::Str)
        );
        assert_eq!(StrFold::Upper.answer_type(PhysType::Float64), None);
    }

    /// The three ends, over a set of one character, which is the shape
    /// TRIM itself is limited to.
    #[test]
    fn each_end_trims_the_end_it_names() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["  ann  ", "bo", "   ", ""]);
        let set = TrimSet::new(" ");
        let both = trim(&mut arena, StrTrim::Both, &set, &v).unwrap();
        assert_eq!(both.phys, PhysType::Str);
        assert_eq!(read(&both), ["ann", "bo", "", ""]);
        let front = trim(&mut arena, StrTrim::Leading, &set, &v).unwrap();
        assert_eq!(read(&front), ["ann  ", "bo", "", ""]);
        let back = trim(&mut arena, StrTrim::Trailing, &set, &v).unwrap();
        assert_eq!(read(&back), ["  ann", "bo", "", ""]);
    }

    /// A set of several characters, which is what btrim and its two
    /// neighbours exist for: any member comes off, in any order, until
    /// something that is not a member is reached.
    #[test]
    fn a_set_takes_any_of_its_members_off() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["xyxaxbyx", "xxx", "axb"]);
        let set = TrimSet::new("xy");
        let out = trim(&mut arena, StrTrim::Both, &set, &v).unwrap();
        assert_eq!(read(&out), ["axb", "", "axb"]);
    }

    /// A member past ASCII is matched as the bytes it is written as,
    /// and matching bytes has to still be matching characters: a trim
    /// of the accented letter must not eat half of it or half of
    /// anything else.
    #[test]
    fn a_wide_member_comes_off_whole() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["ééhélloé", "🙂a🙂", "é🙂é"]);
        let set = TrimSet::new("é🙂");
        let out = trim(&mut arena, StrTrim::Both, &set, &v).unwrap();
        assert_eq!(read(&out), ["héllo", "a", ""]);
        // The accented letter's second byte is a continuation byte and
        // shares it with plenty of others, so a trim that matched bytes
        // one at a time would cut this string apart.
        let v = str_vector(&mut arena, &["ñ"]);
        let out = trim(&mut arena, StrTrim::Both, &TrimSet::new("é"), &v).unwrap();
        assert_eq!(read(&out), ["ñ"]);
    }

    /// An empty set trims nothing, which is what the row engine
    /// answers: nothing is a member, so the first character at either
    /// end stops the walk.
    #[test]
    fn an_empty_set_answers_the_string_it_was_given() {
        let mut arena = MorselArena::new();
        let set = TrimSet::new("");
        assert!(set.is_empty());
        let v = str_vector(&mut arena, &["  ann  ", ""]);
        let out = trim(&mut arena, StrTrim::Both, &set, &v).unwrap();
        assert_eq!(read(&out), ["  ann  ", ""]);
    }

    /// The whole reason a trim costs no memory: a flat argument's
    /// answers point back into the argument's own bytes, so the answer
    /// vector holds the buffers it was handed and fills none of its
    /// own.
    #[test]
    fn a_trimmed_string_points_back_at_what_it_was_trimmed_from() {
        let mut arena = MorselArena::new();
        let long = "  a string well past the twelve bytes a view holds  ";
        let v = str_vector(&mut arena, &[long, "  short  "]);
        let out = trim(&mut arena, StrTrim::Both, &TrimSet::new(" "), &v).unwrap();
        assert_eq!(read(&out), [long.trim(), "short"]);
        let views = out.values::<StrView>();
        assert!(!views[0].is_inline(), "a long answer stays in the buffer");
        assert!(views[1].is_inline(), "a short one fits in its view");
        // Handed over, not copied: the answers are reading the
        // argument's bytes where they already lie.
        let (Aux::Str(theirs), Aux::Str(ours)) = (&v.aux, &out.aux) else {
            panic!("a chunk with a long string in it carries buffers");
        };
        assert!(Arc::ptr_eq(theirs, ours));
    }

    /// One string standing for the chunk is trimmed once, and every row
    /// reads the one answer back.
    #[test]
    fn a_constant_is_trimmed_once() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("  ann  ".as_bytes()),
            3,
        );
        v.aux = Aux::None;
        let out = trim(&mut arena, StrTrim::Both, &TrimSet::new(" "), &v).unwrap();
        assert_eq!(read(&out), ["ann", "ann", "ann"]);
    }

    /// Codes over a table trim the table rather than the rows, and the
    /// answer still has to be what the same strings flat would have
    /// given.
    #[test]
    fn a_dictionary_is_trimmed_a_table_at_a_time() {
        let mut arena = MorselArena::new();
        let entries = ["  a longer entry than a view holds  ", " ann ", "bo  "];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [2u16, 0, 1, 0];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let set = TrimSet::new(" ");
        let out = trim(&mut arena, StrTrim::Both, &set, &v).unwrap();
        let flat = str_vector(&mut arena, &codes.map(|c| entries[c as usize]));
        assert_eq!(
            read(&out),
            read(&trim(&mut arena, StrTrim::Both, &set, &flat).unwrap())
        );
        assert_eq!(read(&out)[0], "bo");
        assert_eq!(read(&out)[1], entries[0].trim());
    }

    /// A null argument answers null, and the rows around it answer what
    /// they would have answered on their own.
    #[test]
    fn a_trim_of_a_null_is_null() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &[" ann ", " bo ", " cy "]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let out = trim(&mut arena, StrTrim::Both, &TrimSet::new(" "), &v).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
        assert!(out.is_valid(0) && out.is_valid(2));
        assert_eq!(read(&out)[0], "ann");
        assert_eq!(read(&out)[2], "cy");
    }

    /// No trim has a row it cannot answer, the one condition in the
    /// family being about the set rather than about a row, and a
    /// trimmed string is a string.
    #[test]
    fn a_trim_cannot_raise_and_answers_a_string() {
        assert!(!StrTrim::Both.may_raise());
        assert_eq!(
            StrTrim::Leading.answer_type(PhysType::Str),
            Some(PhysType::Str)
        );
        assert_eq!(StrTrim::Trailing.answer_type(PhysType::Int64), None);
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
        let Err(err) = fold(&mut arena, StrFold::Upper, &v) else {
            panic!("a number was folded as a string");
        };
        assert!(err.to_string().contains("upper"));
        let Err(err) = trim(&mut arena, StrTrim::Both, &TrimSet::new(" "), &v) else {
            panic!("a number was trimmed as a string");
        };
        assert!(err.to_string().contains("trim"));
    }
}
