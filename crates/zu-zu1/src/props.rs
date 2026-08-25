//! Node property columns. A node table's entry in the table index
//! points at a props directory chain: variable width columns stored as
//! FullZip blob segments, fixed width columns as cascade-encoded u64
//! segments, every column row-aligned with the table's row domain.
//!
//! A column carries a [`LogicalType`] from the lattice of gql/plan/02,
//! which is what tells a reader whether the word it pulls out of a
//! fixed width lane is a count, a truth value, a float, a day or a
//! nanosecond. The lane itself does not care: the cascade encodes 64
//! bit words, so a boolean, a float and a date all ride the encoding
//! the integer columns already had, and only the type at the top says
//! what a word means. This is the whole reason properties can widen
//! past strings and integers without a second storage path.
//!
//! Directory layout: `version: u16`, `node_count: u64`,
//! `column_count: u32`, then per column `name_len: u16` + UTF-8 bytes,
//! `type: u8` (a code from [`TYPE_CODES`], or the list code and then
//! the element's code), the column's `SegmentMeta`, and from version 4
//! a `nullable: u8` flag followed, when it is set, by the segment meta
//! of the column's validity words.
//!
//! A column is dense whether or not it holds a null: every row of the
//! table's domain has a value in the column's segment. What a null adds
//! is a second segment saying which of those values a reader may look
//! at, one bit per row packed into 64 bit words that ride the same
//! integer cascade the fixed width columns do. A column with no null in
//! it has no validity segment, so nothing written before a property
//! could be null costs anything to read now.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use zu_common::{
    Decimal, DurationKind, FloatBits, IdMap, IntBits, LogicalType, PhysicalType, Result, ZuError,
    int_key,
};

use crate::catalog::{Catalog, TableIndex};
use crate::file::{BlockPtr, Zu1File};
use crate::fullzip::write_blob_segment;
use crate::meta;
use crate::rows::{read_rows_range, write_rows};
use crate::segment::{
    CHUNK_ROWS, ChunkCache, ChunkDirectory, SegmentMeta, cached_chunk, chunk_zone, decode_chunk,
    load_chunk_directory_pooled, read_one_cached, read_range, write_segment,
};
use crate::stats;
use crate::txn::Cell;

/// Version 1 held a one byte type that could say string or integer and
/// nothing else. Version 2 holds a code from [`TYPE_CODES`], which is
/// the storable part of the logical lattice. Version 3 adds the one
/// type that needs a second byte, a list, whose code is followed by the
/// code of its element type. Version 4 adds the validity flag, which is
/// the first thing a column entry carries that is not about what the
/// column holds but about which rows of it hold anything. Older
/// directories are still read, because a file written before this is
/// not wrong, it is just narrow. Version 5 adds the label bitset, the
/// second thing after validity that is about the rows rather than about
/// what a column holds. Version 6 packs segment payloads beside each
/// other. Version 7 writes a list element at its own width instead of in
/// eight bytes, which is the first version change that is about the
/// inside of a row rather than about the directory: nothing here moves,
/// and `list_elements` reads a version 6 row and a version 7 one alike.
/// Version 8 lets a column entry carry a declared type in the extended
/// form, so a column may hold a type no single code names: `BINARY(16)`
/// and `STRING(1,5)` were declarable and unstorable before it. Version 9
/// adds the element count a list column's rows are written at, which is
/// what lets the rows stop carrying one each. Version 10 gives a column
/// entry a second segment, for the zone plane of a zoned column, which
/// is the first column here that is not one segment of values. Version
/// 11 adds the exact decimal to the extended form, which is the first
/// column type whose declaration is needed to read a row back: the lane
/// holds unscaled units and the scale that says how large a unit is
/// lives in the type. Version 12 lets a list column's rows say their
/// count in fewer than four bytes, and writes the width they said it in
/// into the column entry, which is the second field there that is about
/// the encoding rather than about the declaration.
const PROPS_VERSION: u16 = 12;
const MAX_NAME_LEN: usize = 256;

/// The code a list column is written under, followed on disk by the
/// code of its element type. It sits outside [`TYPE_CODES`] because it
/// is the one type whose code does not stand on its own.
const LIST_CODE: u8 = 18;

/// The property types a column can be declared as, and the byte that
/// stands for each one on disk.
///
/// This is deliberately a table of whole types rather than a codec over
/// [`LogicalType`]. A property column is one of these or it is not
/// storable, and a reader that meets a code it does not know should say
/// so rather than reconstruct a type out of flags. Codes 0 and 1 keep
/// the meaning they had in version 1 so the two versions read the same.
static TYPE_CODES: [(u8, LogicalType); 18] = [
    (
        0,
        LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        },
    ),
    (
        1,
        LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        },
    ),
    (2, LogicalType::Bool),
    (
        3,
        LogicalType::Float {
            bits: FloatBits::B64,
            precision: None,
        },
    ),
    (4, LogicalType::Date),
    (5, LogicalType::LocalTime),
    (6, LogicalType::LocalDatetime),
    (7, LogicalType::Duration(DurationKind::DayTime)),
    (8, LogicalType::Duration(DurationKind::YearMonth)),
    (
        9,
        LogicalType::Float {
            bits: FloatBits::B32,
            precision: None,
        },
    ),
    (
        10,
        LogicalType::Int {
            signed: true,
            bits: IntBits::B8,
            precision: None,
        },
    ),
    (
        11,
        LogicalType::Int {
            signed: true,
            bits: IntBits::B16,
            precision: None,
        },
    ),
    (
        12,
        LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        },
    ),
    (
        13,
        LogicalType::Int {
            signed: false,
            bits: IntBits::B8,
            precision: None,
        },
    ),
    (
        14,
        LogicalType::Int {
            signed: false,
            bits: IntBits::B16,
            precision: None,
        },
    ),
    (
        15,
        LogicalType::Int {
            signed: false,
            bits: IntBits::B32,
            precision: None,
        },
    ),
    (
        16,
        LogicalType::Int {
            signed: false,
            bits: IntBits::B64,
            precision: None,
        },
    ),
    (
        17,
        LogicalType::Bytes {
            min: None,
            max: None,
            fixed: false,
        },
    ),
];

/// The code a column of this type is written under, `None` when the
/// type is not storable as a property column yet.
fn type_code(ty: &LogicalType) -> Option<u8> {
    TYPE_CODES.iter().find(|(_, t)| t == ty).map(|(c, _)| *c)
}

/// The bytes a column's type is written as: one code, and for a list
/// the element's code behind it. `None` when the type is not storable.
///
/// A list of lists is not storable. The row format below is one count
/// and then fixed width words or length prefixed bytes, which a nested
/// list has no room in, and a directory entry holds one element code.
/// Refusing here is what keeps a file from being written that no
/// reader can take apart again.
fn type_bytes(ty: &LogicalType) -> Option<Vec<u8>> {
    match ty {
        LogicalType::List { elem, max: None } => {
            Some(vec![LIST_CODE, type_code(list_elem(elem)?)?])
        }
        other => Some(vec![type_code(other)?]),
    }
}

/// The bytes a column entry writes for its type, which is a code where
/// a code says it and the extended form where nothing does.
///
/// A code stands for a whole type and there are eighteen of them, so
/// `BINARY(16)` and `STRING(1,5)` had nowhere to go: the catalog could
/// name them, because a name is only a promise about values, and a
/// column could not hold one. The extended form already existed for the
/// catalog and this is the same bytes in the same order, so a type a
/// graph type names and a type a column holds stay one encoding.
///
/// A length bound is the declaration a column takes this way, on a
/// string, a byte string or a list, and a zoned temporal takes it with
/// nothing beside it, since the type is the whole of what it says. A
/// decimal takes its precision and its scale, and `extended_bytes` is
/// where the precisions a lane word holds are told from the ones it does
/// not, because that answer has to be the same for the catalog.
///
/// What is left unstorable is a list of anything but a fixed width
/// element, whose row has to carry lengths of its own; saying so here is
/// what keeps the writer from making a file the reader below would
/// refuse.
fn column_type_bytes(ty: &LogicalType) -> Option<Vec<u8>> {
    match ty {
        _ if bounded_list(ty).is_some() => extended_bytes(&column_type(ty.clone())),
        _ if type_bytes(ty).is_some() => type_bytes(ty),
        LogicalType::Str { .. }
        | LogicalType::Bytes { .. }
        | LogicalType::ZonedTime
        | LogicalType::ZonedDatetime
        | LogicalType::Decimal { .. } => extended_bytes(ty),
        _ => None,
    }
}

/// Whether this is one of the two zoned temporal types, which are the
/// column shape that is two planes rather than one.
///
/// A zoned value is an instant and the offset from UTC it was written
/// with, which is two numbers, and everything else here is one. The two
/// are stored as two segments over the same rows: the instants in the
/// scalar lane, where they get the integer cascade and where a
/// comparison, a sort and a range estimate all read them, and the
/// offsets in a plane of their own that only materialising a value
/// touches. A column of one zone, which is what a column of timestamps
/// almost always is, pays a handful of bytes for the whole of the
/// second plane.
pub fn zoned(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::ZonedTime | LogicalType::ZonedDatetime)
}

/// Whether a column may be declared this type at all.
///
/// A graph type may name a type no column holds, because naming one is
/// a promise about values and holding one is a layout. So the caller
/// that turns a declaration into a table asks this first, and refuses
/// the statement naming the property, rather than letting the store
/// refuse a column halfway through making the table.
pub fn storable(ty: &LogicalType) -> bool {
    column_type_bytes(&column_type(ty.clone())).is_some()
}

/// The bound and the element width of a list column whose declaration
/// fixes both, or `None` for a column whose rows have to say their own
/// length.
///
/// A bound alone is not enough, because `LIST<T>(n)` bounds the maximum
/// and a shorter list is a legal value of it. What the bound buys is
/// that a row which is at the bound needs no count of its own, and that
/// is only a saving where the elements are all the same width, so a
/// list of strings is left out of it.
pub(crate) fn bounded_list(ty: &LogicalType) -> Option<(u32, usize)> {
    match ty {
        LogicalType::List {
            elem,
            max: Some(max),
        } if *max > 0 => {
            let inner = list_elem(elem)?;
            // A decimal rides the lane, so this would otherwise say
            // yes, and the element reader has no decimal arm: a row
            // would go in as digits and come back as an integer of
            // them. Refusing here is what keeps the writer from making
            // a file the reader misreads rather than refuses.
            if matches!(inner, LogicalType::Decimal { .. }) {
                return None;
            }
            let (width, _) = lane_width(inner)?;
            Some((*max, width))
        }
        _ => None,
    }
}

/// How wide the count at the head of a row of this list column is, for
/// a column being written now.
///
/// A count is a number between nought and the declared bound, and the
/// bound is in the catalog, so a `LIST<INT8>(4)` column has no use for
/// four bytes to say a number that cannot exceed four. One byte carries
/// a bound to 255 and two to 65535, and that is the whole of the saving.
///
/// It is worth having where the elements are small and the bound is
/// small with them, which is where the count was the larger part of the
/// row: a `LIST<INT8>(4)` row of four elements is four bytes of payload,
/// and four more to say so doubled it. On an embedding column the same
/// change is two bytes in three thousand and is beneath noticing, which
/// is fine, because an embedding column written whole at its bound
/// carries no count at all.
///
/// A list with no declared bound has no number to be smaller than, so it
/// keeps the four bytes it has always had.
pub(crate) fn count_width(ty: &LogicalType) -> usize {
    match bounded_list(ty) {
        Some((bound, _)) if bound <= u32::from(u8::MAX) => 1,
        Some((bound, _)) if bound <= u32::from(u16::MAX) => 2,
        _ => 4,
    }
}

/// How a row of a list column says how many elements it holds.
///
/// The two are not tellable apart from the bytes, which is why this is
/// carried rather than worked out: a bounded `LIST<INT32>(768)` row of
/// 767 counted elements is the same length as one of 768 uncounted
/// ones. The column entry says, and a reader takes the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRows {
    /// The directory says the count and no row carries one, which is a
    /// column every row of which was written at its declared bound.
    Fixed(usize),
    /// Every row says its own count, in this many bytes.
    Counted(usize),
}

/// The type a column entry holds, which for a list is the declaration
/// with the element's nullability taken off.
///
/// A stored list holds a value in every position, which is the rule
/// `list_elem` is for the code form; this is the same rule for the
/// extended one, so that a bounded list column and an unbounded one
/// agree about what their element type is and one reader serves both.
fn column_type(ty: LogicalType) -> LogicalType {
    match ty {
        LogicalType::List { elem, max } => match list_elem(&elem) {
            Some(inner) => LogicalType::List {
                elem: Box::new(inner.clone()),
                max,
            },
            None => LogicalType::List { elem, max },
        },
        other => other,
    }
}

/// The element type a list column stores, which is the declared element
/// type with its nullability wrapper taken off.
///
/// A stored list holds a value in every position. `LIST<INT>` and
/// `LIST<INT NOT NULL>` therefore store the same way, and the column
/// comes back as the second of the two, because that is the one a read
/// can promise.
fn list_elem(elem: &LogicalType) -> Option<&LogicalType> {
    match elem {
        LogicalType::Nullable(inner) => list_elem(inner),
        other => Some(other),
    }
}

/// The type a list column of this element type is declared as.
fn list_of(elem: LogicalType) -> LogicalType {
    LogicalType::List {
        elem: Box::new(elem),
        max: None,
    }
}

fn code_type(code: u8) -> Option<LogicalType> {
    TYPE_CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, t)| t.clone())
}

/// The code an extended declared type is written under, followed by
/// the kind byte and whatever that kind carries.
///
/// These are types a graph type may name and no column stores. Naming
/// a type and holding a value of one are different promises, and the
/// catalog only makes the first: `STRING(1,5)` is a declaration a
/// reader can hand back word for word, where a column of it would be a
/// layout this file has no lane for.
const EXTENDED_CODE: u8 = 19;

const EXT_ZONED_TIME: u8 = 0;
const EXT_ZONED_DATETIME: u8 = 1;
const EXT_STR: u8 = 2;
const EXT_BYTES: u8 = 3;
/// A list as it was declared rather than as a column holds one: the
/// element's own nullability, the maximum length, and the element type
/// written out in full so that a list of lists has somewhere to go.
const EXT_LIST: u8 = 4;
/// An exact decimal, as its precision and its scale. Both are needed to
/// read a row: the lane holds unscaled units and the scale is what says
/// a unit is a hundredth.
const EXT_DECIMAL: u8 = 5;

/// The count that stands for a bound nobody wrote, since a length is a
/// `u32` and every one of them is a length somebody could write.
const NO_BOUND: u32 = u32::MAX;

/// The bytes a declared type is written as, for a container that names
/// a type without storing values of it. `None` is a type nothing can be
/// declared with, which the caller refuses before writing anything.
pub(crate) fn declared_type_bytes(ty: &LogicalType) -> Option<Vec<u8>> {
    // A list takes the extended form even where a column code would
    // have held it. The column form drops the element's nullability on
    // purpose, because a stored list holds a value in every position,
    // and dropping it here would mean the catalog handing back
    // `LIST<FLOAT32 NOT NULL>` to somebody who wrote `LIST<FLOAT32>`.
    // Those are two types, and the catalog holds what was declared.
    if matches!(ty, LogicalType::List { .. }) {
        return extended_bytes(ty);
    }
    if let Some(bytes) = type_bytes(ty) {
        return Some(bytes);
    }
    extended_bytes(ty)
}

/// The extended form, for the declarable types no column code covers.
fn extended_bytes(ty: &LogicalType) -> Option<Vec<u8>> {
    let bounded = |kind: u8, min: &Option<u32>, max: &Option<u32>, fixed: bool| {
        let mut out = vec![EXTENDED_CODE, kind, u8::from(fixed)];
        out.extend(min.unwrap_or(NO_BOUND).to_le_bytes());
        out.extend(max.unwrap_or(NO_BOUND).to_le_bytes());
        out
    };
    Some(match ty {
        LogicalType::ZonedTime => vec![EXTENDED_CODE, EXT_ZONED_TIME],
        LogicalType::ZonedDatetime => vec![EXTENDED_CODE, EXT_ZONED_DATETIME],
        // A decimal is written where its unscaled units fit the lane,
        // and refused where they do not. The bound belongs here rather
        // than one caller up because the declared form and the column
        // form are one encoding for this type: a precision no column can
        // hold is a precision no element type can name, and the refusal
        // then lands at the declaration, where the user wrote it, rather
        // than at the first insert. Wider precisions want a plane of
        // their own the way a zoned column has one, and that is S2's.
        LogicalType::Decimal { precision, scale } => {
            lane_width(ty)?;
            let mut out = vec![EXTENDED_CODE, EXT_DECIMAL];
            out.extend(precision.to_le_bytes());
            out.extend(scale.to_le_bytes());
            out
        }
        LogicalType::Str { min, max, fixed } => bounded(EXT_STR, min, max, *fixed),
        LogicalType::Bytes { min, max, fixed } => bounded(EXT_BYTES, min, max, *fixed),
        // The element is written by the same function, so a list of
        // lists is a list whose element happens to be one. That is the
        // whole of what nesting costs here, and it is why the maximum
        // and the flag come first: they are this list's, and everything
        // behind them belongs to the element.
        LogicalType::List { elem, max } => {
            let not_null = !matches!(**elem, LogicalType::Nullable(_));
            let inner = list_elem(elem)?;
            let mut out = vec![EXTENDED_CODE, EXT_LIST, u8::from(not_null)];
            out.extend(max.unwrap_or(NO_BOUND).to_le_bytes());
            out.extend(declared_type_bytes(inner)?);
            out
        }
        _ => return None,
    })
}

/// Reads a type written by [`declared_type_bytes`], leaving `pos` after
/// it. The codes are the column codes, so a type the catalog names and
/// a type a column stores are the same byte, and the extended form is
/// the one that carries what a code alone cannot.
pub(crate) fn decode_declared_type(bytes: &[u8], pos: &mut usize) -> Result<LogicalType> {
    let ty = match bytes.get(*pos) {
        Some(&LIST_CODE) => {
            *pos += 1;
            let elem = bytes
                .get(*pos)
                .ok_or_else(|| corrupt("truncated list element type".into()))?;
            let elem =
                code_type(*elem).ok_or_else(|| corrupt(format!("unknown element type {elem}")))?;
            list_of(elem)
        }
        Some(&EXTENDED_CODE) => {
            *pos += 1;
            return decode_extended_type(bytes, pos);
        }
        Some(&code) => code_type(code).ok_or_else(|| corrupt(format!("unknown type {code}")))?,
        None => return Err(corrupt("truncated type".into())),
    };
    *pos += 1;
    Ok(ty)
}

/// The extended form, read back. `pos` is at the kind byte.
fn decode_extended_type(bytes: &[u8], pos: &mut usize) -> Result<LogicalType> {
    let kind = *bytes
        .get(*pos)
        .ok_or_else(|| corrupt("truncated extended type".into()))?;
    *pos += 1;
    if kind == EXT_ZONED_TIME {
        return Ok(LogicalType::ZonedTime);
    }
    if kind == EXT_ZONED_DATETIME {
        return Ok(LogicalType::ZonedDatetime);
    }
    if kind == EXT_DECIMAL {
        let mut digits = || -> Result<u16> {
            let end = *pos + 2;
            let word: [u8; 2] = bytes
                .get(*pos..end)
                .and_then(|slice| slice.try_into().ok())
                .ok_or_else(|| corrupt("truncated decimal digits".into()))?;
            *pos = end;
            Ok(u16::from_le_bytes(word))
        };
        let precision = digits()?;
        let scale = digits()?;
        return Ok(LogicalType::Decimal { precision, scale });
    }
    fn count(bytes: &[u8], pos: &mut usize) -> Result<Option<u32>> {
        let end = *pos + 4;
        let word: [u8; 4] = bytes
            .get(*pos..end)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| corrupt("truncated length bound".into()))?;
        *pos = end;
        Ok(match u32::from_le_bytes(word) {
            NO_BOUND => None,
            bound => Some(bound),
        })
    }
    let flag = |bytes: &[u8], pos: &mut usize| -> Result<bool> {
        let set = match bytes.get(*pos) {
            Some(&byte) => byte != 0,
            None => return Err(corrupt("truncated extended type".into())),
        };
        *pos += 1;
        Ok(set)
    };
    if kind == EXT_LIST {
        let not_null = flag(bytes, pos)?;
        let max = count(bytes, pos)?;
        let elem = decode_declared_type(bytes, pos)?;
        let elem = match not_null {
            true => elem,
            false => LogicalType::Nullable(Box::new(elem)),
        };
        return Ok(LogicalType::List {
            elem: Box::new(elem),
            max,
        });
    }
    let fixed = flag(bytes, pos)?;
    let (min, max) = (count(bytes, pos)?, count(bytes, pos)?);
    match kind {
        EXT_STR => Ok(LogicalType::Str { min, max, fixed }),
        EXT_BYTES => Ok(LogicalType::Bytes { min, max, fixed }),
        other => Err(corrupt(format!("unknown extended type {other}"))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropColumn {
    pub name: String,
    pub ty: LogicalType,
    pub meta: SegmentMeta,
    /// The column's validity words, one bit per row and the low bit of
    /// a word first, set where the row holds a value. `None` is a
    /// column every row of which holds one, which is what every column
    /// was before a property could be null.
    pub validity: Option<SegmentMeta>,
    /// The element count every row of this list column holds, where the
    /// rows do not carry one themselves. `None` is every other column,
    /// and a list column whose rows say their own count.
    ///
    /// This is the encoding half of schema/06 §2 written down: the type
    /// above is the declaration, `LIST<FLOAT32>(768)`, and this says
    /// that this column was written whole at exactly the bound, so the
    /// block may drop what the declaration already gives. It cannot be
    /// worked out from the bytes, which is why it is a field: a bounded
    /// `LIST<INT32>(768)` column of 767 counted elements a row is the
    /// same 3072 bytes a row as one of 768 uncounted ones.
    pub fixed_len: Option<u32>,
    /// How many bytes each row of this list column spends saying its own
    /// element count, where it says one at all. Four is what every
    /// version before 12 wrote and what an unbounded list still writes.
    ///
    /// It is carried rather than worked out from the declared bound
    /// because a column is read at the width its rows were written at,
    /// and those are two different questions once the rule has changed
    /// once. A version 11 column of `LIST<INT8>(4)` rows holds four byte
    /// counts, and a fold that leaves those rows alone has to leave the
    /// four with them, whatever a column written today would have used.
    pub count_width: u8,
    /// The zone plane of a zoned column: one word a row, the offset
    /// from UTC in minutes that the row's value was written with.
    /// `None` is every other column, which has no second plane.
    ///
    /// It is a segment beside `meta` rather than a second column
    /// because it is not a property: no query names it, nothing
    /// declares it, and it means nothing without the instant it sits
    /// beside. Physically it is a lane segment like any other, which is
    /// why a column of one zone costs almost nothing for it.
    pub zones: Option<SegmentMeta>,
}

impl PropColumn {
    /// Whether this column stores in the fixed width lane, which is one
    /// 64 bit word per row through the integer cascade, as against the
    /// blob segments a string or a byte string uses.
    ///
    /// Every fixed stride type of eight bytes or less rides the lane:
    /// the cascade encodes words and does not care what the bits mean,
    /// and the column's type is what says whether a word is a count, a
    /// truth value, a float, a day or a nanosecond.
    pub fn is_lane(&self) -> bool {
        lane_type(&self.ty)
    }

    /// How to read a row of this list column, which is what
    /// [`list_elements`] wants beside the bytes: the count the directory
    /// holds, or the width the row says its own in.
    pub fn list_rows(&self) -> ListRows {
        match self.fixed_len {
            Some(count) => ListRows::Fixed(count as usize),
            None => ListRows::Counted(self.count_width as usize),
        }
    }

    /// The bytes every row of this column occupies, where its type
    /// fixes that, and `None` where a row may be any length.
    ///
    /// Two declarations reach this: a fixed octet count, and a list
    /// written at its bound, whose row is the bound times the element
    /// width and nothing else. Both mean the same thing downstream,
    /// which is that a row of another length would cost the column its
    /// layout and leave its type describing bytes it does not hold.
    pub(crate) fn row_width(&self) -> Option<usize> {
        if let Some(octets) = fixed_octets(&self.ty) {
            return Some(octets as usize);
        }
        let (_, width) = bounded_list(&self.ty)?;
        Some(self.fixed_len? as usize * width)
    }
}

/// Whether values of this type ride the fixed width lane.
///
/// A zoned column rides it and is not in [`lane_width`], and the two
/// answers are not in conflict. The lane holds a zoned column's
/// instants, which is what a comparison, a sort and a range read, and
/// the offsets ride a plane beside it. A list element gets no plane of
/// its own, so eight bytes there would be an instant and no zone, which
/// is not the value; that is `lane_width`'s question and it answers no.
fn lane_type(ty: &LogicalType) -> bool {
    lane_width(ty).is_some() || zoned(ty)
}

/// How wide one value of this type is where it is written out at its
/// natural size, and whether the lane word it came from carries a sign.
///
/// `None` is a type that does not ride the lane at all, so this is the
/// one place the lane set is written down and `lane_type` asks it.
///
/// The scalar lane holds every one of these in a 64 bit word, because
/// the integer cascade encodes words and gets its own narrowing from
/// the values it meets. A list row has no cascade under it: it is a run
/// of bytes inside a blob, so a width that is not the type's is a width
/// paid on every element of every row. Hence the second half of the
/// answer: an `INT32` lane word is sign extended to 64 bits, and a
/// reader that cuts it to four bytes has to put the sign back.
fn lane_width(ty: &LogicalType) -> Option<(usize, bool)> {
    Some(match ty.physical()? {
        // A truth value is one byte here rather than one bit. A bit
        // would want a mask over the whole row, and a list row is
        // read one element at a time by a caller that asks for the
        // element and not for the run.
        PhysicalType::Bool | PhysicalType::U8 => (1, false),
        PhysicalType::I8 => (1, true),
        PhysicalType::U16 => (2, false),
        PhysicalType::I16 => (2, true),
        // A float is bits and not a number, so nothing is extended
        // into the half above it and it is read back unsigned.
        PhysicalType::U32 | PhysicalType::F32 => (4, false),
        // Days before the epoch and months before a zero duration are
        // both negative, so both of these are signed.
        PhysicalType::I32 | PhysicalType::Days32 | PhysicalType::Months32 => (4, true),
        PhysicalType::I64 | PhysicalType::U64 | PhysicalType::F64 | PhysicalType::Nanos64 => {
            (8, false)
        }
        _ => return None,
    })
}

/// Whether a lane word survives being cut to `width` bytes.
///
/// A word that does not is a word the column's own type says it cannot
/// hold, so the cut would not be a narrowing but a loss, and the write
/// is refused instead. Eight bytes is the whole word and always fits.
fn fits_width(word: u64, width: usize, signed: bool) -> bool {
    if width >= 8 {
        return true;
    }
    let spare = 64 - width * 8;
    match signed {
        true => (((word as i64) << spare) >> spare) as u64 == word,
        false => word >> (width * 8) == 0,
    }
}

/// The property columns of one node table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropsDirectory {
    pub node_count: u64,
    pub columns: Vec<PropColumn>,
    /// The label bitset, one word per row, bit `i` set where the row
    /// carries dictionary label `i`. `None` is a table whose rows carry
    /// its name and nothing else, which is every table until something
    /// declares a second label, and reading one costs nothing because
    /// there is nothing to read.
    ///
    /// It rides here rather than in `columns` because it is not a
    /// property: no query names it, no schema declares it, and its
    /// meaning is fixed by the catalog's dictionary rather than by a
    /// column type. Physically it is a column like any other, one lane
    /// segment over the table's row domain.
    pub labels: Option<SegmentMeta>,
}

/// An edge list and one owned column per edge property, which is the
/// pair every reader outside the file produces: the edges in the order
/// the source had them and one value per edge in the same order.
pub type EdgesWithProps = (Vec<(u32, u32)>, Vec<OwnedColumn>);

/// The same pair for a rel file that names its endpoints by the ids a
/// node file gave its rows rather than by row offsets, which is what a
/// dataset of many node tables is written with. Which row of which table
/// each id is takes every node file to answer, so the translation is the
/// loader's and not the reader's.
pub type KeyedEdgesWithProps = (Vec<(u64, u64)>, Vec<OwnedColumn>);

/// A node file read whole: the id of every row in row order, and one
/// owned column per property the header named.
pub type NodesWithProps = (Vec<u64>, Vec<OwnedColumn>);

/// One column held whole, in the shape a reader outside the file
/// produces it: owned values in the row order the source had them.
///
/// `PropValues` borrows, which is right at the store call and wrong for
/// a column that has to survive being read, reordered and only then
/// stored. That is the parquet loader's shape exactly, so the owned
/// form carries the four types an external edge list brings and
/// `store_rel_props_owned` does the borrowing at the one point it is
/// needed.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedColumn {
    pub name: String,
    pub values: OwnedValues,
}

