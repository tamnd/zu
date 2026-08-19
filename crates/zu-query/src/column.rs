//! A result read down its columns instead of across its rows.
//!
//! Every client that hands a result to something else hands it over as
//! columns. Arrow is columns, pandas is columns, polars is columns, and
//! the C ABI's chunked read is columns. A result is [`Vec<Vec<Value>>`],
//! so each of those clients has been writing the same transpose, and
//! writing it badly: `zu-python` walked the whole result once per column
//! to decide the column's type and again per batch to gather the values,
//! which on a million rows of three columns is six strided passes over
//! thirty two byte cells and a `Vec` of pointers per batch per column.
//! `docs/clients/duckdb.md` measured what that costs. Twenty times
//! DuckDB, for the same bytes in the same layout at the other end.
//!
//! This is the transpose written once, in the engine, where the next
//! change makes it free. Two passes over the rows in row order: one to
//! settle each column's type, one to fill each column's buffer. Not two
//! per column, two in total, and both of them sequential over memory the
//! result already had hot. The buffers that come out are Arrow's own
//! layouts, so a client's remaining job is to name them rather than to
//! copy them: a `Vec<i64>` becomes an `Int64Array` by moving, offsets
//! and bytes become a `StringArray` by moving, and a validity bitmap is
//! Arrow's null buffer with the same bit order and the same meaning of
//! set.
//!
//! That was a transpose because the sink used to flatten.
//! `crates/zu-vector` is the vector layer the executor computes in, and
//! the place the vectors died was the sink, which filled a row at a time
//! out of them. It does not any more: on a plan with nothing above the
//! projection the sink fills [`Held`] columns straight out of the
//! vectors, [`QueryResult::columnar`] hands those buffers back without
//! walking anything, and the rows are built only if somebody asks for
//! them. No client changed a line. That is the whole reason the answer
//! is shaped like this and lives here rather than in a binding.
//!
//! The walk below is still what answers for every other plan, because a
//! sort or a dedup or a group is a step over rows and the sink hands
//! those on as rows.

use std::fmt;

use zu_common::{DurationKind, Temporal};

use crate::exec::{QueryResult, Value};

/// The type a whole column turned out to be.
///
/// A column has one type and the values decide it: the first one that
/// is not null settles it, and every value after has to fit. There is
/// exactly one widening, integer to float, because a projection that
/// returns `count(x)` on one row and `avg(x)` on another means a number
/// and every reader of the column reads it as one. Everything else that
/// does not fit is refused and named, because a column that quietly
/// became strings is worse than one that would not build.
///
/// This is not [`LogicalType`]. A logical type is what the language
/// declared, with its nullability and its width and its precision. This
/// is what a finished column holds, which is a smaller question with a
/// smaller answer: nullability is the validity bitmap and never the
/// type, and a width is a check that already happened.
///
/// [`LogicalType`]: zu_common::LogicalType
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ColumnType {
    /// Nothing but nulls. Every columnar format has a type for it, and
    /// a column of nulls with no type would be the one case a reader
    /// could not name.
    Null,
    Bool,
    Int,
    Float,
    Str,
    /// Days since 1970-01-01.
    Date,
    /// Nanoseconds since midnight.
    LocalTime,
    /// Nanoseconds since midnight in the offset's own day, and the
    /// offset in minutes. The first such value in the column names the
    /// offset, the way the first zoned datetime does.
    ZonedTime {
        offset: i16,
    },
    /// Nanoseconds since 1970-01-01T00:00:00.
    LocalDatetime,
    /// Nanoseconds since the epoch in UTC, and the offset in minutes
    /// the value was written with. Later rows may have been written
    /// somewhere else and are the same instant either way, so the
    /// offset decides how the column prints and never what it holds.
    ZonedDatetime {
        offset: i16,
    },
    /// Months.
    YearMonth,
    /// Nanoseconds.
    DayTime,
    Node,
    Rel,
    Path,
    List(Box<ColumnType>),
    Record(Vec<(String, ColumnType)>),
    /// GV60, a reference to a graph.
    Graph,
    /// GV61, a reference to a binding table.
    BindingTable,
}

impl ColumnType {
    /// What to call this in a message a person reads.
    pub fn name(&self) -> String {
        match self {
            ColumnType::Null => "nulls".into(),
            ColumnType::Bool => "booleans".into(),
            ColumnType::Int => "integers".into(),
            ColumnType::Float => "floats".into(),
            ColumnType::Str => "strings".into(),
            ColumnType::Date => "dates".into(),
            ColumnType::LocalTime => "times".into(),
            ColumnType::ZonedTime { .. } => "times with an offset".into(),
            ColumnType::LocalDatetime => "datetimes".into(),
            ColumnType::ZonedDatetime { .. } => "zoned datetimes".into(),
            ColumnType::YearMonth => "year-month durations".into(),
            ColumnType::DayTime => "day-time durations".into(),
            ColumnType::Node => "nodes".into(),
            ColumnType::Rel => "rels".into(),
            ColumnType::Path => "paths".into(),
            ColumnType::List(of) => format!("lists of {}", of.name()),
            ColumnType::Record(_) => "records".into(),
            ColumnType::Graph => "graph references".into(),
            ColumnType::BindingTable => "binding table references".into(),
        }
    }

