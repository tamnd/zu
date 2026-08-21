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
//! into the argument, and the two substring functions are the same
//! thing counted from one end. A fold writes as much as it was handed,
//! an ASCII fold never changing a byte's width. Anything that can
//! change a length, which is what a normal form does, has to write out
//! an amount it cannot know in advance, and that is the case the
//! builder exists for.

use std::sync::Arc;

use zu_common::unicode::{self, NormalForm};
use zu_common::{Result, ZuError, gqlstatus::codes};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::str::{INLINE_LEN, NO_BUFFERS, StrBuffers, StrBuilder, StrView};
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

/// Which of the two normalization functions a call asked for.
///
/// They are one kernel's worth of thinking read two ways. `NORMALIZE`
/// answers the string a form asks for and `IS NORMALIZED` answers
/// whether the argument is that string already, so the second is the
/// first with a comparison in place of an answer, and the standard
/// defines it in exactly those words rather than through the quick
/// check properties.
///
/// What separates them here is where the answer goes. A normalized
/// string is a string and lands in a vector; a test is a predicate and
/// lands in a bitmap, the way a comparison does, so the two have
/// separate entry points below and this says which is which for the
/// compiler's benefit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrNorm {
    /// NORMALIZE(s, form).
    Into(NormalForm),
    /// s IS NORMALIZED form.
    Test(NormalForm),
}

impl StrNorm {
    fn name(self) -> &'static str {
        match self {
            StrNorm::Into(_) => "normalize",
            StrNorm::Test(_) => "is_normalized",
        }
    }

    /// The form the call named, which the statement wrote or which the
    /// standard's default supplied.
    pub fn form(self) -> NormalForm {
        match self {
            StrNorm::Into(form) | StrNorm::Test(form) => form,
        }
    }

    /// The type the answers land in. Both want a string; one gives a
    /// string back and the other gives a truth value.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match (self, arg) {
            (StrNorm::Into(_), PhysType::Str) => Some(PhysType::Str),
            (StrNorm::Test(_), PhysType::Str) => Some(PhysType::Bool),
            _ => None,
        }
    }

    /// Whether a row could have no answer. Every string has a normal
    /// form and every string either is in it or is not, so neither of
    /// these can raise.
    pub fn may_raise(self) -> bool {
        false
    }
}

/// Whether every string in `v` is plain ASCII, in which case a
/// normalization has nothing to do to any of them.
///
/// This is the question worth asking before the work starts. No
/// character below 128 decomposes, canonically or by compatibility, and
/// none of them is a combining mark, so a string of them is in all four
/// forms already and what a normalization answers is what it was
/// handed. Asking is a walk of the bytes with nothing to decide, which
/// runs a word at a time, against a walk that decodes every character,
/// looks it up, sorts what comes back and writes it out again.
///
/// The dictionary encoding is not asked, because its answer would not
/// help. The bytes of a table entry are not bytes a view can point at,
/// so a coded chunk writes its table out either way.
fn all_ascii(v: &ValueVector, bufs: &StrBuffers) -> bool {
    match v.encoding {
        VecEncoding::Constant => v.constant_value::<StrView>().bytes(bufs).is_ascii(),
        VecEncoding::Dict { .. } => false,
        VecEncoding::Flat => v
            .values::<StrView>()
            .iter()
            .all(|view| view.bytes(bufs).is_ascii()),
    }
}

/// One string, normalized into the builder.
///
/// The ASCII test is asked a second time here, a string at a time,
/// because a chunk holding one accented string still holds plenty that
/// are plain and a plain one is a copy rather than a decode. What the
/// test costs when the answer is no is a walk of bytes the
/// normalization is about to walk anyway.
///
/// Bytes that are not UTF-8 cannot arrive, a vector's strings being
/// UTF-8 by construction, and if they somehow did then handing them
/// back as they stand loses nothing that was there. A kernel that
/// promised it cannot raise does not get to change its mind over a
/// buffer that was already wrong.
#[inline]
fn put(build: &mut StrBuilder, form: NormalForm, bytes: &[u8]) -> StrView {
    if bytes.is_ascii() {
        return build.push(bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => build.push(unicode::normalize(s, form).as_bytes()),
        Err(_) => build.push(bytes),
    }
}

/// Evaluate `NORMALIZE(v, form)` into a new flat string vector.
///
/// This is the string function the builder was built for. A trim
/// answers a part of what it was handed and a fold answers something
/// exactly as long, so both of them know where the answer's bytes are
/// going before they start. A normal form knows neither: composing
/// makes a string shorter, decomposing makes it longer, and how much
/// either way is a fact about the characters rather than about the
/// count of them. So the answers are written out and the room for them
/// is a guess, the argument's own long bytes, which is right for
/// everything a query normally holds and grows when it is not.
///
/// The one shape that costs nothing is the one nearly every column is:
/// all ASCII, where the answers are the argument's own strings and the
/// vector hands its buffers over the way a trim does.
pub fn normalize(
    arena: &mut MorselArena,
    form: NormalForm,
    v: &ValueVector,
) -> Result<ValueVector> {
    let op = StrNorm::Into(form);
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
    if all_ascii(v, bufs) {
        let dst = out.values_mut::<StrView>();
        match v.encoding {
            VecEncoding::Constant => dst[..len].fill(v.constant_value::<StrView>()),
            _ => {
                for (slot, view) in dst[..len].iter_mut().zip(v.values::<StrView>()) {
                    *slot = *view;
                }
            }
        }
        // The argument's buffers, held rather than copied, because the
        // answers are its own strings and are reading them where they
        // already lie.
        out.aux = match &v.aux {
            Aux::Str(bufs) => Aux::Str(Arc::clone(bufs)),
            _ => Aux::None,
        };
    } else {
        let mut build = StrBuilder::with_capacity(long_bytes(v));
        match v.encoding {
            VecEncoding::Constant => {
                let view = v.constant_value::<StrView>();
                let one = put(&mut build, form, view.bytes(bufs));
                out.values_mut::<StrView>()[..len].fill(one);
            }
            VecEncoding::Dict { .. } => {
                let dict = v.dictionary();
                let table: Vec<StrView> = (0..dict.len())
                    .map(|i| put(&mut build, form, dict.get(i as u32)))
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
                    *slot = put(&mut build, form, view.bytes(bufs));
                }
            }
        }
        out.aux = Aux::Str(Arc::new(build.finish()));
    }
    out.validity = carried(arena, v, len);
    Ok(out)
}