/// The values of an [`OwnedColumn`].
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedValues {
    Int(Vec<u64>),
    Float(Vec<f64>),
    Bool(Vec<bool>),
    Str(Vec<Vec<u8>>),
}

impl OwnedValues {
    /// Rows held.
    pub fn len(&self) -> usize {
        match self {
            OwnedValues::Int(v) => v.len(),
            OwnedValues::Float(v) => v.len(),
            OwnedValues::Bool(v) => v.len(),
            OwnedValues::Str(v) => v.len(),
        }
    }

    /// Whether the column holds no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The same values in the order `order` names, where `order[i]` is
    /// the position the value now at row `i` came from.
    ///
    /// This is how a property column follows its edges. A bulk load
    /// sorts the edge list, and sorting is what numbers the ordinals a
    /// column is addressed by, so a column read alongside an unsorted
    /// edge list belongs to the wrong rows until it is moved the same
    /// way the edges were.
    #[must_use]
    pub fn permuted(&self, order: &[u32]) -> Self {
        let at = |i: &u32| *i as usize;
        match self {
            OwnedValues::Int(v) => OwnedValues::Int(order.iter().map(|i| v[at(i)]).collect()),
            OwnedValues::Float(v) => OwnedValues::Float(order.iter().map(|i| v[at(i)]).collect()),
            OwnedValues::Bool(v) => OwnedValues::Bool(order.iter().map(|i| v[at(i)]).collect()),
            OwnedValues::Str(v) => {
                OwnedValues::Str(order.iter().map(|i| v[at(i)].clone()).collect())
            }
        }
    }
}

/// Row-ordered values for one column at store time.
///
/// The fixed width arms all end up as 64 bit words in the same lane;
/// they are kept apart here so a caller states what it is storing once,
/// at the point where it still knows, rather than handing over a bag of
/// words and a type that has to agree with them.
#[derive(Debug, Clone, Copy)]
pub enum PropValues<'a> {
    Str(&'a [&'a [u8]]),
    Bytes(&'a [&'a [u8]]),
    Int(&'a [u64]),
    Bool(&'a [bool]),
    Float(&'a [f64]),
    /// Days since the epoch.
    Date(&'a [i32]),
    /// Nanoseconds since midnight.
    LocalTime(&'a [i64]),
    /// Nanoseconds since the epoch.
    LocalDatetime(&'a [i64]),
    /// Nanoseconds since midnight in the offset's own day, and the
    /// offset from UTC in minutes, one of each per row.
    ZonedTime {
        nanos: &'a [i64],
        zones: &'a [i16],
    },
    /// Nanoseconds since the epoch in UTC, and the offset from UTC in
    /// minutes the value was written with, one of each per row.
    ///
    /// The instant is UTC and the offset is beside it rather than
    /// folded into it, which is what lets two rows written in two zones
    /// for one instant be one number in the lane and still each print
    /// where they were written.
    ZonedDatetime {
        nanos: &'a [i64],
        zones: &'a [i16],
    },
    /// Months for a year-month duration, nanoseconds for a day-time one.
    Duration(DurationKind, &'a [i64]),
    /// One list per row, every list holding elements of `elem`.
    ///
    /// The elements arrive already reduced to what the row format
    /// keeps, a word or a run of bytes, so this one arm carries every
    /// element type rather than growing a variant per type the way the
    /// scalar arms do. Which of the two an element must be is settled
    /// by `elem`, and an element that is the other one is refused at
    /// store time.
    List {
        elem: &'a LogicalType,
        rows: &'a [&'a [ListElement<'a>]],
    },
}

/// One element of a stored list, in the shape the row format keeps it.
///
/// A lane type is a word, the same 64 bit word the scalar lane holds
/// for that type, so a list of dates and a date column agree about what
/// a day is. Everything else is a run of bytes with its length in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListElement<'a> {
    Word(u64),
    Blob(&'a [u8]),
}

/// Encodes one row of a list column: the count in as many bytes as
/// `rows` says, where the row says its own count at all, then the
/// elements, each a little endian run of `lane_width` bytes for a lane
/// element type or a `len: u32` and its bytes otherwise.
///
/// Version 6 and older wrote every lane element in eight bytes whatever
/// its type. A list of 768 `FLOAT32` is the type this series is for and
/// that cost it 6148 bytes a row against the 3076 the floats need, so
/// version 7 writes each element at its own width. Version 9 takes the
/// count out of the rows of a column whose declaration already gives it,
/// which is the last of the 3076 that is not a float. Version 12 narrows
/// the count that is left, for the bounded column whose rows are not all
/// at the bound and so still have to carry one. The reader below still
/// reads every form, and schema/06 §2 is the rule all four are an
/// instance of: the catalog holds the declaration, the bytes hold the
/// narrowest thing that carries it.
fn encode_list_row(
    elem: &LogicalType,
    items: &[ListElement<'_>],
    rows: ListRows,
) -> Result<Vec<u8>> {
    let lane = lane_width(elem);
    let stride = lane.map_or(4, |(width, _)| width);
    let head = match rows {
        ListRows::Fixed(_) => 0,
        ListRows::Counted(width) => width,
    };
    let mut out = Vec::with_capacity(head + items.len() * stride);
    if let ListRows::Counted(width) = rows {
        // A count wider than its head is a row that would read back as
        // another row, so it is refused here rather than truncated. The
        // writer picked the width from the declared bound and checked
        // every row against that bound, so nothing reaches this.
        if items.len() as u64 >= 1u64 << (width * 8) {
            return Err(ZuError::InvalidArgument(format!(
                "a list of {} does not fit a {width} byte count",
                items.len()
            )));
        }
        out.extend_from_slice(&(items.len() as u64).to_le_bytes()[..width]);
    }
    for item in items {
        match (item, lane) {
            (ListElement::Word(w), Some((width, signed))) => {
                if !fits_width(*w, width, signed) {
                    return Err(ZuError::InvalidArgument(format!(
                        "a list of {elem} does not hold {}",
                        *w as i64
                    )));
                }
                out.extend_from_slice(&w.to_le_bytes()[..width]);
            }
            (ListElement::Blob(b), None) => {
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
            _ => {
                return Err(ZuError::InvalidArgument(format!(
                    "a list of {elem} does not hold {item:?}"
                )));
            }
        }
    }
    Ok(out)
}

/// Reads back a row written by `encode_list_row`.
///
/// `rows` is [`PropColumn::list_rows`], which says whether the count is
/// in the directory or at the head of the row and how wide the head is.
/// Neither can be settled from the bytes the way the element width can:
/// a bounded `LIST<INT32>(768)` row of 767 counted elements is the same
/// length as one of 768 uncounted ones, and the head that says 767 is
/// one, two or four bytes according to what the column was written at.
/// So the directory says, and this takes the answer rather than guessing
/// at it.
///
/// The elements borrow the buffer they came out of, so a read of a list
/// column is the blob read and a walk over it, with no allocation per
/// element.
pub fn list_elements<'a>(
    elem: &LogicalType,
    bytes: &'a [u8],
    rows: ListRows,
) -> Result<Vec<ListElement<'a>>> {
    let (count, head_len) = match rows {
        ListRows::Fixed(count) => (count, 0usize),
        ListRows::Counted(width) => {
            let head = bytes
                .get(..width)
                .ok_or_else(|| corrupt("truncated list length".into()))?;
            let mut count = 0u64;
            for (i, byte) in head.iter().enumerate() {
                count |= u64::from(*byte) << (i * 8);
            }
            (count as usize, width)
        }
    };
    let body = bytes.len() - head_len;
    // Which width the row was written at is a question its own length
    // answers, and no directory version has to be carried here to ask
    // it. A lane row is a count and a run of equal elements, so the
    // eight byte form and the natural one differ in length on every row
    // that holds an element unless the natural width is eight, and
    // where they do not differ they are the same bytes. A length that
    // is neither is a row nothing wrote.
    let lane = match lane_width(elem) {
        Some((width, signed)) if body == count.saturating_mul(width) => Some((width, signed)),
        Some(_) if body == count.saturating_mul(8) => Some((8, false)),
        Some(_) => {
            return Err(corrupt(format!(
                "a list of {count} {elem} does not fit {} bytes",
                bytes.len()
            )));
        }
        // A blob element is four bytes of length away from its smallest
        // possible payload, so a count the row cannot hold is caught
        // before it sizes an allocation.
        None if count > body / 4 => {
            return Err(corrupt(format!(
                "a list of {count} does not fit {} bytes",
                bytes.len()
            )));
        }
        None => None,
    };
    let mut out = Vec::with_capacity(count);
    let mut pos = head_len;
    for _ in 0..count {
        if let Some((width, signed)) = lane {
            let raw = bytes
                .get(pos..pos + width)
                .ok_or_else(|| corrupt("truncated list element".into()))?;
            let mut word = 0u64;
            for (i, byte) in raw.iter().enumerate() {
                word |= u64::from(*byte) << (i * 8);
            }
            // The lane hands every reader a 64 bit word and the scalar
            // columns sign extend into it, so a negative element that
            // was cut to four bytes has to arrive back as the same
            // word a scalar column of that type would have held.
            if signed && width < 8 {
                let spare = 64 - width * 8;
                word = (((word as i64) << spare) >> spare) as u64;
            }
            out.push(ListElement::Word(word));
            pos += width;
        } else {
            let raw = bytes
                .get(pos..pos + 4)
                .ok_or_else(|| corrupt("truncated list element length".into()))?;
            let len = u32::from_le_bytes(raw.try_into().unwrap()) as usize;
            pos += 4;
            let body = bytes
                .get(pos..pos + len)
                .ok_or_else(|| corrupt("truncated list element".into()))?;
            out.push(ListElement::Blob(body));
            pos += len;
        }
    }
    if pos != bytes.len() {
        return Err(corrupt("trailing bytes in a list".into()));
    }
    Ok(out)
}

impl PropValues<'_> {
    /// Rows this column carries.
    pub fn len(&self) -> usize {
        match self {
            PropValues::Str(v) | PropValues::Bytes(v) => v.len(),
            PropValues::Int(v) => v.len(),
            PropValues::Bool(v) => v.len(),
            PropValues::Float(v) => v.len(),
            PropValues::Date(v) => v.len(),
            PropValues::LocalTime(v) | PropValues::LocalDatetime(v) => v.len(),
            PropValues::ZonedTime { nanos, .. } | PropValues::ZonedDatetime { nanos, .. } => {
                nanos.len()
            }
            PropValues::Duration(_, v) => v.len(),
            PropValues::List { rows, .. } => rows.len(),
        }
    }

    /// The zone plane, `None` for every column that has no second
    /// plane. A row count that disagrees with `len` is refused at store
    /// time, so the two planes describe the same rows or nothing is
    /// written.
    pub(crate) fn zone_plane(&self) -> Option<&[i16]> {
        match self {
            PropValues::ZonedTime { zones, .. } | PropValues::ZonedDatetime { zones, .. } => {
                Some(zones)
            }
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// No values at all, in the arm a column declared `ty` holds them
    /// in, and `None` for a type no column holds.
    ///
    /// This is [`PropValues::ty`] read the other way, and it is not its
    /// inverse. The other direction widens: a run of bytes says only
    /// that it is a run of bytes, so `BINARY(16)` and `BYTES` both
    /// answer the blob arm here and only the second of them comes back
    /// out of `ty`. What a caller wants from this is the arm, since the
    /// arm is which side of the store the column is on, and the
    /// declaration it passes beside it is the rest of the answer.
    ///
    /// A table being made has no rows in it yet, which is why what
    /// comes back holds none.
    pub fn none_of(ty: &LogicalType) -> Option<PropValues<'_>> {
        Some(match ty {
            LogicalType::Bool => PropValues::Bool(&[]),
            // A decimal column is a lane of unscaled units, so what an
            // empty one holds none of is integers. The scale that makes
            // them a number is in the declared type beside them, which
            // is why this can be the same empty lane an integer column
            // gets without the two columns being the same column.
            LogicalType::Int { .. } | LogicalType::Decimal { .. } => PropValues::Int(&[]),
            LogicalType::Float { .. } => PropValues::Float(&[]),
            LogicalType::Str { .. } => PropValues::Str(&[]),
            LogicalType::Bytes { .. } => PropValues::Bytes(&[]),
            LogicalType::Date => PropValues::Date(&[]),
            LogicalType::LocalTime => PropValues::LocalTime(&[]),
            LogicalType::LocalDatetime => PropValues::LocalDatetime(&[]),
            LogicalType::ZonedTime => PropValues::ZonedTime {
                nanos: &[],
                zones: &[],
            },
            LogicalType::ZonedDatetime => PropValues::ZonedDatetime {
                nanos: &[],
                zones: &[],
            },
            LogicalType::Duration(kind) => PropValues::Duration(*kind, &[]),
            LogicalType::List { elem, .. } => PropValues::List {
                elem: list_elem(elem)?,
                rows: &[],
            },
            _ => return None,
        })
    }

    /// The type a column holding these values is declared as.
    pub fn ty(&self) -> LogicalType {
        match self {
            PropValues::Str(_) => LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
            PropValues::Bytes(_) => LogicalType::Bytes {
                min: None,
                max: None,
                fixed: false,
            },
            PropValues::Int(_) => LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            },
            PropValues::Bool(_) => LogicalType::Bool,
            PropValues::Float(_) => LogicalType::Float {
                bits: FloatBits::B64,
                precision: None,
            },
            PropValues::Date(_) => LogicalType::Date,
            PropValues::LocalTime(_) => LogicalType::LocalTime,
            PropValues::LocalDatetime(_) => LogicalType::LocalDatetime,
            PropValues::ZonedTime { .. } => LogicalType::ZonedTime,
            PropValues::ZonedDatetime { .. } => LogicalType::ZonedDatetime,
            PropValues::Duration(kind, _) => LogicalType::Duration(*kind),
            PropValues::List { elem, .. } => list_of((*elem).clone()),
        }
    }

    /// The lane words for a fixed width column, `None` for the blob
    /// arms. Signed values sit in the lane two's complement, floats sit
    /// there as their IEEE bits; the column's type is what says which.
    ///
    /// Integer columns borrow rather than copy, which is not a detail:
    /// they are the column a bulk load is mostly made of, and the load
    /// is measured in nodes per second, so a widening that made every
    /// integer column a fresh allocation would be a real cost paid by
    /// every load for the sake of the arms that need one.
    pub(crate) fn lane(&self) -> Option<Cow<'_, [u64]>> {
        Some(match self {
            PropValues::Str(_) | PropValues::Bytes(_) | PropValues::List { .. } => return None,
            PropValues::Int(v) => Cow::Borrowed(*v),
            PropValues::Bool(v) => v.iter().map(|&b| u64::from(b)).collect(),
            PropValues::Float(v) => v.iter().map(|&f| f.to_bits()).collect(),
            PropValues::Date(v) => v.iter().map(|&d| i64::from(d) as u64).collect(),
            PropValues::LocalTime(v) | PropValues::LocalDatetime(v) => {
                v.iter().map(|&n| n as u64).collect()
            }
            // The lane of a zoned column is its instants. That is the
            // half of the value every comparison reads, so the sort
            // key, the histogram and every range predicate come out of
            // the lane unchanged, and the zones ride the plane beside
            // it where only materialising a value looks.
            PropValues::ZonedTime { nanos, .. } | PropValues::ZonedDatetime { nanos, .. } => {
                nanos.iter().map(|&n| n as u64).collect()
            }
            PropValues::Duration(_, v) => v.iter().map(|&n| n as u64).collect(),
        })
    }

    /// The sort key of every value, what the estimator's histogram is
    /// built over. Floats go through the IEEE total order so a range
    /// estimate over a float column reads the same order a comparison
    /// does, rather than the order of the raw bit pattern.
    fn keys(&self) -> Option<Vec<[u8; 8]>> {
        let words = self.lane()?;
        Some(match self {
            PropValues::Float(_) => words.iter().map(|&w| int_key(float_key(w))).collect(),
            _ => words.iter().map(|&w| int_key(w as i64)).collect(),
        })
    }
}

/// The IEEE 754 total order of a float, as a signed integer: negatives
/// invert so they sort under positives, positives get their sign bit
/// flipped on so they sort over them.
fn float_key(bits: u64) -> i64 {
    let signed = bits as i64;
    if signed < 0 {
        !signed
    } else {
        signed ^ i64::MIN
    }
}

/// One segment meta out of a directory of `version`. Payloads could
/// not be packed beside each other before version 6, so a meta written
/// then has no start word and reads with the older header length.
fn seg(bytes: &[u8], pos: usize, version: u16) -> Result<(SegmentMeta, usize)> {
    match version >= 6 {
        true => SegmentMeta::decode(bytes, pos),
        false => SegmentMeta::decode_unpacked(bytes, pos),
    }
}

/// Whether a directory written under `version` was allowed to carry
/// this type code. Version 1 wrote two codes and version 2 wrote every
/// code but the list one, so a file claiming an older version and
/// carrying a newer code is a file that has been edited.
fn code_allowed(code: u8, version: u16) -> bool {
    match version {
        1 => code <= 1,
        2 => code != LIST_CODE,
        // The extended form was a catalog encoding before version 8 and
        // no column entry carried it, so one in an older directory is a
        // file that has been edited rather than an older column.
        3..=7 => code != EXTENDED_CODE,
        _ => true,
    }
}

fn corrupt(detail: String) -> ZuError {
    ZuError::Corrupt {
        what: "props directory",
        detail,
    }
}

/// A fixed width read asked of a column that is stored as blobs.
fn not_lane(column: &PropColumn) -> ZuError {
    ZuError::InvalidArgument(format!(
        "column '{}' holds {} and is not a fixed width column",
        column.name, column.ty
    ))
}

/// A blob read asked of a column that is stored in the lane.
fn not_blob(column: &PropColumn) -> ZuError {
    ZuError::InvalidArgument(format!(
        "column '{}' holds {} and is not a variable width column",
        column.name, column.ty
    ))
}

impl PropsDirectory {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PROPS_VERSION.to_le_bytes());
        out.extend_from_slice(&self.node_count.to_le_bytes());
        out.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
        for col in &self.columns {
            out.extend_from_slice(&(col.name.len() as u16).to_le_bytes());
            out.extend_from_slice(col.name.as_bytes());
            // Unstorable types are refused at store time, so a column
            // that reached the directory has a code.
            out.extend_from_slice(&column_type_bytes(&col.ty).expect("column type is storable"));
            // The count the rows were written at, beside the type they
            // were written under, because the two are one statement:
            // what the column was declared, and what the block kept of
            // it. `NO_BOUND` is a column whose rows say it themselves.
            out.extend_from_slice(&col.fixed_len.unwrap_or(NO_BOUND).to_le_bytes());
            // And beside that, the width the rows that do say it said it
            // in. A column written before version 12 said it in four,
            // and a fold that leaves those rows where they are writes
            // the four back here, so a directory read forward and
            // written back still describes the rows it points at.
            out.push(col.count_width);
            col.meta.encode(&mut out);
            match &col.validity {
                Some(meta) => {
                    out.push(1);
                    meta.encode(&mut out);
                }
                None => out.push(0),
            }
            // The zone plane needs no flag byte, unlike the validity
            // mask beside it: every zoned column has one and no other
            // column has one, so the type already said whether it is
            // here, and a byte on every column of every table would be
            // paid for a question the type answers.
            if zoned(&col.ty) {
                col.zones
                    .as_ref()
                    .expect("a zoned column is written with its zone plane")
                    .encode(&mut out);
            }
        }
        match &self.labels {
            Some(meta) => {
                out.push(1);
                meta.encode(&mut out);
            }
            None => out.push(0),
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let head = bytes
            .get(..14)
            .ok_or_else(|| corrupt("truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        // Older versions are still read. Version 1 only ever wrote
        // codes 0 and 1 and version 2 never wrote the list code, and
        // every code means in this version what it meant then, so the
        // difference between the versions is which codes may appear
        // rather than how any of them decodes. Version 7 narrowed the
        // list row and that is the one difference inside a value, but
        // it is settled by the row's own length in `list_elements` and
        // never by this number, which is why it is not passed down.
        if version > PROPS_VERSION || version == 0 {
            return Err(ZuError::Unsupported {
                what: "props directory version",
                id: u32::from(version),
            });
        }
        let node_count = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let column_count = u32::from_le_bytes(head[10..14].try_into().unwrap()) as usize;
        // A column entry is at least 3 bytes of name and type ahead of
        // its segment meta, so a count the payload cannot hold is
        // rejected before it sizes an allocation.
        if column_count > bytes.len().saturating_sub(14) / 3 {
            return Err(corrupt("truncated column entry".into()));
        }
        let mut pos = 14usize;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let raw = bytes
                .get(pos..pos + 2)
                .ok_or_else(|| corrupt("truncated name length".into()))?;
            let len = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
            pos += 2;
            if len == 0 || len > MAX_NAME_LEN {
                return Err(corrupt(format!(
                    "name length {len} out of 1..{MAX_NAME_LEN}"
                )));
            }
            let raw = bytes
                .get(pos..pos + len)
                .ok_or_else(|| corrupt("truncated name".into()))?;
            let name =
                String::from_utf8(raw.to_vec()).map_err(|_| corrupt("name is not UTF-8".into()))?;
            pos += len;
            let ty = match bytes.get(pos) {
                Some(&code) if !code_allowed(code, version) => {
                    return Err(corrupt(format!(
                        "column type {code} in a version {version} directory"
                    )));
                }
                Some(&LIST_CODE) => {
                    let elem = bytes
                        .get(pos + 1)
                        .ok_or_else(|| corrupt("truncated list element type".into()))?;
                    let elem = code_type(*elem)
                        .ok_or_else(|| corrupt(format!("unknown element type {elem}")))?;
                    pos += 2;
                    list_of(elem)
                }
                // The extended form is the only type field that is not
                // one byte or two, so it moves `pos` itself. What it
                // may say here is narrower than what it may say in the
                // catalog, and the writer knows the same rule, so a
                // type this refuses is a type nothing wrote.
                Some(&EXTENDED_CODE) => {
                    pos += 1;
                    let ty = decode_extended_type(bytes, &mut pos)?;
                    if column_type_bytes(&ty).is_none() {
                        return Err(corrupt(format!("column type {ty} is not storable")));
                    }
                    // A bounded list was unstorable before version 9,
                    // because the count that makes it worth storing had
                    // nowhere to be written, so one in an older
                    // directory is a file that has been edited.
                    if version < 9 && bounded_list(&ty).is_some() {
                        return Err(corrupt(format!(
                            "column type {ty} in a version {version} directory"
                        )));
                    }
                    // And a zoned column was unstorable before version
                    // 10, for the same reason read the same way: the
                    // second plane it takes had nowhere to be written.
                    if version < 10 && zoned(&ty) {
                        return Err(corrupt(format!(
                            "column type {ty} in a version {version} directory"
                        )));
                    }
                    ty
                }
                Some(&code) => {
                    let ty = code_type(code)
                        .ok_or_else(|| corrupt(format!("unknown column type {code}")))?;
                    pos += 1;
                    ty
                }
                None => return Err(corrupt("truncated column type".into())),
            };
            // A directory older than version 9 has no fixed count, and
            // its list rows all carry their own, which is the same
            // column read either way.
            let fixed_len = match version >= 9 {
                true => {
                    let raw = bytes
                        .get(pos..pos + 4)
                        .ok_or_else(|| corrupt("truncated fixed count".into()))?;
                    pos += 4;
                    match u32::from_le_bytes(raw.try_into().unwrap()) {
                        NO_BOUND => None,
                        count => Some(count),
                    }
                }
                false => None,
            };
            // A directory older than version 12 wrote every count it
            // wrote in four bytes, which is the same column read either
            // way. A width that is none of the three is a byte nothing
            // here wrote, and it decides every row offset below it, so
            // it is refused rather than clamped.
            let count_width = match version >= 12 {
                true => match bytes.get(pos) {
                    Some(&width @ (1 | 2 | 4)) => {
                        pos += 1;
                        width
                    }
                    Some(&other) => {
                        return Err(corrupt(format!(
                            "list count width {other} is not 1, 2 or 4"
                        )));
                    }
                    None => return Err(corrupt("truncated list count width".into())),
                },
                false => 4,
            };
            let (meta, next) = seg(bytes, pos, version)?;
            pos = next;
            // A directory older than version 4 has no flag byte and no
            // null, which is the same column read either way.
            let validity = if version >= 4 {
                match bytes.get(pos) {
                    Some(0) => {
                        pos += 1;
                        None
                    }
                    Some(1) => {
                        let (meta, next) = seg(bytes, pos + 1, version)?;
                        pos = next;
                        Some(meta)
                    }
                    Some(other) => {
                        return Err(corrupt(format!("validity flag {other} is not 0 or 1")));
                    }
                    None => return Err(corrupt("truncated validity flag".into())),
                }
            } else {
                None
            };
            // A count that does not describe the payload beside it is
            // the one thing a reader must not take on trust, because
            // every row offset below follows from it. The rows are the
            // bound times the element width and there are as many of
            // them as the meta says, so the whole of the claim is one
            // multiplication and it is checked here rather than at the
            // first read.
            if let Some(count) = fixed_len {
                let (bound, width) = bounded_list(&ty).ok_or_else(|| {
                    corrupt(format!("column '{name}' holds {ty} at a fixed count"))
                })?;
                let want = u64::from(count)
                    .checked_mul(width as u64)
                    .and_then(|row| row.checked_mul(meta.value_count))
                    .ok_or_else(|| corrupt("fixed count times rows overflows".into()))?;
                if count > bound || want != meta.uncompressed_bytes {
                    return Err(corrupt(format!(
                        "column '{name}' claims {count} elements a row and does not hold them"
                    )));
                }
            }
            // The type says whether a second plane follows, and a plane
            // over a different set of rows than the instants beside it
            // is one a read would pair the wrong zone with, so the two
            // counts are agreed here rather than at the first read.
            let zones = match zoned(&ty) {
                true => {
                    let (plane, next) = seg(bytes, pos, version)?;
                    pos = next;
                    if plane.value_count != meta.value_count {
                        return Err(corrupt(format!(
                            "column '{name}' holds {} instants and {} zones",
                            meta.value_count, plane.value_count
                        )));
                    }
                    Some(plane)
                }
                false => None,
            };
            columns.push(PropColumn {
                name,
                ty,
                meta,
                validity,
                fixed_len,
                count_width,
                zones,
            });
        }
        // A directory older than version 5 has no label bitset, which
        // is a table whose rows carry its name and nothing else.
        let labels = if version >= 5 {
            match bytes.get(pos) {
                Some(0) => {
                    pos += 1;
                    None
                }
                Some(1) => {
                    let (meta, next) = seg(bytes, pos + 1, version)?;
                    pos = next;
                    Some(meta)
                }
                Some(other) => {
                    return Err(corrupt(format!("label flag {other} is not 0 or 1")));
                }
                None => return Err(corrupt("truncated label flag".into())),
            }
        } else {
            None
        };
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes".into()));
        }
        Ok(Self {
            node_count,
            columns,
            labels,
        })
    }
}

/// Copies a props directory and every segment it points at, answering
/// the root of the copy. The mirror of [`free_props`], walking the same
/// pointers in the same order.
pub(crate) fn copy_props(db: &mut Zu1File, root: BlockPtr) -> Result<BlockPtr> {
    let mut directory = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    let done = &mut IdMap::default();
    if let Some(labels) = &mut directory.labels {
        labels.blocks = crate::graph::copy_blocks(db, done, &labels.blocks)?;
    }
    for col in &mut directory.columns {
        col.meta.blocks = crate::graph::copy_blocks(db, done, &col.meta.blocks)?;
        if let Some(validity) = &mut col.validity {
            validity.blocks = crate::graph::copy_blocks(db, done, &validity.blocks)?;
        }
    }
    meta::write_chain(db, &directory.encode())
}

pub(crate) fn free_props(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_props_parts(db, root, true, &[])
}

/// Frees everything a props chain owns apart from the label bitset,
/// which the caller is carrying into the directory that replaces this
/// one. Storing a property column has nothing to say about which labels
/// a row holds, so it leaves that segment where it is.
pub(crate) fn free_props_keeping_labels(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_props_parts(db, root, false, &[])
}

/// Frees a props chain apart from the parts the caller is carrying
/// into the directory that replaces it, whether that is the label
/// bitset or a column: `keep_cols[i]` says the `i`th column's segments
/// are named by the new directory too, so its blocks stay where they
/// are and the two directories share the bytes. A slice shorter than
/// the column list keeps nothing past its end.
pub(crate) fn free_props_reusing(
    db: &mut Zu1File,
    root: BlockPtr,
    keep_labels: bool,
    keep_cols: &[bool],
) -> Result<()> {
    free_props_parts(db, root, !keep_labels, keep_cols)
}

fn free_props_parts(
    db: &mut Zu1File,
    root: BlockPtr,
    labels: bool,
    keep_cols: &[bool],
) -> Result<()> {
    let directory = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    // A block holds several of this directory's payloads, so the
    // question is not which segments are going but which blocks are
    // left with nothing in them. A block a kept column still sits in
    // stays, holding whatever the dropped ones left behind, until the
    // fold that rewrites that column too lets it go.
    let mut going = Sweep::default();
    if labels {
        going.drop_all(directory.labels.iter());
    } else {
        going.keep_all(directory.labels.iter());
    }
    for (ci, col) in directory.columns.iter().enumerate() {
        let segs = [Some(&col.meta), col.validity.as_ref(), col.zones.as_ref()];
        match keep_cols.get(ci) == Some(&true) {
            true => going.keep_all(segs.into_iter().flatten()),
            false => going.drop_all(segs.into_iter().flatten()),
        }
    }
    going.sweep(db)?;
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
}

/// The blocks a free is about to hand back, less the ones something it
/// is keeping still reads. Payloads share blocks, so freeing a
/// segment's block list one segment at a time would hand the same block
/// back twice and would hand back a block a kept segment is still in.
#[derive(Default)]
pub(crate) struct Sweep {
    dropped: BTreeSet<BlockPtr>,
    kept: BTreeSet<BlockPtr>,
}

impl Sweep {
    pub(crate) fn drop_all<'m>(&mut self, segs: impl Iterator<Item = &'m SegmentMeta>) {
        self.dropped.extend(segs.flat_map(|m| &m.blocks).copied());
    }

    pub(crate) fn keep_all<'m>(&mut self, segs: impl Iterator<Item = &'m SegmentMeta>) {
        self.kept.extend(segs.flat_map(|m| &m.blocks).copied());
    }

    pub(crate) fn sweep(self, db: &mut Zu1File) -> Result<()> {
        for ptr in self.dropped.difference(&self.kept) {
            db.free_block(*ptr)?;
        }
        Ok(())
    }
}

/// Whether a mask says every one of `rows` rows holds a value. The
/// bits past the last row are not the caller's to set, so they are not
/// read here either.
fn all_set(mask: &[u64], rows: usize) -> bool {
    let whole = rows / 64;
    let rest = rows % 64;
    mask[..whole].iter().all(|&w| w == u64::MAX)
        && (rest == 0 || mask[whole] & ((1u64 << rest) - 1) == (1u64 << rest) - 1)
}

/// One column at store time: what it is called, the value of every row
/// of it, and which of those rows hold one.
#[derive(Debug, Clone, Copy)]
pub struct PropInput<'a> {
    pub name: &'a str,
    pub values: PropValues<'a>,
    /// One bit per row, the low bit of a word first, set where the row
    /// holds a value. `None` is a column with a value in every row.
    ///
    /// A null row still needs something in `values`, because the column
    /// is dense either way; what goes there is never read, so a zero or
    /// an empty string is the usual choice. The mask is what a reader
    /// consults, and it is the only thing that says the row is null.
    pub validity: Option<&'a [u64]>,
    /// The type the catalog says this column has, where that is
    /// narrower than the type the values arrive in. `None` is a column
    /// whose declaration is exactly what its values imply.
    ///
    /// This is the split of schema/06 §2 at the point it starts: the
    /// declaration comes from the catalog, the values come from the
    /// batch, and the encoding is chosen here from both. A caller with
    /// no catalog to consult passes `None` and gets what it always got.
    /// What the declaration may say is checked against every value
    /// before anything is written, because a column that is declared
    /// `BINARY(16)` and holds a row of 15 octets is a column whose own
    /// type is a lie, and the first reader to trust it reads the next
    /// row's bytes.
    pub declared: Option<&'a LogicalType>,
}

impl<'a> PropInput<'a> {
    /// A column with a value in every row.
    pub fn dense(name: &'a str, values: PropValues<'a>) -> Self {
        Self {
            name,
            values,
            validity: None,
            declared: None,
        }
    }

    /// A column with a value in every row, of the type the catalog says
    /// it has rather than the type its values imply.
    pub fn typed(name: &'a str, values: PropValues<'a>, declared: &'a LogicalType) -> Self {
        Self {
            declared: Some(declared),
            ..Self::dense(name, values)
        }
    }

    /// Whether row `row` holds a value.
    fn holds(&self, row: usize) -> bool {
        match self.validity {
            Some(words) => words
                .get(row / 64)
                .is_some_and(|w| w & (1u64 << (row % 64)) != 0),
            None => true,
        }
    }
}

/// Stores the property columns of `node_table`, replacing any earlier
/// set whole; every column must hold exactly one value per row of the
/// table's domain. The table must already exist, so a load always
/// precedes its properties.
pub fn store_props(
    db: &mut Zu1File,
    node_table: &str,
    columns: &[(&str, PropValues)],
) -> Result<PropsDirectory> {
    let inputs: Vec<PropInput> = columns
        .iter()
        .map(|(name, values)| PropInput::dense(name, *values))
        .collect();
    store_props_nullable(db, node_table, &inputs)
}

/// The same store, for columns a caller holds whole rather than as
/// borrowed slices, which is what a node file read off disk produces.
///
/// This is [`store_rel_props_owned`] over a node table, and it exists
/// for the same reason: `PropValues::Str` wants a slice of slices and an
/// owned column has a vector of vectors, so the pointers have to live
/// somewhere for the length of the call.
pub fn store_props_owned(
    db: &mut Zu1File,
    node_table: &str,
    columns: &[OwnedColumn],
) -> Result<PropsDirectory> {
    let refs: Vec<Vec<&[u8]>> = columns
        .iter()
        .map(|c| match &c.values {
            OwnedValues::Str(v) => v.iter().map(Vec::as_slice).collect(),
            _ => Vec::new(),
        })
        .collect();
    let values: Vec<(&str, PropValues)> = columns
        .iter()
        .zip(&refs)
        .map(|(c, refs)| {
            let values = match &c.values {
                OwnedValues::Int(v) => PropValues::Int(v),
                OwnedValues::Float(v) => PropValues::Float(v),
                OwnedValues::Bool(v) => PropValues::Bool(v),
                OwnedValues::Str(_) => PropValues::Str(refs),
            };
            (c.name.as_str(), values)
        })
        .collect();
    store_props(db, node_table, &values)
}

/// The same store, for columns some rows of which hold no value.
pub fn store_props_nullable(
    db: &mut Zu1File,
    node_table: &str,
    columns: &[PropInput],
) -> Result<PropsDirectory> {
    let catalog = Catalog::load(db)?;
    let table = catalog
        .node_by_name(node_table)
        .ok_or_else(|| ZuError::InvalidArgument(format!("no node table '{node_table}'")))?;
    let (table_id, node_count) = (table.id, table.node_count);
    store_props_for(db, table_id, node_count, columns)
}

/// The same store over a table named by its id, which is what a caller
/// who has already resolved the name means, and the only way to reach a
/// table outside the home graph: a name is a name in a graph, and the
/// store above resolves one in the graph a load writes into.
///
/// `node_count` is the row domain the columns have to cover, which is
/// the table's own count and is nought for a table nothing has written
/// to yet.
pub fn store_props_for(
    db: &mut Zu1File,
    table_id: u32,
    node_count: u64,
    columns: &[PropInput],
) -> Result<PropsDirectory> {
    check_columns(node_count, columns)?;
    let mut index = TableIndex::load(db)?;
    // Which labels a row carries is not the business of a property
    // store, so the bitset moves to the new directory rather than being
    // rewritten or dropped.
    let mut labels = None;
    if let Some(root) = index.get(table_id) {
        labels = load_props_at(db, root)?.labels;
        free_props_keeping_labels(db, root)?;
        index.remove(table_id);
    }
    let (root, directory, col_stats) = write_props(db, node_count, columns, labels)?;
    index.set(table_id, root);
    crate::graph::free_chain(db, db.db_header().table_index_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;

    let mut all = stats::Stats::load(db)?;
    all.cols.insert(table_id, col_stats);
    crate::graph::free_chain(db, db.db_header().stats_root)?;
    all.store(db)?;

    db.checkpoint()?;
    Ok(directory)
}

/// Whether every column holds one value per row of a `rows` row domain,
/// and a validity mask, where it has one, the words that domain wants.
fn check_columns(rows: u64, columns: &[PropInput]) -> Result<()> {
    let words = (rows as usize).div_ceil(64);
    for column in columns {
        let (name, values) = (column.name, &column.values);
        if values.len() as u64 != rows {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {} values over {rows} rows",
                values.len()
            )));
        }
        if let Some(mask) = column.validity
            && mask.len() != words
        {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' has {} validity words over {rows} rows, which wants {words}",
                mask.len()
            )));
        }
        // A zoned column is two planes over one set of rows, and they
        // are paired by row number and by nothing else, so a plane of
        // another length would pair every row past the difference with
        // the wrong zone rather than fail to pair it.
        if let Some(offsets) = values.zone_plane()
            && offsets.len() as u64 != rows
        {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {} zones over {rows} rows",
                offsets.len()
            )));
        }
    }
    Ok(())
}