    /// Whether a column of this type is held as one buffer of fixed
    /// width cells, which is the case a client turns into an array by
    /// moving a `Vec` rather than by walking values.
    pub fn is_flat(&self) -> bool {
        !matches!(
            self,
            ColumnType::Node
                | ColumnType::Rel
                | ColumnType::Path
                | ColumnType::List(_)
                | ColumnType::Record(_)
                | ColumnType::Graph
                | ColumnType::BindingTable
        )
    }

    /// The type of one value on its own, which is where inference
    /// starts.
    ///
    /// Fails on the one disagreement that hides below a row: a list, or
    /// a record holding one, whose own items are types no single list
    /// holds. The row loop cannot see inside a value, so the mixture is
    /// found here and named where the row number is known.
    pub fn of(value: &Value) -> Result<ColumnType, Mixture> {
        Ok(match value {
            Value::Null => ColumnType::Null,
            Value::Bool(_) => ColumnType::Bool,
            Value::Int(_) => ColumnType::Int,
            Value::Float(_) => ColumnType::Float,
            Value::Str(_) => ColumnType::Str,
            Value::Node { .. } => ColumnType::Node,
            Value::Rel { .. } => ColumnType::Rel,
            Value::Path(_) | Value::Chain(_) => ColumnType::Path,
            Value::Graph(_) => ColumnType::Graph,
            Value::BindingTable(_) => ColumnType::BindingTable,
            Value::List(items) => {
                let mut of = ColumnType::Null;
                for item in items {
                    let found = ColumnType::of(item)?;
                    let (held, arrived) = (of.name(), found.name());
                    of = ColumnType::unify(of, found).ok_or(Mixture { held, arrived })?;
                }
                ColumnType::List(Box::new(of))
            }
            Value::Record(fields) => ColumnType::Record(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), ColumnType::of(value)?)))
                    .collect::<Result<Vec<_>, Mixture>>()?,
            ),
            Value::Temporal(temporal) => match *temporal {
                Temporal::Date(_) => ColumnType::Date,
                Temporal::LocalTime(_) => ColumnType::LocalTime,
                Temporal::ZonedTime { offset, .. } => ColumnType::ZonedTime { offset },
                Temporal::LocalDatetime(_) => ColumnType::LocalDatetime,
                Temporal::ZonedDatetime { offset, .. } => ColumnType::ZonedDatetime { offset },
                Temporal::Duration(DurationKind::YearMonth, _) => ColumnType::YearMonth,
                Temporal::Duration(DurationKind::DayTime, _) => ColumnType::DayTime,
            },
        })
    }

    /// The one type two types are both, or nothing when they are not.
    pub fn unify(left: ColumnType, right: ColumnType) -> Option<ColumnType> {
        Some(match (left, right) {
            (ColumnType::Null, other) | (other, ColumnType::Null) => other,
            (ColumnType::Int, ColumnType::Float) | (ColumnType::Float, ColumnType::Int) => {
                ColumnType::Float
            }
            (ColumnType::ZonedTime { offset }, ColumnType::ZonedTime { .. }) => {
                ColumnType::ZonedTime { offset }
            }
            (ColumnType::ZonedDatetime { offset }, ColumnType::ZonedDatetime { .. }) => {
                ColumnType::ZonedDatetime { offset }
            }
            (ColumnType::List(left), ColumnType::List(right)) => {
                ColumnType::List(Box::new(ColumnType::unify(*left, *right)?))
            }
            (ColumnType::Record(left), ColumnType::Record(right)) => {
                if left.len() != right.len() {
                    return None;
                }
                let mut fields = Vec::with_capacity(left.len());
                for ((name, left), (other, right)) in left.into_iter().zip(right) {
                    if name != other {
                        return None;
                    }
                    fields.push((name, ColumnType::unify(left, right)?));
                }
                ColumnType::Record(fields)
            }
            (left, right) if left == right => left,
            _ => return None,
        })
    }
}

/// Two types found inside one value, which is a disagreement with no
/// row number on it yet. [`QueryResult::columnar`] adds the row and the
/// column and turns it into a [`MixedColumn`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mixture {
    pub held: String,
    pub arrived: String,
}