/// Write `v IS [NOT] NORMALIZED form` into `out`, which must be sized to
/// the vector and cleared.
///
/// The negation is carried in rather than done afterwards, and that is
/// not a shortcut but the only correct place for it. A predicate over a
/// column of three-valued logic is a bitmap of the rows that passed, so
/// a null row is off in the bitmap; taking the complement of the bitmap
/// would turn every null row into a row that passed, and a null is not
/// normalized and is not unnormalized either. Deciding the row here and
/// masking the nulls off afterwards answers `IS NOT NORMALIZED` the way
/// the row engine answers it, which is why there is no general NOT over
/// a predicate register anywhere in this crate.
pub fn normalized(
    form: NormalForm,
    negated: bool,
    v: &ValueVector,
    out: &mut Bitmap,
) -> Result<()> {
    let op = StrNorm::Test(form);
    debug_assert_eq!(out.len(), v.len as usize);
    if op.answer_type(v.phys).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?}",
            op.name(),
            v.phys
        )));
    }
    let len = v.len as usize;
    let bufs = v.str_buffers().unwrap_or(&NO_BUFFERS);
    // ASCII is in every form already, which the row engine's answer
    // reads as well, so a plain column is decided without a table being
    // touched. Bytes that are not UTF-8 are called normalized for the
    // same reason the other kernel hands them back untouched.
    let holds = |bytes: &[u8]| match std::str::from_utf8(bytes) {
        Ok(s) => unicode::is_normalized(s, form),
        Err(_) => true,
    };
    match v.encoding {
        VecEncoding::Constant => {
            if holds(v.constant_value::<StrView>().bytes(bufs)) != negated {
                out.words_mut().fill(!0u64);
            }
        }
        VecEncoding::Dict { .. } => {
            let dict = v.dictionary();
            let table: Vec<bool> = (0..dict.len())
                .map(|i| holds(dict.get(i as u32)) != negated)
                .collect();
            for (row, code) in v.codes_u16()[..len].iter().enumerate() {
                if table[*code as usize] {
                    out.set(row);
                }
            }
        }
        VecEncoding::Flat => {
            for (row, view) in v.values::<StrView>()[..len].iter().enumerate() {
                if holds(view.bytes(bufs)) != negated {
                    out.set(row);
                }
            }
        }
    }
    // A row with no string in it has no answer either way.
    if let Some(valid) = &v.validity {
        out.and_with(valid);
    }
    out.mask_tail();
    Ok(())
}

/// Which end of a string a substring function counts from.
///
/// `LEFT` and `RIGHT` are the whole of the substring function in GQL,
/// `SUBSTRING` being a word the standard has reserved and left without
/// a meaning, so a query that wants the middle of a string writes one
/// of these inside the other. They are one kernel because the only
/// thing that differs is which end the walk starts at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrCut {
    /// `LEFT(s, n)`: the first n characters.
    Left,
    /// `RIGHT(s, n)`: the last n characters.
    Right,
}

impl StrCut {
    fn name(self) -> &'static str {
        match self {
            StrCut::Left => "left",
            StrCut::Right => "right",
        }
    }

    /// The type the answers land in. A part of a string is a string,
    /// and the count has to be a whole number by the time it gets here:
    /// a float count is one the row engine either truncates or raises
    /// over, and both of those are its own words, so a column of them
    /// goes back to it.
    pub fn answer_type(self, arg: PhysType, count: PhysType) -> Option<PhysType> {
        match (arg, count) {
            (PhysType::Str, PhysType::Int64) => Some(PhysType::Str),
            _ => None,
        }
    }

    /// Whether a row could have no answer. A count is a number a row
    /// can hold and a negative one raises `22011`, so the answer here
    /// is yes and the compiler settles the case where the statement
    /// wrote the count out.
    pub fn may_raise(self) -> bool {
        true
    }
}