/// Writes every column and then the directory chain over them, and
/// returns the chain's root, the directory, and the statistics the
/// values gave up on the way past.
///
/// Nothing here is published and nothing here knows what the rows are.
/// A node table's rows are its nodes and a rel table's rows are its
/// edges in load order; both store the same way, and which of the two
/// it is decides only where the root gets written down, which is the
/// caller's to do.
fn write_props(
    db: &mut Zu1File,
    rows: u64,
    columns: &[PropInput],
    labels: Option<SegmentMeta>,
) -> Result<(BlockPtr, PropsDirectory, BTreeMap<String, stats::ColStats>)> {
    // One directory is one packing scope, because one directory is what
    // a free takes away: the columns written here are freed together, so
    // they may share blocks with each other and with nothing else.
    let held = db.pack_open();
    let written = write_columns(db, rows, columns, labels);
    db.pack_close(held);
    written
}

/// The octet count every row of a column of this type holds, for a
/// declaration that fixes one. `None` is a column whose rows may differ
/// in length.
///
/// A character bound is not one of these. `STRING(5,5)` is five
/// characters and a character is one to four octets, so the width it
/// fixes is a width in a unit the storage does not count in. A bound on
/// characters is a check; a bound on octets is a layout.
pub(crate) fn fixed_octets(ty: &LogicalType) -> Option<u32> {
    match ty {
        LogicalType::Bytes {
            min: Some(min),
            max: Some(max),
            ..
        } if min == max && *min > 0 => Some(*min),
        _ => None,
    }
}

/// Refuses a declaration the values do not satisfy, before any of them
/// is written.
///
/// Two questions that are one question: the declaration has to be a
/// type this arm of [`PropValues`] carries at all, and every value has
/// to be a value of it. A null row is skipped, because what a null row
/// holds is a placeholder whose writer was told it would never be read,
/// and refusing a column over a byte nobody looks at would make the
/// mask's promise conditional.
///
/// The two length bounds are two branches because they count two
/// things. A character is a Unicode scalar value and an octet is a
/// byte, so `STRING(5,5)` admits five astral characters in twenty
/// bytes, and `BINARY(5)` admits five bytes whatever they spell.
fn check_declared(
    name: &str,
    ty: &LogicalType,
    values: &PropValues<'_>,
    column: &PropInput<'_>,
) -> Result<()> {
    let (min, max, chars) = match (ty, values) {
        // A list bound counts elements, so it is neither of the two
        // lengths below and it is checked here. The element type has to
        // agree as well, and it is compared with the nullability off,
        // because a stored list holds a value in every position and
        // `LIST<INT>` and `LIST<INT NOT NULL>` are one column.
        (
            LogicalType::List { elem, max },
            PropValues::List {
                elem: given,
                rows: items,
            },
        ) => {
            let want = list_elem(elem).unwrap_or(elem);
            if want != *given {
                return Err(ZuError::InvalidArgument(format!(
                    "column '{name}' is declared {ty} and was given a list of {given}"
                )));
            }
            let Some(bound) = max else { return Ok(()) };
            for (row, row_items) in items.iter().enumerate() {
                if column.holds(row) && row_items.len() > *bound as usize {
                    return Err(ZuError::InvalidArgument(format!(
                        "column '{name}' is declared {ty} and row {row} holds {} elements",
                        row_items.len()
                    )));
                }
            }
            return Ok(());
        }
        // Every integer type rides the one lane, and a word is the same
        // word whatever width the declaration reads it at, so a
        // narrower declaration is a promise about the values rather
        // than a different encoding. What is left to check is the
        // promise: a word outside the declared range is a row the
        // column's own type says is not there.
        (LogicalType::Int { signed, bits, .. }, PropValues::Int(words)) => {
            for (row, &word) in words.iter().enumerate() {
                if !column.holds(row) || bits.holds(word, *signed) {
                    continue;
                }
                return Err(ZuError::InvalidArgument(format!(
                    "column '{name}' is declared {ty} and row {row} holds {}",
                    match signed {
                        true => (word as i64).to_string(),
                        false => word.to_string(),
                    }
                )));
            }
            return Ok(());
        }
        // A decimal is the integer arm with the promise read the other
        // way. The lane holds unscaled units and the declaration says
        // how many digits of them a value of this column has, so what
        // is checked is the digit count rather than a width: `1.20` in
        // a `DECIMAL(12,2)` is the word 120 and three digits of the
        // twelve, and a word needing thirteen is a row the column's own
        // type says is not there. The scale is not checked because the
        // lane cannot disagree with it: a unit is whatever the declared
        // scale says a unit is.
        (LogicalType::Decimal { precision, scale }, PropValues::Int(words)) => {
            for (row, &word) in words.iter().enumerate() {
                if !column.holds(row) {
                    continue;
                }
                let held = Decimal::new(i128::from(word as i64), *scale);
                if held.digits() <= *precision {
                    continue;
                }
                return Err(ZuError::InvalidArgument(format!(
                    "column '{name}' is declared {ty} and row {row} holds {held}"
                )));
            }
            return Ok(());
        }
        // A float is the other way round: the lane holds IEEE bits, and
        // an `f32`'s are not a half of an `f64`'s, so a narrower
        // declaration is a different word and not a promise about the
        // same one. The values would have to arrive already narrowed,
        // and this arm is the wide ones, so the only column it admits
        // is one with no row in it yet. That is the column a table
        // being made has, and every row written into it afterwards goes
        // through the statement path, which narrows.
        (LogicalType::Float { bits, .. }, PropValues::Float(v))
            if *bits != FloatBits::B64 && v.is_empty() =>
        {
            return Ok(());
        }
        // A zoned column's declaration says nothing a value could fall
        // outside of, so what is checked is the zone plane rather than
        // the declaration: an offset past eighteen hours names no zone
        // on earth, and a row holding one would print an hour the
        // calendar does not have. It is the same bound `Temporal`
        // enforces when it parses one, said here for the values that
        // arrive already taken apart.
        (
            LogicalType::ZonedTime | LogicalType::ZonedDatetime,
            PropValues::ZonedTime { zones, .. } | PropValues::ZonedDatetime { zones, .. },
        ) if *ty == values.ty() => {
            for (row, &offset) in zones.iter().enumerate() {
                if !column.holds(row) || (-1080..=1080).contains(&offset) {
                    continue;
                }
                return Err(ZuError::InvalidArgument(format!(
                    "column '{name}' is declared {ty} and row {row} is offset {offset} minutes"
                )));
            }
            return Ok(());
        }
        (LogicalType::Str { min, max, .. }, PropValues::Str(_)) => (*min, *max, true),
        (LogicalType::Bytes { min, max, .. }, PropValues::Bytes(_)) => (*min, *max, false),
        _ if *ty == values.ty() => return Ok(()),
        _ => {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' is declared {ty} and was given {}",
                values.ty()
            )));
        }
    };
    if min.is_none() && max.is_none() {
        return Ok(());
    }
    let rows = match values {
        PropValues::Str(v) | PropValues::Bytes(v) => *v,
        _ => unreachable!("only the two blob arms reach a length bound"),
    };
    for (row, value) in rows.iter().enumerate() {
        if !column.holds(row) {
            continue;
        }
        let len = match chars {
            true => std::str::from_utf8(value)
                .map_err(|_| {
                    ZuError::InvalidArgument(format!(
                        "column '{name}' is declared {ty} and row {row} is not UTF-8"
                    ))
                })?
                .chars()
                .count(),
            false => value.len(),
        };
        if min.is_some_and(|n| len < n as usize) || max.is_some_and(|n| len > n as usize) {
            let unit = match chars {
                true => "characters",
                false => "octets",
            };
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' is declared {ty} and row {row} holds {len} {unit}"
            )));
        }
    }
    Ok(())
}

fn write_columns(
    db: &mut Zu1File,
    rows: u64,
    columns: &[PropInput],
    labels: Option<SegmentMeta>,
) -> Result<(BlockPtr, PropsDirectory, BTreeMap<String, stats::ColStats>)> {
    let node_count = rows;
    let mut cols = Vec::with_capacity(columns.len());
    let mut col_stats = BTreeMap::new();
    for column in columns {
        let (name, values) = (column.name, &column.values);
        // The values are all in hand here and nowhere else, so this is
        // where the estimator's statistics get built: a COPY that
        // brings properties leaves the optimizer able to reason about
        // them, without anyone remembering to ANALYZE (perf/12 §1).
        let ty = column_type(match column.declared {
            Some(declared) => declared.clone(),
            None => values.ty(),
        });
        if column_type_bytes(&ty).is_none() {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' has type {ty}, which is not storable as a property"
            )));
        }
        check_declared(name, &ty, values, column)?;
        // A list column is encoded here rather than by its caller, so
        // the row format has one writer and one reader and they sit
        // next to each other.
        // A declared bound the column is written whole at takes the
        // count out of every row and leaves it in the catalog, which is
        // schema/06 §2 applied to the one field of a list row that is
        // not an element. It is only sound where every row is at the
        // bound: a shorter row in a bounded column still has to say how
        // long it is, so the column keeps the form that says.
        let fixed_len = match (bounded_list(&ty), values) {
            (Some((bound, _)), PropValues::List { rows, .. })
                if rows
                    .iter()
                    .enumerate()
                    .all(|(row, items)| !column.holds(row) || items.len() == bound as usize) =>
            {
                Some(bound)
            }
            _ => None,
        };
        // And where they are not all at it, the bound is still worth
        // something: a count cannot exceed it, `check_declared` has just
        // said so for every row, and a number that cannot exceed 255
        // does not need four bytes to say. This is the same sentence as
        // the one above with the count kept rather than dropped.
        //
        // The width goes in the column entry whether the rows use it or
        // not: a column of anything but a list has no count to write,
        // and a fixed one has taken its counts out, so both write the
        // four an older file wrote and neither is ever asked.
        let counts = count_width(&ty) as u8;
        let list_rows = match fixed_len {
            Some(count) => ListRows::Fixed(count as usize),
            None => ListRows::Counted(counts as usize),
        };
        let encoded: Vec<Vec<u8>> = match values {
            PropValues::List { elem, rows } => {
                let elem = list_elem(elem).expect("storable");
                // A null row of a fixed count column is the bound in
                // zero elements, for the reason a null row of a fixed
                // octet column is the width in zero bytes: a
                // placeholder of another length makes the rows ragged
                // and costs the column its layout for bytes nothing
                // reads.
                let absent = vec![ListElement::Word(0); fixed_len.unwrap_or(0) as usize];
                rows.iter()
                    .enumerate()
                    .map(
                        |(row, items)| match fixed_len.is_some() && !column.holds(row) {
                            true => encode_list_row(elem, &absent, list_rows),
                            false => encode_list_row(elem, items, list_rows),
                        },
                    )
                    .collect::<Result<_>>()?
            }
            _ => Vec::new(),
        };
        let blobs: Vec<&[u8]> = encoded.iter().map(|b| &b[..]).collect();
        let lane_keys = values.keys();
        // A list column's statistics are built over its encoded rows,
        // which counts distinct lists correctly and gives a range no
        // comparison asks about, since a list column is not a column
        // anything ranges over.
        let sortable: Vec<&[u8]> = match (&lane_keys, values) {
            (Some(keys), _) => keys.iter().map(|k| &k[..]).collect(),
            (None, PropValues::Str(v) | PropValues::Bytes(v)) => v.to_vec(),
            (None, PropValues::List { .. }) => blobs.clone(),
            (None, _) => unreachable!("every fixed width column has lane keys"),
        };
        // Statistics describe what a query can find, and a null row
        // holds nothing to find, so the rows without a value are left
        // out of them. A column whose range came from its placeholders
        // would have the optimizer expecting rows no predicate returns.
        let stat = match column.validity {
            None => stats::column_stats(&sortable),
            Some(_) => {
                let live: Vec<&[u8]> = sortable
                    .iter()
                    .enumerate()
                    .filter(|(row, _)| column.holds(*row))
                    .map(|(_, value)| *value)
                    .collect();
                stats::column_stats(&live)
            }
        };
        col_stats.insert(name.to_string(), stat);
        // A null row's value is a placeholder nothing reads, and the
        // caller was told it may write anything there. In a column whose
        // declaration fixes the width, anything is the one thing it may
        // not be: a placeholder of another length makes the rows ragged
        // and costs the whole column its layout for the sake of bytes no
        // reader looks at. So the writer makes them the declared width,
        // which is its job and not the caller's.
        let padded: Vec<Vec<u8>> = match (fixed_octets(&ty), values, column.validity) {
            (Some(width), PropValues::Bytes(v), Some(_)) => v
                .iter()
                .enumerate()
                .map(|(row, value)| match column.holds(row) {
                    true => value.to_vec(),
                    false => vec![0u8; width as usize],
                })
                .collect(),
            _ => Vec::new(),
        };
        let padded_refs: Vec<&[u8]> = padded.iter().map(|b| &b[..]).collect();
        let meta = match (values.lane(), values) {
            (Some(words), _) => write_segment(db, &words)?,
            // A declaration that fixes the octet count says the rows are
            // equal length, so the column goes to the layout for those
            // and costs the width and nothing else. An unbounded byte
            // string column stays zipped, because rows that happen to
            // agree today are not rows a type says will agree.
            (None, PropValues::Bytes(v)) if fixed_octets(&ty).is_some() => {
                match padded.is_empty() {
                    true => write_rows(db, v)?,
                    false => write_rows(db, &padded_refs)?,
                }
            }
            (None, PropValues::Str(v) | PropValues::Bytes(v)) => write_blob_segment(db, v)?,
            (None, PropValues::List { .. }) => write_rows(db, &blobs)?,
            (None, _) => unreachable!("every variable width column is a blob"),
        };
        // A mask with every bit set says nothing a reader does not
        // already assume, so it is not written: a caller that hands one
        // over gets the column it would have got without it.
        let validity = match column.validity {
            Some(mask) if !all_set(mask, node_count as usize) => Some(write_segment(db, mask)?),
            _ => None,
        };
        // The zone plane is written whole, placeholders and all. A null
        // row's offset is a zone nothing reads, the way a null row's
        // instant is, and leaving a hole in one plane of two would cost
        // both of them the row numbering they are paired by.
        let zones = match values.zone_plane() {
            Some(offsets) => {
                let words: Vec<u64> = offsets.iter().map(|&z| i64::from(z) as u64).collect();
                Some(write_segment(db, &words)?)
            }
            None => None,
        };
        cols.push(PropColumn {
            name: name.to_string(),
            ty,
            meta,
            validity,
            fixed_len,
            count_width: counts,
            zones,
        });
    }
    let directory = PropsDirectory {
        node_count,
        columns: cols,
        labels,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    Ok((root, directory, col_stats))
}