/// Two rows of one column that no single column holds.
///
/// The row is in it because that is the only part a caller can act on:
/// the column name says where to look and the row number says where to
/// look in it, and a message with neither is a message that sends
/// somebody to read a million rows by hand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MixedColumn {
    pub column: String,
    pub row: usize,
    /// What the column held before this row.
    pub held: String,
    /// What this row brought.
    pub arrived: String,
    /// Set when the disagreement is between the items of one list
    /// rather than between rows of the column.
    pub in_list: bool,
}

impl fmt::Display for MixedColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let MixedColumn {
            column,
            row,
            held,
            arrived,
            in_list,
        } = self;
        if *in_list {
            write!(
                f,
                "the list at row {row} of column '{column}' mixes {held} and {arrived}, and a \
                 columnar list holds one type"
            )
        } else {
            write!(
                f,
                "column '{column}' mixes {held} and {arrived} at row {row}, and a columnar \
                 result holds one type per column"
            )
        }
    }
}

impl std::error::Error for MixedColumn {}

/// Validity, in the layout every columnar format keeps it in: one bit
/// per row, least significant bit first, set meaning the row has a
/// value.
///
/// Absent from a [`Column`] when nothing in it is null, which is the
/// common case and the one where a reader gets to skip the AND
/// entirely. Present means at least one null, so a client never has to
/// count to find out whether the buffer is worth attaching.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Validity {
    /// Packed bits, `len.div_ceil(8)` bytes of them.
    pub bits: Vec<u8>,
    /// How many rows the bits cover.
    pub len: usize,
    /// How many of them are null, which is the count a format's null
    /// buffer carries beside the bits.
    pub nulls: usize,
}

impl Validity {
    fn new(len: usize) -> Validity {
        Validity {
            bits: vec![0xff; len.div_ceil(8)],
            len: 0,
            nulls: 0,
        }
    }

    fn push(&mut self, valid: bool) {
        if !valid {
            self.bits[self.len / 8] &= !(1u8 << (self.len % 8));
            self.nulls += 1;
        }
        self.len += 1;
    }

    /// Whether row `at` has a value.
    pub fn is_valid(&self, at: usize) -> bool {
        self.bits[at / 8] & (1u8 << (at % 8)) != 0
    }
}

/// A string column in the layout Arrow calls Utf8 and LargeUtf8: the
/// bytes of every string end to end, and `len + 1` offsets into them,
/// where row `i` spans `offsets[i]..offsets[i + 1]`.
///
/// The offsets start narrow and widen once, when the bytes pass what a
/// 32 bit offset addresses. Widening is a walk of the offsets and never
/// of the bytes, so the column that needed it pays a pass over four
/// bytes a row and the columns that did not pay nothing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StrColumn {
    pub bytes: Vec<u8>,
    pub offsets: Offsets,
}

impl StrColumn {
    /// Where row `at` starts and ends in the bytes.
    pub fn span(&self, at: usize) -> (usize, usize) {
        match &self.offsets {
            Offsets::I32(o) => (o[at] as usize, o[at + 1] as usize),
            Offsets::I64(o) => (o[at] as usize, o[at + 1] as usize),
        }
    }
}

/// String offsets, narrow until they cannot be.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Offsets {
    I32(Vec<i32>),
    I64(Vec<i64>),
}

impl Offsets {
    /// How many strings these offsets describe.
    pub fn len(&self) -> usize {
        match self {
            Offsets::I32(o) => o.len() - 1,
            Offsets::I64(o) => o.len() - 1,
        }
    }

    /// Whether the column has no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The buffer holding one column's values.
///
/// Every arm but [`ColumnData::Complex`] is a contiguous buffer of one
/// physical type, in the layout a columnar reader wants, and a client
/// turns it into an array by taking ownership rather than by walking
/// it. `Complex` is the fallback for what no primitive covers: nodes,
/// rels, paths, lists, records and the two handles. Those still arrive
/// as values, borrowed from the result rather than cloned out of it,
/// and a client builds them the way it always did. They are also the
/// columns nobody exports a million of.
///
/// A null row still occupies its cell in a flat buffer, holding
/// whatever the type's zero is. That is what the validity bitmap is
/// for, and it is why the buffers are strided and can be moved rather
/// than rebuilt.
#[derive(Clone, PartialEq, Debug)]
pub enum ColumnData<'a> {
    /// A column of nothing but nulls. It has a length and no buffer,
    /// because there is nothing to put in one.
    Null,
    /// One bit per row, least significant bit first, the same packing
    /// as [`Validity`].
    Bool {
        bits: Vec<u8>,
    },
    Int(Vec<i64>),
    Float(Vec<f64>),
    Str(StrColumn),
    /// Days, for [`ColumnType::Date`].
    Days(Vec<i32>),
    /// Nanoseconds, for every time, datetime and day-time duration.
    Nanos(Vec<i64>),
    /// Months, for [`ColumnType::YearMonth`].
    Months(Vec<i64>),
    /// The values themselves, for the types no buffer covers.
    Complex(Vec<&'a Value>),
}

/// One column of a result: its name, the type its rows turned out to
/// be, its buffer, and which of its rows have a value.
#[derive(Clone, PartialEq, Debug)]
pub struct Column<'a> {
    pub name: &'a str,
    pub ty: ColumnType,
    pub data: ColumnData<'a>,
    /// Absent when every row has a value.
    pub validity: Option<Validity>,
    pub len: usize,
}

