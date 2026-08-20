//! A result as Arrow arrays, off the buffers the engine already filled.
//!
//! This is the fast way out of the database and the reason a client
//! links the engine crates rather than the C ABI. A result that becomes
//! one object per row costs an allocation, a type check and a reference
//! count a cell; a result that becomes Arrow costs one buffer a column
//! and no objects at all, and pandas, polars, DuckDB and every dataframe
//! in JavaScript read it without copying it again.
//!
//! The columns are not built here. [`zu_query::column`] reads a result
//! down its columns in the engine, and on a plain projection the sink
//! never built rows to read: the buffers arrive in the layout Arrow
//! already uses, values end to end, a validity bitmap that is absent
//! when nothing is null, strings as bytes and offsets. This crate puts a
//! type and a bitmap around them, which for integers, floats, booleans,
//! strings, dates, times, datetimes and durations is a move and not a
//! copy.
//!
//! It lives in the engine tree rather than in a client because there is
//! one right answer per column type and it is the same answer in every
//! language. `docs/clients/duckdb.md` is the reason: the first version
//! of this translation lived in the Python client, the Node client was
//! about to grow a second copy of it, and a second copy is a second set
//! of rules about what a year-month duration is.
//!
//! What is left to build by hand is what no buffer covers: nodes, rels,
//! paths, lists and records, which arrive as borrowed values and become
//! structs and lists in [`values`]. They are also the columns nobody
//! exports a million of.
//!
//! A column has one type, which the engine decides and this crate only
//! translates. Two refusals live here, because they are Arrow's facts
//! and not the engine's: a time with an offset has no Arrow type, and
//! neither has a handle to a graph or a binding table.
//!
//! Two ways out, one per feature, because a runtime either can read a C
//! struct or cannot. `ffi` gives an [`FFI_ArrowArrayStream`], which is
//! the C Data Interface and what Python, C, Go and the JVM take.
//! `ipc` gives the bytes of an Arrow IPC stream, which is what a
//! JavaScript runtime takes, since nothing in it can dereference a
//! pointer.
//!
//! [`FFI_ArrowArrayStream`]: https://docs.rs/arrow/latest/arrow/ffi_stream/struct.FFI_ArrowArrayStream.html

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, DurationNanosecondArray, Float64Array, Int64Array,
    IntervalMonthDayNanoArray, LargeStringArray, NullArray, StringArray, Time64NanosecondArray,
    TimestampNanosecondArray,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{
    DataType, Field, FieldRef, Fields, IntervalMonthDayNano, IntervalUnit, Schema, SchemaRef,
    TimeUnit,
};
use arrow::error::ArrowError;
use arrow::record_batch::{RecordBatch, RecordBatchOptions, RecordBatchReader};

use zu_query::column::{
    ColumnData, ColumnType, Columns, Held, HeldColumn, HeldData, Offsets, Validity,
};
use zu_query::exec::QueryResult;

pub mod values;

/// How many rows go in one record batch, when a caller has no opinion.
///
/// A result is already in memory and the arrays are built whole, so a
/// batch is a view into them rather than a copy: the boundary exists
/// because readers expect one and because a working set that fits in
/// cache is faster to consume, not because anything is allocated at it.
pub const BATCH: usize = 65_536;