/// Stores the property columns of the rel table `rel_table`, replacing
/// any earlier set whole. Every column holds one value per edge, in the
/// order the edges were loaded in, which is sorted by source and then by
/// destination: value `i` belongs to edge `i` of that order, and that is
/// the only thing tying a column to an edge, so a caller that hands over
/// a column in another order has silently mislabeled its graph.
///
/// A pair may run twice. Two edges over the same endpoints are two
/// slots of the forward list and two values of the column, and a walk
/// that counts as it goes has each one's own
/// ([`crate::graph::GraphReader::out_neighbors_from`]). What such a
/// table cannot do is answer a lookup given nothing but the pair with
/// both values, and `edge_ordinal` says which of the two it picks.
pub fn store_rel_props(
    db: &mut Zu1File,
    rel_table: &str,
    columns: &[(&str, PropValues)],
) -> Result<PropsDirectory> {
    let inputs: Vec<PropInput> = columns
        .iter()
        .map(|(name, values)| PropInput::dense(name, *values))
        .collect();
    store_rel_props_nullable(db, rel_table, &inputs)
}

/// The same store, for columns a caller holds whole rather than as
/// borrowed slices, which is what a parquet edge list produces.
///
/// The string arm is the only reason this exists: `PropValues::Str`
/// wants a slice of slices and an owned column has a vector of
/// vectors, so the pointers have to live somewhere for the length of
/// the call.
pub fn store_rel_props_owned(
    db: &mut Zu1File,
    rel_table: &str,
    columns: &[OwnedColumn],
) -> Result<PropsDirectory> {
    let refs: Vec<Vec<&[u8]>> = columns
        .iter()
        .map(|c| match &c.values {
            OwnedValues::Str(v) => v.iter().map(Vec::as_slice).collect(),
            _ => Vec::new(),
        })
        .collect();
    let values: Vec<(&str, PropValues)> = columns
        .iter()
        .zip(&refs)
        .map(|(c, refs)| {
            let values = match &c.values {
                OwnedValues::Int(v) => PropValues::Int(v),
                OwnedValues::Float(v) => PropValues::Float(v),
                OwnedValues::Bool(v) => PropValues::Bool(v),
                OwnedValues::Str(_) => PropValues::Str(refs),
            };
            (c.name.as_str(), values)
        })
        .collect();
    store_rel_props(db, rel_table, &values)
}

/// The same store, for columns some edges of which hold no value.
pub fn store_rel_props_nullable(
    db: &mut Zu1File,
    rel_table: &str,
    columns: &[PropInput],
) -> Result<PropsDirectory> {
    let catalog = Catalog::load(db)?;
    let rel = catalog
        .rel_by_name(rel_table)
        .ok_or_else(|| ZuError::InvalidArgument(format!("no rel table '{rel_table}'")))?;
    let (rel_id, edge_count) = (rel.id, rel.edge_count);
    store_rel_props_for(db, rel_id, edge_count, columns)
}