/// A whole result read down its columns.
#[derive(Clone, PartialEq, Debug)]
pub struct Columns<'a> {
    pub columns: Vec<Column<'a>>,
    /// Rows in the result, which is every column's length and is also
    /// the answer for a result with no columns at all.
    pub rows: usize,
}

impl<'a> Columns<'a> {
    /// How many columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether the result had no columns, which is what a statement
    /// with no projection gives back.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// A whole result the executor filled down its columns, owning every
/// buffer in it.
///
/// [`Columns`] is the same answer borrowed: its complex arm points at
/// values that live in a [`QueryResult`], which is what lets the walk
/// below hand back node and list columns without cloning them. A sink
/// that filled the buffers itself has nothing to point at, so this is
/// the shape it keeps them in, and [`Held::borrow`] is the view a client
/// reads.
///
/// The arms are the ones a projection produces: a stored column is an
/// integer, a real or a string, a row id is an integer, and everything
/// else a RETURN item can name is one value at a time. Dates and
/// durations have buffers in [`ColumnData`] and no arm here on purpose,
/// because the only way one reaches a projection in this executor is as
/// a constant, and a constant column that went through a buffer and
/// back would have to be told which of its rows carried which offset.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Held {
    pub columns: Vec<HeldColumn>,
    /// Rows in the result, which is every column's length.
    pub rows: usize,
}

/// One column of a [`Held`] result.
#[derive(Clone, PartialEq, Debug)]
pub struct HeldColumn {
    pub name: String,
    pub ty: ColumnType,
    pub data: HeldData,
    /// Absent when every row has a value.
    pub validity: Option<Validity>,
}

/// The buffer one [`HeldColumn`] holds, in the layouts [`ColumnData`]
/// describes.
#[derive(Clone, PartialEq, Debug)]
pub enum HeldData {
    Null,
    Bool {
        bits: Vec<u8>,
    },
    Int(Vec<i64>),
    Float(Vec<f64>),
    Str(StrColumn),
    /// The values themselves, for nodes and for anything else no buffer
    /// covers.
    Complex(Vec<Value>),
}

impl Held {
    /// This answer as the borrowed view a client reads.
    ///
    /// The flat buffers are copied and the complex ones are borrowed,
    /// which is the one copy left on the export path: a memcpy of the
    /// bytes that are already contiguous and already the right shape,
    /// against the two strided walks of a row vector this replaced. It
    /// goes when a client takes the result by value instead of by
    /// reference, and not before, because a caller is allowed to ask
    /// twice.
    pub fn borrow(&self) -> Columns<'_> {
        let columns = self
            .columns
            .iter()
            .map(|held| Column {
                name: held.name.as_str(),
                ty: held.ty.clone(),
                data: match &held.data {
                    HeldData::Null => ColumnData::Null,
                    HeldData::Bool { bits } => ColumnData::Bool { bits: bits.clone() },
                    HeldData::Int(values) => ColumnData::Int(values.clone()),
                    HeldData::Float(values) => ColumnData::Float(values.clone()),
                    HeldData::Str(strings) => ColumnData::Str(strings.clone()),
                    HeldData::Complex(values) => ColumnData::Complex(values.iter().collect()),
                },
                validity: held.validity.clone(),
                len: self.rows,
            })
            .collect();
        Columns {
            columns,
            rows: self.rows,
        }
    }

    /// The column names, which are the result's own.
    pub fn names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// This answer read back across its rows, for the callers that want
    /// rows after all.
    ///
    /// One row per row and one allocation per row, which is what a row
    /// costs whoever builds it. Reading down the columns to write across
    /// the rows would touch every buffer once per row and miss the cache
    /// doing it, so this fills the rows a column at a time instead: the
    /// row vectors are all there before any value goes in, and each
    /// column is then read start to end.
    pub fn rows(&self) -> Vec<Vec<Value>> {
        let width = self.columns.len();
        let mut rows: Vec<Vec<Value>> = (0..self.rows)
            .map(|_| {
                let mut row = Vec::with_capacity(width);
                row.resize_with(width, || Value::Null);
                row
            })
            .collect();
        for (ix, held) in self.columns.iter().enumerate() {
            for (at, row) in rows.iter_mut().enumerate() {
                if held.validity.as_ref().is_some_and(|v| !v.is_valid(at)) {
                    continue;
                }
                row[ix] = held.value(at);
            }
        }
        rows
    }
}