/// Where the first `count` characters of `bytes` end.
///
/// The walk is over what it takes rather than over what it was handed,
/// which is what makes a cut cheap on a long string: `LEFT(s, 3)` looks
/// at three characters however many the column holds. A character is
/// one lead byte and the continuation bytes behind it, and a byte that
/// is not UTF-8 counts as a character of its own, which is the only
/// answer available and never reached through the row engine's types.
#[inline(always)]
fn prefix_end(bytes: &[u8], count: usize) -> usize {
    let mut at = 0;
    for _ in 0..count {
        if at == bytes.len() {
            break;
        }
        at += 1;
        while at < bytes.len() && bytes[at] & 0xC0 == 0x80 {
            at += 1;
        }
    }
    at
}

/// Where the last `count` characters of `bytes` begin, walked from the
/// back the same way.
#[inline(always)]
fn suffix_start(bytes: &[u8], count: usize) -> usize {
    let mut at = bytes.len();
    for _ in 0..count {
        if at == 0 {
            break;
        }
        at -= 1;
        while at > 0 && bytes[at] & 0xC0 == 0x80 {
            at -= 1;
        }
    }
    at
}

/// The part of `bytes` a cut keeps: where it starts and how long it is.
#[inline(always)]
fn taken(cut: StrCut, bytes: &[u8], count: usize) -> (usize, usize) {
    match cut {
        StrCut::Left => (0, prefix_end(bytes, count)),
        StrCut::Right => {
            let at = suffix_start(bytes, count);
            (at, bytes.len() - at)
        }
    }
}

/// The count a row asks for, clamped at nought.
///
/// A negative count has already raised by the time this runs, unless
/// the row is one nobody selected or one whose string is null, and
/// neither of those has an answer anybody reads.
#[inline(always)]
fn count_at(n: &ValueVector, row: usize) -> usize {
    let c = match n.encoding {
        VecEncoding::Constant => n.constant_value::<i64>(),
        _ => n.values::<i64>()[row],
    };
    c.max(0) as usize
}

/// Raises what the row engine raises for the first selected row that
/// asks for a negative number of characters.
///
/// The order matters and is the row engine's. A null string answers
/// null before the count is looked at, so `LEFT(null, -1)` is null and
/// not a condition, and a null count answers null as well. What is left
/// is a row holding a string and a negative number, which is the one
/// the standard gives `22011` to.
///
/// The fold comes first: a chunk whose counts are all at nought or
/// above has no such row in it whatever the strings are, and that is
/// every chunk a statement with a written count produces.
fn check_counts(
    cut: StrCut,
    s: &ValueVector,
    n: &ValueVector,
    sel: Option<&SelVector>,
    len: usize,
) -> Result<()> {
    let lowest = match n.encoding {
        VecEncoding::Constant => n.constant_value::<i64>(),
        _ => n.values::<i64>()[..len].iter().copied().min().unwrap_or(0),
    };
    if lowest >= 0 {
        return Ok(());
    }
    let visit = |row: usize| -> Result<()> {
        if !s.is_valid(row) || !n.is_valid(row) {
            return Ok(());
        }
        let asked = match n.encoding {
            VecEncoding::Constant => n.constant_value::<i64>(),
            _ => n.values::<i64>()[row],
        };
        if asked < 0 {
            return Err(ZuError::gql(
                codes::C22011,
                format!(
                    "{}() was asked for {asked} characters, and a string has no negative number of them",
                    cut.name()
                ),
            ));
        }
        Ok(())
    };
    match sel {
        Some(sel) => {
            for &row in sel.as_slice() {
                visit(row as usize)?;
            }
        }
        None => {
            for row in 0..len {
                visit(row)?;
            }
        }
    }
    Ok(())
}