/// The same store over a rel table named by its id, which is what a
/// caller who has already resolved the name means, and the only way to
/// reach a table outside the home graph. This is [`store_props_for`] on
/// the edge side, and the difference is where the columns hang: a node
/// table's props are the table index entry, a rel table's are a field
/// of the directory that entry holds.
pub fn store_rel_props_for(
    db: &mut Zu1File,
    rel_id: u32,
    edge_count: u64,
    columns: &[PropInput],
) -> Result<PropsDirectory> {
    check_columns(edge_count, columns)?;
    let mut index = TableIndex::load(db)?;
    let root = index.get(rel_id).ok_or_else(|| ZuError::Corrupt {
        what: "table index",
        detail: format!("rel table {rel_id} has no directory entry"),
    })?;
    let mut directory = crate::graph::Directory::decode(&meta::read_chain(db, root)?)?;
    if directory.props != crate::file::NULL_BLOCK {
        free_props(db, directory.props)?;
    }
    let (props_root, stored, col_stats) = write_props(db, edge_count, columns, None)?;
    directory.props = props_root;
    crate::graph::free_chain(db, root)?;
    index.set(rel_id, meta::write_chain(db, &directory.encode())?);
    crate::graph::free_chain(db, db.db_header().table_index_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;

    let mut all = stats::Stats::load(db)?;
    all.cols.insert(rel_id, col_stats);
    crate::graph::free_chain(db, db.db_header().stats_root)?;
    all.store(db)?;

    db.checkpoint()?;
    Ok(stored)
}

/// Stores the label bitset of a node table, replacing any earlier one,
/// and declares in the catalog every label a row of it carries.
///
/// Word `i` is row `i` of the table and bit `l` of it says the row
/// carries dictionary label `l`. The table's own label is set on every
/// row whether the caller sets it or not: a row of a table is what that
/// table is called, and a bitset that said otherwise would answer a
/// pattern differently from the catalog.
///
/// Declaring is the point of the catalog half. A table that never
/// declares a label cannot hold a row with it, so a pattern naming that
/// label prunes the table at plan time rather than reading a word per
/// row to find out.
pub fn store_labels<S: AsRef<str>>(
    db: &mut Zu1File,
    node_table: &str,
    rows: &[Vec<S>],
) -> Result<()> {
    let mut catalog = Catalog::load(db)?;
    let table = catalog
        .node_by_name(node_table)
        .ok_or_else(|| ZuError::InvalidArgument(format!("no node table '{node_table}'")))?;
    let (table_id, node_count, primary) = (table.id, table.node_count, table.primary_label());
    if rows.len() as u64 != node_count {
        return Err(ZuError::InvalidArgument(format!(
            "node table '{node_table}' holds {node_count} rows and the label set holds {}",
            rows.len()
        )));
    }
    let mut words = Vec::with_capacity(rows.len());
    for labels in rows {
        let mut word = 1u64 << primary;
        for label in labels {
            word |= 1 << catalog.declare_label(table_id, label.as_ref())?;
        }
        words.push(word);
    }

    let mut index = TableIndex::load(db)?;
    let mut directory = match index.get(table_id) {
        Some(root) => {
            let directory = load_props_at(db, root)?;
            // The bitset is going and the columns are staying, and the
            // two of them share blocks, so what goes back is only what
            // no column is left in.
            let mut going = Sweep::default();
            going.drop_all(directory.labels.iter());
            for col in &directory.columns {
                going.keep_all(
                    [Some(&col.meta), col.validity.as_ref()]
                        .into_iter()
                        .flatten(),
                );
            }
            going.sweep(db)?;
            crate::graph::free_chain(db, root)?;
            index.remove(table_id);
            directory
        }
        None => PropsDirectory {
            node_count,
            columns: Vec::new(),
            labels: None,
        },
    };
    directory.labels = Some(write_segment(db, &words)?);
    index.set(table_id, meta::write_chain(db, &directory.encode())?);
    crate::graph::free_chain(db, db.db_header().table_index_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;
    crate::graph::free_chain(db, db.db_header().catalog_root)?;
    let catalog_root = meta::write_chain(db, &catalog.encode())?;
    db.db_header_mut().catalog_root = catalog_root;
    db.checkpoint()?;
    Ok(())
}

/// Loads the props directory a chain root names.
pub fn load_props_at(db: &mut Zu1File, root: BlockPtr) -> Result<PropsDirectory> {
    PropsDirectory::decode(&meta::read_chain(db, root)?)
}

/// Loads the edge property directory of a rel table, `None` when the
/// table stores no edge properties.
pub fn load_rel_props(db: &mut Zu1File, rel_id: u32) -> Result<Option<PropsDirectory>> {
    let Some(root) = TableIndex::load(db)?.get(rel_id) else {
        return Ok(None);
    };
    let directory = crate::graph::Directory::decode(&meta::read_chain(db, root)?)?;
    if directory.props == crate::file::NULL_BLOCK {
        return Ok(None);
    }
    Ok(Some(load_props_at(db, directory.props)?))
}

/// Loads the props directory of a node table, `None` when the table
/// has no properties stored.
pub fn load_props(db: &mut Zu1File, table_id: u32) -> Result<Option<PropsDirectory>> {
    let index = TableIndex::load(db)?;
    let Some(root) = index.get(table_id) else {
        return Ok(None);
    };
    Ok(Some(PropsDirectory::decode(&meta::read_chain(db, root)?)?))
}

/// The label word row `row` carries in a stored bitset, `None` when the
/// table stores none or the row is past the end of it, which is a row
/// carrying its table's own label and nothing else.
///
/// This is [`PropsReader::label_word`] for a caller holding a directory
/// rather than a reader, which is the write side: it asks once per
/// statement, to put a change's masks over the word the row had, so
/// there is nothing for a cache to save.
pub fn stored_label_word(
    db: &mut Zu1File,
    directory: &PropsDirectory,
    row: u64,
) -> Result<Option<u64>> {
    let Some(meta) = &directory.labels else {
        return Ok(None);
    };
    if row >= directory.node_count {
        return Ok(None);
    }
    let mut word = Vec::with_capacity(1);
    read_range(db, meta, row, row + 1, &mut word)?;
    Ok(word.first().copied())
}

/// One decoded FullZip chunk of a string column: the values of rows
/// `chunk * CHUNK_ROWS` onward concatenated in `bytes`, row `i` of the
/// chunk ending at `ends[i]`.
#[derive(Debug)]
struct StrChunk {
    chunk: u64,
    bytes: Vec<u8>,
    ends: Vec<u64>,
}

/// The rows a column has been written into since the run beside them
/// was last rebuilt.
///
/// A reader asks about a chunk at a time and wants the rows of it in
/// order, which a sorted run answers as a subslice. The writer adds to
/// it on every deferred commit while the readers hold the version
/// before that one, so a commit that wrote into the run would have to
/// copy the whole of it, and a few hundred commits of that is quadratic
/// in how long the run gets. So the recent cells sit in a map instead
/// and the run is left alone, which is what [`CellPatch::seal`] is
/// about. A map is in order too, and what its range costs over a
/// subslice is a walk down a handful of nodes for the few dozen entries
/// this is allowed to hold.
type Rows<T> = BTreeMap<u64, T>;

/// How many cells a column's map takes before it is folded into the run
/// beside it.
///
/// The two halves cost opposite things. A commit copies the map, so a
/// short map is a cheap commit; sealing walks the run, so a long map
/// means sealing less often. Sealing every `k` cells over a run that
/// reaches `n` copies about `n` squared over `k` entries in the seals
/// and `k` per commit, which is smallest where `k` is the square root
/// of `n`. The deferral bounds hold a run to about a thousand cells, so
/// thirty two is that, and like the chunk size in [`RowPatch`] it is a
/// size rather than a tuning.
const PATCH_SEAL: usize = 32;

/// The cell writes a commit made that no fold has sealed into the
/// columns of one table yet.
///
/// A commit used to fold because a reader read the sealed file and
/// nothing else, so a change that was not folded was a change the next
/// `MATCH` could not see. Rewriting a column to change one cell of it
/// is most of what a point write cost. A reader holding one of these
/// reads the column as it stands and puts the newer values over the
/// rows the patch names, which is what lets the fold wait for the
/// checkpoint.
///
/// A value goes over the value the row already holds, so what is
/// refused is the write that has nowhere to go over: a value taken
/// away, which lives in the validity mask. It folds the way it always
/// did. A label has a bitset of its own and [`LabelPatch`] carries it.
#[derive(Debug, Default, Clone)]
pub struct CellPatch {
    /// The cells of each lane column in a sorted run, which is the
    /// shape a reader wants and the shape nothing writes into. A copy
    /// of this patch shares the runs rather than copying them.
    runs: BTreeMap<usize, Arc<Vec<(u64, u64)>>>,
    /// Row and new word for each lane column written into since. Bounded
    /// by [`PATCH_SEAL`], so this is what a commit copies.
    cols: BTreeMap<usize, Rows<u64>>,
    /// Row and new bytes the same way, for the columns stored as
    /// blobs. They are apart from the words because the two are read
    /// through different paths all the way down, a lane gather against
    /// a blob range, and nothing asks for both at once.
    strs: BTreeMap<usize, Rows<Vec<u8>>>,
    /// What the strings hold between them, carried rather than counted
    /// because the writer asks on every commit and the answer is a walk
    /// over every string in here.
    bytes: usize,
    /// How many rows the two maps name between them, for the same
    /// reason.
    cells: usize,
}

impl CellPatch {
    pub fn is_empty(&self) -> bool {
        self.cells == 0
    }

    /// How many cells this holds, which is what a writer bounds when it
    /// decides whether to keep deferring the fold.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// How many bytes the strings in it hold between them, the other
    /// thing that writer bounds: a word is a word, but a string is
    /// whatever the statement wrote, and a run of long ones would be
    /// carried until the next fold.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Writes `word` onto row `row` of lane column `col`.
    ///
    /// A row written twice before a fold is in here once, holding what
    /// the later of the two commits left, which is why this answers
    /// with nothing: there is no history to keep, only the value a read
    /// gets now.
    pub fn set(&mut self, col: usize, row: u64, word: u64) {
        // Asked before the write rather than off what the map hands
        // back, because a row already in the run is not a new cell
        // either and the map has never heard of it.
        let fresh = self.get(col, row).is_none();
        let recent = self.cols.entry(col).or_default();
        recent.insert(row, word);
        if fresh {
            self.cells += 1;
        }
        if recent.len() >= PATCH_SEAL {
            self.seal(col);
        }
    }

    /// Folds a column's recent cells into its run.
    ///
    /// This is the whole of what the run costs: the copy a commit was
    /// making of every cell it had accumulated happens here instead,
    /// once every [`PATCH_SEAL`] cells rather than once a commit. The
    /// readers holding the patch as it was keep the run they were given,
    /// which is why the new one is built beside it rather than into it.
    fn seal(&mut self, col: usize) {
        let recent = self.cols.remove(&col).unwrap_or_default();
        let was = self.runs.get(&col).map_or(&[][..], |run| run.as_slice());
        let mut run = Vec::with_capacity(was.len() + recent.len());
        let mut older = was.iter().copied().peekable();
        for (&row, &word) in &recent {
            while older.peek().is_some_and(|(at, _)| *at < row) {
                run.push(older.next().expect("peeked"));
            }
            // The same row in both is the newer of the two, and the
            // older one goes nowhere: there is no history here, only
            // what a read gets now.
            older.next_if(|(at, _)| *at == row);
            run.push((row, word));
        }
        run.extend(older);
        self.runs.insert(col, Arc::new(run));
    }

    /// The same for a blob column, where what the row held also has to
    /// come off the byte count the writer bounds.
    pub fn set_bytes(&mut self, col: usize, row: u64, value: Vec<u8>) {
        self.bytes += value.len();
        match self.strs.entry(col).or_default().insert(row, value) {
            Some(was) => self.bytes -= was.len(),
            None => self.cells += 1,
        }
    }

    /// Whether a lane column has anything patched over it at all.
    fn holds(&self, col: usize) -> bool {
        self.runs.get(&col).is_some_and(|run| !run.is_empty())
            || self.cols.get(&col).is_some_and(|rows| !rows.is_empty())
    }

    /// The entries for rows `lo..hi`, the run first and the recent
    /// cells after it.
    ///
    /// Row order within each half and not across the two, which is what
    /// both callers want: one writes each entry into a decoded chunk at
    /// its own offset, so a row in both halves has to arrive from the
    /// run before it arrives from the map and does, and the other is
    /// widening a pair of bounds and does not care what order it sees
    /// them in.
    fn span(&self, col: usize, lo: u64, hi: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        let run = self.runs.get(&col).map_or(&[][..], |run| run.as_slice());
        let from = run.partition_point(|(row, _)| *row < lo);
        let to = run.partition_point(|(row, _)| *row < hi);
        run[from..to].iter().copied().chain(
            self.cols
                .get(&col)
                .into_iter()
                .flat_map(move |rows| rows.range(lo..hi))
                .map(|(&row, &word)| (row, word)),
        )
    }

    fn get(&self, col: usize, row: u64) -> Option<u64> {
        if let Some(word) = self.cols.get(&col).and_then(|rows| rows.get(&row)) {
            return Some(*word);
        }
        let run = self.runs.get(&col)?;
        let at = run.partition_point(|(held, _)| *held < row);
        run.get(at)
            .filter(|(held, _)| *held == row)
            .map(|(_, w)| *w)
    }

    /// The bytes written onto row `row` of blob column `col`.
    fn bytes_of(&self, col: usize, row: u64) -> Option<&[u8]> {
        self.strs.get(&col)?.get(&row).map(Vec::as_slice)
    }

    /// The blob entries for rows `lo..hi`, in row order for the same
    /// reason [`Self::span`] is.
    fn str_span(&self, col: usize, lo: u64, hi: u64) -> impl Iterator<Item = (u64, &[u8])> + '_ {
        self.strs
            .get(&col)
            .into_iter()
            .flat_map(move |rows| rows.range(lo..hi))
            .map(|(&row, bytes)| (row, bytes.as_slice()))
    }

    /// `bounds` widened over whatever this holds for rows `lo..hi`.
    ///
    /// A zone is there to be skipped past, so it has to hold every
    /// value the reader can see. The word a patched row used to carry
    /// is still inside the stored bounds and the reader never returns
    /// it, so widening is all this owes: a bound that is too wide costs
    /// a chunk read and a bound that is too narrow loses a row.
    fn widen(&self, col: usize, lo: u64, hi: u64, bounds: (u64, u64)) -> (u64, u64) {
        let (mut min, mut max) = bounds;
        for (_, value) in self.span(col, lo, hi) {
            min = min.min(value);
            max = max.max(value);
        }
        (min, max)
    }
}

/// The labels a commit put on rows that no fold has written into the
/// bitset yet.
///
/// A label change is written as a pair of masks, the bits to put on a
/// row and the bits to take off it, because two statements can name two
/// labels of one row and both have to land. What a reader wants is the
/// word, so the composing is done once, on the way in: the writer reads
/// the word the row carried, puts the masks over it, and what is kept
/// here is the answer. That also puts the rules a change has to keep,
/// which is the fold's business, where the row it lands on is known.
#[derive(Debug, Default, Clone)]
pub struct LabelPatch {
    /// Row and the word it is left carrying, ascending by row.
    words: BTreeMap<u64, u64>,
}

impl LabelPatch {
    /// The word `row` carries, `None` when no commit has named it and
    /// the bitset underneath is the whole answer.
    pub fn get(&self, row: u64) -> Option<u64> {
        self.words.get(&row).copied()
    }

    /// Puts `word` on `row`, which is what the row carries from here
    /// until a fold seals it.
    pub fn set(&mut self, row: u64, word: u64) {
        self.words.insert(row, word);
    }

    /// How many rows this names, which is what a writer bounds when it
    /// decides whether to keep deferring the fold.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// The rows a commit added that no fold has put in the columns yet.
///
/// [`CellPatch`] carries a new value for a row the columns already hold,
/// which is an update. This carries whole rows past the end of them,
/// which is what an edge insert leaves behind once the adjacency reader
/// stopped needing the CSR rebuilt to see it: the edge takes the next
/// ordinal, its property values sit here under that ordinal, and a
/// gather asking for it reads them from here rather than off a column
/// that has not been rewritten.
///
/// Any value at all, unlike the lane patch, because these rows are not
/// stored anywhere yet and there is nothing to lay a word over: a
/// string is the bytes and an absent value is [`Cell::Null`].
/// How many rows a sealed chunk of a [`RowPatch`] holds.
///
/// The patch grows a row at a time and is copied whole every time a
/// reader is looking at it, which every reader between two commits is,
/// so what a copy costs decides what a long run of appends costs. Cut
/// into chunks, a copy is a pointer per sealed chunk and the rows of
/// the one being filled, which is smallest where the chunk is about the
/// square root of what the run reaches. The deferral bounds keep a run
/// to a few thousand rows, so sixty four rows a chunk is that, and it
/// is a size rather than a tuning: anything within a factor of two of
/// the root is within a few percent of the best a chunk size can do.
const PATCH_CHUNK: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct RowPatch {
    /// The first row this holds, which is what the columns count.
    base: u64,
    /// The rows in ordinal order, a chunk at a time, each row a cell
    /// for every column the table stores, by position in the directory.
    /// A chunk in here is full and never written into again, so a copy
    /// of this patch shares the chunks rather than the rows.
    chunks: Arc<Vec<Arc<Vec<Vec<Cell>>>>>,
    /// The chunk being filled, which is the only one an append touches
    /// and so the only one a copy has to leave alone.
    tail: Arc<Vec<Vec<Cell>>>,
    /// The column a key lookup asks about, where the table has one.
    ///
    /// A key index is built by a fold, so the rows in here are in none
    /// and [`Self::row_with`] is what answers instead. Walking them
    /// works and is what it used to do, and it costs the length of the
    /// run on every lookup, which is a per read cost that grows with
    /// how long the writer is allowed to defer. So the sealed chunks
    /// carry their own index of this column and the walk is over the
    /// chunk being filled.
    key: Option<usize>,
    /// Row by the word it holds in [`Self::key`], one map per sealed
    /// chunk so that a copy of this patch shares them the same way it
    /// shares the chunks.
    keyed: Arc<Vec<Arc<HashMap<u64, u64>>>>,
    /// How many rows the two hold between them.
    len: usize,
    /// What the strings among them hold, carried rather than counted.
    ///
    /// The writer asks this of the whole patch before every deferred
    /// commit, to decide whether the run can go on, and counting it
    /// means walking every cell of every row added since the last fold.
    /// That is a walk of the run on every commit of the run, which is
    /// quadratic in how long the run is allowed to get, and how long
    /// the run is allowed to get is the thing that decides how often a
    /// fold lands on a statement. So it is carried.
    bytes: usize,
}

impl RowPatch {
    /// An empty patch over columns holding `base` rows, where `key` is
    /// the column a key lookup will ask about if the table has one.
    pub fn new(base: u64, key: Option<usize>) -> Self {
        RowPatch {
            base,
            key,
            ..RowPatch::default()
        }
    }

    /// Takes one row, a cell per column, and answers with the row
    /// number it was given.
    pub fn push(&mut self, cells: Vec<Cell>) -> u64 {
        let row = self.base + self.len as u64;
        self.bytes += cells
            .iter()
            .map(|cell| match cell {
                Cell::Str(bytes) => bytes.len(),
                _ => 0,
            })
            .sum::<usize>();
        // The copy a reader is owed happens here and nowhere else, and
        // it is a copy of the chunk being filled rather than of the run.
        let tail = Arc::make_mut(&mut self.tail);
        tail.push(cells);
        if tail.len() == PATCH_CHUNK {
            let full = std::mem::replace(&mut self.tail, Arc::new(Vec::with_capacity(PATCH_CHUNK)));
            if let Some(col) = self.key {
                // Built once, as the chunk is sealed, rather than on
                // the lookups that read it. The last row wins where two
                // hold the same word, for the reason [`Self::row_with`]
                // reads the run backwards.
                let first = self.base + (self.chunks.len() * PATCH_CHUNK) as u64;
                let mut index = HashMap::with_capacity(full.len());
                for (at, cells) in full.iter().enumerate() {
                    if let Some(Cell::Int(word)) = cells.get(col) {
                        index.insert(*word, first + at as u64);
                    }
                }
                Arc::make_mut(&mut self.keyed).push(Arc::new(index));
            }
            Arc::make_mut(&mut self.chunks).push(full);
        }
        self.len += 1;
        row
    }

    /// The first row this holds, so a caller with a row number can tell
    /// an unfolded row from a stored one.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// How many rows this holds, which is what a writer bounds when it
    /// decides whether to keep deferring the fold.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many bytes of string the rows hold between them, which a
    /// writer bounds along with the count of them.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The `at`th row this holds, counting from the first one it added.
    fn row_at(&self, at: usize) -> Option<&Vec<Cell>> {
        match self.chunks.get(at / PATCH_CHUNK) {
            Some(chunk) => chunk.get(at % PATCH_CHUNK),
            None => self.tail.get(at - self.chunks.len() * PATCH_CHUNK),
        }
    }

    /// The `at`th row again, to write into. The chunk holding it is
    /// copied where a reader is still looking at the one this patch
    /// was handed, which is the same copy on write [`Self::push`]
    /// makes of the chunk it fills.
    fn row_at_mut(&mut self, at: usize) -> Option<&mut Vec<Cell>> {
        let sealed = self.chunks.len() * PATCH_CHUNK;
        if at >= sealed {
            return Arc::make_mut(&mut self.tail).get_mut(at - sealed);
        }
        let chunk = Arc::make_mut(&mut self.chunks).get_mut(at / PATCH_CHUNK)?;
        Arc::make_mut(chunk).get_mut(at % PATCH_CHUNK)
    }

    /// Whether [`Self::set`] would take a write of `col` on row `row`,
    /// which a writer asks before it takes the commit that would make
    /// one. It has to be asked rather than tried, because a commit that
    /// cannot be patched has to fold whole and nothing of it may reach
    /// the patch first.
    pub fn settable(&self, col: usize, row: u64) -> bool {
        self.key != Some(col) && self.get(col, row).is_some()
    }

    /// Writes `value` over what row `row` holds in `col`, and answers
    /// whether it could.
    ///
    /// A row this patch appended is one no column holds yet, so a
    /// statement writing onto it has nowhere else for the value to go:
    /// the lane patch lays words over stored rows and there is no
    /// stored row under this one. The value goes over the one the
    /// append carried and a read gets what the last write left, which
    /// is what the fold seals as well, because it asks the overlay for
    /// each appended row rather than taking the append at its word.
    ///
    /// The key column is refused. Each sealed chunk carries an index of
    /// it, built as the chunk was sealed and shared with every reader
    /// holding an older copy of this patch, so moving a key here would
    /// mean rebuilding an index those readers are still reading out of.
    /// A key write onto an unfolded row folds instead.
    pub fn set(&mut self, col: usize, row: u64, value: Cell) -> bool {
        if !self.settable(col, row) {
            return false;
        }
        // Read before the write, because the byte count this carries
        // has to lose what the row held here and gain what it is about
        // to hold, and neither is countable once the cell is gone.
        let was = match self.get(col, row) {
            Some(Cell::Str(bytes)) => bytes.len(),
            _ => 0,
        };
        let now = match &value {
            Cell::Str(bytes) => bytes.len(),
            _ => 0,
        };
        let at = (row - self.base) as usize;
        let cells = self.row_at_mut(at).expect("settable found the row");
        cells[col] = value;
        self.bytes = self.bytes - was + now;
        true
    }

    /// Every row it holds, in the order their ordinals are in.
    fn all(&self) -> impl Iterator<Item = &Vec<Cell>> + '_ {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
            .chain(self.tail.iter())
    }

    /// The cell row `row` holds in `col`, or `None` when the row is one
    /// the columns already hold or the column was never given a value.
    fn get(&self, col: usize, row: u64) -> Option<&Cell> {
        let at = row.checked_sub(self.base)? as usize;
        self.row_at(at)?.get(col)
    }

    /// The word row `row` holds in `col`, for the lane read paths. A
    /// string or an absence in a lane column is a patch the writer
    /// should not have built, and reads as zero rather than raising
    /// from inside a gather.
    fn word(&self, col: usize, row: u64) -> Option<u64> {
        match self.get(col, row) {
            Some(Cell::Int(word)) => Some(*word),
            Some(_) => Some(0),
            None => None,
        }
    }

    /// The bytes row `row` holds in `col`, empty where it holds none.
    fn bytes_of(&self, col: usize, row: u64) -> Option<&[u8]> {
        match self.get(col, row) {
            Some(Cell::Str(bytes)) => Some(bytes),
            Some(_) => Some(&[]),
            None => None,
        }
    }

    /// The last row whose `col` holds `word`, `None` when none of them
    /// does.
    ///
    /// This is the key index for the rows nothing has folded yet. A
    /// key index is built by a fold and a deferred commit runs none,
    /// so the rows in here are in no index, and what the fold would
    /// have put in one is the value each of them holds in the table's
    /// `id` column.
    ///
    /// The last rather than the first, because a run of deferred
    /// commits can hold one key twice: a row takes a key, goes away,
    /// and a row after it takes the key the first one left free. Only
    /// one of them is a row that is still there, and it is the last,
    /// because a key is only free to be taken again once the row
    /// holding it has gone and a row that has gone does not come back.
    ///
    /// The sealed chunks answer out of the index each of them built as
    /// it was sealed, and the chunk being filled is walked, so what a
    /// lookup costs is a probe per sealed chunk and a walk of at most
    /// [`PATCH_CHUNK`] rows. Asked about any other column it walks the
    /// whole run, which is what it used to do for every column and is
    /// what the fold's own index is for.
    pub fn row_with(&self, col: usize, word: u64) -> Option<u64> {
        if self.key != Some(col) {
            let mut found = None;
            for (at, cells) in self.all().enumerate() {
                if matches!(cells.get(col), Some(Cell::Int(held)) if *held == word) {
                    found = Some(self.base + at as u64);
                }
            }
            return found;
        }
        let first = self.base + (self.chunks.len() * PATCH_CHUNK) as u64;
        let tail = self
            .tail
            .iter()
            .rposition(|cells| matches!(cells.get(col), Some(Cell::Int(held)) if *held == word))
            .map(|at| first + at as u64);
        if tail.is_some() {
            return tail;
        }
        self.keyed
            .iter()
            .rev()
            .find_map(|index| index.get(&word))
            .copied()
    }

    /// The rows of this patch that fall in `lo..hi`, which is what a
    /// read of that range of the column has to put on the end of what
    /// it read.
    fn span(&self, lo: u64, hi: u64) -> std::ops::Range<u64> {
        let start = lo.max(self.base);
        let end = hi.min(self.base + self.len as u64);
        start..end.max(start)
    }

    /// `bounds` widened to take in what the rows of `lo..hi` hold in
    /// `col`. A zone says a scan may skip a chunk, and a row the column
    /// does not hold yet is still a row of the table, so a value of it
    /// outside the stored bounds has to move them.
    fn widen(&self, col: usize, lo: u64, hi: u64, bounds: (u64, u64)) -> (u64, u64) {
        let (mut min, mut max) = bounds;
        for row in self.span(lo, hi) {
            let Some(word) = self.word(col, row) else {
                continue;
            };
            min = min.min(word);
            max = max.max(word);
        }
        (min, max)
    }
}

/// Point reads over one table's property columns, keeping decoded
/// chunks between reads the way the key reader does: integer columns
/// through the segment chunk cache, string columns as the last decoded
/// FullZip chunk per column, so a scan or a hot row set stays on slice
/// copies instead of a chunk decode per read.
#[derive(Debug)]
pub struct PropsReader {
    directory: PropsDirectory,
    int_state: BTreeMap<usize, (Arc<ChunkDirectory>, ChunkCache)>,
    str_state: BTreeMap<usize, StrChunk>,
    /// The same cache again for the validity words of the columns that
    /// have any. It is a second map rather than a second entry in the
    /// first because most columns have no validity segment and the ones
    /// that do are read through a different index, a word per 64 rows.
    valid_state: BTreeMap<usize, (Arc<ChunkDirectory>, ChunkCache)>,
    /// And again for the zone plane of the zoned columns. A third map
    /// for the same reason the second one is one: almost no column has
    /// a plane here, and the read that wants it is materialising a
    /// value rather than scanning, so it does not share the cache the
    /// scan is using.
    zone_state: BTreeMap<usize, (Arc<ChunkDirectory>, ChunkCache)>,
    /// The same again for the label bitset, which is one word per row
    /// rather than one per 64 and belongs to no column.
    label_state: Option<(Arc<ChunkDirectory>, ChunkCache)>,
    /// Row order scratch for the gathers, reused across calls.
    order_scratch: Vec<u32>,
    /// Committed cells the columns below do not hold yet. Shared,
    /// because a query hands the same patch to every worker it forks.
    patch: Option<Arc<CellPatch>>,
    /// Committed rows they do not hold yet, the same way.
    added: Option<Arc<RowPatch>>,
    /// Committed label words the bitset does not hold yet, again the
    /// same way.
    marks: Option<Arc<LabelPatch>>,
}

impl PropsReader {
    pub fn new(directory: PropsDirectory) -> Self {
        Self {
            directory,
            int_state: BTreeMap::new(),
            str_state: BTreeMap::new(),
            valid_state: BTreeMap::new(),
            zone_state: BTreeMap::new(),
            label_state: None,
            order_scratch: Vec::new(),
            patch: None,
            added: None,
            marks: None,
        }
    }

    /// Hands this reader the committed cells its columns do not hold
    /// yet, or takes them away when a fold has sealed them.
    pub fn set_patch(&mut self, patch: Option<Arc<CellPatch>>) {
        self.patch = patch.filter(|p| !p.is_empty());
    }

    /// The same for the committed rows past the end of them.
    pub fn set_added(&mut self, added: Option<Arc<RowPatch>>) {
        self.added = added.filter(|p| !p.is_empty());
    }

    /// And for the labels the bitset does not carry yet.
    pub fn set_marks(&mut self, marks: Option<Arc<LabelPatch>>) {
        self.marks = marks.filter(|p| !p.is_empty());
    }

    /// Whether row `row` of `col` holds a value.
    ///
    /// A column with no validity segment holds one in every row, which
    /// is the answer without a read, so a graph that stores no null
    /// pays nothing for the question.
    pub fn is_valid(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<bool> {
        if let Some(cell) = self.added.as_ref().and_then(|p| p.get(col, row)) {
            return Ok(!matches!(cell, Cell::Null));
        }
        let Some(meta) = self.directory.columns[col].validity.clone() else {
            return Ok(true);
        };
        if let std::collections::btree_map::Entry::Vacant(slot) = self.valid_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, &meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.valid_state.get_mut(&col).expect("just inserted");
        let word = read_one_cached(db, &meta, dir, cache, row / 64)?;
        Ok(word & (1u64 << (row % 64)) != 0)
    }

    /// The label bitset of row `row`, `None` when the table stores
    /// none, which is a table whose rows carry its name and nothing
    /// else. The bits are dictionary positions, so a caller tests a
    /// pattern's mask against the word with one AND.
    pub fn label_word(&mut self, db: &mut Zu1File, row: u64) -> Result<Option<u64>> {
        // The patch is asked first and answers whole: what a commit
        // left on a row is the labels it carries, bitset or no bitset,
        // because the writer composed it against what the row had.
        if let Some(word) = self.marks.as_ref().and_then(|marks| marks.get(row)) {
            return Ok(Some(word));
        }
        let Some(meta) = self.directory.labels.clone() else {
            return Ok(None);
        };
        // A row the bitset does not reach carries its table's own label
        // and nothing else, which is what the caller answers when there
        // is no word here.
        if row >= self.stored() {
            return Ok(None);
        }
        if self.label_state.is_none() {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, &meta)?;
            self.label_state = Some((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.label_state.as_mut().expect("just inserted");
        Ok(Some(read_one_cached(db, &meta, dir, cache, row)?))
    }

    /// Whether the table has a label bitset at all, which is what says
    /// whether a scan has a word to read per row. A commit that has not
    /// folded counts: its words are labels the rows carry, and the
    /// first of them on a table that stored none is the reason the
    /// bitset is about to exist.
    pub fn has_labels(&self) -> bool {
        self.directory.labels.is_some() || self.marks.is_some()
    }

    /// Whether `col` has a null in it anywhere, which is what says
    /// whether a reader has to ask [`Self::is_valid`] at all.
    pub fn is_nullable(&self, col: usize) -> bool {
        self.directory.columns[col].validity.is_some()
    }

    pub fn columns(&self) -> &[PropColumn] {
        &self.directory.columns
    }

    /// How to read a row of list column `col`, which is what
    /// [`list_elements`] wants beside the bytes.
    pub fn list_rows(&self, col: usize) -> ListRows {
        self.directory.columns[col].list_rows()
    }

    /// Rows in the table's domain; every column is row-aligned to it.
    ///
    /// The rows a commit appended are in here as well, because they are
    /// rows of the table and a scan that stopped short of them would
    /// answer a query without them. What tells the two apart is
    /// [`Self::stored`], which is where the columns end.
    pub fn rows(&self) -> u64 {
        self.stored() + self.added.as_ref().map_or(0, |p| p.len() as u64)
    }

    /// Rows the columns themselves hold, which is where a read stops
    /// going to the file and starts reading the patch.
    fn stored(&self) -> u64 {
        self.directory.node_count
    }

    pub fn col(&self, name: &str) -> Option<usize> {
        self.directory.columns.iter().position(|c| c.name == name)
    }

    /// The segment meta of `col`: value count and the segment-level
    /// zone bounds, what a scan needs to size itself and to skip the
    /// whole column without reading a block.
    pub fn meta(&self, col: usize) -> &SegmentMeta {
        &self.directory.columns[col].meta
    }

    /// The value bounds of `chunk` in `col`, `None` when the column
    /// was not written sorted and only the segment-level zone applies.
    pub fn chunk_bounds(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        chunk: usize,
    ) -> Result<Option<(u64, u64)>> {
        let dir = self.int_dir(db, col)?;
        let meta = &self.directory.columns[col].meta;
        let Some(mut bounds) = chunk_zone(meta, &dir, chunk) else {
            return Ok(None);
        };
        let lo = (chunk * CHUNK_ROWS) as u64;
        let hi = lo + CHUNK_ROWS as u64;
        if let Some(patch) = &self.patch {
            bounds = patch.widen(col, lo, hi, bounds);
        }
        if let Some(added) = &self.added {
            bounds = added.widen(col, lo, hi, bounds);
        }
        Ok(Some(bounds))
    }

    /// The value bounds of the whole of `col`, which is what says
    /// whether a scan has to look at the column at all. `None` for an
    /// empty column, which bounds nothing.
    ///
    /// This is [`Self::meta`] with the unsealed words folded in, and
    /// the reason to ask it rather than read the bounds off the meta is
    /// that a write the columns do not hold yet can sit outside them.
    pub fn zone(&self, col: usize) -> Option<(u64, u64)> {
        let meta = &self.directory.columns[col].meta;
        if meta.value_count == 0 {
            return None;
        }
        let mut bounds = (meta.min, meta.max);
        if let Some(patch) = &self.patch {
            bounds = patch.widen(col, 0, u64::MAX, bounds);
        }
        if let Some(added) = &self.added {
            bounds = added.widen(col, 0, u64::MAX, bounds);
        }
        Some(bounds)
    }

    /// The chunk directory of an integer column, loaded through the
    /// shared pool once and held on this reader after. A scan calls
    /// per chunk, and the pool lock is shared across every worker, so
    /// the reader-local copy keeps the hot loop off it.
    fn int_dir(&mut self, db: &mut Zu1File, col: usize) -> Result<Arc<ChunkDirectory>> {
        if let std::collections::btree_map::Entry::Vacant(slot) = self.int_state.entry(col) {
            let meta = &self.directory.columns[col].meta;
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        Ok(Arc::clone(&self.int_state[&col].0))
    }

    /// Decodes `chunk` of an integer column into `out`, the scan unit
    /// read: one chunk into the caller's reusable buffer, nothing held.
    pub fn scan_int_chunk(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        chunk: usize,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let column = &self.directory.columns[col];
        if !column.is_lane() {
            return Err(not_lane(column));
        }
        let base = (chunk * CHUNK_ROWS) as u64;
        let stored = self.stored();
        // The chunk as the column holds it, cut to the rows it really
        // has: a decode hands back a full chunk whatever the last one
        // is filled to, and the appended rows go where the filling
        // stops. A chunk past the end of the column has none of it and
        // is all appended rows.
        match base < stored {
            true => {
                let dir = self.int_dir(db, col)?;
                let meta = &self.directory.columns[col].meta;
                decode_chunk(db, meta, &dir, chunk, out)?;
                out.resize(((stored - base).min(CHUNK_ROWS as u64)) as usize, 0);
            }
            false => out.clear(),
        }
        if let Some(patch) = &self.patch {
            for (row, value) in patch.span(col, base, base + out.len() as u64) {
                out[(row - base) as usize] = value;
            }
        }
        if let Some(added) = &self.added {
            for row in added.span(base, base + CHUNK_ROWS as u64) {
                out.push(added.word(col, row).unwrap_or(0));
            }
        }
        Ok(())
    }

    /// Decodes the whole of an integer column into `out`, chunk after
    /// chunk through one reusable buffer.
    ///
    /// This is for a caller that wants every value at once and in
    /// order, which a whole-graph kernel does: an edge weight is read
    /// in whatever order the frontier settles, so a lazy chunk read
    /// would revisit chunks and a gather would want an index vector as
    /// big as the answer. Nothing is cached on the reader, so the eight
    /// bytes an edge stay the caller's to drop.
    pub fn read_int_column(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let column = &self.directory.columns[col];
        if !column.is_lane() {
            return Err(not_lane(column));
        }
        let chunks = column.meta.chunk_count();
        let rows = column.meta.value_count as usize;
        out.clear();
        out.reserve(rows);
        let mut buf = Vec::new();
        for chunk in 0..chunks {
            self.scan_int_chunk(db, col, chunk, &mut buf)?;
            out.extend_from_slice(&buf);
        }
        out.truncate(rows);
        // The unfolded rows are past the end of the column, so they go
        // on the end here, which is the order their ordinals are in.
        if let Some(added) = &self.added {
            for row in 0..added.len() as u64 {
                out.push(added.word(col, added.base() + row).unwrap_or(0));
            }
        }
        Ok(())
    }

    /// Reads the string values of rows `start..end` of `col`, the scan
    /// unit for string columns: values concatenated in `bytes`, row
    /// `i - start` ending at `ends[i - start]`.
    pub fn scan_str_range(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        start: u64,
        end: u64,
        bytes: &mut Vec<u8>,
        ends: &mut Vec<u64>,
    ) -> Result<()> {
        let column = &self.directory.columns[col];
        if column.is_lane() {
            return Err(not_blob(column));
        }
        bytes.clear();
        ends.clear();
        let stored = self.stored();
        if start < end.min(stored) {
            read_rows_range(db, &column.meta, start, end.min(stored), bytes, ends)?;
        }
        self.overwrite(col, start, bytes, ends);
        // The appended rows come after the stored ones, which is the
        // order their offsets are in, so they go on the end of the
        // buffer the same way.
        if let Some(added) = &self.added {
            for row in added.span(start, end) {
                bytes.extend_from_slice(added.bytes_of(col, row).unwrap_or(&[]));
                ends.push(bytes.len() as u64);
            }
        }
        Ok(())
    }

    /// Puts the unsealed strings of `col` over the range just read into
    /// `bytes` and `ends`, which starts at row `start`.
    ///
    /// A blob range is one buffer with the values end to end, so a
    /// value that changed length moves every value behind it and there
    /// is nothing to write in place. The range is rebuilt instead, and
    /// only when the patch has something inside it, which is what keeps
    /// a scan of a column no commit has touched exactly as it was.
    fn overwrite(&self, col: usize, start: u64, bytes: &mut Vec<u8>, ends: &mut Vec<u64>) {
        let Some(patch) = &self.patch else {
            return;
        };
        let mut written = patch
            .str_span(col, start, start + ends.len() as u64)
            .peekable();
        if written.peek().is_none() {
            return;
        }
        let mut fresh = Vec::with_capacity(bytes.len());
        let mut moved = Vec::with_capacity(ends.len());
        let mut lo = 0usize;
        for (i, &hi) in ends.iter().enumerate() {
            match written.next_if(|(row, _)| *row == start + i as u64) {
                Some((_, new)) => fresh.extend_from_slice(new),
                None => fresh.extend_from_slice(&bytes[lo..hi as usize]),
            }
            moved.push(fresh.len() as u64);
            lo = hi as usize;
        }
        *bytes = fresh;
        *ends = moved;
    }

    pub fn read_int(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<u64> {
        if let Some(word) = self.added.as_ref().and_then(|p| p.word(col, row)) {
            return Ok(word);
        }
        let meta = &self.directory.columns[col].meta;
        if let std::collections::btree_map::Entry::Vacant(slot) = self.int_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.int_state.get_mut(&col).expect("just inserted");
        let value = read_one_cached(db, meta, dir, cache, row)?;
        Ok(match &self.patch {
            Some(patch) => patch.get(col, row).unwrap_or(value),
            None => value,
        })
    }

    /// The whole of row `row` of a zoned column: the instant in the
    /// lane, and the offset from UTC in minutes the value was written
    /// with, from the plane beside it.
    ///
    /// Every other read of a zoned column answers the instant, which is
    /// what a comparison, a sort and a range all want and is what the
    /// lane holds. This is the read that materialises a value, and it
    /// is the only one that touches the second plane, so a scan that
    /// filters on a timestamp and returns no timestamp never pays for
    /// the zones at all.
    pub fn read_zoned(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<(i64, i16)> {
        let nanos = self.read_int(db, col, row)? as i64;
        let column = &self.directory.columns[col];
        let Some(meta) = column.zones.clone() else {
            return Err(ZuError::InvalidArgument(format!(
                "column '{}' holds {} and is not a zoned column",
                column.name, column.ty
            )));
        };
        // A row a commit left behind carries its instant in the patch
        // and has no zone there to carry, because a patch cell is one
        // word. Such a row is UTC, which is the offset a value written
        // without one already has, and the statement path that would
        // write another is not here yet.
        if self
            .added
            .as_ref()
            .is_some_and(|p| p.word(col, row).is_some())
            || self
                .patch
                .as_ref()
                .is_some_and(|p| p.get(col, row).is_some())
        {
            return Ok((nanos, 0));
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = self.zone_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, &meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.zone_state.get_mut(&col).expect("just inserted");
        let word = read_one_cached(db, &meta, dir, cache, row)?;
        Ok((nanos, word as i64 as i16))
    }

    /// Gathers `col` for arbitrary `rows`, writing `out[i]` for
    /// `rows[i]`, the batched read of perf/04 section 5: rows sort by
    /// position, runs sharing a chunk decode it once, and values
    /// scatter back to the caller's order.
    ///
    /// The decode goes through the same chunk cache the point read
    /// keeps, which is what makes a small gather cheap. An expand hands
    /// this a handful of rows per call and calls it once per source
    /// row, so a per-call decode meant decoding a whole chunk to read
    /// one value out of it, and a pipeline reading a property off an
    /// expanded level ran slower than the row at a time engine it
    /// replaces. Warm, a gather is a binary search and a slice copy.
    /// The cache holds every chunk it touches, the same bound the point
    /// read carries, and eviction stays the buffer manager's job
    /// (docs/09, M3).
    pub fn gather_int(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        rows: &[u64],
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let meta = &self.directory.columns[col].meta;
        if !self.directory.columns[col].is_lane() {
            return Err(not_lane(&self.directory.columns[col]));
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = self.int_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.int_state.get_mut(&col).expect("just inserted");
        let order = &mut self.order_scratch;
        order.clear();
        order.extend(0..rows.len() as u32);
        order.sort_unstable_by_key(|&i| rows[i as usize]);
        out.clear();
        out.resize(rows.len(), 0);
        let mut i = 0;
        while i < order.len() {
            let row = rows[order[i] as usize];
            if row >= meta.value_count {
                // Sorted by row, so everything left is past the column
                // too, and the unfolded rows are read as a tail below.
                break;
            }
            let chunk = (row / CHUNK_ROWS as u64) as usize;
            let values = cached_chunk(db, meta, dir, cache, chunk)?;
            while i < order.len() {
                let r = rows[order[i] as usize];
                // A row past the column can share a chunk with the last
                // one in it, so the tail is cut here as well as above.
                if r >= meta.value_count || r / CHUNK_ROWS as u64 != chunk as u64 {
                    break;
                }
                out[order[i] as usize] = values[(r % CHUNK_ROWS as u64) as usize];
                i += 1;
            }
        }
        while i < order.len() {
            let at = order[i] as usize;
            let row = rows[at];
            out[at] = self
                .added
                .as_ref()
                .and_then(|p| p.word(col, row))
                .ok_or_else(|| {
                    ZuError::InvalidArgument(format!(
                        "row {row} out of 0..{}",
                        self.directory.columns[col].meta.value_count
                    ))
                })?;
            i += 1;
        }
        // The unsealed words go over the gathered ones at the end
        // rather than inside the walk above, because the walk is sorted
        // by chunk and this is a lookup per row: doing it here keeps
        // the whole of it off a column no commit has written into.
        if let Some(patch) = &self.patch
            && patch.holds(col)
        {
            for (at, &row) in rows.iter().enumerate() {
                if let Some(value) = patch.get(col, row) {
                    out[at] = value;
                }
            }
        }
        Ok(())
    }

    /// Gathers a string column for arbitrary `rows`: the values land
    /// concatenated in `out_bytes` in row-argument order, value `i`
    /// ending at `out_ends[i]`. Chunk runs decode once, like
    /// [`Self::gather_int`]; the scatter goes through a span table so
    /// the output order is the caller's even though decoding walks the
    /// rows sorted.
    pub fn gather_str(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        rows: &[u64],
        out_bytes: &mut Vec<u8>,
        out_ends: &mut Vec<u64>,
    ) -> Result<()> {
        let meta = &self.directory.columns[col].meta;
        if self.directory.columns[col].is_lane() {
            return Err(not_blob(&self.directory.columns[col]));
        }
        let order = &mut self.order_scratch;
        order.clear();
        order.extend(0..rows.len() as u32);
        order.sort_unstable_by_key(|&i| rows[i as usize]);
        let mut staged = Vec::new();
        let mut spans = vec![(0usize, 0usize); rows.len()];
        let mut chunk_bytes = Vec::new();
        let mut chunk_ends: Vec<u64> = Vec::new();
        let mut cur_chunk = u64::MAX;
        let mut chunk_start = 0u64;
        for &ix in order.iter() {
            let row = rows[ix as usize];
            if row >= meta.value_count {
                // Past the column, so the bytes come from the rows no
                // fold has written into it yet.
                let bytes = self
                    .added
                    .as_ref()
                    .and_then(|p| p.bytes_of(col, row))
                    .ok_or_else(|| {
                        ZuError::InvalidArgument(format!(
                            "row {row} out of 0..{}",
                            meta.value_count
                        ))
                    })?;
                spans[ix as usize] = (staged.len(), staged.len() + bytes.len());
                staged.extend_from_slice(bytes);
                continue;
            }
            // A row the column holds but a commit has written over
            // since, taken here rather than after the walk the way the
            // words are: the walk stages the bytes it decodes, so
            // going over them afterwards would mean moving the spans
            // of every row behind this one.
            if let Some(bytes) = self.patch.as_ref().and_then(|p| p.bytes_of(col, row)) {
                spans[ix as usize] = (staged.len(), staged.len() + bytes.len());
                staged.extend_from_slice(bytes);
                continue;
            }
            let chunk = row / CHUNK_ROWS as u64;
            if chunk != cur_chunk {
                chunk_start = chunk * CHUNK_ROWS as u64;
                let end = meta.value_count.min(chunk_start + CHUNK_ROWS as u64);
                chunk_bytes.clear();
                chunk_ends.clear();
                read_rows_range(
                    db,
                    meta,
                    chunk_start,
                    end,
                    &mut chunk_bytes,
                    &mut chunk_ends,
                )?;
                cur_chunk = chunk;
            }
            let local = (row - chunk_start) as usize;
            let lo = if local == 0 {
                0
            } else {
                chunk_ends[local - 1] as usize
            };
            let hi = chunk_ends[local] as usize;
            spans[ix as usize] = (staged.len(), staged.len() + (hi - lo));
            staged.extend_from_slice(&chunk_bytes[lo..hi]);
        }
        out_bytes.clear();
        out_ends.clear();
        for &(lo, hi) in &spans {
            out_bytes.extend_from_slice(&staged[lo..hi]);
            out_ends.push(out_bytes.len() as u64);
        }
        Ok(())
    }

    pub fn read_str(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        row: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if let Some(bytes) = self.added.as_ref().and_then(|p| p.bytes_of(col, row)) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        let meta = &self.directory.columns[col].meta;
        if row >= meta.value_count {
            return Err(ZuError::InvalidArgument(format!(
                "row {row} out of 0..{}",
                meta.value_count
            )));
        }
        if let Some(bytes) = self.patch.as_ref().and_then(|p| p.bytes_of(col, row)) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        let chunk = row / CHUNK_ROWS as u64;
        if !self.str_state.get(&col).is_some_and(|c| c.chunk == chunk) {
            let start = chunk * CHUNK_ROWS as u64;
            let end = meta.value_count.min(start + CHUNK_ROWS as u64);
            let mut fresh = StrChunk {
                chunk,
                bytes: Vec::new(),
                ends: Vec::new(),
            };
            read_rows_range(db, meta, start, end, &mut fresh.bytes, &mut fresh.ends)?;
            self.str_state.insert(col, fresh);
        }
        let c = self.str_state.get(&col).expect("just decoded");
        let i = (row % CHUNK_ROWS as u64) as usize;
        let start = if i == 0 { 0 } else { c.ends[i - 1] as usize };
        out.extend_from_slice(&c.bytes[start..c.ends[i] as usize]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::bulk_load_keyed;

    fn setup(dir: &std::path::Path) -> Zu1File {
        let mut db = Zu1File::create(&dir.join("props.zu1")).unwrap();
        bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None).unwrap();
        db
    }

    #[test]
    fn roundtrip_and_point_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let names: Vec<&[u8]> = vec![b"Ada", b"Grace", b"Edsger", b"Barbara"];
        let ids = [14u64, 16, 32, 4398046517420];
        let stored = store_props(
            &mut db,
            "person",
            &[
                ("firstName", PropValues::Str(&names)),
                ("id", PropValues::Int(&ids)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let loaded = load_props(&mut db, table).unwrap().unwrap();
        assert_eq!(loaded, stored);
        let mut reader = PropsReader::new(loaded);
        let mut out = Vec::new();
        for (row, name) in names.iter().enumerate() {
            out.clear();
            let col = reader.col("firstName").unwrap();
            reader.read_str(&mut db, col, row as u64, &mut out).unwrap();
            assert_eq!(&out, name);
            let col = reader.col("id").unwrap();
            assert_eq!(reader.read_int(&mut db, col, row as u64).unwrap(), ids[row]);
        }
        assert!(reader.col("lastName").is_none());
    }

    #[test]
    fn gathers_batch_across_chunks_in_caller_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("gather.zu1")).unwrap();
        let n = 3000u64;
        crate::graph::bulk_load_as(&mut db, "person", "knows", n, &[(0, 1)]).unwrap();
        let ints: Vec<u64> = (0..n).map(|i| i * 7).collect();
        let strs: Vec<Vec<u8>> = (0..n).map(|i| format!("v{i}").into_bytes()).collect();
        let str_refs: Vec<&[u8]> = strs.iter().map(|v| v.as_slice()).collect();
        store_props(
            &mut db,
            "person",
            &[
                ("i", PropValues::Int(&ints)),
                ("s", PropValues::Str(&str_refs)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        // Unsorted rows with chunk revisits and a duplicate, so the
        // scatter back to argument order is what the assertions see.
        let rows = [2999u64, 0, 1024, 1023, 512, 2048, 0];
        let icol = reader.col("i").unwrap();
        let mut out = Vec::new();
        reader.gather_int(&mut db, icol, &rows, &mut out).unwrap();
        let want: Vec<u64> = rows.iter().map(|&r| r * 7).collect();
        assert_eq!(out, want);
        let scol = reader.col("s").unwrap();
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        reader
            .gather_str(&mut db, scol, &rows, &mut bytes, &mut ends)
            .unwrap();
        let mut lo = 0usize;
        for (i, &r) in rows.iter().enumerate() {
            let hi = ends[i] as usize;
            assert_eq!(&bytes[lo..hi], format!("v{r}").as_bytes());
            lo = hi;
        }
        // Out-of-range rows fail loud on both types.
        assert!(reader.gather_int(&mut db, icol, &[n], &mut out).is_err());
        assert!(
            reader
                .gather_str(&mut db, scol, &[n], &mut bytes, &mut ends)
                .is_err()
        );
        // And so does gathering across the type divide.
        assert!(reader.gather_int(&mut db, scol, &rows, &mut out).is_err());
    }

    /// Rows appended past the end of a column are read out of the
    /// patch by every path that reads the column: the point reads, the
    /// batched gathers, and the whole-column read a scan takes.
    #[test]
    fn appended_rows_are_read_by_every_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        let (age, name) = (reader.col("age").unwrap(), reader.col("name").unwrap());

        let mut patch = RowPatch::new(4, Some(age));
        assert_eq!(
            patch.push(vec![Cell::Int(50), Cell::Str(b"eva".to_vec())]),
            4
        );
        assert_eq!(patch.push(vec![Cell::Int(60), Cell::Null]), 5);
        reader.set_added(Some(Arc::new(patch)));

        assert_eq!(reader.read_int(&mut db, age, 4).unwrap(), 50);
        let mut bytes = Vec::new();
        reader.read_str(&mut db, name, 4, &mut bytes).unwrap();
        assert_eq!(bytes, b"eva");
        assert!(reader.is_valid(&mut db, name, 4).unwrap());
        assert!(!reader.is_valid(&mut db, name, 5).unwrap());

        // Gathers take rows in caller order and cross the end of the
        // column in the middle of the batch.
        let rows = [5u64, 1, 4, 0];
        let mut out = Vec::new();
        reader.gather_int(&mut db, age, &rows, &mut out).unwrap();
        assert_eq!(out, [60, 20, 50, 10]);
        let mut ends = Vec::new();
        bytes.clear();
        reader
            .gather_str(&mut db, name, &[4, 2], &mut bytes, &mut ends)
            .unwrap();
        assert_eq!(ends, [3, 6]);
        assert_eq!(&bytes[..], b"evajoe");

        // The whole column is the stored rows with the appended ones
        // after them, in the order they arrived.
        let mut column = Vec::new();
        reader.read_int_column(&mut db, age, &mut column).unwrap();
        assert_eq!(column, [10, 20, 30, 40, 50, 60]);

        // A row past the patch is still out of range.
        assert!(reader.gather_int(&mut db, age, &[6], &mut out).is_err());
    }

    /// A key lookup over the appended rows answers the same whether the
    /// row it wants is in a sealed chunk or in the one being filled,
    /// and the same as walking every row would have.
    #[test]
    fn appended_rows_answer_a_key_lookup_from_any_chunk() {
        // Past two seals and a bit, so the answer comes out of the
        // index for some of these and out of the walk for the rest.
        let rows = PATCH_CHUNK * 2 + 5;
        let mut patch = RowPatch::new(100, Some(0));
        for at in 0..rows {
            patch.push(vec![Cell::Int(1000 + at as u64), Cell::Int(7)]);
        }

        for at in 0..rows {
            assert_eq!(
                patch.row_with(0, 1000 + at as u64),
                Some(100 + at as u64),
                "row {at} of {rows}"
            );
        }
        assert_eq!(patch.row_with(0, 999), None);
        assert_eq!(patch.row_with(0, 1000 + rows as u64), None);

        // A column that is not the key one is walked, and a word every
        // row holds answers with the last of them either way.
        assert_eq!(patch.row_with(1, 7), Some(100 + rows as u64 - 1));
        assert_eq!(patch.row_with(1, 8), None);
    }

    /// A write over an appended row lands wherever the row is, in a
    /// sealed chunk or in the one being filled, and leaves the copy a
    /// reader was handed alone.
    #[test]
    fn a_write_over_an_appended_row_leaves_an_older_copy_alone() {
        let rows = PATCH_CHUNK * 2 + 5;
        let mut patch = RowPatch::new(100, Some(0));
        for at in 0..rows {
            patch.push(vec![
                Cell::Int(1000 + at as u64),
                Cell::Int(7),
                Cell::Str(b"ab".to_vec()),
            ]);
        }
        assert_eq!(patch.bytes(), rows * 2);

        // What a reader opened at this commit is holding, which nothing
        // written after it may move.
        let held = patch.clone();

        for at in 0..rows {
            let row = 100 + at as u64;
            assert!(patch.set(1, row, Cell::Int(at as u64)), "row {at}");
            assert!(patch.set(2, row, Cell::Str(b"long".to_vec())), "row {at}");
        }
        // Two bytes a row given up and four taken, counted rather than
        // walked, so the count has to come out where a walk would.
        assert_eq!(patch.bytes(), rows * 4);

        for at in 0..rows {
            let row = 100 + at as u64;
            assert_eq!(patch.word(1, row), Some(at as u64), "row {at}");
            assert_eq!(patch.bytes_of(2, row), Some(&b"long"[..]), "row {at}");
            assert_eq!(held.word(1, row), Some(7), "row {at} of the older copy");
            assert_eq!(held.bytes_of(2, row), Some(&b"ab"[..]), "row {at}");
        }
        assert_eq!(held.bytes(), rows * 2);

        // The key column is refused, because the sealed chunks carry an
        // index of it that older copies are still reading out of.
        assert!(!patch.set(0, 100, Cell::Int(9)));
        assert_eq!(patch.word(0, 100), Some(1000));
        // So are a row the patch does not hold, on either side of it,
        // and a column the rows do not have.
        assert!(!patch.set(1, 99, Cell::Int(9)));
        assert!(!patch.set(1, 100 + rows as u64, Cell::Int(9)));
        assert!(!patch.set(3, 100, Cell::Int(9)));
        // And what it refuses is what it says it would refuse.
        assert!(!patch.settable(0, 100));
        assert!(!patch.settable(1, 99));
        assert!(patch.settable(1, 100));
    }

    /// Two rows under one key answer with the second, which is the only
    /// one that can still be there, whichever side of a seal the pair
    /// falls.
    #[test]
    fn a_repeated_key_answers_with_the_row_that_took_it_last() {
        let mut words: Vec<u64> = (0..PATCH_CHUNK * 2 + 3)
            .map(|at| 1000 + at as u64)
            .collect();
        // A pair inside one sealed chunk, a pair either side of a seal,
        // and a pair with the second of them in the chunk being filled.
        words[1] = words[0];
        words[PATCH_CHUNK] = words[PATCH_CHUNK - 1];
        words[PATCH_CHUNK * 2 + 1] = words[PATCH_CHUNK + 2];

        let mut patch = RowPatch::new(0, Some(0));
        for word in &words {
            patch.push(vec![Cell::Int(*word)]);
        }
        assert_eq!(patch.row_with(0, words[0]), Some(1));
        assert_eq!(
            patch.row_with(0, words[PATCH_CHUNK]),
            Some(PATCH_CHUNK as u64)
        );
        assert_eq!(
            patch.row_with(0, words[PATCH_CHUNK * 2 + 1]),
            Some(PATCH_CHUNK as u64 * 2 + 1)
        );
    }

    /// Strings written over rows the column already holds are read the
    /// same way, by the same three paths, and the lengths change under
    /// them: a blob range is one buffer of values end to end, so a
    /// value that grew moves every value behind it.
    #[test]
    fn written_strings_are_read_by_every_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(&mut db, "person", &[("name", PropValues::Str(&names))]).unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        let name = reader.col("name").unwrap();

        // One longer than what it goes over and one shorter, so neither
        // direction of the shift is the untested one.
        let mut patch = CellPatch::default();
        patch.set_bytes(name, 1, b"katherine".to_vec());
        patch.set_bytes(name, 3, b"al".to_vec());
        reader.set_patch(Some(Arc::new(patch)));

        let mut bytes = Vec::new();
        reader.read_str(&mut db, name, 1, &mut bytes).unwrap();
        assert_eq!(bytes, b"katherine");
        bytes.clear();
        reader.read_str(&mut db, name, 2, &mut bytes).unwrap();
        assert_eq!(bytes, b"joe");

        // A gather takes its rows in caller order, and a written row in
        // the middle of one must not move the rows around it.
        let mut ends = Vec::new();
        bytes.clear();
        reader
            .gather_str(&mut db, name, &[3, 1, 0], &mut bytes, &mut ends)
            .unwrap();
        assert_eq!(ends, [2, 11, 14]);
        assert_eq!(&bytes[..], b"alkatherineada");

        // And the range a scan reads is rebuilt around what it holds.
        bytes.clear();
        ends.clear();
        reader
            .scan_str_range(&mut db, name, 0, 4, &mut bytes, &mut ends)
            .unwrap();
        assert_eq!(ends, [3, 12, 15, 17]);
        assert_eq!(&bytes[..], b"adakatherinejoeal");

        // A range that holds none of them is left exactly as it was.
        bytes.clear();
        ends.clear();
        reader
            .scan_str_range(&mut db, name, 2, 3, &mut bytes, &mut ends)
            .unwrap();
        assert_eq!(ends, [3]);
        assert_eq!(&bytes[..], b"joe");
    }

    #[test]
    fn scan_units_decode_one_chunk_and_expose_zones() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("scan.zu1")).unwrap();
        let n = 3000u64;
        crate::graph::bulk_load_as(&mut db, "person", "knows", n, &[(0, 1)]).unwrap();
        let sorted: Vec<u64> = (0..n).map(|i| i * 7).collect();
        let shuffled: Vec<u64> = (0..n).map(|i| (n - 1 - i) * 7).collect();
        let strs: Vec<Vec<u8>> = (0..n).map(|i| format!("v{i}").into_bytes()).collect();
        let str_refs: Vec<&[u8]> = strs.iter().map(|v| v.as_slice()).collect();
        store_props(
            &mut db,
            "person",
            &[
                ("a", PropValues::Int(&sorted)),
                ("d", PropValues::Int(&shuffled)),
                ("s", PropValues::Str(&str_refs)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        let (a, d, s) = (
            reader.col("a").unwrap(),
            reader.col("d").unwrap(),
            reader.col("s").unwrap(),
        );
        assert!(reader.meta(a).sorted);
        assert!(!reader.meta(d).sorted);
        // Chunk 1 holds rows 1024..2048; its zone brackets those values
        // from the previous fence to its own.
        let (lo, hi) = reader.chunk_bounds(&mut db, a, 1).unwrap().unwrap();
        assert_eq!((lo, hi), (1023 * 7, 2047 * 7));
        // A descending column keeps only the segment-level zone.
        assert!(reader.chunk_bounds(&mut db, d, 1).unwrap().is_none());
        // One chunk decodes alone, the short tail included.
        let mut out = Vec::new();
        reader.scan_int_chunk(&mut db, a, 2, &mut out).unwrap();
        assert_eq!(out.len(), (n - 2048) as usize);
        assert_eq!(out[0], 2048 * 7);
        assert_eq!(*out.last().unwrap(), (n - 1) * 7);
        // A string range spanning a chunk boundary reads contiguously.
        let (mut bytes, mut ends) = (Vec::new(), Vec::new());
        reader
            .scan_str_range(&mut db, s, 1022, 1026, &mut bytes, &mut ends)
            .unwrap();
        assert_eq!(ends.len(), 4);
        let mut lo = 0usize;
        for (i, row) in (1022u64..1026).enumerate() {
            let hi = ends[i] as usize;
            assert_eq!(&bytes[lo..hi], format!("v{row}").as_bytes());
            lo = hi;
        }
        // Type confusion fails loud in both directions.
        assert!(reader.scan_int_chunk(&mut db, s, 0, &mut out).is_err());
        assert!(
            reader
                .scan_str_range(&mut db, a, 0, 4, &mut bytes, &mut ends)
                .is_err()
        );
    }

    #[test]
    fn replacing_props_frees_the_old_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let a: Vec<&[u8]> = vec![b"a"; 4];
        store_props(&mut db, "person", &[("x", PropValues::Str(&a))]).unwrap();
        let before = db.db_header().block_count;
        for _ in 0..8 {
            store_props(&mut db, "person", &[("x", PropValues::Str(&a))]).unwrap();
        }
        // Freed blocks recycle instead of growing the file without
        // bound; one round of slack covers checkpoint staging.
        assert!(
            db.db_header().block_count <= before + 4,
            "blocks grew {} -> {}",
            before,
            db.db_header().block_count
        );
    }

    #[test]
    fn a_node_carries_its_table_label_and_whatever_else_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None).unwrap();
        store_labels(
            &mut db,
            "person",
            &[
                vec![],
                vec!["Employee"],
                vec!["Employee", "Manager"],
                vec!["Manager"],
            ],
        )
        .unwrap();

        let catalog = Catalog::load(&mut db).unwrap();
        // The table's own name is a label, and it is the first one,
        // because a table exists before anything is declared on it.
        assert_eq!(catalog.labels(), ["person", "Employee", "Manager"]);
        let person = catalog.node_by_name("person").unwrap();
        assert_eq!(person.labels, [0, 1, 2]);
        assert_eq!(person.primary_label(), 0);
        assert_eq!(person.label_mask(), 0b111);
        assert_eq!(catalog.tables_with_label(1), [person.id]);
        assert!(catalog.tables_with_label(9).is_empty());

        let table = person.id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        assert!(reader.has_labels());
        let words: Vec<u64> = (0..4)
            .map(|row| reader.label_word(&mut db, row).unwrap().unwrap())
            .collect();
        assert_eq!(words, [0b001, 0b011, 0b111, 0b101]);
        drop(db);
        crate::verify(&path).unwrap();
    }

    #[test]
    fn labels_and_columns_do_not_disturb_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("both.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None).unwrap();
        store_labels(
            &mut db,
            "person",
            &[vec![], vec!["Bot"], vec![], vec!["Bot"]],
        )
        .unwrap();
        // A property store says nothing about labels, so it carries the
        // bitset across to the directory it writes rather than dropping
        // it or rewriting it.
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[10, 20, 30, 40]))],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        assert_eq!(reader.label_word(&mut db, 1).unwrap(), Some(0b11));
        assert_eq!(reader.read_int(&mut db, 0, 1).unwrap(), 20);

        // And the other way round: storing labels again leaves the
        // columns where they are.
        store_labels(&mut db, "person", &[vec!["Bot"], vec![], vec![], vec![]]).unwrap();
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        assert_eq!(reader.label_word(&mut db, 0).unwrap(), Some(0b11));
        assert_eq!(reader.label_word(&mut db, 1).unwrap(), Some(0b01));
        assert_eq!(reader.read_int(&mut db, 0, 3).unwrap(), 40);
        assert_eq!(reader.columns().len(), 1);
        drop(db);
        crate::verify(&path).unwrap();
    }

    #[test]
    fn a_table_without_a_bitset_answers_from_the_catalog_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[1, 2, 3, 4]))],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_props(&mut db, table).unwrap().unwrap());
        // Nothing declared a second label, so there is no word per row
        // to read and the table's own label is the whole answer.
        assert!(!reader.has_labels());
        assert_eq!(reader.label_word(&mut db, 2).unwrap(), None);
        assert_eq!(
            Catalog::load(&mut db)
                .unwrap()
                .node_by_id(table)
                .unwrap()
                .labels,
            [0]
        );
    }

    #[test]
    fn a_label_set_that_does_not_fit_the_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let err = store_labels(&mut db, "person", &[vec!["Bot"], vec![]]).unwrap_err();
        assert!(
            err.to_string().contains("4 rows and the label set holds 2"),
            "{err}"
        );
        let err = store_labels(&mut db, "nobody", &[vec!["Bot"]]).unwrap_err();
        assert!(err.to_string().contains("no node table"), "{err}");
        // A bitset is one word, so the dictionary stops at 64 names and
        // says so rather than writing a bit nothing can read.
        let names: Vec<String> = (0..64).map(|i| format!("L{i}")).collect();
        let rows: Vec<Vec<&str>> = vec![
            names.iter().map(String::as_str).skip(1).collect(),
            vec![],
            vec![],
            vec![],
        ];
        store_labels(&mut db, "person", &rows).unwrap();
        let err = store_labels(
            &mut db,
            "person",
            &[vec!["one_too_many"], vec![], vec![], vec![]],
        )
        .unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
    }

    /// Builds a file whose person table declares `Bot` and then puts
    /// `words` in the bitset behind the writer's back, which is the
    /// only way a file gets a bitset the catalog disagrees with.
    fn file_with_label_words(path: &std::path::Path, words: &[u64]) {
        let mut db = Zu1File::create(path).unwrap();
        bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None).unwrap();
        store_labels(&mut db, "person", &[vec!["Bot"], vec![], vec![], vec![]]).unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut index = TableIndex::load(&mut db).unwrap();
        let root = index.get(table).unwrap();
        let mut directory = load_props_at(&mut db, root).unwrap();
        for &ptr in directory.labels.iter().flat_map(|m| &m.blocks) {
            db.free_block(ptr).unwrap();
        }
        crate::graph::free_chain(&mut db, root).unwrap();
        index.remove(table);
        directory.labels = Some(write_segment(&mut db, words).unwrap());
        index.set(
            table,
            meta::write_chain(&mut db, &directory.encode()).unwrap(),
        );
        let old_index = db.db_header().table_index_root;
        crate::graph::free_chain(&mut db, old_index).unwrap();
        let index_root = meta::write_chain(&mut db, &index.encode()).unwrap();
        db.db_header_mut().table_index_root = index_root;
        db.checkpoint().unwrap();
    }

    #[test]
    fn verify_refuses_a_bitset_the_catalog_does_not_back() {
        let dir = tempfile::tempdir().unwrap();
        // A bit no table declared.
        let path = dir.path().join("undeclared.zu1");
        file_with_label_words(&path, &[0b11, 0b101, 1, 1]);
        let err = crate::verify(&path).unwrap_err().to_string();
        assert!(err.contains("row 1 carries labels 0x5"), "{err}");
        // A row that dropped the label its table gives every row.
        let path = dir.path().join("missing.zu1");
        file_with_label_words(&path, &[0b11, 0, 1, 1]);
        let err = crate::verify(&path).unwrap_err().to_string();
        assert!(err.contains("row 1 carries labels 0x0"), "{err}");
        // A bitset that does not cover the rows.
        let path = dir.path().join("short.zu1");
        file_with_label_words(&path, &[1, 1, 1]);
        let err = crate::verify(&path).unwrap_err().to_string();
        assert!(err.contains("3 words over 4 rows"), "{err}");
    }

    #[test]
    fn edge_columns_are_read_by_the_ordinal_the_csr_gives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relprops.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        // The load order is sorted by source and then by destination,
        // and value `i` of a column belongs to edge `i` of that order.
        let edges = vec![(0u32, 1u32), (0, 2), (0, 3), (1, 2), (2, 3), (3, 0)];
        bulk_load_keyed(&mut db, "person", "knows", 4, &edges, None).unwrap();
        let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2000 + i).collect();
        let tags: Vec<Vec<u8>> = edges
            .iter()
            .map(|(s, d)| format!("{s}->{d}").into_bytes())
            .collect();
        let tag_refs: Vec<&[u8]> = tags.iter().map(|v| v.as_slice()).collect();
        store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&since)),
                ("tag", PropValues::Str(&tag_refs)),
            ],
        )
        .unwrap();

        let rel = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_rel_props(&mut db, rel).unwrap().unwrap());
        let mut graph = crate::graph::GraphReader::load_table(&mut db, "knows").unwrap();
        let (int_col, str_col) = (reader.col("since").unwrap(), reader.col("tag").unwrap());
        let mut out = Vec::new();
        for (i, &(s, d)) in edges.iter().enumerate() {
            let row = graph
                .edge_ordinal(&mut db, u64::from(s), u64::from(d))
                .unwrap()
                .expect("edge is in the graph");
            assert_eq!(row, i as u64, "edge {s}->{d}");
            assert_eq!(reader.read_int(&mut db, int_col, row).unwrap(), since[i]);
            out.clear();
            reader.read_str(&mut db, str_col, row, &mut out).unwrap();
            assert_eq!(out, tags[i]);
        }
        // The columns are the rel table's own, so the node table still
        // has none of its own and neither reads the other's.
        let person = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        assert!(load_props(&mut db, person).unwrap().is_none());
        drop(db);
        crate::verify(&path).unwrap();
    }

    #[test]
    fn replacing_edge_columns_frees_the_old_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let two = [1u64, 2];
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&two))]).unwrap();
        let before = db.db_header().block_count;
        for _ in 0..8 {
            store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&two))]).unwrap();
        }
        assert!(
            db.db_header().block_count <= before + 4,
            "blocks grew {} -> {}",
            before,
            db.db_header().block_count
        );
    }

    #[test]
    fn edge_columns_hold_a_value_for_each_copy_of_a_repeated_pair() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("dup.zu1")).unwrap();
        bulk_load_keyed(
            &mut db,
            "person",
            "knows",
            4,
            &[(0, 1), (0, 1), (2, 3)],
            None,
        )
        .unwrap();
        let three = [1u64, 2, 3];
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&three))]).unwrap();
        let rel = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut reader = PropsReader::new(load_rel_props(&mut db, rel).unwrap().unwrap());
        let col = reader.col("since").unwrap();
        let mut values = Vec::new();
        reader.read_int_column(&mut db, col, &mut values).unwrap();
        assert_eq!(values, three, "one value an edge, copies included");
        // The pair alone reaches the first of the two, which is the
        // ordinal a lookup with nothing else to go on has to answer.
        let mut graph = crate::graph::GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(graph.edge_ordinal(&mut db, 0, 1).unwrap(), Some(0));

        // A column of the wrong length is counted against edges, not nodes.
        let mut db = setup(dir.path());
        let four = [1u64, 2, 3, 4];
        let err =
            store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&four))]).unwrap_err();
        assert!(err.to_string().contains("4 values over 2 rows"), "{err}");
        let err =
            store_rel_props(&mut db, "nobody", &[("since", PropValues::Int(&four))]).unwrap_err();
        assert!(err.to_string().contains("no rel table"), "{err}");
    }

    #[test]
    fn store_rejects_misaligned_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let short: Vec<&[u8]> = vec![b"a"; 3];
        let err = store_props(&mut db, "person", &[("x", PropValues::Str(&short))]).unwrap_err();
        assert!(err.to_string().contains("3 values over 4 rows"), "{err}");
        let four: Vec<&[u8]> = vec![b"a"; 4];
        let err = store_props(&mut db, "nobody", &[("x", PropValues::Str(&four))]).unwrap_err();
        assert!(err.to_string().contains("no node table"), "{err}");
    }

    #[test]
    fn every_lane_type_round_trips_through_the_same_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let flags = [true, false, false, true];
        let scores = [1.5f64, -0.25, f64::MAX, 0.0];
        let days = [0i32, -1, 19_000, i32::MIN];
        let nanos = [0i64, 86_399_999_999_999, -5, i64::MAX];
        let months = [0i64, 13, -13, 240];
        let raw: Vec<&[u8]> = vec![b"\x00\xff", b"", b"\x01", b"\xfe\xed"];
        store_props(
            &mut db,
            "person",
            &[
                ("flag", PropValues::Bool(&flags)),
                ("score", PropValues::Float(&scores)),
                ("born", PropValues::Date(&days)),
                ("at", PropValues::LocalTime(&nanos)),
                (
                    "age",
                    PropValues::Duration(DurationKind::YearMonth, &months),
                ),
                ("blob", PropValues::Bytes(&raw)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let loaded = load_props(&mut db, table).unwrap().unwrap();
        // The type survives the directory, which is the whole point: a
        // word out of the lane means nothing without it.
        assert_eq!(loaded.columns[0].ty, LogicalType::Bool);
        assert_eq!(
            loaded.columns[1].ty,
            LogicalType::Float {
                bits: FloatBits::B64,
                precision: None
            }
        );
        assert_eq!(loaded.columns[2].ty, LogicalType::Date);
        assert_eq!(loaded.columns[3].ty, LogicalType::LocalTime);
        assert_eq!(
            loaded.columns[4].ty,
            LogicalType::Duration(DurationKind::YearMonth)
        );
        assert!(loaded.columns[..5].iter().all(PropColumn::is_lane));
        assert!(!loaded.columns[5].is_lane());
        let mut reader = PropsReader::new(loaded);
        for row in 0..4u64 {
            let mut word = |reader: &mut PropsReader, name: &str| {
                let col = reader.col(name).unwrap();
                reader.read_int(&mut db, col, row).unwrap()
            };
            assert_eq!(word(&mut reader, "flag") != 0, flags[row as usize]);
            assert_eq!(
                f64::from_bits(word(&mut reader, "score")),
                scores[row as usize]
            );
            assert_eq!(
                word(&mut reader, "born") as i64,
                i64::from(days[row as usize])
            );
            assert_eq!(word(&mut reader, "at") as i64, nanos[row as usize]);
            assert_eq!(word(&mut reader, "age") as i64, months[row as usize]);
            let col = reader.col("blob").unwrap();
            let mut out = Vec::new();
            reader.read_str(&mut db, col, row, &mut out).unwrap();
            assert_eq!(out, raw[row as usize]);
        }
        // The two shapes refuse each other's reads, and the message
        // names the type rather than saying "not an integer column".
        let blob = reader.col("blob").unwrap();
        let err = reader
            .gather_int(&mut db, blob, &[0], &mut Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("BYTES"), "{err}");
        let score = reader.col("score").unwrap();
        let err = reader
            .scan_str_range(&mut db, score, 0, 1, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("FLOAT"), "{err}");
    }

    #[test]
    fn a_kept_column_keeps_the_block_the_dropped_one_shared() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("p.zu1")).unwrap();
        let kept: Vec<u64> = (0..40u64).map(|i| i * 3).collect();
        let gone: Vec<u64> = (0..40u64).map(|i| i * 5 + 1).collect();
        let inputs = [
            PropInput::dense("kept", PropValues::Int(&kept)),
            PropInput::dense("gone", PropValues::Int(&gone)),
        ];
        let (root, directory, _) = write_props(&mut db, 40, &inputs, None).unwrap();
        let shared = directory.columns[0].meta.blocks[0];
        assert_eq!(
            directory.columns[1].meta.blocks[0], shared,
            "two tiny columns of one directory land in one block"
        );

        // The fold that rewrote the second column and carried the first
        // one across. The block under both of them is still read, so it
        // is not handed back however the second one goes.
        free_props_reusing(&mut db, root, false, &[true]).unwrap();
        db.checkpoint().unwrap();
        for _ in 0..8 {
            assert_ne!(
                db.allocate_block(),
                shared,
                "the block a kept column sits in was handed back"
            );
        }
        let mut out = Vec::new();
        crate::segment::read_segment(&mut db, &directory.columns[0].meta, &mut out).unwrap();
        assert_eq!(out, kept, "and it still reads what it held");
    }

    #[test]
    fn a_version_5_directory_still_reads_its_columns() {
        // Version 5 is the last one written before payloads could share
        // a block, so its metas carry no start word and its columns each
        // begin where their first block does. A store written then has
        // to keep reading, values and all, not just decode.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("p.zu1")).unwrap();
        let values: Vec<u64> = (0..500u64).map(|i| i * 17 + 3).collect();
        let meta = crate::segment::write_segment(&mut db, &values).unwrap();
        assert_eq!(meta.start, 0, "nothing is packed outside a scope");

        let mut old = Vec::new();
        old.extend_from_slice(&5u16.to_le_bytes());
        old.extend_from_slice(&(values.len() as u64).to_le_bytes());
        old.extend_from_slice(&1u32.to_le_bytes());
        old.extend_from_slice(&2u16.to_le_bytes());
        old.extend_from_slice(b"id");
        old.extend_from_slice(
            &type_bytes(&LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            })
            .unwrap(),
        );
        meta.encode_unpacked(&mut old);
        old.push(0);
        old.push(0);

        let decoded = PropsDirectory::decode(&old).unwrap();
        assert_eq!(decoded.columns.len(), 1);
        assert_eq!(decoded.columns[0].meta.start, 0);
        let mut out = Vec::new();
        crate::segment::read_segment(&mut db, &decoded.columns[0].meta, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn a_version_1_directory_still_reads() {
        // Written by hand in the old layout: one string column and one
        // integer column, type codes 0 and 1, which is every type
        // version 1 could hold.
        let meta = SegmentMeta {
            value_count: 2,
            payload_len: 16,
            uncompressed_bytes: 16,
            min: 1,
            max: 2,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
            start: 0,
            blocks: vec![7],
        };
        let mut old = Vec::new();
        old.extend_from_slice(&1u16.to_le_bytes());
        old.extend_from_slice(&2u64.to_le_bytes());
        old.extend_from_slice(&2u32.to_le_bytes());
        for (name, code) in [("s", 0u8), ("i", 1u8)] {
            old.extend_from_slice(&(name.len() as u16).to_le_bytes());
            old.extend_from_slice(name.as_bytes());
            old.push(code);
            meta.encode_unpacked(&mut old);
        }
        let dir = PropsDirectory::decode(&old).unwrap();
        assert_eq!(
            dir.columns[0].ty,
            LogicalType::Str {
                min: None,
                max: None,
                fixed: false
            }
        );
        assert_eq!(
            dir.columns[1].ty,
            LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None
            }
        );
        // A version 2 code in a version 1 directory is a file that
        // cannot have been written by either version.
        let mut forged = old.clone();
        let at = forged.len() - meta_len(&meta) - 1;
        forged[at] = 3;
        assert!(PropsDirectory::decode(&forged).is_err());
        // Re-encoding lifts it to the current version without moving
        // any bytes of the two columns' meaning.
        let again = PropsDirectory::decode(&dir.encode()).unwrap();
        assert_eq!(again, dir);
        assert_eq!(
            u16::from_le_bytes(dir.encode()[..2].try_into().unwrap()),
            PROPS_VERSION
        );
    }

    /// Encoded length of a segment meta, so the version 1 test can find
    /// the last column's type byte without hardcoding a size.
    fn meta_len(meta: &SegmentMeta) -> usize {
        let mut out = Vec::new();
        meta.encode_unpacked(&mut out);
        out.len()
    }

    /// A list column is stored, read back, and its element type comes
    /// back with it. The element type is the whole point of the second
    /// code: without it a reader has a run of bytes and no way to say
    /// whether it is words or lengths.
    #[test]
    fn a_list_column_round_trips_with_its_element_type() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let int = LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        };
        let text = LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        };
        let words: Vec<Vec<ListElement>> = vec![
            vec![ListElement::Word(1), ListElement::Word(u64::MAX)],
            vec![],
            vec![ListElement::Word(0)],
            vec![ListElement::Word(9); 300],
        ];
        let blobs: Vec<Vec<ListElement>> = vec![
            vec![ListElement::Blob(b"")],
            vec![ListElement::Blob(b"one"), ListElement::Blob(b"two")],
            vec![],
            vec![ListElement::Blob(&[0xff; 4096])],
        ];
        let word_rows: Vec<&[ListElement]> = words.iter().map(|r| r.as_slice()).collect();
        let blob_rows: Vec<&[ListElement]> = blobs.iter().map(|r| r.as_slice()).collect();
        let directory = store_props(
            &mut db,
            "person",
            &[
                (
                    "xs",
                    PropValues::List {
                        elem: &int,
                        rows: &word_rows,
                    },
                ),
                (
                    "tags",
                    PropValues::List {
                        elem: &text,
                        rows: &blob_rows,
                    },
                ),
            ],
        )
        .unwrap();
        assert_eq!(directory.columns[0].ty, list_of(int.clone()));
        assert_eq!(directory.columns[1].ty, list_of(text.clone()));
        assert!(!directory.columns[0].is_lane());
        assert_eq!(
            PropsDirectory::decode(&directory.encode()).unwrap(),
            directory
        );

        let mut reader = PropsReader::new(directory);
        let mut buf = Vec::new();
        for (col, elem, want) in [(0usize, &int, &words), (1, &text, &blobs)] {
            for (row, items) in want.iter().enumerate() {
                buf.clear();
                reader.read_str(&mut db, col, row as u64, &mut buf).unwrap();
                assert_eq!(
                    &list_elements(elem, &buf, ListRows::Counted(4)).unwrap(),
                    items,
                    "row {row}"
                );
            }
        }
    }

    /// The element type says what a row's bytes are, so an element of
    /// the other shape is refused at store time rather than written as
    /// something no read takes apart the same way.
    #[test]
    fn an_element_of_the_wrong_shape_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let int = LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        };
        let rows: Vec<&[ListElement]> = vec![&[ListElement::Blob(b"x")], &[], &[], &[]];
        assert!(
            store_props(
                &mut db,
                "person",
                &[(
                    "xs",
                    PropValues::List {
                        elem: &int,
                        rows: &rows,
                    },
                )],
            )
            .is_err()
        );
        // A list of lists has no row format and no second element code,
        // so it is refused as a column type outright.
        let nested = list_of(int);
        let rows: Vec<&[ListElement]> = vec![&[], &[], &[], &[]];
        assert!(
            store_props(
                &mut db,
                "person",
                &[(
                    "xs",
                    PropValues::List {
                        elem: &nested,
                        rows: &rows,
                    },
                )],
            )
            .is_err()
        );
    }

    #[test]
    fn a_truncated_or_overlong_list_row_is_refused() {
        let int = LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        };
        let text = LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        };
        let good = encode_list_row(
            &int,
            &[ListElement::Word(3), ListElement::Word(4)],
            ListRows::Counted(4),
        )
        .unwrap();
        assert_eq!(
            list_elements(&int, &good, ListRows::Counted(4)).unwrap(),
            vec![ListElement::Word(3), ListElement::Word(4)]
        );
        for len in 0..good.len() {
            assert!(
                list_elements(&int, &good[..len], ListRows::Counted(4)).is_err(),
                "prefix {len}"
            );
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(list_elements(&int, &trailing, ListRows::Counted(4)).is_err());
        // A count no payload can hold must not size an allocation, and
        // reading a list of words as a list of strings must not read a
        // length out of a value.
        let mut hostile = good.clone();
        hostile[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(list_elements(&int, &hostile, ListRows::Counted(4)).is_err());
        assert!(list_elements(&text, &good, ListRows::Counted(4)).is_err());
    }

    /// A list element takes the width its own type needs. This is where
    /// the embedding column's size comes from: 768 `FLOAT32` is 3076
    /// bytes of row against the 6148 the eight byte form cost, and an
    /// `INT8` list is an eighth of what it was.
    #[test]
    fn a_list_element_takes_the_width_its_type_needs() {
        let int = |bits, signed| LogicalType::Int {
            signed,
            bits,
            precision: None,
        };
        let float = |bits| LogicalType::Float {
            bits,
            precision: None,
        };
        // The word is what the lane holds for that element, so the
        // signed rows are given the sign extended word a scalar column
        // of that type would have carried.
        let table: &[(LogicalType, usize, u64)] = &[
            (LogicalType::Bool, 1, 1),
            (int(IntBits::B8, true), 1, -128i64 as u64),
            (int(IntBits::B8, false), 1, 255),
            (int(IntBits::B16, true), 2, -32768i64 as u64),
            (int(IntBits::B32, true), 4, -2147483648i64 as u64),
            (int(IntBits::B32, false), 4, u32::MAX as u64),
            (float(FloatBits::B32), 4, f32::to_bits(-1.5) as u64),
            (LogicalType::Date, 4, -1i64 as u64),
            (
                LogicalType::Duration(DurationKind::YearMonth),
                4,
                -13i64 as u64,
            ),
            (int(IntBits::B64, true), 8, -1i64 as u64),
            (float(FloatBits::B64), 8, f64::to_bits(-1.5)),
            (LogicalType::LocalDatetime, 8, -1i64 as u64),
        ];
        for (elem, width, word) in table {
            let row =
                encode_list_row(elem, &[ListElement::Word(*word)], ListRows::Counted(4)).unwrap();
            assert_eq!(row.len(), 4 + width, "{elem}");
            assert_eq!(
                list_elements(elem, &row, ListRows::Counted(4)).unwrap(),
                vec![ListElement::Word(*word)],
                "{elem}"
            );
        }
        // The number the milestone is about, on the type it is about.
        let row = encode_list_row(
            &float(FloatBits::B32),
            &vec![ListElement::Word(0); 768],
            ListRows::Counted(4),
        )
        .unwrap();
        assert_eq!(row.len(), 3076, "a 768 dimension FLOAT32 embedding row");
    }

    /// A word wider than the element type is a value that column cannot
    /// hold, so cutting it would be a loss and not a narrowing.
    #[test]
    fn a_list_element_too_wide_for_its_type_is_refused() {
        let byte = LogicalType::Int {
            signed: false,
            bits: IntBits::B8,
            precision: None,
        };
        assert!(encode_list_row(&byte, &[ListElement::Word(255)], ListRows::Counted(4)).is_ok());
        assert!(encode_list_row(&byte, &[ListElement::Word(256)], ListRows::Counted(4)).is_err());
        let small = LogicalType::Int {
            signed: true,
            bits: IntBits::B16,
            precision: None,
        };
        assert!(
            encode_list_row(
                &small,
                &[ListElement::Word(-32768i64 as u64)],
                ListRows::Counted(4)
            )
            .is_ok()
        );
        assert!(
            encode_list_row(
                &small,
                &[ListElement::Word(-32769i64 as u64)],
                ListRows::Counted(4)
            )
            .is_err()
        );
        assert!(
            encode_list_row(&small, &[ListElement::Word(32768)], ListRows::Counted(4)).is_err()
        );
    }

    /// Version 6 and older wrote every lane element in eight bytes, and
    /// those rows are still in files. The row's own length is what says
    /// which form it is, so no version has to reach the reader.
    #[test]
    fn a_list_row_written_in_the_old_eight_byte_form_still_reads() {
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let words = [7i64, -7, i32::MIN as i64];
        let mut old = (words.len() as u32).to_le_bytes().to_vec();
        for word in words {
            old.extend_from_slice(&word.to_le_bytes());
        }
        let want: Vec<ListElement> = words.iter().map(|w| ListElement::Word(*w as u64)).collect();
        assert_eq!(
            list_elements(&elem, &old, ListRows::Counted(4)).unwrap(),
            want
        );
        // And the same values written now are the same values read
        // back, in half the bytes.
        let new = encode_list_row(&elem, &want, ListRows::Counted(4)).unwrap();
        assert_eq!(new.len(), 4 + 3 * 4);
        assert_eq!(
            list_elements(&elem, &new, ListRows::Counted(4)).unwrap(),
            want
        );
        // A length that is neither form is a row nothing wrote.
        assert!(list_elements(&elem, &old[..old.len() - 1], ListRows::Counted(4)).is_err());
    }

    /// The column an embedding lands in, end to end. Every row is the
    /// same length, so the column takes the Stride layout and costs the
    /// bytes the floats are and the four the count is: no per row
    /// length, no chunk index, and no symbol table trained on float
    /// bytes to find out that they do not compress.
    #[test]
    fn an_embedding_column_lands_in_the_stride_layout() {
        const ROWS: usize = 64;
        const DIM: usize = 768;
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("props.zu1")).unwrap();
        bulk_load_keyed(&mut db, "doc", "cites", ROWS as u64, &[(0, 1)], None).unwrap();
        let elem = LogicalType::Float {
            bits: FloatBits::B32,
            precision: None,
        };
        let vectors: Vec<Vec<ListElement>> = (0..ROWS)
            .map(|r| {
                (0..DIM)
                    .map(|i| ListElement::Word(u64::from(((r * DIM + i) as f32).to_bits())))
                    .collect()
            })
            .collect();
        let rows: Vec<&[ListElement]> = vectors.iter().map(|v| v.as_slice()).collect();
        let directory = store_props(
            &mut db,
            "doc",
            &[(
                "embedding",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
            )],
        )
        .unwrap();

        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.value_count, ROWS as u64);
        // Four bytes of stride word for the column, and 3076 a row:
        // 3072 of floats and the four the row's own count takes. The
        // eight byte element form cost 6148 and FullZip a length on top
        // of that.
        assert_eq!(meta.payload_len, 4 + (ROWS * 3076) as u64);

        // And a row read back out of the middle is the vector that went
        // in, element for element.
        let mut reader = PropsReader::new(directory);
        let mut buf = Vec::new();
        reader.read_str(&mut db, 0, 40, &mut buf).unwrap();
        assert_eq!(
            list_elements(&elem, &buf, ListRows::Counted(4)).unwrap(),
            vectors[40]
        );
    }

    /// The same embedding under a declaration that gives its length.
    /// The rows stop carrying a count they all agree on, so the column
    /// is the floats and nothing else: 3072 bytes a row, which is what
    /// an ANN index wants to be handed as a flat `&[f32]`.
    #[test]
    fn a_declared_bound_takes_the_count_out_of_every_row() {
        const ROWS: usize = 64;
        const DIM: usize = 768;
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("props.zu1")).unwrap();
        bulk_load_keyed(&mut db, "doc", "cites", ROWS as u64, &[(0, 1)], None).unwrap();
        let elem = LogicalType::Float {
            bits: FloatBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(DIM as u32),
        };
        let vectors: Vec<Vec<ListElement>> = (0..ROWS)
            .map(|r| {
                (0..DIM)
                    .map(|i| ListElement::Word(u64::from(((r * DIM + i) as f32).to_bits())))
                    .collect()
            })
            .collect();
        let rows: Vec<&[ListElement]> = vectors.iter().map(|v| v.as_slice()).collect();
        let directory = store_props_nullable(
            &mut db,
            "doc",
            &[PropInput::typed(
                "embedding",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap();

        assert_eq!(directory.columns[0].ty, declared);
        assert_eq!(directory.columns[0].fixed_len, Some(DIM as u32));
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        // 3072 a row and the stride word once. 3076 was the figure with
        // the count still in the row, 6148 before the element took its
        // own width, and more again before the layout.
        assert_eq!(meta.payload_len, 4 + (ROWS * DIM * 4) as u64);

        let mut reader = PropsReader::new(directory);
        let mut buf = Vec::new();
        reader.read_str(&mut db, 0, 40, &mut buf).unwrap();
        let rows = reader.list_rows(0);
        assert_eq!(rows, ListRows::Fixed(DIM));
        assert_eq!(list_elements(&elem, &buf, rows).unwrap(), vectors[40]);
    }

    /// The bound is a maximum, so a column of shorter lists is a legal
    /// column of the same type and its rows keep their counts. Nothing
    /// about the type says which of the two a column is, which is why
    /// the directory says.
    ///
    /// It says the width as well as the fact. A count that cannot pass
    /// four does not need four bytes to say four, so the rows of this
    /// column spend one byte each on saying how long they are, and the
    /// widths a row can be written at are the widths a directory entry
    /// can name: one, two or four.
    #[test]
    fn a_column_short_of_its_bound_keeps_the_count_in_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(4),
        };
        let vectors: Vec<Vec<ListElement>> = (0..4)
            .map(|r| (0..=r).map(|i| ListElement::Word(i as u64)).collect())
            .collect();
        let rows: Vec<&[ListElement]> = vectors.iter().map(|v| v.as_slice()).collect();
        let directory = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed(
                "xs",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap();

        assert_eq!(directory.columns[0].fixed_len, None);
        assert_eq!(directory.columns[0].count_width, 1);
        assert_eq!(
            PropsDirectory::decode(&directory.encode()).unwrap(),
            directory,
            "the width survives a round trip through the directory"
        );
        let mut reader = PropsReader::new(directory);
        let mut buf = Vec::new();
        for (row, want) in vectors.iter().enumerate() {
            buf.clear();
            reader.read_str(&mut db, 0, row as u64, &mut buf).unwrap();
            // One byte of count and four of element each, where the same
            // rows written before version 12 spent four and four.
            assert_eq!(buf.len(), 1 + want.len() * 4, "row {row}");
            assert_eq!(
                &list_elements(&elem, &buf, reader.list_rows(0)).unwrap(),
                want,
                "row {row}"
            );
        }
    }

    /// A segment that points at nothing, for the two tests below, which
    /// are about a directory entry rather than about the bytes it names.
    fn blank_meta() -> SegmentMeta {
        SegmentMeta {
            value_count: 0,
            payload_len: 0,
            uncompressed_bytes: 0,
            min: 0,
            max: 0,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
            start: 0,
            blocks: Vec::new(),
        }
    }

    /// The width the rows said their counts in is read off the column
    /// entry and not worked out from the declared bound, and this is the
    /// test that makes the two different.
    ///
    /// A fold rewrites a directory at the current version over row blobs
    /// it leaves where they are, so a column written at version 11 comes
    /// back through `encode` with a version 12 header and rows that
    /// still spend four bytes on a count a one byte field would hold.
    /// Deriving the width from the bound would read those rows as a
    /// count of a few and a great many elements, which is to say it
    /// would refuse a file that is not damaged.
    #[test]
    fn a_narrow_bound_written_before_version_12_still_reads_four_byte_counts() {
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(4),
        };
        // What the column entry of such a file decodes to: a narrow
        // bound and the wide count its rows were written with.
        let column = PropColumn {
            name: "xs".into(),
            ty: declared,
            meta: blank_meta(),
            validity: None,
            fixed_len: None,
            count_width: 4,
            zones: None,
        };
        assert_eq!(column.list_rows(), ListRows::Counted(4));
        assert_eq!(count_width(&column.ty), 1, "a fresh write would say one");
        let row = encode_list_row(
            &elem,
            &[ListElement::Word(1), ListElement::Word(2)],
            ListRows::Counted(4),
        )
        .unwrap();
        assert_eq!(
            list_elements(&elem, &row, column.list_rows()).unwrap(),
            vec![ListElement::Word(1), ListElement::Word(2)]
        );
    }

    /// One, two or four, because those are the widths a write can
    /// choose. Any other byte in that field is a directory nothing here
    /// wrote, and a length read at a width nobody wrote it at is a row
    /// of nonsense, so it is refused where it is read rather than
    /// carried to whatever reads the row.
    #[test]
    fn a_list_count_width_no_write_chooses_is_refused() {
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        // The encoder writes the field it is given, so a directory with
        // a width no write chooses is the way to make those bytes
        // without going looking for the byte in a buffer.
        let of = |count_width| PropsDirectory {
            node_count: 1,
            columns: vec![PropColumn {
                name: "xs".into(),
                ty: LogicalType::List {
                    elem: Box::new(elem.clone()),
                    max: Some(4),
                },
                meta: blank_meta(),
                validity: None,
                fixed_len: None,
                count_width,
                zones: None,
            }],
            labels: None,
        };
        for width in [1u8, 2, 4] {
            let want = of(width);
            assert_eq!(PropsDirectory::decode(&want.encode()).unwrap(), want);
        }
        for width in [0u8, 3, 8, 255] {
            let err = PropsDirectory::decode(&of(width).encode())
                .expect_err("a width nothing writes")
                .to_string();
            assert!(err.contains("corrupt"), "width {width}: {err}");
        }
    }

    /// A zoned column is two planes over one set of rows, and both come
    /// back. The instants ride the lane, where a comparison and a sort
    /// already read them, and the offsets ride the plane beside it, so
    /// two rows written in two zones for one instant are one word in
    /// the lane and still each print where they were written.
    #[test]
    fn a_zoned_column_keeps_the_instant_and_the_zone_it_was_written_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        // 2024-01-15T10:00:00+07:00 and the same instant written in
        // UTC, then one before the epoch and one east of Greenwich.
        let nanos = [
            1_705_285_200_000_000_000i64,
            1_705_285_200_000_000_000,
            -1,
            0,
        ];
        let zones = [420i16, 0, -480, 1080];
        let directory = store_props(
            &mut db,
            "person",
            &[(
                "at",
                PropValues::ZonedDatetime {
                    nanos: &nanos,
                    zones: &zones,
                },
            )],
        )
        .unwrap();

        assert_eq!(directory.columns[0].ty, LogicalType::ZonedDatetime);
        assert!(directory.columns[0].zones.is_some());
        assert!(directory.columns[0].is_lane());
        let mut reader = PropsReader::new(directory);
        for row in 0..nanos.len() {
            assert_eq!(
                reader.read_zoned(&mut db, 0, row as u64).unwrap(),
                (nanos[row], zones[row]),
                "row {row}"
            );
            // And the lane on its own still answers the instant, which
            // is what every read that is not materialising a value
            // wants and what the two rows of one instant agree on.
            assert_eq!(
                reader.read_int(&mut db, 0, row as u64).unwrap() as i64,
                nanos[row],
                "row {row}"
            );
        }
        drop(db);
        crate::verify(&dir.path().join("props.zu1")).unwrap();
    }

    /// The two planes are paired by row number and by nothing else, so
    /// a plane of another length would pair rows rather than fail to.
    #[test]
    fn a_zone_plane_that_is_not_as_long_as_the_rows_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let nanos = [1i64, 2, 3, 4];
        let zones = [0i16, 60];
        let err = store_props(
            &mut db,
            "person",
            &[(
                "at",
                PropValues::ZonedDatetime {
                    nanos: &nanos,
                    zones: &zones,
                },
            )],
        )
        .unwrap_err();
        assert!(err.to_string().contains("2 zones"), "{err}");
    }

    /// An offset past eighteen hours names no zone on earth, and a row
    /// holding one would print an hour the calendar does not have.
    #[test]
    fn an_offset_no_zone_has_is_refused_by_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let nanos = [1i64, 2, 3, 4];
        let zones = [0i16, 60, 1440, -60];
        let err = store_props(
            &mut db,
            "person",
            &[(
                "at",
                PropValues::ZonedTime {
                    nanos: &nanos,
                    zones: &zones,
                },
            )],
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("row 2"), "{text}");
        assert!(text.contains("1440"), "{text}");
    }

    /// A zoned column had nowhere to put its second plane before
    /// version 10, so one in an older directory is a file that has been
    /// edited rather than an older column.
    #[test]
    fn a_zoned_column_in_an_older_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let nanos = [1i64, 2, 3, 4];
        let zones = [0i16, 60, -60, 120];
        let directory = store_props(
            &mut db,
            "person",
            &[(
                "at",
                PropValues::ZonedTime {
                    nanos: &nanos,
                    zones: &zones,
                },
            )],
        )
        .unwrap();
        let mut bytes = directory.encode();
        // The first two bytes are the version, whatever it has reached.
        // What this test is about is the one below it, so it reads the
        // current one rather than naming a number that has to be
        // corrected every time the format gains a column type.
        assert_eq!(
            u16::from_le_bytes(bytes[..2].try_into().unwrap()),
            PROPS_VERSION
        );
        bytes[..2].copy_from_slice(&9u16.to_le_bytes());
        let err = PropsDirectory::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("version 9 directory"), "{err}");
    }

    /// Every integer type rides the one lane, so a column declared
    /// narrower than the words arrive in is the same words read at
    /// another width. The declaration lands in the column entry and
    /// nothing about the payload changes, which is the promote edge of
    /// schema/06 §2: the catalog holds the declaration and the block
    /// holds the narrowest encoding that carries it.
    #[test]
    fn a_narrower_integer_declaration_is_a_promise_about_the_same_words() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let declared = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let words: Vec<u64> = vec![7, -9i64 as u64, i32::MAX as u64, i32::MIN as i64 as u64];
        let directory = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed("n", PropValues::Int(&words), &declared)],
        )
        .unwrap();

        assert_eq!(directory.columns[0].ty, declared);
        let mut reader = PropsReader::new(directory);
        for (row, want) in words.iter().enumerate() {
            assert_eq!(
                reader.read_int(&mut db, 0, row as u64).unwrap(),
                *want,
                "row {row}"
            );
        }
    }

    /// And the promise is kept before anything is written. A word
    /// outside the declared width is a row the column's own type says
    /// is not there, and no reader would ever notice it, because the
    /// lane holds the word whole whatever width reads it.
    #[test]
    fn a_word_the_declared_integer_width_does_not_hold_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let declared = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let words: Vec<u64> = vec![1, 2, i32::MAX as u64 + 1, 4];
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed("n", PropValues::Int(&words), &declared)],
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("row 2"), "{text}");
        assert!(text.contains("2147483648"), "{text}");
    }

    /// A row past the bound is refused, the way a row past an octet
    /// bound is, and for the same reason: the declaration is what every
    /// reader after this takes at its word.
    #[test]
    fn a_row_past_the_list_bound_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(2),
        };
        let long: Vec<ListElement> = (0..3).map(ListElement::Word).collect();
        let short: Vec<ListElement> = (0..2).map(ListElement::Word).collect();
        let rows: Vec<&[ListElement]> = vec![&short, &long, &short, &short];
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed(
                "xs",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap_err();
        assert!(err.to_string().contains("row 1 holds 3 elements"), "{err}");
    }

    /// A null row of a fixed count column is the bound in zero
    /// elements, so one null does not cost every row beside it the
    /// encoding. The mask still says the row is absent.
    #[test]
    fn a_null_row_keeps_a_fixed_count_column_uniform() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(3),
        };
        let full: Vec<ListElement> = (1..=3).map(ListElement::Word).collect();
        let rows: Vec<&[ListElement]> = vec![&full, &full, &[], &full];
        let mask = [0b1011u64];
        let mut input = PropInput::typed(
            "xs",
            PropValues::List {
                elem: &elem,
                rows: &rows,
            },
            &declared,
        );
        input.validity = Some(&mask);
        let directory = store_props_nullable(&mut db, "person", &[input]).unwrap();

        assert_eq!(directory.columns[0].fixed_len, Some(3));
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.payload_len, 4 + 4 * 3 * 4);

        let mut reader = PropsReader::new(directory);
        assert!(!reader.is_valid(&mut db, 0, 2).unwrap());
        let mut buf = Vec::new();
        reader.read_str(&mut db, 0, 2, &mut buf).unwrap();
        assert_eq!(
            list_elements(&elem, &buf, ListRows::Fixed(3)).unwrap(),
            vec![ListElement::Word(0); 3]
        );
    }

    /// The count is checked against the payload it describes, because
    /// every row offset follows from it. A directory that claims one
    /// the rows do not hold is refused before a row is named.
    #[test]
    fn a_fixed_count_the_payload_does_not_hold_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let elem = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(3),
        };
        let full: Vec<ListElement> = (1..=3).map(ListElement::Word).collect();
        let rows: Vec<&[ListElement]> = vec![&full, &full, &full, &full];
        let directory = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed(
                "xs",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap();
        assert!(PropsDirectory::decode(&directory.encode()).is_ok());

        let mut wrong = directory.clone();
        wrong.columns[0].fixed_len = Some(2);
        let err = PropsDirectory::decode(&wrong.encode()).unwrap_err();
        assert!(err.to_string().contains("does not hold them"), "{err}");
        // And a count on a column whose type gives no bound at all is
        // the same refusal, since there is nothing for it to mean.
        let mut placeless = directory;
        placeless.columns[0].ty = list_of(elem);
        placeless.columns[0].fixed_len = Some(3);
        let err = PropsDirectory::decode(&placeless.encode()).unwrap_err();
        assert!(err.to_string().contains("at a fixed count"), "{err}");
    }

    /// A version 2 directory holds every code but the list one, and a
    /// list code in one is a file that neither version wrote.
    #[test]
    fn a_list_code_in_a_version_2_directory_is_refused() {
        // The meta is a placeholder this test never reads through, but it
        // still has to be one the decoder will accept: a one value
        // MiniBlock payload is a four byte chunk count, a four byte index
        // entry, an eight byte fence and a body, and the decoder now
        // holds `value_count` to what the payload can describe.
        let meta = SegmentMeta {
            value_count: 1,
            payload_len: 17,
            uncompressed_bytes: 8,
            min: 0,
            max: 0,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
            start: 0,
            blocks: vec![7],
        };
        let mut old = Vec::new();
        old.extend_from_slice(&2u16.to_le_bytes());
        old.extend_from_slice(&1u64.to_le_bytes());
        old.extend_from_slice(&1u32.to_le_bytes());
        old.extend_from_slice(&1u16.to_le_bytes());
        old.extend_from_slice(b"x");
        old.push(LIST_CODE);
        old.push(1);
        meta.encode_unpacked(&mut old);
        assert!(PropsDirectory::decode(&old).is_err());
        old[0] = 3;
        let dir = PropsDirectory::decode(&old).unwrap();
        assert_eq!(
            dir.columns[0].ty,
            list_of(LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None
            })
        );
    }

    /// A hash column, which is the case the fixed octet declaration
    /// exists for: 16 bytes a row and nothing else, no length beside
    /// each row and no symbol table trained on bytes that are random by
    /// construction.
    #[test]
    fn a_column_declared_binary_costs_its_width_a_row() {
        const ROWS: usize = 64;
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("props.zu1")).unwrap();
        bulk_load_keyed(&mut db, "doc", "cites", ROWS as u64, &[(0, 1)], None).unwrap();
        let hashes: Vec<[u8; 16]> = (0..ROWS)
            .map(|r| {
                let mut h = [0u8; 16];
                h[..8].copy_from_slice(&(r as u64).to_le_bytes());
                h[8..].copy_from_slice(&(!(r as u64)).to_le_bytes());
                h
            })
            .collect();
        let rows: Vec<&[u8]> = hashes.iter().map(|h| &h[..]).collect();
        let binary16 = LogicalType::Bytes {
            min: Some(16),
            max: Some(16),
            fixed: true,
        };
        let directory = store_props_nullable(
            &mut db,
            "doc",
            &[PropInput::typed(
                "hash",
                PropValues::Bytes(&rows),
                &binary16,
            )],
        )
        .unwrap();

        // The declaration survives the round trip, which is the whole
        // of what version 8 added: before it, this column came back
        // spelled `BYTES`.
        assert_eq!(directory.columns[0].ty, binary16);
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.payload_len, 4 + (ROWS * 16) as u64);

        let mut reader = PropsReader::new(directory);
        let mut buf = Vec::new();
        reader.read_str(&mut db, 0, 40, &mut buf).unwrap();
        assert_eq!(buf, hashes[40]);
    }

    /// A row the declaration does not admit is refused, and the column
    /// is not written at all: a `BINARY(16)` holding fifteen octets is
    /// a column whose own type is a lie, and the first reader to take
    /// the type at its word reads the next row's bytes.
    #[test]
    fn a_row_the_declaration_does_not_admit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let short: Vec<&[u8]> = vec![&[0u8; 16], &[0u8; 15], &[0u8; 16], &[0u8; 16]];
        let binary16 = LogicalType::Bytes {
            min: Some(16),
            max: Some(16),
            fixed: true,
        };
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed(
                "hash",
                PropValues::Bytes(&short),
                &binary16,
            )],
        )
        .unwrap_err();
        assert!(err.to_string().contains("row 1 holds 15 octets"), "{err}");
    }

    /// A character bound counts characters, so five characters may be
    /// more than five bytes and a column of them is not a column of
    /// equal length rows. The bound is a check and not a layout, which
    /// is why a bounded string column stays zipped: text is what FSST
    /// is for, and a currency code column would lose that by being
    /// uniform.
    #[test]
    fn a_bounded_string_counts_characters_and_not_octets() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let five = LogicalType::Str {
            min: Some(1),
            max: Some(5),
            fixed: false,
        };
        let fits: Vec<&[u8]> = vec!["héllo".as_bytes(), b"a", "\u{1f600}".as_bytes(), b"abcde"];
        assert_eq!(fits[0].len(), 6, "five characters in six octets");
        let directory = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed("code", PropValues::Str(&fits), &five)],
        )
        .unwrap();
        assert_eq!(directory.columns[0].ty, five);
        assert_eq!(
            directory.columns[0].meta.structural,
            crate::segment::Structural::FullZip
        );

        let over: Vec<&[u8]> = vec![b"abcdef", b"a", b"b", b"c"];
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed("code", PropValues::Str(&over), &five)],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("row 0 holds 6 characters"),
            "{err}"
        );
    }

    /// A declaration is checked against the values it is given, so a
    /// caller cannot label a column of strings as a column of octets
    /// and have the two disagree from then on.
    #[test]
    fn a_declaration_the_values_do_not_carry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let text: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let binary16 = LogicalType::Bytes {
            min: Some(16),
            max: Some(16),
            fixed: true,
        };
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput::typed("hash", PropValues::Str(&text), &binary16)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("was given STRING"), "{err}");
    }

    /// A null row holds a placeholder nothing reads, and the caller may
    /// write anything there. In a fixed width column the writer makes it
    /// the declared width anyway, so one null does not cost the column
    /// its layout for bytes no reader looks at.
    #[test]
    fn a_null_row_keeps_a_fixed_width_column_uniform() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let rows: Vec<&[u8]> = vec![&[1u8; 16], b"", &[3u8; 16], &[4u8; 16]];
        let binary16 = LogicalType::Bytes {
            min: Some(16),
            max: Some(16),
            fixed: true,
        };
        let mask = [0b1101u64];
        let directory = store_props_nullable(
            &mut db,
            "person",
            &[PropInput {
                name: "hash",
                values: PropValues::Bytes(&rows),
                validity: Some(&mask),
                declared: Some(&binary16),
            }],
        )
        .unwrap();
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.payload_len, 4 + 4 * 16);

        let mut reader = PropsReader::new(directory);
        assert!(!reader.is_valid(&mut db, 0, 1).unwrap());
        let mut buf = Vec::new();
        reader.read_str(&mut db, 0, 1, &mut buf).unwrap();
        assert_eq!(buf, [0u8; 16], "the placeholder is the declared width");
    }

    /// The extended form was a catalog encoding before version 8 and no
    /// column entry carried it, so one in an older directory is a file
    /// somebody edited rather than an older column.
    #[test]
    fn an_extended_column_type_in_a_version_7_directory_is_refused() {
        let meta = SegmentMeta {
            value_count: 1,
            payload_len: 17,
            uncompressed_bytes: 8,
            min: 0,
            max: 0,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
            start: 0,
            blocks: vec![7],
        };
        let mut old = Vec::new();
        old.extend_from_slice(&7u16.to_le_bytes());
        old.extend_from_slice(&1u64.to_le_bytes());
        old.extend_from_slice(&1u32.to_le_bytes());
        old.extend_from_slice(&1u16.to_le_bytes());
        old.extend_from_slice(b"x");
        let binary16 = LogicalType::Bytes {
            min: Some(16),
            max: Some(16),
            fixed: true,
        };
        old.extend_from_slice(&column_type_bytes(&binary16).unwrap());
        meta.encode(&mut old);
        old.push(0);
        old.push(0);
        assert!(PropsDirectory::decode(&old).is_err());
        old[0] = 8;
        let dir = PropsDirectory::decode(&old).unwrap();
        assert_eq!(dir.columns[0].ty, binary16);
    }

    /// A column entry may carry a length bound, a zoned temporal, and
    /// nothing else of the extended form. A bounded list of a variable
    /// width element has no layout the bound buys anything in, since
    /// its rows carry lengths of their own whatever the bound says, so
    /// it stays declarable and unstorable.
    #[test]
    fn an_extended_column_type_a_column_cannot_hold_is_refused() {
        // A zoned temporal is the one this change made storable, and it
        // is the same bytes the catalog writes, so a type a graph type
        // names and a type a column holds stay one encoding.
        for ty in [LogicalType::ZonedTime, LogicalType::ZonedDatetime] {
            assert_eq!(column_type_bytes(&ty), declared_type_bytes(&ty));
        }
        let bounded = LogicalType::List {
            elem: Box::new(LogicalType::string()),
            max: Some(3),
        };
        assert!(declared_type_bytes(&bounded).is_some());
        assert!(column_type_bytes(&bounded).is_none());
        // A bounded list of a fixed width element is the one this
        // series made storable, and it is the same bytes the catalog
        // writes, so the two encodings stay one.
        let fixed = LogicalType::List {
            elem: Box::new(LogicalType::Bool),
            max: Some(3),
        };
        assert_eq!(column_type_bytes(&fixed), declared_type_bytes(&fixed));
        // A decimal is the first type whose storability turns on an
        // argument rather than on the type. It rides the lane as a whole
        // number of unscaled units, so the question is whether the units
        // fit a lane word, and eighteen digits is the last precision
        // that does. Both forms answer alike, so a decimal a graph type
        // names is a decimal a column holds and there is no gap between
        // them for a declaration to fall into.
        let decimal = |precision| LogicalType::Decimal {
            precision,
            scale: 2,
        };
        for precision in [1, 9, 18] {
            let ty = decimal(precision);
            assert_eq!(column_type_bytes(&ty), declared_type_bytes(&ty), "{ty}");
            assert!(storable(&ty), "{ty}");
        }
        for precision in [19, 38] {
            let ty = decimal(precision);
            assert!(declared_type_bytes(&ty).is_none(), "{ty}");
            assert!(column_type_bytes(&ty).is_none(), "{ty}");
            assert!(!storable(&ty), "{ty}");
        }
    }

    #[test]
    fn decode_rejects_hostile_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let vals: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let good = store_props(&mut db, "person", &[("x", PropValues::Str(&vals))])
            .unwrap()
            .encode();
        assert!(PropsDirectory::decode(&good).is_ok());
        for len in 0..good.len() {
            assert!(
                PropsDirectory::decode(&good[..len]).is_err(),
                "prefix {len}"
            );
        }
        let mut bad = good.clone();
        bad[0] = 99;
        assert!(PropsDirectory::decode(&bad).is_err());
        // A hostile column count must not size an allocation.
        let mut hostile = good.clone();
        hostile[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(PropsDirectory::decode(&hostile).is_err());
        let mut trailing = good;
        trailing.push(0);
        assert!(PropsDirectory::decode(&trailing).is_err());
    }

    #[test]
    fn a_column_says_which_of_its_rows_hold_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let ids = [10u64, 0, 30, 0];
        let names: Vec<&[u8]> = vec![b"Ada", b"Grace", b"Edsger", b"Barbara"];
        // Rows 0 and 2 hold an id, rows 1 and 3 do not. The placeholder
        // is still written, because the column is dense either way.
        let mask = [0b0101u64];
        let stored = store_props_nullable(
            &mut db,
            "person",
            &[
                PropInput {
                    name: "id",
                    values: PropValues::Int(&ids),
                    validity: Some(&mask),
                    declared: None,
                },
                PropInput::dense("firstName", PropValues::Str(&names)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let loaded = load_props(&mut db, table).unwrap().unwrap();
        assert_eq!(loaded, stored);

        let mut reader = PropsReader::new(loaded);
        let id = reader.col("id").unwrap();
        let name = reader.col("firstName").unwrap();
        assert!(reader.is_nullable(id));
        // A column with a value on every row carries no validity words
        // at all, so it costs nothing to be able to hold a null.
        assert!(!reader.is_nullable(name));
        for row in 0..4u64 {
            assert_eq!(
                reader.is_valid(&mut db, id, row).unwrap(),
                row % 2 == 0,
                "row {row}"
            );
            assert!(reader.is_valid(&mut db, name, row).unwrap());
        }
        assert_eq!(reader.read_int(&mut db, id, 2).unwrap(), 30);

        // The statistics a scan reads count only the rows holding a
        // value, so a null cannot pass for one the optimizer then counts.
        let stats = stats::Stats::load(&mut db).unwrap();
        let col = &stats.cols[&table]["id"];
        assert_eq!(col.rows, 2);
    }

    /// The family a type belongs to, written as a match with no
    /// wildcard arm so that adding a variant to the lattice fails to
    /// compile here until [`frontier`] names it.
    fn family(ty: &LogicalType) -> &'static str {
        match ty {
            LogicalType::Null => "null",
            LogicalType::Nothing => "nothing",
            LogicalType::Bool => "bool",
            LogicalType::Int { .. } => "int",
            LogicalType::Decimal { .. } => "decimal",
            LogicalType::Float { .. } => "float",
            LogicalType::Str { .. } => "str",
            LogicalType::Bytes { .. } => "bytes",
            LogicalType::Date => "date",
            LogicalType::LocalTime => "local time",
            LogicalType::LocalDatetime => "local datetime",
            LogicalType::ZonedTime => "zoned time",
            LogicalType::ZonedDatetime => "zoned datetime",
            LogicalType::Duration(_) => "duration",
            LogicalType::Node(_) => "node",
            LogicalType::Edge(_) => "edge",
            LogicalType::Graph(_) => "graph",
            LogicalType::BindingTable(_) => "binding table",
            LogicalType::Path(_) => "path",
            LogicalType::List { .. } => "list",
            LogicalType::Record(_) => "record",
            LogicalType::Any => "any",
            LogicalType::Union(_) => "union",
            LogicalType::AnyProperty => "any property",
            LogicalType::Nullable(_) => "nullable",
        }
    }

    /// Every type a graph type may name a property with, and whether
    /// this file can write a column of it today.
    ///
    /// This is the declarable and storable frontier, and it is a table
    /// rather than a set of ignored tests so that the gap is one diff
    /// and not a search. Moving a row from `false` to `true` is what
    /// adding an encoding looks like, and the count assertion below
    /// stops the frontier growing by accident.
    fn frontier() -> Vec<(LogicalType, bool)> {
        let chars = |min, max, fixed| LogicalType::Str { min, max, fixed };
        let octets = |min, max, fixed| LogicalType::Bytes { min, max, fixed };
        let bounded_list = |elem: LogicalType, max| LogicalType::List {
            elem: Box::new(elem),
            max: Some(max),
        };
        vec![
            // The eighteen column codes, which are the storable set.
            (LogicalType::Bool, true),
            (LogicalType::int(IntBits::B8), true),
            (LogicalType::int(IntBits::B16), true),
            (LogicalType::int(IntBits::B32), true),
            (LogicalType::int(IntBits::B64), true),
            (LogicalType::uint(IntBits::B8), true),
            (LogicalType::uint(IntBits::B16), true),
            (LogicalType::uint(IntBits::B32), true),
            (LogicalType::uint(IntBits::B64), true),
            (LogicalType::float(FloatBits::B32), true),
            (LogicalType::float(FloatBits::B64), true),
            (LogicalType::string(), true),
            (LogicalType::bytes(), true),
            (LogicalType::Date, true),
            (LogicalType::LocalTime, true),
            (LogicalType::LocalDatetime, true),
            (LogicalType::Duration(DurationKind::DayTime), true),
            (LogicalType::Duration(DurationKind::YearMonth), true),
            // Declarable and storable through the extended form, which
            // names a type without giving it a lane of its own.
            (LogicalType::ZonedTime, true),
            (LogicalType::ZonedDatetime, true),
            (chars(None, Some(512), false), true),
            (chars(Some(2), Some(2), true), true),
            (octets(Some(16), Some(16), true), true),
            (list_of(LogicalType::int(IntBits::B64)), true),
            (list_of(LogicalType::float(FloatBits::B32)), true),
            // The four list types of schema/02 section 2 D3, which are
            // two words about two different things: the one in front of
            // the list name is about the elements and the one behind
            // the length is about the list. All four are declarable and
            // the embedding column is the last of them.
            (
                list_of(LogicalType::Nullable(Box::new(LogicalType::float(
                    FloatBits::B32,
                )))),
                true,
            ),
            (
                bounded_list(
                    LogicalType::Nullable(Box::new(LogicalType::float(FloatBits::B32))),
                    768,
                ),
                true,
            ),
            (bounded_list(LogicalType::float(FloatBits::B32), 768), true),
            (list_of(list_of(LogicalType::string())), true),
            // A decimal whose unscaled units fit the lane. This is the
            // one row of the table whose sibling is on the other side of
            // it: the type is storable and an argument to it is what
            // decides, which no other row here can say.
            (
                LogicalType::Decimal {
                    precision: 12,
                    scale: 2,
                },
                true,
            ),
            // Declarable and not storable. Each of these is an entry in
            // schema/06 section 6, and S2 turns them true one at a time.
            (
                LogicalType::Decimal {
                    precision: 38,
                    scale: 2,
                },
                false,
            ),
            (LogicalType::int(IntBits::B128), false),
            (LogicalType::int(IntBits::B256), false),
            (LogicalType::uint(IntBits::B128), false),
            (LogicalType::float(FloatBits::B16), false),
            (LogicalType::float(FloatBits::B128), false),
            (LogicalType::float(FloatBits::B256), false),
            (
                LogicalType::Record(zu_common::RecordType::closed(vec![
                    zu_common::Field {
                        name: "lat".into(),
                        ty: LogicalType::float(FloatBits::B64),
                    },
                    zu_common::Field {
                        name: "lon".into(),
                        ty: LogicalType::float(FloatBits::B64),
                    },
                ])),
                false,
            ),
            (LogicalType::Any, false),
            (LogicalType::AnyProperty, false),
            (
                LogicalType::Union(vec![LogicalType::int(IntBits::B64), LogicalType::string()]),
                false,
            ),
            (LogicalType::Path(None), false),
            (LogicalType::Node(None), false),
            (LogicalType::Edge(None), false),
            (LogicalType::Graph(None), false),
            (LogicalType::BindingTable(None), false),
            (LogicalType::Null, false),
            (LogicalType::Nothing, false),
            // Nullability is a flag on the property and not a wrapper on
            // its type, so a wrapped type never reaches the encoder and
            // is refused rather than silently unwrapped.
            (
                LogicalType::Nullable(Box::new(LogicalType::int(IntBits::B64))),
                false,
            ),
        ]
    }

    /// A declared type is written and read back as itself, or it is
    /// refused before anything is written. There is no third answer, and
    /// which types get which is the table above.
    #[test]
    fn the_declarable_and_storable_frontier_is_where_the_table_says() {
        let mut unstorable = Vec::new();
        for (ty, storable) in frontier() {
            let Some(bytes) = declared_type_bytes(&ty) else {
                assert!(!storable, "{ty} is storable and the encoder refused it");
                unstorable.push(format!("{ty}"));
                continue;
            };
            assert!(storable, "{ty} is not storable and the encoder wrote it");
            let mut pos = 0;
            let back = decode_declared_type(&bytes, &mut pos).unwrap();
            assert_eq!(pos, bytes.len(), "{ty} left bytes behind");
            assert_eq!(back, ty, "{ty} did not come back as itself");
        }
        // The frontier shrinks and never grows. Each name here is an
        // encoding S2 owes, and taking one off is a deliberate diff.
        assert_eq!(
            unstorable.len(),
            19,
            "the unstorable set changed: {unstorable:?}"
        );
    }

    /// Every variant of the lattice appears in the frontier table, so a
    /// type added to `zu_common` cannot arrive without an answer to the
    /// question of whether a column can hold one.
    #[test]
    fn the_frontier_table_covers_every_variant_of_the_lattice() {
        let covered: BTreeSet<&str> = frontier().iter().map(|(ty, _)| family(ty)).collect();
        let every = [
            "null",
            "nothing",
            "bool",
            "int",
            "decimal",
            "float",
            "str",
            "bytes",
            "date",
            "local time",
            "local datetime",
            "zoned time",
            "zoned datetime",
            "duration",
            "node",
            "edge",
            "graph",
            "binding table",
            "path",
            "list",
            "record",
            "any",
            "union",
            "any property",
            "nullable",
        ];
        for name in every {
            assert!(covered.contains(name), "no frontier entry is a {name}");
        }
        assert_eq!(covered.len(), every.len(), "a variant has no family name");
    }

    /// A declared list keeps the element's nullability, which a column
    /// of one does not.
    ///
    /// These are two different questions and they used to have one
    /// answer. A stored list holds a value in every position, so the
    /// column form drops the wrapper and is right to. The catalog is
    /// not storing values, it is remembering a promise, and
    /// `LIST<FLOAT32 NOT NULL>` promises something `LIST<FLOAT32>` does
    /// not: a column with no child validity mask, which is the whole
    /// difference between an embedding a SIMD kernel can be handed flat
    /// and one it cannot. S2 needs the distinction to survive the round
    /// trip before it can spend it.
    #[test]
    fn a_declared_list_remembers_whether_its_elements_admit_null() {
        let nulls = list_of(LogicalType::Nullable(Box::new(LogicalType::float(
            FloatBits::B32,
        ))));
        let none = list_of(LogicalType::float(FloatBits::B32));
        assert_ne!(nulls, none, "the two are two types");
        for asked in [&nulls, &none] {
            let bytes = declared_type_bytes(asked).expect("a list of floats is declarable");
            let mut pos = 0;
            let back = decode_declared_type(&bytes, &mut pos).unwrap();
            assert_eq!(pos, bytes.len());
            assert_eq!(&back, asked);
        }
        // The column form still drops it, because a column really does
        // hold a value in every position.
        let bytes = type_bytes(&nulls).expect("a column takes a list of floats");
        let mut pos = 0;
        assert_eq!(decode_declared_type(&bytes, &mut pos).unwrap(), none);
    }

    /// A bounded list is declarable, and the bound comes back.
    ///
    /// `LIST<FLOAT32 NOT NULL>[768]` is the embedding column of
    /// schema/06 section 6 item 1, and until this it was a legal GQL
    /// declaration that zu refused with `a type this file cannot
    /// write`. It has no column encoding yet, so what this pins is the
    /// catalog remembering the number: 768 is the dimension every later
    /// check is against, and a catalog that forgot it could not make
    /// one of them.
    #[test]
    fn a_bounded_list_is_declarable_and_keeps_its_bound() {
        let asked = LogicalType::List {
            elem: Box::new(LogicalType::float(FloatBits::B32)),
            max: Some(768),
        };
        let bytes = declared_type_bytes(&asked).expect("a bounded list is declarable");
        let mut pos = 0;
        assert_eq!(decode_declared_type(&bytes, &mut pos).unwrap(), asked);
        assert_eq!(pos, bytes.len());
        // And a column still will not take one, which is the next item
        // rather than this one.
        assert!(type_bytes(&asked).is_none(), "a column has no lane for it");
    }

    /// A list of lists is declarable, because the element is written by
    /// the same function that wrote the list.
    #[test]
    fn a_list_of_lists_is_declarable() {
        let asked = list_of(list_of(LogicalType::string()));
        let bytes = declared_type_bytes(&asked).expect("nesting costs nothing here");
        let mut pos = 0;
        assert_eq!(decode_declared_type(&bytes, &mut pos).unwrap(), asked);
        assert_eq!(pos, bytes.len());
        assert!(type_bytes(&asked).is_none(), "a column has no lane for it");
    }

    #[test]
    fn a_validity_mask_of_the_wrong_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let ids = [1u64, 2, 3, 4];
        let short: [u64; 0] = [];
        let err = store_props_nullable(
            &mut db,
            "person",
            &[PropInput {
                name: "id",
                values: PropValues::Int(&ids),
                validity: Some(&short),
                declared: None,
            }],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("validity"), "{err}");
    }
}