impl HeldColumn {
    /// The value at row `at`, which the caller has already found to be
    /// there.
    fn value(&self, at: usize) -> Value {
        match &self.data {
            HeldData::Null => Value::Null,
            HeldData::Bool { bits } => Value::Bool(bits[at / 8] & (1u8 << (at % 8)) != 0),
            HeldData::Int(values) => Value::Int(values[at]),
            HeldData::Float(values) => Value::Float(values[at]),
            HeldData::Str(strings) => {
                let (from, to) = strings.span(at);
                Value::Str(String::from_utf8_lossy(&strings.bytes[from..to]).into_owned())
            }
            HeldData::Complex(values) => values[at].clone(),
        }
    }
}

/// The per-column state the fill pass writes into.
enum Fill<'a> {
    Null,
    Bool(Vec<u8>),
    Int(Vec<i64>),
    Float(Vec<f64>),
    Str { bytes: Vec<u8>, offsets: Vec<i64> },
    Days(Vec<i32>),
    Nanos(Vec<i64>),
    Months(Vec<i64>),
    Complex(Vec<&'a Value>),
}

impl<'a> Fill<'a> {
    fn new(ty: &ColumnType, rows: usize) -> Fill<'a> {
        match ty {
            ColumnType::Null => Fill::Null,
            ColumnType::Bool => Fill::Bool(vec![0; rows.div_ceil(8)]),
            ColumnType::Int => Fill::Int(Vec::with_capacity(rows)),
            ColumnType::Float => Fill::Float(Vec::with_capacity(rows)),
            ColumnType::Str => Fill::Str {
                // One offset more than there are rows: the last one
                // closes the last string.
                offsets: {
                    let mut offsets = Vec::with_capacity(rows + 1);
                    offsets.push(0);
                    offsets
                },
                bytes: Vec::new(),
            },
            ColumnType::Date => Fill::Days(Vec::with_capacity(rows)),
            ColumnType::YearMonth => Fill::Months(Vec::with_capacity(rows)),
            ColumnType::LocalTime
            | ColumnType::ZonedTime { .. }
            | ColumnType::LocalDatetime
            | ColumnType::ZonedDatetime { .. }
            | ColumnType::DayTime => Fill::Nanos(Vec::with_capacity(rows)),
            _ => Fill::Complex(Vec::with_capacity(rows)),
        }
    }

    /// Row `at` into the buffer. Answers whether the row has a value,
    /// which is what the validity bitmap wants; a null still occupies
    /// its cell.
    ///
    /// `at` is only read by the bit-packed arm, which cannot count its
    /// own rows the way a `Vec` does.
    fn push(&mut self, at: usize, value: &'a Value) -> bool {
        match self {
            Fill::Null => false,
            Fill::Complex(values) => {
                values.push(value);
                !matches!(value, Value::Null)
            }
            Fill::Bool(bits) => match value {
                Value::Bool(true) => {
                    bits[at / 8] |= 1u8 << (at % 8);
                    true
                }
                // The buffer was born zeroed, so false and null are
                // both already written.
                Value::Bool(false) => true,
                _ => false,
            },
            Fill::Int(values) => match value {
                Value::Int(n) => {
                    values.push(*n);
                    true
                }
                _ => {
                    values.push(0);
                    false
                }
            },
            Fill::Float(values) => match value {
                Value::Float(f) => {
                    values.push(*f);
                    true
                }
                // Widened where the column holds both, which is the
                // only way an integer reaches a float column.
                Value::Int(n) => {
                    values.push(*n as f64);
                    true
                }
                _ => {
                    values.push(0.0);
                    false
                }
            },
            Fill::Str { bytes, offsets } => match value {
                Value::Str(s) => {
                    bytes.extend_from_slice(s.as_bytes());
                    offsets.push(bytes.len() as i64);
                    true
                }
                _ => {
                    offsets.push(bytes.len() as i64);
                    false
                }
            },
            Fill::Days(values) => match value {
                Value::Temporal(Temporal::Date(days)) => {
                    values.push(*days);
                    true
                }
                _ => {
                    values.push(0);
                    false
                }
            },
            Fill::Months(values) => match value {
                Value::Temporal(Temporal::Duration(DurationKind::YearMonth, months)) => {
                    values.push(*months);
                    true
                }
                _ => {
                    values.push(0);
                    false
                }
            },
            Fill::Nanos(values) => match value {
                Value::Temporal(
                    Temporal::LocalTime(nanos)
                    | Temporal::ZonedTime { nanos, .. }
                    | Temporal::LocalDatetime(nanos)
                    | Temporal::ZonedDatetime { nanos, .. }
                    | Temporal::Duration(DurationKind::DayTime, nanos),
                ) => {
                    values.push(*nanos);
                    true
                }
                _ => {
                    values.push(0);
                    false
                }
            },
        }
    }

    fn finish(self, rows: usize) -> ColumnData<'a> {
        match self {
            Fill::Null => ColumnData::Null,
            Fill::Bool(bits) => ColumnData::Bool { bits },
            Fill::Int(values) => ColumnData::Int(values),
            Fill::Float(values) => ColumnData::Float(values),
            Fill::Days(values) => ColumnData::Days(values),
            Fill::Nanos(values) => ColumnData::Nanos(values),
            Fill::Months(values) => ColumnData::Months(values),
            Fill::Complex(values) => ColumnData::Complex(values),
            Fill::Str { bytes, offsets } => {
                debug_assert_eq!(offsets.len(), rows + 1);
                // Narrow if the bytes fit a 32 bit offset, which they
                // do for every result short of two gigabytes of text.
                // The check is the last offset and the walk is over
                // offsets, never over the bytes.
                let offsets = if bytes.len() <= i32::MAX as usize {
                    Offsets::I32(offsets.into_iter().map(|off| off as i32).collect())
                } else {
                    Offsets::I64(offsets)
                };
                ColumnData::Str(StrColumn { bytes, offsets })
            }
        }
    }
}