/// Evaluate `cut(s, n)` into a new flat string vector.
///
/// This is the trim's trick a second time. What a cut answers is a part
/// of what it was handed, so a flat or a constant argument's answers
/// are views back into the argument's own bytes and the answer vector
/// holds the same buffers rather than a copy of them. What is new is
/// that the part is decided by a second column rather than by something
/// the statement wrote, so the two encodings multiply: a count that is
/// the same for the whole chunk lets a dictionary cut its table once,
/// and a count that varies makes a coded column write a row at a time,
/// since two rows over one entry no longer answer the same thing.
///
/// A dictionary is the one shape that has to write at all, an entry of
/// a table not being bytes a view can name. Everything else copies
/// nothing.
pub fn cut(
    arena: &mut MorselArena,
    cut: StrCut,
    s: &ValueVector,
    n: &ValueVector,
    sel: Option<&SelVector>,
) -> Result<ValueVector> {
    debug_assert_eq!(s.len, n.len);
    if cut.answer_type(s.phys, n.phys).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?} and {:?}",
            cut.name(),
            s.phys,
            n.phys
        )));
    }
    if matches!(n.encoding, VecEncoding::Dict { .. }) {
        return Err(ZuError::InvalidArgument(
            "substring counts on dict vectors: materialize first".into(),
        ));
    }
    let len = s.len as usize;
    check_counts(cut, s, n, sel, len)?;
    let one_count = matches!(n.encoding, VecEncoding::Constant);
    let bufs = s.str_buffers().unwrap_or(&NO_BUFFERS);
    let mut out = ValueVector::flat_uninit(arena, PhysType::Str, len);
    // Set only where the answers' bytes had to be written somewhere,
    // which is the dictionary and nothing else.
    let mut made = None;
    match s.encoding {
        VecEncoding::Constant => {
            let view = s.constant_value::<StrView>();
            let bytes = view.bytes(bufs);
            let dst = out.values_mut::<StrView>();
            if one_count {
                let (at, took) = taken(cut, bytes, count_at(n, 0));
                dst[..len].fill(view.sub(bytes, at, took));
            } else {
                for (row, slot) in dst[..len].iter_mut().enumerate() {
                    let (at, took) = taken(cut, bytes, count_at(n, row));
                    *slot = view.sub(bytes, at, took);
                }
            }
        }
        VecEncoding::Dict { .. } => {
            let dict = s.dictionary();
            let codes = s.codes_u16();
            let mut build = StrBuilder::with_capacity(long_bytes(s));
            let dst = out.values_mut::<StrView>();
            if one_count {
                let count = count_at(n, 0);
                let table: Vec<StrView> = (0..dict.len())
                    .map(|i| {
                        let entry = dict.get(i as u32);
                        let (at, took) = taken(cut, entry, count);
                        build.push(&entry[at..at + took])
                    })
                    .collect();
                for (slot, code) in dst[..len].iter_mut().zip(codes) {
                    *slot = table[*code as usize];
                }
            } else {
                for (row, slot) in dst[..len].iter_mut().enumerate() {
                    let entry = dict.get(codes[row] as u32);
                    let (at, took) = taken(cut, entry, count_at(n, row));
                    *slot = build.push(&entry[at..at + took]);
                }
            }
            made = Some(build.finish());
        }
        VecEncoding::Flat => {
            let views = s.values::<StrView>();
            let dst = out.values_mut::<StrView>();
            for (row, (slot, view)) in dst[..len].iter_mut().zip(views).enumerate() {
                let bytes = view.bytes(bufs);
                let (at, took) = taken(cut, bytes, count_at(n, row));
                *slot = view.sub(bytes, at, took);
            }
        }
    }
    out.aux = match made {
        Some(bufs) => Aux::Str(Arc::new(bufs)),
        // The argument's buffers, held rather than copied, because the
        // answers are pointing into them.
        None => match &s.aux {
            Aux::Str(bufs) => Aux::Str(Arc::clone(bufs)),
            _ => Aux::None,
        },
    };
    // Either argument being null makes the answer null, which is two
    // columns' worth of validity rather than one.
    out.validity = match (&s.validity, &n.validity) {
        (None, None) => None,
        (left, right) => {
            let mut copy = Bitmap::new_in(arena, len, true);
            if let Some(valid) = left {
                copy.and_with(valid);
            }
            if let Some(valid) = right {
                copy.and_with(valid);
            }
            Some(copy)
        }
    };
    Ok(out)
}

/// The longest a number takes here: twenty digits and a sign.
const DIGITS: usize = 21;