/// What goes wrong on the way out.
///
/// Three kinds, because the two clients raise three different things. A
/// value of the wrong type in a column is [`Error::Type`], a value of
/// the right type that will not fit is [`Error::Value`], and anything
/// Arrow itself refused is [`Error::Arrow`], which is this crate getting
/// its own input wrong rather than the caller.
#[derive(Debug)]
pub enum Error {
    Type(String),
    Value(String),
    Arrow(ArrowError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Type(detail) | Error::Value(detail) => f.write_str(detail),
            Error::Arrow(err) => write!(f, "arrow could not build the result: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ArrowError> for Error {
    fn from(err: ArrowError) -> Error {
        Error::Arrow(err)
    }
}

/// What the tables in a result are called.
///
/// A node value carries the id of the table it came from and nothing
/// else, because that is what a row holds, and every client already
/// keeps the names it took off the catalog when the statement ran. So
/// this asks for them rather than holding a third copy, and it borrows
/// them, because a column of a hundred million nodes is a hundred
/// million lookups and none of them should allocate.
pub trait Tables {
    /// The name of a node table, or nothing for a table the catalog no
    /// longer has.
    fn node(&self, id: u32) -> Option<&str>;

    /// The name of a rel table, under the same rule.
    fn rel(&self, id: u32) -> Option<&str>;
}

/// Nothing knows any name, which is what a result with no node or rel
/// column needs and what a test that does not care about names uses.
impl Tables for () {
    fn node(&self, _id: u32) -> Option<&str> {
        None
    }

    fn rel(&self, _id: u32) -> Option<&str> {
        None
    }
}

/// A whole result as Arrow: the schema, one array a column, and the row
/// count, which a result with no columns at all still has.
///
/// The arrays are built whole and eagerly, because the refusals have to
/// happen while there is still a caller to raise them at. Cutting them
/// into batches afterwards is a slice and not a copy.
pub struct Table {
    schema: SchemaRef,
    arrays: Vec<ArrayRef>,
    rows: usize,
}

impl Table {
    /// The result read down its columns and turned into arrays.
    ///
    /// The columns come from [`QueryResult::columnar`], which is where a
    /// column of two types is refused and named.
    pub fn of<T: Tables + ?Sized>(result: &QueryResult, tables: &T) -> Result<Table, Error> {
        let columns = result
            .columnar()
            .map_err(|mixed| Error::Type(mixed.to_string()))?;
        Table::from_columns(columns, tables)
    }

    /// The same, for a caller that already has the columns in hand.
    pub fn from_columns<T: Tables + ?Sized>(
        columns: Columns<'_>,
        tables: &T,
    ) -> Result<Table, Error> {
        let rows = columns.rows;
        let mut fields = Vec::with_capacity(columns.len());
        let mut arrays = Vec::with_capacity(columns.len());
        for held in columns.columns {
            let array = column(
                held.name,
                &held.ty,
                held.data,
                held.validity,
                held.len,
                tables,
            )?;
            fields.push(field(held.name, array.data_type().clone()));
            arrays.push(array);
        }
        Ok(Table {
            schema: Arc::new(Schema::new(Fields::from(fields))),
            arrays,
            rows,
        })
    }

    /// The result taken rather than read, which is the export that
    /// copies nothing.
    ///
    /// [`Table::of`] borrows the result, and a borrowed buffer has to be
    /// copied on the way into Arrow, because the buffer behind the
    /// borrow stays where it is and the array has to own what it points
    /// at. So the whole answer is memcpied once, every time, purely
    /// because the caller might ask again.
    ///
    /// This is the other bargain. The result is consumed, its buffers
    /// are moved into Arrow arrays, and a column of a hundred million
    /// integers costs a pointer. The engine already filled those buffers
    /// in the layout Arrow reads, so between the sink and the consumer
    /// there is now no copy of the data at all.
    ///
    /// Two arms still allocate, and neither is data. A year-month
    /// interval is 64 bits in the engine and 96 in Arrow, so it is
    /// rebuilt; and a complex column is values rather than a buffer, so
    /// it is built the way it always was. Those are the columns nobody
    /// exports a million of.
    pub fn taken<T: Tables + ?Sized>(result: QueryResult, tables: &T) -> Result<Table, Error> {
        match result.into_columns() {
            Ok(held) => Table::from_held(held, tables),
            // The sink built rows rather than columns, so there are no
            // buffers to take and the borrowing path is the only path.
            // It reads the rows down their columns and allocates its
            // buffers fresh, which is not a copy of anything.
            Err(result) => Table::of(&result, tables),
        }
    }

    /// The same, for a caller that already owns the columns.
    pub fn from_held<T: Tables + ?Sized>(held: Held, tables: &T) -> Result<Table, Error> {
        let rows = held.rows;
        let mut fields = Vec::with_capacity(held.columns.len());
        let mut arrays = Vec::with_capacity(held.columns.len());
        for HeldColumn {
            name,
            ty,
            data,
            validity,
        } in held.columns
        {
            // A complex column has no buffer to move: its values are
            // read where they lie and the array is built out of them.
            // Naming the vector out here is what keeps it alive for as
            // long as the borrowed view of it exists.
            let values;
            let data = match data {
                HeldData::Null => ColumnData::Null,
                HeldData::Bool { bits } => ColumnData::Bool { bits },
                HeldData::Int(buffer) => ColumnData::Int(buffer),
                HeldData::Float(buffer) => ColumnData::Float(buffer),
                HeldData::Str(strings) => ColumnData::Str(strings),
                HeldData::Complex(held) => {
                    values = held;
                    ColumnData::Complex(values.iter().collect())
                }
            };
            let array = column(&name, &ty, data, validity, rows, tables)?;
            fields.push(field(&name, array.data_type().clone()));
            arrays.push(array);
        }
        Ok(Table {
            schema: Arc::new(Schema::new(Fields::from(fields))),
            arrays,
            rows,
        })
    }

    /// The schema, which a result with no rows has as much as any other.
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// How many rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The batches, cut as the reader asks for them.
    pub fn batches(self, rows: usize) -> Batches {
        Batches {
            schema: self.schema,
            arrays: self.arrays,
            rows: self.rows,
            batch: rows.max(1),
            at: 0,
            given: 0,
        }
    }

    /// This table as a C Data Interface stream.
    ///
    /// The arrays go across as they are. Whoever imports the stream
    /// shares the buffers with it and releases them through the callback
    /// the struct carries, so nothing here is copied and nothing is
    /// freed until the other side says so.
    #[cfg(feature = "ffi")]
    pub fn into_stream(self, rows: usize) -> arrow::ffi_stream::FFI_ArrowArrayStream {
        arrow::ffi_stream::FFI_ArrowArrayStream::new(Box::new(self.batches(rows)))
    }

    /// This table as the bytes of an Arrow IPC stream.
    ///
    /// The bytes are written once, into a buffer sized from what the
    /// arrays hold, and the schema goes first, so a reader that takes
    /// them knows the columns before the first batch and an empty result
    /// is a schema and one empty batch rather than nothing at all.
    #[cfg(feature = "ipc")]
    pub fn into_ipc(self, rows: usize) -> Result<Vec<u8>, Error> {
        let schema = self.schema();
        // Every batch is a slice of arrays that are already in memory,
        // so the writer's output is about the size of the buffers plus
        // the flatbuffer headers. Guessing it once saves the doubling.
        let guess = self
            .arrays
            .iter()
            .map(|array| array.get_buffer_memory_size())
            .sum::<usize>()
            + 1024;
        let mut bytes = Vec::with_capacity(guess);
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(&mut bytes, &schema)?;
        for batch in self.batches(rows) {
            writer.write(&batch?)?;
        }
        writer.finish()?;
        drop(writer);
        Ok(bytes)
    }
}

/// The batches, cut out of the finished arrays as they are asked for.
///
/// A result with no rows still has a schema, and a reader that gets no
/// batch at all cannot tell what the columns were, so an empty result
/// gives one empty batch and then stops.
pub struct Batches {
    schema: SchemaRef,
    arrays: Vec<ArrayRef>,
    rows: usize,
    batch: usize,
    at: usize,
    given: usize,
}

impl Iterator for Batches {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Result<RecordBatch, ArrowError>> {
        if self.at >= self.rows && self.given > 0 {
            return None;
        }
        let take = self.batch.min(self.rows - self.at);
        let columns: Vec<ArrayRef> = self
            .arrays
            .iter()
            .map(|array| array.slice(self.at, take))
            .collect();
        self.at += take;
        self.given += 1;
        // The row count goes in by hand because a result with no columns
        // still has rows, and a batch of no columns cannot say how many
        // any other way.
        Some(RecordBatch::try_new_with_options(
            self.schema.clone(),
            columns,
            &RecordBatchOptions::new().with_row_count(Some(take)),
        ))
    }
}

impl RecordBatchReader for Batches {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

/// The result as a C Data Interface stream, ready to hand to anything in
/// the process that speaks Arrow.
///
/// This borrows the result, so the buffers are copied on the way out. A
/// caller that is finished with the result should take it instead, with
/// [`Table::taken`] and [`Table::into_stream`].
#[cfg(feature = "ffi")]
pub fn stream<T: Tables + ?Sized>(
    result: &QueryResult,
    tables: &T,
    rows: usize,
) -> Result<arrow::ffi_stream::FFI_ArrowArrayStream, Error> {
    Ok(Table::of(result, tables)?.into_stream(rows))
}

/// The result taken and handed over as a C Data Interface stream, which
/// is the export that copies nothing.
#[cfg(feature = "ffi")]
pub fn stream_taken<T: Tables + ?Sized>(
    result: QueryResult,
    tables: &T,
    rows: usize,
) -> Result<arrow::ffi_stream::FFI_ArrowArrayStream, Error> {
    Ok(Table::taken(result, tables)?.into_stream(rows))
}

/// The result as the bytes of an Arrow IPC stream.
///
/// For a runtime that cannot dereference a pointer, which is every
/// JavaScript one.
#[cfg(feature = "ipc")]
pub fn ipc<T: Tables + ?Sized>(
    result: &QueryResult,
    tables: &T,
    rows: usize,
) -> Result<Vec<u8>, Error> {
    Table::of(result, tables)?.into_ipc(rows)
}

/// The same out of a result the caller is finished with.
///
/// The bytes are written either way, because that is what a serialised
/// stream is, but taking the result keeps the arrays from being a second
/// copy of it while they are written, which halves what a large answer
/// costs at its peak.
#[cfg(feature = "ipc")]
pub fn ipc_taken<T: Tables + ?Sized>(
    result: QueryResult,
    tables: &T,
    rows: usize,
) -> Result<Vec<u8>, Error> {
    Table::taken(result, tables)?.into_ipc(rows)
}

/// Every field is nullable, here and in the nested types, because a null
/// row of a struct column is a null in each of its children and there is
/// no other place to put it.
pub(crate) fn field(name: &str, data_type: DataType) -> FieldRef {
    Arc::new(Field::new(name, data_type, true))
}

pub(crate) fn item(data_type: DataType) -> FieldRef {
    field("item", data_type)
}

pub(crate) fn node_fields() -> Fields {
    Fields::from(vec![
        field("table", DataType::Utf8),
        field("offset", DataType::UInt64),
    ])
}

pub(crate) fn rel_fields() -> Fields {
    Fields::from(vec![
        field("table", DataType::Utf8),
        field("src", DataType::UInt64),
        field("dst", DataType::UInt64),
        field("ord", DataType::UInt64),
    ])
}

pub(crate) fn path_fields() -> Fields {
    Fields::from(vec![
        field(
            "nodes",
            DataType::List(item(DataType::Struct(node_fields()))),
        ),
        field("rels", DataType::List(item(DataType::Struct(rel_fields())))),
    ])
}

/// A buffer that does not match the type the engine decided for it,
/// which is this crate reading its own input wrong.
pub(crate) fn mismatch(name: &str, ty: &ColumnType) -> Error {
    Error::Arrow(ArrowError::SchemaError(format!(
        "column '{name}' came back as {} in a buffer that does not hold one",
        ty.name()
    )))
}

/// The refusal for a type Arrow has nowhere to put.
///
/// Two of them, and both are Arrow's facts rather than the engine's,
/// which is why they live here and not in `columnar()`.
pub(crate) fn unsupported(name: &str, ty: &ColumnType) -> Error {
    match ty {
        // Arrow has a time and a timestamp and nothing in between: there
        // is no time-with-offset type to put this in, and dropping the
        // offset would move the value.
        ColumnType::ZonedTime { .. } => Error::Type(format!(
            "column '{name}' holds a time with an offset, which Arrow has no type for"
        )),
        // GV60 and GV61. A handle is a reference, and a column of
        // references is a column of nothing a frame can hold: the graph
        // is in the file and the binding table is behind the handle. A
        // caller who wants one reads the rows, where it arrives as the
        // string that names it, or projects the columns of the table
        // instead of the table.
        ColumnType::Graph | ColumnType::BindingTable => Error::Type(format!(
            "column '{name}' holds a reference to a graph or a binding table, which Arrow has no type for"
        )),
        _ => mismatch(name, ty),
    }
}

/// An offset in minutes as the name Arrow keeps a timezone under.
///
/// A fixed offset rather than a region, because a fixed offset is what
/// the value carries: the engine stores when a zoned datetime happened
/// and how far from UTC it was written, and no amount of arithmetic
/// recovers `Europe/Paris` from `+01:00`.
pub(crate) fn zone(offset: i16) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// The Arrow type a column type becomes, and the two places where the
/// answer is that it does not become one.
///
/// The column name rides along because a refusal without it sends
/// somebody to read a schema by hand, and because a nested refusal is
/// still about the column it is nested in.
pub fn data_type(name: &str, ty: &ColumnType) -> Result<DataType, Error> {
    Ok(match ty {
        ColumnType::Null => DataType::Null,
        ColumnType::Bool => DataType::Boolean,
        ColumnType::Int => DataType::Int64,
        ColumnType::Float => DataType::Float64,
        ColumnType::Str => DataType::Utf8,
        ColumnType::Date => DataType::Date32,
        ColumnType::LocalTime => DataType::Time64(TimeUnit::Nanosecond),
        ColumnType::LocalDatetime => DataType::Timestamp(TimeUnit::Nanosecond, None),
        ColumnType::ZonedDatetime { offset } => {
            DataType::Timestamp(TimeUnit::Nanosecond, Some(zone(*offset).into()))
        }
        // Arrow has a year-month interval, which is exactly what this is,
        // and pyarrow cannot build a Python array of one: its type id has
        // no class behind it, so reading such a column raises
        // `KeyError: 21`. Month-day-nano is the interval every reader
        // implements, and a year-month duration is one with no days and
        // no nanoseconds in it.
        ColumnType::YearMonth => DataType::Interval(IntervalUnit::MonthDayNano),
        ColumnType::DayTime => DataType::Duration(TimeUnit::Nanosecond),
        ColumnType::Node => DataType::Struct(node_fields()),
        ColumnType::Rel => DataType::Struct(rel_fields()),
        ColumnType::Path => DataType::Struct(path_fields()),
        ColumnType::List(of) => DataType::List(item(data_type(name, of)?)),
        ColumnType::Record(fields) => DataType::Struct(
            fields
                .iter()
                .map(|(held, ty)| Ok(field(held, data_type(name, ty)?)))
                .collect::<Result<Fields, Error>>()?,
        ),
        ColumnType::ZonedTime { .. } | ColumnType::Graph | ColumnType::BindingTable => {
            return Err(unsupported(name, ty));
        }
    })
}

/// The bitmap Arrow keeps beside a buffer, out of the one the engine
/// filled. Absent means every row has a value, in both layouts.
fn nulls(validity: Option<Validity>) -> Option<NullBuffer> {
    validity
        .map(|held| NullBuffer::new(BooleanBuffer::new(Buffer::from_vec(held.bits), 0, held.len)))
}

/// One whole column as an Arrow array.
///
/// Every flat arm here moves a `Vec` into an Arrow buffer and allocates
/// nothing: the engine filled it in the layout Arrow reads, and the only
/// work left is putting a type and a bitmap around it. The two
/// exceptions are year-month intervals, which are 96 bits in Arrow and
/// 64 in the engine, and the complex types, which have no buffer.
fn column<T: Tables + ?Sized>(
    name: &str,
    ty: &ColumnType,
    data: ColumnData<'_>,
    validity: Option<Validity>,
    len: usize,
    tables: &T,
) -> Result<ArrayRef, Error> {
    let valid = nulls(validity);
    Ok(match data {
        ColumnData::Null => Arc::new(NullArray::new(len)),
        ColumnData::Bool { bits } => Arc::new(BooleanArray::new(
            BooleanBuffer::new(Buffer::from_vec(bits), 0, len),
            valid,
        )),
        ColumnData::Int(values) => Arc::new(Int64Array::new(ScalarBuffer::from(values), valid)),
        ColumnData::Float(values) => Arc::new(Float64Array::new(ScalarBuffer::from(values), valid)),
        ColumnData::Str(held) => match held.offsets {
            Offsets::I32(offsets) => Arc::new(StringArray::try_new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from_vec(held.bytes),
                valid,
            )?),
            // Past two gigabytes of text in one column, which is where a
            // 32 bit offset stops addressing the bytes. Arrow's own
            // answer is the wider type and every reader has it.
            Offsets::I64(offsets) => Arc::new(LargeStringArray::try_new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from_vec(held.bytes),
                valid,
            )?),
        },
        ColumnData::Days(values) => Arc::new(Date32Array::new(ScalarBuffer::from(values), valid)),
        ColumnData::Nanos(values) => {
            let values = ScalarBuffer::from(values);
            match ty {
                ColumnType::LocalTime => Arc::new(Time64NanosecondArray::new(values, valid)),
                ColumnType::LocalDatetime => Arc::new(TimestampNanosecondArray::new(values, valid)),
                ColumnType::ZonedDatetime { offset } => Arc::new(
                    TimestampNanosecondArray::new(values, valid).with_timezone(zone(*offset)),
                ),
                ColumnType::DayTime => Arc::new(DurationNanosecondArray::new(values, valid)),
                // A time with an offset fills a nanosecond buffer like
                // any other time, and this is where it stops.
                _ => return Err(unsupported(name, ty)),
            }
        }
        ColumnData::Months(counts) => Arc::new(IntervalMonthDayNanoArray::new(
            ScalarBuffer::from(months(name, &counts)?),
            valid,
        )),
        // The types with no buffer: nodes, rels, paths, lists, records,
        // and the two handles, which reach here as values and are refused
        // there.
        ColumnData::Complex(values) => values::build(name, ty, &values, tables)?,
    })
}

/// Month counts as the interval Arrow carries them in.
///
/// Arrow counts the months of an interval in 32 bits and the engine
/// counts them in 64, so the far end of the range has nowhere to go.
/// Refusing it is the only honest answer; wrapping would move the value
/// by centuries.
pub(crate) fn months(name: &str, counts: &[i64]) -> Result<Vec<IntervalMonthDayNano>, Error> {
    let mut months = Vec::with_capacity(counts.len());
    for (row, count) in counts.iter().enumerate() {
        let count = i32::try_from(*count).map_err(|_| {
            Error::Value(format!(
                "the duration at row {row} of column '{name}' is {count} months, which is more than an Arrow interval holds"
            ))
        })?;
        months.push(IntervalMonthDayNano::new(count, 0, 0));
    }
    Ok(months)
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::array::{StructArray, UInt64Array};
    use zu_common::Temporal;
    use zu_query::column::{Column, StrColumn};
    use zu_query::exec::Value;
    use zu_query::refs::GraphHandle;

    /// The tables of a small graph, which is what a client hands over.
    struct Catalog;

    impl Tables for Catalog {
        fn node(&self, id: u32) -> Option<&str> {
            match id {
                0 => Some("person"),
                _ => None,
            }
        }

        fn rel(&self, id: u32) -> Option<&str> {
            match id {
                0 => Some("knows"),
                _ => None,
            }
        }
    }

    /// One column, with every row present.
    fn one<'a>(name: &'a str, ty: ColumnType, data: ColumnData<'a>, len: usize) -> Columns<'a> {
        Columns {
            columns: vec![Column {
                name,
                ty,
                data,
                validity: None,
                len,
            }],
            rows: len,
        }
    }

    /// The bits the engine packs, so a test says which rows are there
    /// rather than which bytes.
    fn validity(rows: &[bool]) -> Validity {
        let mut bits = vec![0u8; rows.len().div_ceil(8)];
        let mut nulls = 0;
        for (at, &there) in rows.iter().enumerate() {
            if there {
                bits[at / 8] |= 1 << (at % 8);
            } else {
                nulls += 1;
            }
        }
        Validity {
            bits,
            len: rows.len(),
            nulls,
        }
    }

    fn only(columns: Columns<'_>) -> ArrayRef {
        let table = Table::from_columns(columns, &Catalog).expect("arrays");
        table.arrays[0].clone()
    }

    #[test]
    fn an_integer_column_arrives_as_the_buffer_the_engine_filled() {
        let array = only(one("n", ColumnType::Int, ColumnData::Int(vec![7, 8, 9]), 3));
        let ints = array.as_any().downcast_ref::<Int64Array>().expect("int64");
        assert_eq!(ints.values().to_vec(), vec![7, 8, 9]);
        assert_eq!(ints.null_count(), 0);
    }

    #[test]
    fn a_null_row_keeps_its_cell_and_loses_its_bit() {
        let mut columns = one("n", ColumnType::Int, ColumnData::Int(vec![7, 0, 9]), 3);
        columns.columns[0].validity = Some(validity(&[true, false, true]));
        let array = only(columns);
        let ints = array.as_any().downcast_ref::<Int64Array>().expect("int64");
        assert_eq!(ints.null_count(), 1);
        assert!(ints.is_null(1));
        assert_eq!(ints.value(2), 9);
    }

    #[test]
    fn a_string_column_is_utf8_and_a_wide_one_is_large_utf8() {
        let narrow = only(one(
            "s",
            ColumnType::Str,
            ColumnData::Str(StrColumn {
                bytes: b"abcd".to_vec(),
                offsets: Offsets::I32(vec![0, 2, 4]),
            }),
            2,
        ));
        assert_eq!(narrow.data_type(), &DataType::Utf8);
        let strings = narrow.as_any().downcast_ref::<StringArray>().expect("utf8");
        assert_eq!(strings.value(0), "ab");
        assert_eq!(strings.value(1), "cd");

        let wide = only(one(
            "s",
            ColumnType::Str,
            ColumnData::Str(StrColumn {
                bytes: b"abcd".to_vec(),
                offsets: Offsets::I64(vec![0, 2, 4]),
            }),
            2,
        ));
        assert_eq!(wide.data_type(), &DataType::LargeUtf8);
    }

    #[test]
    fn a_zoned_datetime_carries_the_offset_it_was_written_with() {
        let array = only(one(
            "at",
            ColumnType::ZonedDatetime { offset: 330 },
            ColumnData::Nanos(vec![1_000]),
            1,
        ));
        assert_eq!(
            array.data_type(),
            &DataType::Timestamp(TimeUnit::Nanosecond, Some("+05:30".into()))
        );
        assert_eq!(zone(-330), "-05:30");
    }

    #[test]
    fn a_year_month_duration_is_an_interval_of_months_and_nothing_else() {
        let array = only(one(
            "d",
            ColumnType::YearMonth,
            ColumnData::Months(vec![14]),
            1,
        ));
        let intervals = array
            .as_any()
            .downcast_ref::<IntervalMonthDayNanoArray>()
            .expect("interval");
        assert_eq!(intervals.value(0), IntervalMonthDayNano::new(14, 0, 0));
    }

    #[test]
    fn more_months_than_an_interval_holds_are_refused_by_row() {
        let held = Table::from_columns(
            one(
                "d",
                ColumnType::YearMonth,
                ColumnData::Months(vec![0, i64::from(i32::MAX) + 1]),
                2,
            ),
            &Catalog,
        );
        match held {
            Err(Error::Value(detail)) => {
                assert!(detail.contains("row 1"), "{detail}");
                assert!(detail.contains("column 'd'"), "{detail}");
            }
            other => panic!(
                "expected a value refusal, got {other:?}",
                other = other.err()
            ),
        }
    }

    #[test]
    fn a_time_with_an_offset_has_no_arrow_type_and_says_so() {
        let held = Table::from_columns(
            one(
                "t",
                ColumnType::ZonedTime { offset: 60 },
                ColumnData::Nanos(vec![1]),
                1,
            ),
            &Catalog,
        );
        match held {
            Err(Error::Type(detail)) => assert!(detail.contains("time with an offset"), "{detail}"),
            other => panic!(
                "expected a type refusal, got {other:?}",
                other = other.err()
            ),
        }
    }

    #[test]
    fn a_handle_to_a_graph_has_no_arrow_type_either() {
        let value = Value::Graph(GraphHandle {
            id: 0,
            schema: "/".into(),
            name: "g".into(),
            epoch: 1,
        });
        let held = Table::from_columns(
            one("g", ColumnType::Graph, ColumnData::Complex(vec![&value]), 1),
            &Catalog,
        );
        match held {
            Err(Error::Type(detail)) => {
                assert!(detail.contains("reference to a graph"), "{detail}")
            }
            other => panic!(
                "expected a type refusal, got {other:?}",
                other = other.err()
            ),
        }
    }

    #[test]
    fn a_result_with_no_rows_still_has_its_schema_and_one_batch() {
        let table = Table::from_columns(
            one("n", ColumnType::Int, ColumnData::Int(vec![]), 0),
            &Catalog,
        )
        .expect("arrays");
        assert_eq!(table.schema().field(0).data_type(), &DataType::Int64);
        let batches: Vec<RecordBatch> = table
            .batches(BATCH)
            .collect::<Result<_, ArrowError>>()
            .expect("batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 0);
        assert_eq!(batches[0].schema().field(0).name(), "n");
    }

    #[test]
    fn the_batches_are_cut_at_the_size_that_was_asked_for() {
        let table = Table::from_columns(
            one("n", ColumnType::Int, ColumnData::Int((0..10).collect()), 10),
            &Catalog,
        )
        .expect("arrays");
        let batches: Vec<RecordBatch> = table
            .batches(4)
            .collect::<Result<_, ArrowError>>()
            .expect("batches");
        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
    }

    #[test]
    fn a_node_column_is_a_struct_of_the_table_and_the_row() {
        let values = [
            Value::Node {
                table: 0,
                offset: 3,
            },
            Value::Node {
                table: 9,
                offset: 4,
            },
        ];
        let borrowed: Vec<&Value> = values.iter().collect();
        let array = only(one("p", ColumnType::Node, ColumnData::Complex(borrowed), 2));
        let held = array
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("struct");
        let tables = held
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("names");
        let offsets = held
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("offsets");
        assert_eq!(tables.value(0), "person");
        // A table the catalog no longer has still prints, as its id.
        assert_eq!(tables.value(1), "#9");
        assert_eq!(offsets.value(0), 3);
    }

    #[test]
    fn a_list_column_is_a_list_and_a_record_is_a_struct() {
        let values = [
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::List(vec![]),
        ];
        let borrowed: Vec<&Value> = values.iter().collect();
        let array = only(one(
            "xs",
            ColumnType::List(Box::new(ColumnType::Int)),
            ColumnData::Complex(borrowed),
            2,
        ));
        assert_eq!(array.data_type(), &DataType::List(item(DataType::Int64)));

        let records = [Value::Record(vec![("n".into(), Value::Int(5))])];
        let borrowed: Vec<&Value> = records.iter().collect();
        let array = only(one(
            "r",
            ColumnType::Record(vec![("n".into(), ColumnType::Int)]),
            ColumnData::Complex(borrowed),
            1,
        ));
        let held = array
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("struct");
        assert_eq!(held.column_by_name("n").expect("field").len(), 1);
    }

    #[test]
    fn a_path_is_the_nodes_it_walked_and_the_edges_it_crossed() {
        let walk = Value::Path(vec![
            Value::Node {
                table: 0,
                offset: 1,
            },
            Value::Rel {
                table: 0,
                src: 1,
                dst: 2,
                ord: 0,
            },
            Value::Node {
                table: 0,
                offset: 2,
            },
        ]);
        let borrowed = vec![&walk];
        let array = only(one("w", ColumnType::Path, ColumnData::Complex(borrowed), 1));
        let held = array
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("struct");
        let nodes = held.column_by_name("nodes").expect("nodes");
        let rels = held.column_by_name("rels").expect("rels");
        assert_eq!(nodes.len(), 1);
        assert_eq!(rels.len(), 1);
        assert_eq!(array.data_type(), &DataType::Struct(path_fields()));
    }

    #[test]
    fn a_temporal_below_the_top_level_is_built_a_value_at_a_time() {
        let values = [Value::List(vec![Value::Temporal(Temporal::Date(19_000))])];
        let borrowed: Vec<&Value> = values.iter().collect();
        let array = only(one(
            "ds",
            ColumnType::List(Box::new(ColumnType::Date)),
            ColumnData::Complex(borrowed),
            1,
        ));
        assert_eq!(array.data_type(), &DataType::List(item(DataType::Date32)));
    }

    /// A result the sink filled down its columns, which is the shape the
    /// taking path is for.
    fn filled(columns: Vec<HeldColumn>, rows: usize) -> QueryResult {
        let held = Held { columns, rows };
        QueryResult {
            columns: held.names(),
            rows: zu_query::exec::Rows::held(held),
            notices: Vec::new(),
        }
    }

    fn column_of(name: &str, ty: ColumnType, data: HeldData) -> HeldColumn {
        HeldColumn {
            name: name.to_string(),
            ty,
            data,
            validity: None,
        }
    }

    #[test]
    fn taking_a_result_moves_its_buffers_and_borrowing_one_copies_them() {
        let values: Vec<i64> = (0..4096).collect();
        let filled_at = values.as_ptr();
        let result = filled(
            vec![column_of("n", ColumnType::Int, HeldData::Int(values))],
            4096,
        );

        // Borrowed, the buffer behind the borrow stays with the result,
        // so the array has to have its own.
        let copied = Table::of(&result, &Catalog).expect("arrays");
        let ints = copied.arrays[0]
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_ne!(ints.values().as_ptr(), filled_at);
        assert_eq!(ints.value(4095), 4095);

        // Taken, the buffer the sink filled is the buffer Arrow reads.
        // Same address, so nothing between the two ever touched it.
        let moved = Table::taken(result, &Catalog).expect("arrays");
        let ints = moved.arrays[0]
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(ints.values().as_ptr(), filled_at);
        assert_eq!(ints.value(4095), 4095);
    }

    #[test]
    fn the_bytes_of_a_string_column_move_too() {
        let bytes = b"alicebob".to_vec();
        let filled_at = bytes.as_ptr();
        let result = filled(
            vec![column_of(
                "s",
                ColumnType::Str,
                HeldData::Str(StrColumn {
                    bytes,
                    offsets: Offsets::I32(vec![0, 5, 8]),
                }),
            )],
            2,
        );
        let table = Table::taken(result, &Catalog).expect("arrays");
        let strings = table.arrays[0]
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("utf8");
        assert_eq!(strings.value(1), "bob");
        assert_eq!(strings.value_data().as_ptr(), filled_at);
    }

    #[test]
    fn taken_and_borrowed_are_the_same_answer() {
        let columns = || {
            vec![
                column_of("n", ColumnType::Int, HeldData::Int(vec![1, 2])),
                column_of("f", ColumnType::Float, HeldData::Float(vec![1.5, 2.5])),
                column_of("b", ColumnType::Bool, HeldData::Bool { bits: vec![0b10] }),
                column_of("z", ColumnType::Null, HeldData::Null),
                column_of(
                    "s",
                    ColumnType::Str,
                    HeldData::Str(StrColumn {
                        bytes: b"ab".to_vec(),
                        offsets: Offsets::I32(vec![0, 1, 2]),
                    }),
                ),
                column_of(
                    "p",
                    ColumnType::Node,
                    HeldData::Complex(vec![
                        Value::Node {
                            table: 0,
                            offset: 1,
                        },
                        Value::Node {
                            table: 0,
                            offset: 2,
                        },
                    ]),
                ),
            ]
        };
        let borrowed = Table::of(&filled(columns(), 2), &Catalog).expect("arrays");
        let taken = Table::taken(filled(columns(), 2), &Catalog).expect("arrays");
        assert_eq!(borrowed.schema(), taken.schema());
        assert_eq!(borrowed.rows(), taken.rows());
        for (one, two) in borrowed.arrays.iter().zip(taken.arrays.iter()) {
            assert_eq!(one.as_ref(), two.as_ref());
        }
    }

    #[test]
    fn a_result_that_was_built_across_its_rows_is_handed_back_rather_than_half_taken() {
        let result = QueryResult {
            columns: vec!["n".to_string()],
            rows: zu_query::exec::Rows::of(vec![vec![Value::Int(1)], vec![Value::Int(2)]]),
            notices: Vec::new(),
        };
        let table = Table::taken(result, &Catalog).expect("arrays");
        assert_eq!(table.rows(), 2);
        let ints = table.arrays[0]
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(ints.values().to_vec(), vec![1, 2]);
    }

    #[test]
    fn a_column_that_arrow_refuses_is_refused_out_of_a_taken_result_too() {
        let result = filled(
            vec![column_of(
                "g",
                ColumnType::Graph,
                HeldData::Complex(vec![Value::Graph(GraphHandle {
                    id: 0,
                    schema: "/".into(),
                    name: "g".into(),
                    epoch: 1,
                })]),
            )],
            1,
        );
        match Table::taken(result, &Catalog) {
            Err(Error::Type(detail)) => {
                assert!(detail.contains("reference to a graph"), "{detail}")
            }
            other => panic!(
                "expected a type refusal, got {other:?}",
                other = other.err()
            ),
        }
    }
}