impl QueryResult {
    /// This result read down its columns.
    ///
    /// Two passes over the rows in row order, both of them sequential:
    /// the first settles each column's type, the second fills each
    /// column's buffer. Not two passes per column. A client that wanted
    /// Arrow used to walk the result once per column to infer and once
    /// per column per batch to gather, which on a wide result is a
    /// multiple of this and on any result is strided.
    ///
    /// Fails only where a column's rows are types no single column
    /// holds, and says which column, which row, and what the two types
    /// were. Every other refusal belongs to the client, because the
    /// format decides it: a time with an offset has no Arrow type and
    /// that is Arrow's fact rather than the engine's.
    ///
    /// The buffers are owned and the complex columns borrow, so the
    /// result stays readable afterwards and a client may ask twice.
    ///
    /// A sink that filled the columns itself is answered from those and
    /// nothing here runs, which is the case that matters: it is the
    /// plain projection, it is what an export asks for, and it is the
    /// one where the rows are never built at all.
    pub fn columnar(&self) -> Result<Columns<'_>, MixedColumn> {
        if let Some(held) = self.rows.columns() {
            return Ok(held.borrow());
        }
        let rows = self.rows.len();
        let width = self.columns.len();

        // Pass one: the type of each column. Row order rather than
        // column order, so the cells are read the way they are laid
        // out, and one traversal settles every column at once.
        let mut types = vec![ColumnType::Null; width];
        for (at, row) in self.rows.iter().enumerate() {
            for (ix, value) in row.iter().enumerate().take(width) {
                let found = ColumnType::of(value).map_err(|mixture| MixedColumn {
                    column: self.columns[ix].clone(),
                    row: at,
                    held: mixture.held,
                    arrived: mixture.arrived,
                    in_list: true,
                })?;
                // The overwhelming case is a column that settled on its
                // type at row zero and agrees with itself forever. Test
                // it before unifying anything.
                if types[ix] == found {
                    continue;
                }
                let (held, arrived) = (types[ix].name(), found.name());
                types[ix] = ColumnType::unify(types[ix].clone(), found).ok_or(MixedColumn {
                    column: self.columns[ix].clone(),
                    row: at,
                    held,
                    arrived,
                    in_list: false,
                })?;
            }
        }

        // Pass two: the buffers. Same order, same reason.
        let mut fills: Vec<Fill<'_>> = types.iter().map(|ty| Fill::new(ty, rows)).collect();
        let mut validity: Vec<Option<Validity>> = types
            .iter()
            .map(|ty| (*ty != ColumnType::Null).then(|| Validity::new(rows)))
            .collect();
        for (at, row) in self.rows.iter().enumerate() {
            for (ix, value) in row.iter().enumerate().take(width) {
                let valid = fills[ix].push(at, value);
                if let Some(bitmap) = validity[ix].as_mut() {
                    bitmap.push(valid);
                }
            }
        }

