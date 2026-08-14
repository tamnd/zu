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
use std::collections::BTreeMap;
use std::sync::Arc;

use zu_common::{
    DurationKind, FloatBits, IntBits, LogicalType, PhysicalType, Result, ZuError, int_key,
};

use crate::catalog::{Catalog, TableIndex};
use crate::file::{BlockPtr, Zu1File};
use crate::fullzip::{read_blob_range, write_blob_segment};
use crate::meta;
use crate::segment::{
    CHUNK_ROWS, ChunkCache, ChunkDirectory, SegmentMeta, cached_chunk, chunk_zone, decode_chunk,
    load_chunk_directory_pooled, read_one_cached, write_segment,
};
use crate::stats;

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
/// what a column holds.
const PROPS_VERSION: u16 = 5;
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

/// The bytes a declared type is written as, for a container that names
/// a type without storing values of it. `None` is a type nothing can be
/// declared with, which the caller refuses before writing anything.
pub(crate) fn declared_type_bytes(ty: &LogicalType) -> Option<Vec<u8>> {
    type_bytes(ty)
}

/// Reads a type written by [`declared_type_bytes`], leaving `pos` after
/// it. The codes are the column codes, so a type the catalog names and
/// a type a column stores are the same byte.
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
        Some(&code) => code_type(code).ok_or_else(|| corrupt(format!("unknown type {code}")))?,
        None => return Err(corrupt("truncated type".into())),
    };
    *pos += 1;
    Ok(ty)
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
}