/// `n` written into the end of `buf`, answered as the part of `buf`
/// that holds it.
///
/// Backwards, because the last digit is the one arithmetic hands over
/// first. Writing it forwards means measuring the number before writing
/// it, which is the same chain of divides a second time, and a division
/// per digit is most of what an identifier costs.
fn written(n: i64, buf: &mut [u8; DIGITS]) -> &[u8] {
    let mut at = buf.len();
    let mut left = n.unsigned_abs();
    loop {
        at -= 1;
        buf[at] = b'0' + (left % 10) as u8;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    if n < 0 {
        at -= 1;
        buf[at] = b'-';
    }
    &buf[at..]
}

/// The identifiers of a chunk's nodes, which is `n:` and then the table
/// and the row each node sits at.
///
/// A level of a plan has one table for every row it will ever produce
/// and carries the row itself as a column, so the identifier is a head
/// written once for the whole chunk and a number written once a row.
/// That is the reason this is a kernel rather than a call: the argument
/// is a node and no chunk holds one, but the two numbers a node is made
/// of are both here already.
///
/// The row is an integer and not a string, so nothing about the answer
/// is read out of the argument the way a trim or a substring reads its
/// answer out of the string it was handed. Every identifier is written,
/// and nearly all of them are written into the view itself: a table
/// under ten and a row under nine digits is twelve bytes, which is what
/// fits inline, so a chunk of a normal graph fills no buffer at all.
///
/// A negative row cannot come out of storage and is written with its
/// sign rather than being refused, since a kernel that answers for
/// every input it can be handed is one fewer thing to get right.
pub fn element_ids(arena: &mut MorselArena, table: u32, rows: &ValueVector) -> Result<ValueVector> {
    if !matches!(rows.phys, PhysType::Int64) {
        return Err(ZuError::InvalidArgument(format!(
            "no ELEMENT_ID() kernel for {:?}",
            rows.phys
        )));
    }
    if matches!(rows.encoding, VecEncoding::Dict { .. }) {
        return Err(ZuError::InvalidArgument(
            "element identifiers over a dict vector: materialize first".into(),
        ));
    }
    let len = rows.len as usize;
    // `n:` and the table and a colon, which is at most thirteen bytes
    // and the same bytes for every row of the chunk.
    let mut digits = [0u8; DIGITS];
    let table = written(i64::from(table), &mut digits);
    let mut buf = [0u8; 13];
    buf[0] = b'n';
    buf[1] = b':';
    buf[2..2 + table.len()].copy_from_slice(table);
    buf[2 + table.len()] = b':';
    let head = &buf[..3 + table.len()];

    let mut out = ValueVector::flat_uninit(arena, PhysType::Str, len);
    let mut build = StrBuilder::new();
    let mut row_digits = [0u8; DIGITS];
    let one = |build: &mut StrBuilder, row: i64, room: &mut [u8; DIGITS]| {
        let tail = written(row, room);
        build.push_with(head.len() + tail.len(), |dst| {
            dst[..head.len()].copy_from_slice(head);
            dst[head.len()..].copy_from_slice(tail);
        })
    };
    match rows.encoding {
        VecEncoding::Constant => {
            let view = one(&mut build, rows.constant_value::<i64>(), &mut row_digits);
            out.values_mut::<StrView>()[..len].fill(view);
        }
        // Flat, the dictionary having been turned away above.
        _ => {
            let vals = rows.values::<i64>();
            let dst = out.values_mut::<StrView>();
            for (slot, &row) in dst[..len].iter_mut().zip(vals) {
                *slot = one(&mut build, row, &mut row_digits);
            }
        }
    }
    out.aux = Aux::Str(Arc::new(build.finish()));
    // A row broadcast in from an optional match that found nothing is
    // null, and the identifier of a node that is not there is null too.
    out.validity = carried(arena, rows, len);
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

    /// The composed letter and the letter with its accent behind it are
    /// the same string, and which of the two a form answers is the
    /// whole of what the two canonical forms disagree about.
    #[test]
    fn a_form_puts_an_accent_together_or_leaves_it_apart() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["h\u{e9}llo", "he\u{301}llo", "plain"]);
        let together = normalize(&mut arena, NormalForm::Nfc, &v).unwrap();
        assert_eq!(together.phys, PhysType::Str);
        assert_eq!(read(&together), ["h\u{e9}llo", "h\u{e9}llo", "plain"]);
        let apart = normalize(&mut arena, NormalForm::Nfd, &v).unwrap();
        assert_eq!(read(&apart), ["he\u{301}llo", "he\u{301}llo", "plain"]);
    }

    /// The compatibility forms answer something the canonical ones do
    /// not, and the answer is a different length from the argument,
    /// which is the case that rules out both the trim's trick and the
    /// fold's sizing.
    #[test]
    fn a_compatibility_form_answers_a_string_of_another_length() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["o\u{fb01}ce"]);
        let canonical = normalize(&mut arena, NormalForm::Nfc, &v).unwrap();
        assert_eq!(read(&canonical), ["o\u{fb01}ce"]);
        let compat = normalize(&mut arena, NormalForm::Nfkc, &v).unwrap();
        assert_eq!(read(&compat), ["ofice"]);
        assert!("o\u{fb01}ce".len() > "ofice".len());
    }

    /// The column nearly every query holds: plain strings, which are in
    /// all four forms already. The answers are the argument's own
    /// strings, so the vector holds the buffers it was handed and fills
    /// none of its own, and not a byte is copied.
    #[test]
    fn a_plain_column_is_answered_without_a_byte_being_copied() {
        let mut arena = MorselArena::new();
        let long = "a plain string well past the twelve bytes a view holds";
        let v = str_vector(&mut arena, &[long, "short"]);
        let out = normalize(&mut arena, NormalForm::Nfd, &v).unwrap();
        assert_eq!(read(&out), [long, "short"]);
        let (Aux::Str(theirs), Aux::Str(ours)) = (&v.aux, &out.aux) else {
            panic!("a chunk with a long string in it carries buffers");
        };
        assert!(Arc::ptr_eq(theirs, ours));
    }

    /// An answer longer than a view holds lands in the builder's buffer
    /// and reads back through it, which is what a decomposition of a
    /// string that was already nearly too long asks for.
    #[test]
    fn a_long_answer_lands_in_a_buffer_of_its_own() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["h\u{e9}llo th\u{e9}re"]);
        let out = normalize(&mut arena, NormalForm::Nfd, &v).unwrap();
        assert_eq!(read(&out), ["he\u{301}llo the\u{301}re"]);
        assert!(!out.values::<StrView>()[0].is_inline());
    }

    /// One string standing for the chunk is normalized once, and every
    /// row reads the one answer back.
    #[test]
    fn a_constant_is_normalized_once() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("h\u{e9}llo".as_bytes()),
            3,
        );
        v.aux = Aux::None;
        let out = normalize(&mut arena, NormalForm::Nfd, &v).unwrap();
        assert_eq!(read(&out), ["he\u{301}llo"; 3]);
    }

    /// Codes over a table normalize the table rather than the rows, and
    /// the answer still has to be what the same strings flat would have
    /// given.
    #[test]
    fn a_dictionary_is_normalized_a_table_at_a_time() {
        let mut arena = MorselArena::new();
        let entries = [
            "a\u{301} longer entry than a view holds",
            "bo",
            "h\u{e9}llo",
        ];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [2u16, 0, 1, 0];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let out = normalize(&mut arena, NormalForm::Nfc, &v).unwrap();
        let flat = str_vector(&mut arena, &codes.map(|c| entries[c as usize]));
        assert_eq!(
            read(&out),
            read(&normalize(&mut arena, NormalForm::Nfc, &flat).unwrap())
        );
        assert_eq!(read(&out)[0], "h\u{e9}llo");
        assert_eq!(read(&out)[1], "\u{e1} longer entry than a view holds");
    }

    /// A null argument answers null, and the rows around it answer what
    /// they would have answered on their own.
    #[test]
    fn a_normalize_of_a_null_is_null() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["he\u{301}llo", "bo", "cy"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let out = normalize(&mut arena, NormalForm::Nfc, &v).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
        assert!(out.is_valid(0) && out.is_valid(2));
        assert_eq!(read(&out)[0], "h\u{e9}llo");
        assert_eq!(read(&out)[2], "cy");
    }

    /// Reads a predicate back as the rows that passed.
    fn passed(bits: &Bitmap) -> Vec<usize> {
        (0..bits.len()).filter(|i| bits.get(*i)).collect()
    }

    /// The test answers the rows that are in the form already, and the
    /// negated test answers the rows that are not, plain strings being
    /// in every form and so in neither answer's way.
    #[test]
    fn the_test_answers_the_rows_that_are_in_the_form() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["h\u{e9}llo", "he\u{301}llo", "plain"]);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfc, false, &v, &mut bits).unwrap();
        assert_eq!(passed(&bits), [0, 2]);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfc, true, &v, &mut bits).unwrap();
        assert_eq!(passed(&bits), [1]);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfd, false, &v, &mut bits).unwrap();
        assert_eq!(passed(&bits), [1, 2]);
    }

    /// The other two encodings answer what the same strings flat would
    /// have answered, a constant deciding the chunk in one test and a
    /// table deciding it in one test an entry.
    #[test]
    fn the_test_reads_a_constant_and_a_table_the_same_way() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("he\u{301}llo".as_bytes()),
            3,
        );
        v.aux = Aux::None;
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfc, false, &v, &mut bits).unwrap();
        assert!(passed(&bits).is_empty());
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfd, false, &v, &mut bits).unwrap();
        assert_eq!(passed(&bits), [0, 1, 2]);

        // Sorted the way a table on disk is sorted, which puts the
        // decomposed spelling first: its second byte is the plain
        // letter and the composed one's is the front of an accent.
        let entries = ["he\u{301}llo", "h\u{e9}llo", "plain"];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [0u16, 1, 2, 0];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let mut bits = Bitmap::new_in(&mut arena, 4, false);
        normalized(NormalForm::Nfc, false, &v, &mut bits).unwrap();
        let want: Vec<usize> = codes
            .iter()
            .enumerate()
            .filter(|(_, c)| entries[**c as usize] != "he\u{301}llo")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(passed(&bits), want);
    }

    /// A null is not normalized and is not unnormalized either, so it
    /// is missing from both answers. This is the reason the negation is
    /// carried into the kernel: a complement of the plain answer would
    /// have called this row unnormalized.
    #[test]
    fn a_null_is_in_neither_answer() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["h\u{e9}llo", "he\u{301}llo", "cy"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfc, false, &v, &mut bits).unwrap();
        assert_eq!(passed(&bits), [0, 2]);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        normalized(NormalForm::Nfc, true, &v, &mut bits).unwrap();
        assert!(passed(&bits).is_empty(), "the null row is in neither");
    }

    /// Neither normalization has a row it cannot answer, and the two
    /// land in different places: a string for one, a truth value for
    /// the other.
    #[test]
    fn a_normalization_cannot_raise_and_the_two_answer_apart() {
        let into = StrNorm::Into(NormalForm::Nfkd);
        let test = StrNorm::Test(NormalForm::Nfkd);
        assert!(!into.may_raise() && !test.may_raise());
        assert_eq!(into.form(), NormalForm::Nfkd);
        assert_eq!(into.answer_type(PhysType::Str), Some(PhysType::Str));
        assert_eq!(test.answer_type(PhysType::Str), Some(PhysType::Bool));
        assert_eq!(into.answer_type(PhysType::Int64), None);
        assert_eq!(test.answer_type(PhysType::Float64), None);
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
        let Err(err) = normalize(&mut arena, NormalForm::Nfc, &v) else {
            panic!("a number was normalized as a string");
        };
        assert!(err.to_string().contains("normalize"));
        let mut bits = Bitmap::new_in(&mut arena, 2, false);
        let Err(err) = normalized(NormalForm::Nfc, false, &v, &mut bits) else {
            panic!("a number was tested as a string");
        };
        assert!(err.to_string().contains("is_normalized"));
    }

    /// A count that is the same for the whole chunk, which is what a
    /// statement writing the number out produces.
    fn same_count(arena: &mut MorselArena, n: i64, len: usize) -> ValueVector {
        ValueVector::constant(arena, PhysType::Int64, n, len)
    }

    /// A count a row, which is what a column in the second argument
    /// produces.
    fn counts(arena: &mut MorselArena, vals: &[i64]) -> ValueVector {
        ValueVector::flat_from(arena, PhysType::Int64, vals)
    }

    /// The two ends of the one walk, counted in characters rather than
    /// in bytes, so an accented letter and an emoji each count once and
    /// neither is cut in half.
    #[test]
    fn a_cut_counts_characters_and_not_bytes() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["h\u{e9}llo", "\u{1f642}ab", "plain"]);
        let two = same_count(&mut arena, 2, 3);
        let left = cut(&mut arena, StrCut::Left, &v, &two, None).unwrap();
        assert_eq!(left.phys, PhysType::Str);
        assert_eq!(read(&left), ["h\u{e9}", "\u{1f642}a", "pl"]);
        let right = cut(&mut arena, StrCut::Right, &v, &two, None).unwrap();
        assert_eq!(read(&right), ["lo", "ab", "in"]);
    }

    /// A cut asked for more characters than there are answers the whole
    /// string, and one asked for none answers the empty string, which
    /// are the two ends of the range and both of them are answers
    /// rather than conditions.
    #[test]
    fn the_ends_of_the_range_are_answers_and_not_conditions() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["abc", "", "h\u{e9}llo"]);
        let plenty = same_count(&mut arena, 40, 3);
        let out = cut(&mut arena, StrCut::Left, &v, &plenty, None).unwrap();
        assert_eq!(read(&out), ["abc", "", "h\u{e9}llo"]);
        let none = same_count(&mut arena, 0, 3);
        let out = cut(&mut arena, StrCut::Right, &v, &none, None).unwrap();
        assert_eq!(read(&out), ["", "", ""]);
    }

    /// The trim's trick again: a part of a string is bytes that are
    /// already in a buffer somebody is holding, so a flat column is cut
    /// without a byte being copied and the answer holds the argument's
    /// own buffers.
    #[test]
    fn a_plain_column_is_cut_without_a_byte_being_copied() {
        let mut arena = MorselArena::new();
        let long = "a plain string well past the twelve bytes a view holds";
        let v = str_vector(&mut arena, &[long, "short"]);
        let count = same_count(&mut arena, 30, 2);
        let out = cut(&mut arena, StrCut::Right, &v, &count, None).unwrap();
        assert_eq!(read(&out), [&long[long.len() - 30..], "short"]);
        let (Aux::Str(theirs), Aux::Str(ours)) = (&v.aux, &out.aux) else {
            panic!("a chunk with a long string in it carries buffers");
        };
        assert!(Arc::ptr_eq(theirs, ours));
    }

    /// One string standing for the chunk is cut once where the count is
    /// one number, and a row at a time where the count is a column,
    /// since two rows over one string no longer answer the same thing.
    #[test]
    fn a_constant_is_cut_once_or_a_row_at_a_time() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::constant(
            &mut arena,
            PhysType::Str,
            StrView::inline("h\u{e9}llo".as_bytes()),
            3,
        );
        v.aux = Aux::None;
        let two = same_count(&mut arena, 2, 3);
        let out = cut(&mut arena, StrCut::Left, &v, &two, None).unwrap();
        assert_eq!(read(&out), ["h\u{e9}", "h\u{e9}", "h\u{e9}"]);
        let varying = counts(&mut arena, &[1, 3, 5]);
        let out = cut(&mut arena, StrCut::Left, &v, &varying, None).unwrap();
        assert_eq!(read(&out), ["h", "h\u{e9}l", "h\u{e9}llo"]);
    }

    /// A dictionary is the one shape that has to write, an entry of a
    /// table not being bytes a view can name. One count cuts the table
    /// once and the rows gather over the answers; a count a row cuts a
    /// row at a time. Both have to answer what the same strings flat
    /// would have answered.
    #[test]
    fn a_dictionary_is_cut_a_table_at_a_time_or_a_row_at_a_time() {
        let mut arena = MorselArena::new();
        let entries = ["a longer entry than a view holds", "bo", "h\u{e9}llo"];
        let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
        let codes = [2u16, 0, 1, 0];
        let v = ValueVector::dict_str(&mut arena, &codes, dict);
        let flat = str_vector(&mut arena, &codes.map(|c| entries[c as usize]));
        let four = same_count(&mut arena, 4, 4);
        let out = cut(&mut arena, StrCut::Left, &v, &four, None).unwrap();
        assert_eq!(
            read(&out),
            read(&cut(&mut arena, StrCut::Left, &flat, &four, None).unwrap())
        );
        assert_eq!(read(&out)[0], "h\u{e9}ll");
        let varying = counts(&mut arena, &[1, 2, 2, 40]);
        let out = cut(&mut arena, StrCut::Right, &v, &varying, None).unwrap();
        assert_eq!(
            read(&out),
            read(&cut(&mut arena, StrCut::Right, &flat, &varying, None).unwrap())
        );
        assert_eq!(read(&out)[3], "a longer entry than a view holds");
    }

    /// Either argument being null makes the answer null, which is two
    /// columns' worth of validity rather than one.
    #[test]
    fn a_null_on_either_side_answers_null() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["abc", "def", "ghi"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(0);
        v.validity = Some(valid);
        let mut n = counts(&mut arena, &[2, 2, 2]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        n.validity = Some(valid);
        let out = cut(&mut arena, StrCut::Left, &v, &n, None).unwrap();
        assert!(!out.is_valid(0), "a null string answers null");
        assert!(!out.is_valid(1), "a null count answers null");
        assert!(out.is_valid(2));
        assert_eq!(read(&out)[2], "gh");
    }

    /// A string has no negative number of characters, and the condition
    /// the standard gives that is the row engine's, in the row engine's
    /// words, so a statement cannot tell which engine answered it.
    #[test]
    fn a_negative_count_raises_the_standards_condition() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["abc", "def"]);
        let n = counts(&mut arena, &[1, -2]);
        let Err(err) = cut(&mut arena, StrCut::Right, &v, &n, None) else {
            panic!("a negative count was answered");
        };
        assert_eq!(err.gqlstatus().map(|s| s.to_string()), Some("22011".into()));
        assert!(err.to_string().contains("right()"), "{err}");
        assert!(err.to_string().contains("-2"), "{err}");
    }

    /// The rows the condition is raised over are the rows the row
    /// engine evaluated: a row nobody selected is not one of them, and
    /// neither is a row whose string is null, since a null answers null
    /// before the count is looked at.
    #[test]
    fn a_row_that_was_never_evaluated_raises_nothing() {
        let mut arena = MorselArena::new();
        let mut v = str_vector(&mut arena, &["abc", "def", "ghi"]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let n = counts(&mut arena, &[3, -1, -1]);
        let mut bits = Bitmap::new_in(&mut arena, 3, false);
        bits.set(0);
        bits.set(1);
        let sel = SelVector::from_bitmap(&mut arena, &bits);
        let out = cut(&mut arena, StrCut::Left, &v, &n, Some(&sel)).unwrap();
        assert_eq!(read(&out)[0], "abc");
        let Err(err) = cut(&mut arena, StrCut::Left, &v, &n, None) else {
            panic!("the unselected row's count was never looked at");
        };
        assert!(err.to_string().contains("left()"), "{err}");
    }

    /// A cut says it can raise, which is what keeps it out of a
    /// projection unless the compiler settled the count. A number is
    /// not a string here either, and neither is a string a count.
    #[test]
    fn a_cut_can_raise_and_wants_a_string_and_a_number() {
        assert!(StrCut::Left.may_raise() && StrCut::Right.may_raise());
        assert_eq!(
            StrCut::Left.answer_type(PhysType::Str, PhysType::Int64),
            Some(PhysType::Str)
        );
        assert!(
            StrCut::Left
                .answer_type(PhysType::Str, PhysType::Float64)
                .is_none()
        );
        let mut arena = MorselArena::new();
        let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2]);
        let n = counts(&mut arena, &[1, 1]);
        let Err(err) = cut(&mut arena, StrCut::Left, &v, &n, None) else {
            panic!("a number was cut as a string");
        };
        assert!(err.to_string().contains("left()"), "{err}");
    }

    /// An identifier is the head the chunk shares and the row itself,
    /// and the rows a graph really holds are counted from nought, so
    /// the digits are the whole of what varies.
    #[test]
    fn an_identifier_is_the_table_and_the_row() {
        let mut arena = MorselArena::new();
        let rows = ValueVector::flat_from(&mut arena, PhysType::Int64, &[0i64, 7, 1234567]);
        let ids = element_ids(&mut arena, 3, &rows).unwrap();
        assert_eq!(ids.phys, PhysType::Str);
        assert_eq!(read(&ids), vec!["n:3:0", "n:3:7", "n:3:1234567"]);
    }

    /// The row engine writes the same two numbers with the same
    /// punctuation, and the two engines answering one query differently
    /// is the failure this kernel exists to not have.
    #[test]
    fn an_identifier_is_what_the_row_engine_writes() {
        let mut arena = MorselArena::new();
        for (table, row) in [(0u32, 0i64), (1, 2), (10, 99), (u32::MAX, 4_294_967_296)] {
            let rows = ValueVector::flat_from(&mut arena, PhysType::Int64, &[row]);
            let ids = element_ids(&mut arena, table, &rows).unwrap();
            assert_eq!(read(&ids), vec![format!("n:{table}:{row}")]);
        }
    }

    /// A row past the twelve bytes a view holds is written into the
    /// buffer behind it, and the answer reads back the same either way.
    #[test]
    fn an_identifier_too_long_to_sit_inline_goes_to_the_buffer() {
        let mut arena = MorselArena::new();
        let rows = ValueVector::flat_from(&mut arena, PhysType::Int64, &[5i64, 9_876_543_210_123]);
        let ids = element_ids(&mut arena, 65535, &rows).unwrap();
        let views = ids.values::<StrView>();
        assert!(views[0].is_inline(), "a short identifier stays in the view");
        assert!(!views[1].is_inline(), "a long one does not");
        assert_eq!(
            read(&ids),
            vec!["n:65535:5", "n:65535:9876543210123"],
            "and both read back the same"
        );
    }

    /// One row standing for the chunk is written once.
    #[test]
    fn a_constant_row_is_written_once() {
        let mut arena = MorselArena::new();
        let rows = ValueVector::constant(&mut arena, PhysType::Int64, 12i64, 4);
        let ids = element_ids(&mut arena, 0, &rows).unwrap();
        assert_eq!(read(&ids), vec!["n:0:12"; 4]);
    }

    /// A node that is not there has no identifier, which is the row an
    /// optional match left behind.
    #[test]
    fn a_null_row_has_no_identifier() {
        let mut arena = MorselArena::new();
        let mut rows = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 0, 3]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        rows.validity = Some(valid);
        let ids = element_ids(&mut arena, 2, &rows).unwrap();
        let valid = ids.validity.as_ref().expect("the answer carries validity");
        assert!(valid.get(0) && !valid.get(1) && valid.get(2));
    }

    /// The two numbers a node is made of are numbers, so a column of
    /// anything else is not a chunk of nodes and is refused rather than
    /// read as one.
    #[test]
    fn identifiers_want_a_column_of_rows() {
        let mut arena = MorselArena::new();
        let v = str_vector(&mut arena, &["ann"]);
        let Err(err) = element_ids(&mut arena, 0, &v) else {
            panic!("a column of strings was read as rows");
        };
        assert!(err.to_string().contains("ELEMENT_ID()"), "{err}");
    }
}