        let columns = self
            .columns
            .iter()
            .zip(types)
            .zip(fills)
            .zip(validity)
            .map(|(((name, ty), fill), bitmap)| Column {
                name: name.as_str(),
                ty,
                data: fill.finish(rows),
                // A bitmap with nothing null in it is a buffer every
                // reader would AND against for no reason.
                validity: bitmap.filter(|bitmap| bitmap.nulls > 0),
                len: rows,
            })
            .collect();
        Ok(Columns { columns, rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult::new(columns.iter().map(|c| c.to_string()).collect(), rows)
    }

    fn str(s: &str) -> Value {
        Value::Str(s.to_string())
    }

    #[test]
    fn an_integer_column_is_one_buffer() {
        let result = answer(&["n"], vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        let columns = result.columnar().expect("one type");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns.rows, 2);
        assert_eq!(columns.columns[0].name, "n");
        assert_eq!(columns.columns[0].ty, ColumnType::Int);
        assert_eq!(columns.columns[0].data, ColumnData::Int(vec![1, 2]));
        assert!(columns.columns[0].validity.is_none());
    }

    #[test]
    fn a_null_takes_its_cell_and_its_bit() {
        let result = answer(
            &["n"],
            vec![vec![Value::Int(1)], vec![Value::Null], vec![Value::Int(3)]],
        );
        let columns = result.columnar().expect("one type");
        let column = &columns.columns[0];
        // The cell is there so the buffer stays strided; the bitmap is
        // what says it means nothing.
        assert_eq!(column.data, ColumnData::Int(vec![1, 0, 3]));
        let validity = column.validity.as_ref().expect("a null");
        assert_eq!(validity.nulls, 1);
        assert_eq!(validity.len, 3);
        assert!(validity.is_valid(0));
        assert!(!validity.is_valid(1));
        assert!(validity.is_valid(2));
    }

    #[test]
    fn a_column_of_nulls_has_a_type_and_no_buffer() {
        let result = answer(&["n"], vec![vec![Value::Null], vec![Value::Null]]);
        let columns = result.columnar().expect("one type");
        assert_eq!(columns.columns[0].ty, ColumnType::Null);
        assert_eq!(columns.columns[0].data, ColumnData::Null);
        // Everything is null, so a bitmap would say the same thing the
        // type already says.
        assert!(columns.columns[0].validity.is_none());
    }

    #[test]
    fn integers_widen_to_floats_and_nothing_else_does() {
        let result = answer(&["n"], vec![vec![Value::Int(1)], vec![Value::Float(2.5)]]);
        let columns = result.columnar().expect("a number column");
        assert_eq!(columns.columns[0].ty, ColumnType::Float);
        assert_eq!(columns.columns[0].data, ColumnData::Float(vec![1.0, 2.5]));

        let mixed = answer(&["n"], vec![vec![Value::Int(1)], vec![str("two")]]);
        let refused = mixed
            .columnar()
            .expect_err("integers and strings are not one column");
        assert_eq!(refused.row, 1);
        assert_eq!(refused.column, "n");
        assert_eq!(refused.held, "integers");
        assert_eq!(refused.arrived, "strings");
        assert!(refused.to_string().contains("column 'n' mixes integers"));
    }

    #[test]
    fn the_widening_holds_whichever_way_round_it_arrives() {
        let result = answer(&["n"], vec![vec![Value::Float(2.5)], vec![Value::Int(1)]]);
        let columns = result.columnar().expect("a number column");
        assert_eq!(columns.columns[0].data, ColumnData::Float(vec![2.5, 1.0]));
    }

    #[test]
    fn strings_are_bytes_and_offsets() {
        let result = answer(
            &["s"],
            vec![vec![str("ab")], vec![Value::Null], vec![str("cde")]],
        );
        let columns = result.columnar().expect("one type");
        let ColumnData::Str(strings) = &columns.columns[0].data else {
            panic!("a string column");
        };
        assert_eq!(strings.bytes, b"abcde");
        // The null's offsets are equal, which is how a variable width
        // buffer spells an empty span.
        assert_eq!(strings.offsets, Offsets::I32(vec![0, 2, 2, 5]));
        assert_eq!(strings.offsets.len(), 3);
        assert_eq!(columns.columns[0].validity.as_ref().unwrap().nulls, 1);
    }

    #[test]
    fn booleans_are_bits() {
        let mut rows = Vec::new();
        for at in 0..9 {
            rows.push(vec![Value::Bool(at % 4 == 0)]);
        }
        let result = answer(&["b"], rows);
        let columns = result.columnar().expect("one type");
        let ColumnData::Bool { bits } = &columns.columns[0].data else {
            panic!("a boolean column");
        };
        assert_eq!(bits.len(), 2);
        assert_eq!(bits[0], 0b0001_0001);
        assert_eq!(bits[1], 0b0000_0001);
    }

    #[test]
    fn every_column_is_settled_in_one_walk() {
        let result = answer(
            &["n", "s", "f"],
            vec![
                vec![Value::Int(1), str("a"), Value::Float(1.5)],
                vec![Value::Int(2), str("bb"), Value::Null],
            ],
        );
        let columns = result.columnar().expect("three types");
        assert_eq!(columns.columns[0].ty, ColumnType::Int);
        assert_eq!(columns.columns[1].ty, ColumnType::Str);
        assert_eq!(columns.columns[2].ty, ColumnType::Float);
        assert_eq!(columns.columns[2].data, ColumnData::Float(vec![1.5, 0.0]));
        assert_eq!(columns.columns[2].validity.as_ref().unwrap().nulls, 1);
    }

    #[test]
    fn temporals_land_in_the_buffer_their_unit_belongs_to() {
        let result = answer(
            &["d", "t", "ym"],
            vec![vec![
                Value::Temporal(Temporal::Date(19_000)),
                Value::Temporal(Temporal::LocalTime(3_600_000_000_000)),
                Value::Temporal(Temporal::Duration(DurationKind::YearMonth, 14)),
            ]],
        );
        let columns = result.columnar().expect("three types");
        assert_eq!(columns.columns[0].data, ColumnData::Days(vec![19_000]));
        assert_eq!(
            columns.columns[1].data,
            ColumnData::Nanos(vec![3_600_000_000_000])
        );
        assert_eq!(columns.columns[2].data, ColumnData::Months(vec![14]));
    }

    #[test]
    fn the_first_zoned_value_names_the_offset() {
        let result = answer(
            &["z"],
            vec![
                vec![Value::Temporal(Temporal::ZonedDatetime {
                    nanos: 1,
                    offset: 60,
                })],
                vec![Value::Temporal(Temporal::ZonedDatetime {
                    nanos: 2,
                    offset: -300,
                })],
            ],
        );
        let columns = result.columnar().expect("one type");
        assert_eq!(
            columns.columns[0].ty,
            ColumnType::ZonedDatetime { offset: 60 }
        );
        assert_eq!(columns.columns[0].data, ColumnData::Nanos(vec![1, 2]));
    }

    #[test]
    fn a_node_column_keeps_its_values() {
        let node = Value::Node {
            table: 0,
            offset: 7,
        };
        let result = answer(&["n"], vec![vec![node.clone()]]);
        let columns = result.columnar().expect("one type");
        assert_eq!(columns.columns[0].ty, ColumnType::Node);
        assert!(!columns.columns[0].ty.is_flat());
        let ColumnData::Complex(values) = &columns.columns[0].data else {
            panic!("a complex column");
        };
        assert_eq!(values.len(), 1);
        assert_eq!(*values[0], node);
    }

    #[test]
    fn a_list_column_that_disagrees_with_itself_names_the_row() {
        let result = answer(
            &["xs"],
            vec![
                vec![Value::List(vec![Value::Int(1)])],
                vec![Value::List(vec![Value::Int(1), str("two")])],
            ],
        );
        let refused = result.columnar().expect_err("one list, two types");
        assert!(refused.in_list);
        assert_eq!(refused.row, 1);
        assert!(
            refused
                .to_string()
                .contains("the list at row 1 of column 'xs'")
        );
    }

    #[test]
    fn lists_of_one_type_unify_across_rows() {
        let result = answer(
            &["xs"],
            vec![
                vec![Value::List(vec![Value::Int(1)])],
                vec![Value::List(vec![Value::Float(2.5)])],
            ],
        );
        let columns = result.columnar().expect("lists of numbers");
        assert_eq!(
            columns.columns[0].ty,
            ColumnType::List(Box::new(ColumnType::Float))
        );
    }

    #[test]
    fn records_unify_field_by_field_and_refuse_a_different_shape() {
        let one = Value::record(vec![("a".into(), Value::Int(1))]);
        let two = Value::record(vec![("a".into(), Value::Float(2.5))]);
        let widened = answer(&["r"], vec![vec![one.clone()], vec![two]]);
        let columns = widened.columnar().expect("one record type");
        assert_eq!(
            columns.columns[0].ty,
            ColumnType::Record(vec![("a".into(), ColumnType::Float)])
        );

        let other = Value::record(vec![("b".into(), Value::Int(1))]);
        let renamed = answer(&["r"], vec![vec![one], vec![other]]);
        renamed
            .columnar()
            .expect_err("two field names are two record types");
    }

    #[test]
    fn a_result_with_no_rows_still_has_its_columns() {
        let result = answer(&["n", "s"], Vec::new());
        let columns = result.columnar().expect("no rows, no disagreement");
        assert_eq!(columns.rows, 0);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns.columns[0].ty, ColumnType::Null);
        assert_eq!(columns.columns[0].len, 0);
    }

    #[test]
    fn a_result_with_no_columns_is_empty_and_not_an_error() {
        let empty = QueryResult::default();
        let columns = empty.columnar().expect("nothing to mix");
        assert!(columns.is_empty());
        assert_eq!(columns.rows, 0);
    }
}
