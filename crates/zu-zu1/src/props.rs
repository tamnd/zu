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
//! `type: u8` (a code from [`TYPE_CODES`]), and the column's
//! `SegmentMeta`.

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
/// the storable part of the logical lattice. Version 1 directories are
/// still read, because a file written before this is not wrong, it is
/// just narrow.
const PROPS_VERSION: u16 = 2;
const MAX_NAME_LEN: usize = 256;

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

fn code_type(code: u8) -> Option<LogicalType> {
    TYPE_CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, t)| t.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropColumn {
    pub name: String,
    pub ty: LogicalType,
    pub meta: SegmentMeta,
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
            PropValues::Str(_) | PropValues::Bytes(_) => return None,
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
            out.push(type_code(&col.ty).expect("column type is storable"));
            col.meta.encode(&mut out);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let head = bytes
            .get(..14)
            .ok_or_else(|| corrupt("truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        // Version 1 is still read. It only ever wrote codes 0 and 1,
        // and those two codes mean in version 2 what they meant then,
        // so the difference between the versions is which codes may
        // appear rather than how any of them decodes.
        if version != PROPS_VERSION && version != 1 {
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
                Some(&code) if version >= PROPS_VERSION || code <= 1 => {
                    code_type(code).ok_or_else(|| corrupt(format!("unknown column type {code}")))?
                }
                Some(code) => {
                    return Err(corrupt(format!(
                        "column type {code} in a version {version} directory"
                    )));
                }
                None => return Err(corrupt("truncated column type".into())),
            };
            pos += 1;
            let (meta, next) = SegmentMeta::decode(bytes, pos)?;
            pos = next;
            columns.push(PropColumn { name, ty, meta });
        }
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes".into()));
        }
        Ok(Self {
            node_count,
            columns,
        })
    }
}

pub(crate) fn free_props(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    let directory = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    for col in &directory.columns {
        for &ptr in &col.meta.blocks {
            db.free_block(ptr)?;
        }
    }
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
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
    let catalog = Catalog::load(db)?;
    let table = catalog
        .node_by_name(node_table)
        .ok_or_else(|| ZuError::InvalidArgument(format!("no node table '{node_table}'")))?;
    let (table_id, node_count) = (table.id, table.node_count);
    for (name, values) in columns {
        if values.len() as u64 != node_count {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {} values over {node_count} rows",
                values.len()
            )));
        }
    }
    let mut index = TableIndex::load(db)?;
    if let Some(root) = index.get(table_id) {
        free_props(db, root)?;
        index.remove(table_id);
    }
    let mut cols = Vec::with_capacity(columns.len());
    let mut col_stats = BTreeMap::new();
    for (name, values) in columns {
        // The values are all in hand here and nowhere else, so this is
        // where the estimator's statistics get built: a COPY that
        // brings properties leaves the optimizer able to reason about
        // them, without anyone remembering to ANALYZE (perf/12 §1).
        let ty = values.ty();
        if type_code(&ty).is_none() {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' has type {ty}, which is not storable as a property"
            )));
        }
        let stat = match values.keys() {
            Some(keys) => {
                let refs: Vec<&[u8]> = keys.iter().map(|k| &k[..]).collect();
                stats::column_stats(&refs)
            }
            None => match values {
                PropValues::Str(v) | PropValues::Bytes(v) => stats::column_stats(v),
                _ => unreachable!("every fixed width column has lane keys"),
            },
        };
        col_stats.insert((*name).to_string(), stat);
        let meta = match (values.lane(), values) {
            (Some(words), _) => write_segment(db, &words)?,
            (None, PropValues::Str(v) | PropValues::Bytes(v)) => write_blob_segment(db, v)?,
            (None, _) => unreachable!("every variable width column is a blob"),
        };
        cols.push(PropColumn {
            name: (*name).to_string(),
            ty,
            meta,
        });
    }
    let directory = PropsDirectory {
        node_count,
        columns: cols,
    };
    let root = meta::write_chain(db, &directory.encode())?;
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
    /// Row order scratch for the gathers, reused across calls.
    order_scratch: Vec<u32>,
}

impl PropsReader {
    pub fn new(directory: PropsDirectory) -> Self {
        Self {
            directory,
            int_state: BTreeMap::new(),
            str_state: BTreeMap::new(),
            order_scratch: Vec::new(),
        }
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
        assert_eq!(u16::from_le_bytes(dir.encode()[..2].try_into().unwrap()), 2);
    }

    /// Encoded length of a segment meta, so the version 1 test can find
    /// the last column's type byte without hardcoding a size.
    fn meta_len(meta: &SegmentMeta) -> usize {
        let mut out = Vec::new();
        meta.encode(&mut out);
        out.len()
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
}