/// Whether values of this type ride the fixed width lane.
fn lane_type(ty: &LogicalType) -> bool {
    matches!(
        ty.physical(),
        Some(
            PhysicalType::Bool
                | PhysicalType::I8
                | PhysicalType::I16
                | PhysicalType::I32
                | PhysicalType::I64
                | PhysicalType::U8
                | PhysicalType::U16
                | PhysicalType::U32
                | PhysicalType::U64
                | PhysicalType::F32
                | PhysicalType::F64
                | PhysicalType::Days32
                | PhysicalType::Nanos64
                | PhysicalType::Months32
        )
    )
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

/// Encodes one row of a list column: `count: u32`, then the elements,
/// each a little endian word for a lane element type or a `len: u32`
/// and its bytes otherwise.
fn encode_list_row(elem: &LogicalType, items: &[ListElement<'_>]) -> Result<Vec<u8>> {
    let lane = lane_type(elem);
    let mut out = Vec::with_capacity(4 + items.len() * 8);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for item in items {
        match (item, lane) {
            (ListElement::Word(w), true) => out.extend_from_slice(&w.to_le_bytes()),
            (ListElement::Blob(b), false) => {
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
/// The elements borrow the buffer they came out of, so a read of a list
/// column is the blob read and a walk over it, with no allocation per
/// element.
pub fn list_elements<'a>(elem: &LogicalType, bytes: &'a [u8]) -> Result<Vec<ListElement<'a>>> {
    let head = bytes
        .get(..4)
        .ok_or_else(|| corrupt("truncated list length".into()))?;
    let count = u32::from_le_bytes(head.try_into().unwrap()) as usize;
    let lane = lane_type(elem);
    // A count is four bytes of header away from its smallest possible
    // payload, so a count the row cannot hold is caught before it sizes
    // an allocation.
    let least = if lane { 8 } else { 4 };
    if count > (bytes.len() - 4) / least {
        return Err(corrupt(format!(
            "a list of {count} does not fit {} bytes",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(count);
    let mut pos = 4usize;
    for _ in 0..count {
        if lane {
            let raw = bytes
                .get(pos..pos + 8)
                .ok_or_else(|| corrupt("truncated list element".into()))?;
            out.push(ListElement::Word(u64::from_le_bytes(
                raw.try_into().unwrap(),
            )));
            pos += 8;
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
            PropValues::Duration(_, v) => v.len(),
            PropValues::List { rows, .. } => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

/// Whether a directory written under `version` was allowed to carry
/// this type code. Version 1 wrote two codes and version 2 wrote every
/// code but the list one, so a file claiming an older version and
/// carrying a newer code is a file that has been edited.
fn code_allowed(code: u8, version: u16) -> bool {
    match version {
        1 => code <= 1,
        2 => code != LIST_CODE,
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
            out.extend_from_slice(&type_bytes(&col.ty).expect("column type is storable"));
            col.meta.encode(&mut out);
            match &col.validity {
                Some(meta) => {
                    out.push(1);
                    meta.encode(&mut out);
                }
                None => out.push(0),
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
        // rather than how any of them decodes.
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
                    pos += 1;
                    let elem = bytes
                        .get(pos)
                        .ok_or_else(|| corrupt("truncated list element type".into()))?;
                    let elem = code_type(*elem)
                        .ok_or_else(|| corrupt(format!("unknown element type {elem}")))?;
                    list_of(elem)
                }
                Some(&code) => {
                    code_type(code).ok_or_else(|| corrupt(format!("unknown column type {code}")))?
                }
                None => return Err(corrupt("truncated column type".into())),
            };
            pos += 1;
            let (meta, next) = SegmentMeta::decode(bytes, pos)?;
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
                        let (meta, next) = SegmentMeta::decode(bytes, pos + 1)?;
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
            columns.push(PropColumn {
                name,
                ty,
                meta,
                validity,
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
                    let (meta, next) = SegmentMeta::decode(bytes, pos + 1)?;
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

pub(crate) fn free_props(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_props_parts(db, root, true)
}

/// Frees everything a props chain owns apart from the label bitset,
/// which the caller is carrying into the directory that replaces this
/// one. Storing a property column has nothing to say about which labels
/// a row holds, so it leaves that segment where it is.
pub(crate) fn free_props_keeping_labels(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_props_parts(db, root, false)
}

fn free_props_parts(db: &mut Zu1File, root: BlockPtr, labels: bool) -> Result<()> {
    let directory = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    if labels {
        for &ptr in directory.labels.iter().flat_map(|m| &m.blocks) {
            db.free_block(ptr)?;
        }
    }
    for col in &directory.columns {
        for &ptr in &col.meta.blocks {
            db.free_block(ptr)?;
        }
        for &ptr in col.validity.iter().flat_map(|m| &m.blocks) {
            db.free_block(ptr)?;
        }
    }
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
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
}

impl<'a> PropInput<'a> {
    /// A column with a value in every row.
    pub fn dense(name: &'a str, values: PropValues<'a>) -> Self {
        Self {
            name,
            values,
            validity: None,
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
    let node_count = rows;
    let mut cols = Vec::with_capacity(columns.len());
    let mut col_stats = BTreeMap::new();
    for column in columns {
        let (name, values) = (column.name, &column.values);
        // The values are all in hand here and nowhere else, so this is
        // where the estimator's statistics get built: a COPY that
        // brings properties leaves the optimizer able to reason about
        // them, without anyone remembering to ANALYZE (perf/12 §1).
        let ty = values.ty();
        if type_bytes(&ty).is_none() {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' has type {ty}, which is not storable as a property"
            )));
        }
        // A list column is encoded here rather than by its caller, so
        // the row format has one writer and one reader and they sit
        // next to each other.
        let encoded: Vec<Vec<u8>> = match values {
            PropValues::List { elem, rows } => rows
                .iter()
                .map(|items| encode_list_row(list_elem(elem).expect("storable"), items))
                .collect::<Result<_>>()?,
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
        let meta = match (values.lane(), values) {
            (Some(words), _) => write_segment(db, &words)?,
            (None, PropValues::Str(v) | PropValues::Bytes(v)) => write_blob_segment(db, v)?,
            (None, PropValues::List { .. }) => write_blob_segment(db, &blobs)?,
            (None, _) => unreachable!("every variable width column is a blob"),
        };
        // A mask with every bit set says nothing a reader does not
        // already assume, so it is not written: a caller that hands one
        // over gets the column it would have got without it.
        let validity = match column.validity {
            Some(mask) if !all_set(mask, node_count as usize) => Some(write_segment(db, mask)?),
            _ => None,
        };
        cols.push(PropColumn {
            name: name.to_string(),
            ty,
            meta,
            validity,
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
/// Edges must be unique. The ordinal of an edge is found by searching
/// the forward list for its destination, which two edges with the same
/// endpoints would answer the same way, so a table that stores
/// properties may not hold a pair twice and a load that would make one
/// is refused here rather than answered wrongly later.
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
    check_columns(edge_count, columns)?;
    let mut index = TableIndex::load(db)?;
    let root = index.get(rel_id).ok_or_else(|| ZuError::Corrupt {
        what: "table index",
        detail: format!("rel table '{rel_table}' has no directory entry"),
    })?;
    let mut directory = crate::graph::Directory::decode(&meta::read_chain(db, root)?)?;
    reject_duplicate_edges(db, rel_table, &directory)?;
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
            for &ptr in directory.labels.iter().flat_map(|m| &m.blocks) {
                db.free_block(ptr)?;
            }
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

/// Errors when any node lists a destination twice.
///
/// This walks the forward adjacency once, which is the cost of reading
/// what is about to be written a column of anyway, and it runs before
/// anything is written. A list is stored sorted, so a repeat is a
/// neighbor equal to the one before it and the check is a comparison per
/// edge with no state.
fn reject_duplicate_edges(
    db: &mut Zu1File,
    name: &str,
    directory: &crate::graph::Directory,
) -> Result<()> {
    let mut values = Vec::new();
    for (g, group) in directory.groups.iter().enumerate() {
        let mut offsets = Vec::new();
        crate::segment::read_segment(db, &group.fwd.offsets, &mut offsets)?;
        values.clear();
        crate::segment::read_segment(db, &group.fwd.neighbors, &mut values)?;
        for row in 0..group.row_count as usize {
            let list = &values[offsets[row] as usize..offsets[row + 1] as usize];
            if let Some(w) = list.windows(2).find(|w| w[0] == w[1]) {
                let node = g as u64 * zu_common::GROUP_ROWS as u64 + row as u64;
                return Err(ZuError::InvalidArgument(format!(
                    "rel table '{name}' holds the edge ({node}, {}) twice, which an edge \
                     property column cannot tell apart",
                    w[0]
                )));
            }
        }
    }
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

/// One decoded FullZip chunk of a string column: the values of rows
/// `chunk * CHUNK_ROWS` onward concatenated in `bytes`, row `i` of the
/// chunk ending at `ends[i]`.
#[derive(Debug)]
struct StrChunk {
    chunk: u64,
    bytes: Vec<u8>,
    ends: Vec<u64>,
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
    /// The same again for the label bitset, which is one word per row
    /// rather than one per 64 and belongs to no column.
    label_state: Option<(Arc<ChunkDirectory>, ChunkCache)>,
    /// Row order scratch for the gathers, reused across calls.
    order_scratch: Vec<u32>,
}

impl PropsReader {
    pub fn new(directory: PropsDirectory) -> Self {
        Self {
            directory,
            int_state: BTreeMap::new(),
            str_state: BTreeMap::new(),
            valid_state: BTreeMap::new(),
            label_state: None,
            order_scratch: Vec::new(),
        }
    }

    /// Whether row `row` of `col` holds a value.
    ///
    /// A column with no validity segment holds one in every row, which
    /// is the answer without a read, so a graph that stores no null
    /// pays nothing for the question.
    pub fn is_valid(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<bool> {
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
        let Some(meta) = self.directory.labels.clone() else {
            return Ok(None);
        };
        if self.label_state.is_none() {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, &meta)?;
            self.label_state = Some((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.label_state.as_mut().expect("just inserted");
        Ok(Some(read_one_cached(db, &meta, dir, cache, row)?))
    }

    /// Whether the table stores a label bitset at all, which is what
    /// says whether a scan has a word to read per row.
    pub fn has_labels(&self) -> bool {
        self.directory.labels.is_some()
    }

    /// Whether `col` has a null in it anywhere, which is what says
    /// whether a reader has to ask [`Self::is_valid`] at all.
    pub fn is_nullable(&self, col: usize) -> bool {
        self.directory.columns[col].validity.is_some()
    }

    pub fn columns(&self) -> &[PropColumn] {
        &self.directory.columns
    }

    /// Rows in the table's domain; every column is row-aligned to it.
    pub fn rows(&self) -> u64 {
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
        Ok(chunk_zone(meta, &dir, chunk))
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
        let dir = self.int_dir(db, col)?;
        let meta = &self.directory.columns[col].meta;
        decode_chunk(db, meta, &dir, chunk, out)
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
        read_blob_range(db, &column.meta, start, end, bytes, ends)
    }

    pub fn read_int(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<u64> {
        let meta = &self.directory.columns[col].meta;
        if let std::collections::btree_map::Entry::Vacant(slot) = self.int_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.int_state.get_mut(&col).expect("just inserted");
        read_one_cached(db, meta, dir, cache, row)
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
                return Err(ZuError::InvalidArgument(format!(
                    "row {row} out of 0..{}",
                    meta.value_count
                )));
            }
            let chunk = (row / CHUNK_ROWS as u64) as usize;
            let values = cached_chunk(db, meta, dir, cache, chunk)?;
            while i < order.len() {
                let r = rows[order[i] as usize];
                if r / CHUNK_ROWS as u64 != chunk as u64 {
                    break;
                }
                out[order[i] as usize] = values[(r % CHUNK_ROWS as u64) as usize];
                i += 1;
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
                return Err(ZuError::InvalidArgument(format!(
                    "row {row} out of 0..{}",
                    meta.value_count
                )));
            }
            let chunk = row / CHUNK_ROWS as u64;
            if chunk != cur_chunk {
                chunk_start = chunk * CHUNK_ROWS as u64;
                let end = meta.value_count.min(chunk_start + CHUNK_ROWS as u64);
                chunk_bytes.clear();
                chunk_ends.clear();
                read_blob_range(
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
        let meta = &self.directory.columns[col].meta;
        if row >= meta.value_count {
            return Err(ZuError::InvalidArgument(format!(
                "row {row} out of 0..{}",
                meta.value_count
            )));
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
            read_blob_range(db, meta, start, end, &mut fresh.bytes, &mut fresh.ends)?;
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
        let graph = crate::graph::GraphReader::load_table(&mut db, "knows").unwrap();
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
    fn edge_columns_reject_a_table_that_holds_a_pair_twice() {
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
        let err =
            store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&three))]).unwrap_err();
        assert!(
            err.to_string().contains("holds the edge (0, 1) twice"),
            "{err}"
        );
        // The refusal comes before anything is written, so the table is
        // left as it was rather than half converted.
        let rel = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        assert!(load_rel_props(&mut db, rel).unwrap().is_none());

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
            meta.encode(&mut old);
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
        meta.encode(&mut out);
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
                assert_eq!(&list_elements(elem, &buf).unwrap(), items, "row {row}");
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
        let good = encode_list_row(&int, &[ListElement::Word(3), ListElement::Word(4)]).unwrap();
        assert_eq!(
            list_elements(&int, &good).unwrap(),
            vec![ListElement::Word(3), ListElement::Word(4)]
        );
        for len in 0..good.len() {
            assert!(list_elements(&int, &good[..len]).is_err(), "prefix {len}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(list_elements(&int, &trailing).is_err());
        // A count no payload can hold must not size an allocation, and
        // reading a list of words as a list of strings must not read a
        // length out of a value.
        let mut hostile = good.clone();
        hostile[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(list_elements(&int, &hostile).is_err());
        assert!(list_elements(&text, &good).is_err());
    }

    /// A version 2 directory holds every code but the list one, and a
    /// list code in one is a file that neither version wrote.
    #[test]
    fn a_list_code_in_a_version_2_directory_is_refused() {
        let meta = SegmentMeta {
            value_count: 1,
            payload_len: 8,
            uncompressed_bytes: 8,
            min: 0,
            max: 0,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
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
        meta.encode(&mut old);
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
        // value, so a null cannot pass for one the planner then counts.
        let stats = stats::Stats::load(&mut db).unwrap();
        let col = &stats.cols[&table]["id"];
        assert_eq!(col.rows, 2);
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
            }],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("validity"), "{err}");
    }
}
