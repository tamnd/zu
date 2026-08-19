//! The bulk-load appender of dx/04 §6.
//!
//! A row at a time through a write statement is the wrong shape for
//! loading data: every row would be parsed, bound, planned, and
//! committed, and the commit is the expensive part, so a million rows
//! would be a million commits and the load would be dominated by
//! durability work nobody asked for. The appender is the other shape.
//! Rows go into per-column buffers in memory, and a flush turns the
//! whole buffer into sealed segments in the data file with one
//! `IngestRef` frame in the log, which is the WAL bypass docs/08 §6
//! describes: the log stays a handful of bytes whether the flush
//! carries ten rows or ten million.
//!
//! ```no_run
//! use zu::Database;
//!
//! let db = Database::open("social.zu1")?;
//! let mut conn = db.connect()?;
//! let mut app = conn.appender("person")?;
//! app.append_row((1i64, "ada"))?;
//! app.append_row((2i64, "grace"))?;
//! app.close()?;
//! # Ok::<(), zu::ZuError>(())
//! ```
//!
//! A row is every column of the table, in the order the table declares
//! them, and a column is a position rather than a name: naming the
//! columns per row would cost a lookup per value on the one path where
//! per-value cost is the whole story, and a loader knows its own
//! column order.
//!
//! A flush is one commit. When it returns the rows are durable and
//! every later query sees them, and before it returns nothing sees
//! anything, because a flush ends by folding the sealed segments into
//! the base the query path reads. That fold costs time proportional to
//! the table rather than to the batch, which is the whole reason the
//! appender buffers: flush once per load, not once per row.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use zu_common::{DurationKind, FloatBits, IntBits, LogicalType, Result, Temporal, ZuError};

use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{checkpoint_fold, recover};
use crate::zu1::graph::GraphReader;
use crate::zu1::ingest::{ingest_edges, ingest_nodes};
use crate::zu1::props::{PropValues, load_props};
use crate::zu1::wal::Wal;

/// One value on its way into a column.
///
/// This is deliberately not [`crate::query::Value`]. A value coming out
/// of a query owns its string, because it outlives the row it was read
/// from; a value going into a column borrows one, because the caller
/// already has it and the appender is about to copy it into a buffer
/// either way. On a string column that is the difference between one
/// copy per row and two.
///
/// There is no null. A column that holds a null cannot be appended to
/// at all, for the reason [`Appender`] gives, so a null here could only
/// ever be refused, and refusing it in the type is better than
/// refusing it at flush.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Field<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    Bytes(&'a [u8]),
    Temporal(Temporal),
}

impl From<bool> for Field<'_> {
    fn from(v: bool) -> Self {
        Field::Bool(v)
    }
}

impl From<i64> for Field<'_> {
    fn from(v: i64) -> Self {
        Field::Int(v)
    }
}

impl From<i32> for Field<'_> {
    fn from(v: i32) -> Self {
        Field::Int(i64::from(v))
    }
}

impl From<u32> for Field<'_> {
    fn from(v: u32) -> Self {
        Field::Int(i64::from(v))
    }
}

impl From<f64> for Field<'_> {
    fn from(v: f64) -> Self {
        Field::Float(v)
    }
}

impl<'a> From<&'a str> for Field<'a> {
    fn from(v: &'a str) -> Self {
        Field::Str(v)
    }
}

impl<'a> From<&'a String> for Field<'a> {
    fn from(v: &'a String) -> Self {
        Field::Str(v)
    }
}

impl<'a> From<&'a [u8]> for Field<'a> {
    fn from(v: &'a [u8]) -> Self {
        Field::Bytes(v)
    }
}

impl From<Temporal> for Field<'_> {
    fn from(v: Temporal) -> Self {
        Field::Temporal(v)
    }
}

/// Where a row's fields go as they are read out of it.
///
/// A row writes itself into the appender's column buffers directly,
/// with no vector of values in between, because the row is the hot
/// path of a bulk load and an allocation per row is the one cost the
/// appender exists to avoid. A field that a column will not take
/// leaves the whole row unwritten: the sink counts what it kept, and
/// the appender pops it back off on the way out, so a refused row is a
/// row that never happened rather than half of one nobody can find.
pub struct RowSink<'t, 'a> {
    target: &'t mut Target,
    /// Which column the next field goes into, and afterwards how many
    /// of them were written.
    next: usize,
    err: Option<ZuError>,
    /// The sink hands out no borrows, so nothing here holds an `'a`;
    /// the parameter is on the type so that a field written into it
    /// cannot outlive what it borrowed.
    marker: PhantomData<&'a ()>,
}

impl<'a> RowSink<'_, 'a> {
    /// Writes the next field of this row.
    ///
    /// A field of the wrong type, or one past the end of the row, is
    /// recorded and the rest of the row is written nowhere. Nothing
    /// here returns a result, because a row implementation pushing its
    /// fields in order has nothing useful to do with one and the
    /// appender is where the error belongs.
    pub fn push(&mut self, field: Field<'a>) {
        if self.err.is_some() {
            return;
        }
        let i = self.next;
        let out = match &mut *self.target {
            Target::Nodes { cols, .. } => match cols.get_mut(i) {
                Some(col) => col.push(field).map_err(|wanted| {
                    ZuError::InvalidArgument(format!(
                        "value {i} of the row is not what column {i} takes, which is {wanted}"
                    ))
                }),
                None => Err(ZuError::InvalidArgument(format!(
                    "row carries more than the {} columns the table has",
                    cols.len()
                ))),
            },
            Target::Edges { src, dst, .. } => match (i, field) {
                (0, Field::Int(v)) if v >= 0 => {
                    src.push(v as u64);
                    Ok(())
                }
                (1, Field::Int(v)) if v >= 0 => {
                    dst.push(v as u64);
                    Ok(())
                }
                (0 | 1, _) => Err(ZuError::InvalidArgument(
                    "a rel table row is a source offset and a destination offset, \
                     both of them counts"
                        .into(),
                )),
                _ => Err(ZuError::InvalidArgument(
                    "a rel table row carries two values".into(),
                )),
            },
        };
        match out {
            Ok(()) => self.next += 1,
            Err(e) => self.err = Some(e),
        }
    }
}

/// What [`Appender::append_row`] accepts: a tuple of things that are
/// fields, or a slice of fields already.
///
/// A tuple is what a loader writing rows by hand has, and a slice is
/// what a loader forwarding rows from somewhere else has.
pub trait AppendRow<'a> {
    /// Writes this row's fields, in column order.
    fn write_into(self, sink: &mut RowSink<'_, 'a>);
}

impl<'a> AppendRow<'a> for &[Field<'a>] {
    fn write_into(self, sink: &mut RowSink<'_, 'a>) {
        for field in self {
            sink.push(*field);
        }
    }
}

impl<'a, const N: usize> AppendRow<'a> for [Field<'a>; N] {
    fn write_into(self, sink: &mut RowSink<'_, 'a>) {
        for field in self {
            sink.push(field);
        }
    }
}

macro_rules! row_tuple {
    ($($name:ident),+) => {
        impl<'a, $($name: Into<Field<'a>>),+> AppendRow<'a> for ($($name,)+) {
            #[allow(non_snake_case)]
            fn write_into(self, sink: &mut RowSink<'_, 'a>) {
                let ($($name,)+) = self;
                $(sink.push($name.into());)+
            }
        }
    };
}

row_tuple!(A);
row_tuple!(A, B);
row_tuple!(A, B, C);
row_tuple!(A, B, C, D);
row_tuple!(A, B, C, D, E);
row_tuple!(A, B, C, D, E, F);
row_tuple!(A, B, C, D, E, F, G);
row_tuple!(A, B, C, D, E, F, G, H);
row_tuple!(A, B, C, D, E, F, G, H, I);
row_tuple!(A, B, C, D, E, F, G, H, I, J);
row_tuple!(A, B, C, D, E, F, G, H, I, J, K);
row_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

/// One column's buffered values, in the shape [`PropValues`] wants
/// them.
///
/// The arms are the storage arms and not the logical types: a date and
/// a count are both words, and which of the two a word is comes from
/// the column's declared type, which the appender checked once when it
/// opened. Buffering in the storage shape means a flush hands the
/// buffer straight over with no pass to convert it.
#[derive(Debug)]
enum ColBuf {
    Int(Vec<u64>),
    Bool(Vec<bool>),
    Float(Vec<f64>),
    Date(Vec<i32>),
    LocalTime(Vec<i64>),
    LocalDatetime(Vec<i64>),
    Duration(DurationKind, Vec<i64>),
    Str(Vec<Vec<u8>>),
    Bytes(Vec<Vec<u8>>),
}

impl ColBuf {
    /// The buffer a column of this type appends into, or `None` for a
    /// type the ingest path cannot carry.
    ///
    /// The match is on the exact declared type and not on its family,
    /// because that is what the ingest checks: it compares the stored
    /// column's type against the type its values claim, so an `INT32`
    /// column or a `VARCHAR(20)` one has no buffer here even though
    /// its bits would fit the same lane. Better to say so when the
    /// appender opens than to fill a buffer that cannot be committed.
    fn for_type(ty: &LogicalType) -> Option<ColBuf> {
        Some(match ty {
            LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            } => ColBuf::Int(Vec::new()),
            LogicalType::Bool => ColBuf::Bool(Vec::new()),
            LogicalType::Float {
                bits: FloatBits::B64,
                precision: None,
            } => ColBuf::Float(Vec::new()),
            LogicalType::Date => ColBuf::Date(Vec::new()),
            LogicalType::LocalTime => ColBuf::LocalTime(Vec::new()),
            LogicalType::LocalDatetime => ColBuf::LocalDatetime(Vec::new()),
            LogicalType::Duration(kind) => ColBuf::Duration(*kind, Vec::new()),
            LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            } => ColBuf::Str(Vec::new()),
            LogicalType::Bytes {
                min: None,
                max: None,
                fixed: false,
            } => ColBuf::Bytes(Vec::new()),
            _ => return None,
        })
    }

    fn clear(&mut self) {
        match self {
            ColBuf::Int(v) => v.clear(),
            ColBuf::Bool(v) => v.clear(),
            ColBuf::Float(v) => v.clear(),
            ColBuf::Date(v) => v.clear(),
            ColBuf::LocalTime(v) | ColBuf::LocalDatetime(v) => v.clear(),
            ColBuf::Duration(_, v) => v.clear(),
            ColBuf::Str(v) | ColBuf::Bytes(v) => v.clear(),
        }
    }

    /// Drops the value written last, which is how a refused row takes
    /// back the fields it managed to write before the one that failed.
    fn pop(&mut self) {
        match self {
            ColBuf::Int(v) => drop(v.pop()),
            ColBuf::Bool(v) => drop(v.pop()),
            ColBuf::Float(v) => drop(v.pop()),
            ColBuf::Date(v) => drop(v.pop()),
            ColBuf::LocalTime(v) | ColBuf::LocalDatetime(v) => drop(v.pop()),
            ColBuf::Duration(_, v) => drop(v.pop()),
            ColBuf::Str(v) | ColBuf::Bytes(v) => drop(v.pop()),
        }
    }

    /// Takes one field, or says what it was expecting instead.
    ///
    /// This is the inverse of the read side's `word_value`, and the
    /// only place that knows a boolean column stores a truth value as
    /// a word and a float column stores an IEEE bit pattern. A
    /// temporal arrives as the count its meaning implies, and a
    /// mismatched meaning is refused here rather than written as
    /// though days were nanoseconds. An integer into a float column is
    /// the one widening allowed, because a caller writing a whole
    /// number into a float column meant it.
    fn push(&mut self, field: Field<'_>) -> std::result::Result<(), &'static str> {
        match (&mut *self, field) {
            (ColBuf::Int(v), Field::Int(n)) => v.push(n as u64),
            (ColBuf::Bool(v), Field::Bool(b)) => v.push(b),
            (ColBuf::Float(v), Field::Float(f)) => v.push(f),
            (ColBuf::Float(v), Field::Int(n)) => v.push(n as f64),
            (ColBuf::Date(v), Field::Temporal(Temporal::Date(d))) => v.push(d),
            (ColBuf::LocalTime(v), Field::Temporal(Temporal::LocalTime(t))) => v.push(t),
            (ColBuf::LocalDatetime(v), Field::Temporal(Temporal::LocalDatetime(t))) => v.push(t),
            (ColBuf::Duration(want, v), Field::Temporal(Temporal::Duration(kind, n)))
                if *want == kind =>
            {
                v.push(n)
            }
            (ColBuf::Str(v), Field::Str(s)) => v.push(s.as_bytes().to_vec()),
            (ColBuf::Bytes(v), Field::Bytes(b)) => v.push(b.to_vec()),
            _ => return Err(self.wanted()),
        }
        Ok(())
    }

    /// What this buffer takes, for the message when it was handed
    /// something else.
    fn wanted(&self) -> &'static str {
        match self {
            ColBuf::Int(_) => "an integer",
            ColBuf::Bool(_) => "a boolean",
            ColBuf::Float(_) => "a float",
            ColBuf::Date(_) => "a date",
            ColBuf::LocalTime(_) => "a local time",
            ColBuf::LocalDatetime(_) => "a local datetime",
            ColBuf::Duration(DurationKind::YearMonth, _) => "a year-month duration",
            ColBuf::Duration(DurationKind::DayTime, _) => "a day-time duration",
            ColBuf::Str(_) => "a string",
            ColBuf::Bytes(_) => "a byte string",
        }
    }
}

/// Borrows a buffer as the values an ingest takes. The blob arms need
/// somewhere for their pointers to live, which is what `blobs` is: it
/// holds the slice of slices for exactly the length of the call.
fn values_of<'a>(buf: &'a ColBuf, blobs: &'a [&'a [u8]]) -> PropValues<'a> {
    match buf {
        ColBuf::Int(v) => PropValues::Int(v),
        ColBuf::Bool(v) => PropValues::Bool(v),
        ColBuf::Float(v) => PropValues::Float(v),
        ColBuf::Date(v) => PropValues::Date(v),
        ColBuf::LocalTime(v) => PropValues::LocalTime(v),
        ColBuf::LocalDatetime(v) => PropValues::LocalDatetime(v),
        ColBuf::Duration(kind, v) => PropValues::Duration(*kind, v),
        ColBuf::Str(_) => PropValues::Str(blobs),
        ColBuf::Bytes(_) => PropValues::Bytes(blobs),
    }
}

/// Which table an appender writes to, and what a row of it is.
#[derive(Debug)]
enum Target {
    /// A node table: one buffer per stored column, in declared order.
    Nodes { table: u32, cols: Vec<ColBuf> },
    /// A rel table: a row is a source and a destination, both of them
    /// row offsets in the tables the rel runs between.
    Edges {
        rel: u32,
        src: Vec<u64>,
        dst: Vec<u64>,
    },
}

impl Target {
    /// Undoes the `written` fields of a row that was refused.
    fn undo(&mut self, written: usize) {
        match self {
            Target::Nodes { cols, .. } => {
                for col in cols.iter_mut().take(written) {
                    col.pop();
                }
            }
            Target::Edges { src, dst, .. } => {
                if written >= 1 {
                    src.pop();
                }
                if written >= 2 {
                    dst.pop();
                }
            }
        }
    }

    /// How many fields a whole row of this table has.
    fn width(&self) -> usize {
        match self {
            Target::Nodes { cols, .. } => cols.len(),
            Target::Edges { .. } => 2,
        }
    }
}

/// A bulk load in progress: rows buffered in memory, written to the
/// database when you flush.
///
/// Take one with [`crate::Connection::appender`], append rows to it,
/// and call [`Appender::close`]. Dropping it without closing flushes
/// too, because a loader that forgot is better served by its data
/// arriving than by it vanishing, but a drop has nowhere to put an
/// error and so cannot tell you whether the flush worked. That is the
/// one footgun dx/04 §6 accepts, and `#[must_use]` is what points a
/// caller at the version that reports. The drop does not panic on a
/// buffer it is handed, in debug builds either: a loader that hits an
/// error between rows and returns is exactly the case where an
/// appender is dropped with rows in it, and turning that into an abort
/// on the way out of an unwind would be the worse failure.
///
/// Two things a table can be that an appender will not append to, both
/// refused when the appender opens rather than at the flush that would
/// have failed. A column that holds a null has no way in this API to
/// say which rows the appended values are for, so the write statements
/// of G3 are what answers it. A table whose row domain a rel table's
/// key index is built over cannot grow without that index being
/// reallocated, so the fold refuses it, and this refuses it earlier,
/// before a load is buffered against it.
#[must_use = "an appender dropped without close() cannot report a failed flush"]
pub struct Appender<'c> {
    db: &'c mut Zu1File,
    wal: Wal,
    target: Target,
    /// Rows written since the last flush, which is what a flush turns
    /// into segments and what a drop warns about.
    buffered: u64,
    /// Rows this appender has committed, across every flush.
    committed: u64,
    /// Whether a flush has failed. A failed flush keeps its rows, so
    /// the caller can look at what did not go in, and stops the drop
    /// from trying the same write again on the way out.
    poisoned: bool,
}

impl<'c> Appender<'c> {
    /// Opens an appender on `table`, which is a node table or a rel
    /// table of the graph the file holds.
    pub(crate) fn open(db: &'c mut Zu1File, table: &str) -> Result<Appender<'c>> {
        let catalog = Catalog::load(db)?;
        let target = if let Some(node) = catalog.node_by_name(table) {
            let id = node.id;
            let dir = load_props(db, id)?.ok_or(ZuError::Unsupported {
                what: "appending to a node table that stores no properties",
                id,
            })?;
            let mut cols = Vec::with_capacity(dir.columns.len());
            for col in &dir.columns {
                if col.validity.is_some() {
                    return Err(ZuError::InvalidArgument(format!(
                        "column '{}' of '{table}' holds a null, and an appended row has \
                         no way to say whether it holds a value",
                        col.name
                    )));
                }
                cols.push(ColBuf::for_type(&col.ty).ok_or_else(|| {
                    ZuError::InvalidArgument(format!(
                        "column '{}' of '{table}' holds {}, which this engine cannot \
                         yet append to",
                        col.name, col.ty
                    ))
                })?);
            }
            refuse_keyed(db, &catalog, id, dir.columns.iter().any(|c| c.name == "id"))?;
            Target::Nodes { table: id, cols }
        } else if let Some(rel) = catalog.rel_by_name(table) {
            Target::Edges {
                rel: rel.id,
                src: Vec::new(),
                dst: Vec::new(),
            }
        } else {
            return Err(ZuError::InvalidArgument(format!(
                "no node table or rel table '{table}'"
            )));
        };
        let wal = Wal::open_in(db.vfs(), &sidecar(db.path()))?;
        Ok(Appender {
            db,
            wal,
            target,
            buffered: 0,
            committed: 0,
            poisoned: false,
        })
    }

    /// Appends one row: every column of a node table in declared
    /// order, or a source and a destination for a rel table.
    ///
    /// The row goes into memory and nothing else happens, so this is a
    /// push per column and, on a string column, one copy. A row of the
    /// wrong width or with a field of the wrong type is refused with
    /// nothing of it kept, so an appender is still usable after a
    /// caller fixes the row and tries again.
    pub fn append_row<'r, R: AppendRow<'r>>(&mut self, row: R) -> Result<()> {
        let mut sink = RowSink {
            target: &mut self.target,
            next: 0,
            err: None,
            marker: PhantomData,
        };
        row.write_into(&mut sink);
        let (written, err) = (sink.next, sink.err);
        if let Some(err) = err {
            self.target.undo(written);
            return Err(err);
        }
        let width = self.target.width();
        if written != width {
            self.target.undo(written);
            return Err(ZuError::InvalidArgument(format!(
                "row carries {written} values, the table takes {width}"
            )));
        }
        self.buffered += 1;
        Ok(())
    }

    /// Rows buffered and not yet written.
    pub fn buffered(&self) -> u64 {
        self.buffered
    }

    /// Rows this appender has committed.
    pub fn committed(&self) -> u64 {
        self.committed
    }

    /// Writes every buffered row and makes it readable.
    ///
    /// One commit, whatever the buffer holds: the values are sealed
    /// into the data file as segments, one frame naming them is
    /// appended and synced to the log, which is the commit point, and
    /// the fold that follows seals them into the base every query
    /// reads. On return the buffer is empty and the rows are there.
    ///
    /// A flush with nothing buffered touches no file and returns, so a
    /// loader can flush on a timer without writing empty commits.
    pub fn flush(&mut self) -> Result<()> {
        if self.buffered == 0 {
            return Ok(());
        }
        let rows = self.buffered;
        if let Err(e) = self.write_buffer() {
            self.poisoned = true;
            return Err(e);
        }
        self.buffered = 0;
        self.committed += rows;
        Ok(())
    }

    /// The write itself, split out so that one place marks the
    /// appender when any step of it fails.
    fn write_buffer(&mut self) -> Result<()> {
        let mut mvcc = recover(self.db, &mut self.wal)?;
        match &self.target {
            Target::Nodes { table, cols } => {
                // A blob column needs its slice of slices to outlive
                // the call, and there is one per column, so they are
                // all built first and borrowed second.
                let blobs: Vec<Vec<&[u8]>> = cols
                    .iter()
                    .map(|c| match c {
                        ColBuf::Str(v) | ColBuf::Bytes(v) => v.iter().map(Vec::as_slice).collect(),
                        _ => Vec::new(),
                    })
                    .collect();
                let values: Vec<(u32, PropValues)> = cols
                    .iter()
                    .zip(&blobs)
                    .enumerate()
                    .map(|(i, (buf, blobs))| (i as u32, values_of(buf, blobs)))
                    .collect();
                ingest_nodes(self.db, &mut self.wal, &mut mvcc, *table, &values)?;
            }
            Target::Edges { rel, src, dst } => {
                ingest_edges(self.db, &mut self.wal, &mut mvcc, *rel, src, dst)?;
            }
        }
        // The ingest is committed in the log by this point and the
        // fold is what puts it where a reader looks. A failure here
        // leaves the rows durable and unreadable until the next
        // writable open replays them, which is why a failed flush says
        // the flush failed and not that the rows are gone.
        checkpoint_fold(self.db, &mut mvcc, &mut self.wal)?;
        match &mut self.target {
            Target::Nodes { cols, .. } => cols.iter_mut().for_each(ColBuf::clear),
            Target::Edges { src, dst, .. } => {
                src.clear();
                dst.clear();
            }
        }
        Ok(())
    }

    /// Flushes and gives back the rows committed, which is how to
    /// finish an appender when you want to know that it worked.
    pub fn close(mut self) -> Result<u64> {
        self.flush()?;
        Ok(self.committed)
    }
}

/// Written by hand rather than derived, because the log this holds has
/// no `Debug` and the file it holds would print a page of block
/// bookkeeping. What a reader of a debug line wants is which table is
/// being loaded and how far it has got.
impl std::fmt::Debug for Appender<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Appender")
            .field("target", &self.target)
            .field("buffered", &self.buffered)
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

impl Drop for Appender<'_> {
    fn drop(&mut self) {
        if self.buffered == 0 || self.poisoned {
            return;
        }
        // The result goes nowhere because there is nowhere for it to
        // go, which is the whole reason close() exists. A poisoned
        // appender is skipped rather than retried: its last flush
        // already failed and repeating it here would only fail again,
        // silently, at a point the caller cannot observe.
        let _ = self.flush();
    }
}

/// Where the WAL sits: beside the database with `.wal` appended, which
/// is the sidecar name docs/04 §1 gives.
pub(crate) fn sidecar(db: &Path) -> PathBuf {
    let mut path = db.as_os_str().to_os_string();
    path.push(".wal");
    PathBuf::from(path)
}

/// Refuses a node table whose row domain a rel table's key index is
/// built over and which has no `id` column to key an appended row by.
///
/// The fold grows a key index by reading the key of each appended row
/// out of that column, so a table that has one can be appended to and a
/// table that has not cannot. The fold says so either way, and saying
/// it here means the caller learns before buffering a load rather than
/// after.
fn refuse_keyed(db: &mut Zu1File, catalog: &Catalog, table: u32, has_id: bool) -> Result<()> {
    if has_id {
        return Ok(());
    }
    let rels: Vec<String> = catalog
        .rel_tables()
        .iter()
        .filter(|r| r.from == table || r.to == table)
        .map(|r| r.name.clone())
        .collect();
    for rel in rels {
        if GraphReader::load_table(db, &rel)?
            .directory()
            .keys
            .is_some()
        {
            return Err(ZuError::Unsupported {
                what: "appending rows to a keyed table with no 'id' column to key them by",
                id: table,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::query::Value;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::store_props;
    use crate::{Config, Database};

    const PEOPLE: u64 = 8;

    /// A database whose `person` table has an integer column and a
    /// string one, which is the smallest thing an appender can be
    /// pointed at, plus the `follows` rel table between people.
    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..64u32).map(|i| (i % 8, (i * 5 + 1) % 8)).collect();
        edges.sort_unstable();
        edges.dedup();
        bulk_load_as(&mut db, "person", "follows", PEOPLE, &edges).expect("load");
        let names: Vec<&[u8]> = vec![
            b"ada", b"grace", b"alan", b"edsger", b"barbara", b"donald", b"john", b"tony",
        ];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[36, 45, 41, 72, 84, 86, 54, 90])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
    }

    fn scratch(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        seeded(&path);
        (dir, path)
    }

    /// The one number every test here checks against: how many rows the
    /// table holds as far as a statement is concerned.
    fn people(conn: &mut crate::Connection) -> i64 {
        let r = conn
            .query("MATCH (p:person) RETURN count(p) AS n")
            .expect("count");
        match r.rows.first().and_then(|row| row.first()) {
            Some(Value::Int(n)) => *n,
            other => panic!("expected a count, got {other:?}"),
        }
    }

    #[test]
    fn rows_appended_are_readable_by_the_next_query() {
        let (_dir, path) = scratch("append.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(people(&mut conn), PEOPLE as i64);
        let mut app = conn.appender("person").expect("appender");
        app.append_row((30i64, "linus")).expect("row");
        app.append_row((28i64, "ken")).expect("row");
        assert_eq!(app.buffered(), 2);
        assert_eq!(app.close().expect("close"), 2);
        assert_eq!(people(&mut conn), PEOPLE as i64 + 2);
    }

    #[test]
    fn an_appended_row_carries_its_columns_in_the_order_the_table_declares() {
        let (_dir, path) = scratch("columns.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        app.append_row((30i64, "linus")).expect("row");
        app.close().expect("close");
        let r = conn
            .query("MATCH (p:person) WHERE p.name = 'linus' RETURN p.age AS age")
            .expect("read");
        assert_eq!(r.rows.len(), 1, "one row named linus");
        assert_eq!(r.rows[0][0], Value::Int(30), "and it is the age appended");
    }

    #[test]
    fn edges_appended_join_the_graph() {
        let (_dir, path) = scratch("edges.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let before = conn
            .query("MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n")
            .expect("count");
        let before = match before.rows[0][0] {
            Value::Int(n) => n,
            _ => panic!("count"),
        };
        let mut app = conn.appender("follows").expect("appender");
        // Two edges that the seeded list does not hold: 7 follows
        // nobody there, and every source is under 8.
        app.append_row((7i64, 2i64)).expect("edge");
        app.append_row((7i64, 3i64)).expect("edge");
        assert_eq!(app.close().expect("close"), 2);
        let after = conn
            .query("MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n")
            .expect("count");
        assert_eq!(after.rows[0][0], Value::Int(before + 2));
    }

    #[test]
    fn an_edge_to_a_row_that_is_not_there_leaves_the_database_writable() {
        // The check this leans on is at the ingest and not at the fold.
        // A fold that refuses has already committed the frame it is
        // refusing, so the refusal comes back again from every writer
        // that opens the file afterwards and the database has no
        // writers left. Refused at the ingest, nothing is written.
        let (_dir, path) = scratch("outside.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("follows").expect("appender");
        app.append_row((0i64, 99i64)).expect("buffered");
        let err = app.close().expect_err("no row 99");
        assert!(err.to_string().contains("hold 8 and 8 rows"), "{err}");
        drop(conn);
        drop(db);

        let db = Database::open(&path).expect("the database still opens for writing");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("follows").expect("appender");
        app.append_row((7i64, 2i64)).expect("buffered");
        assert_eq!(app.close().expect("the good edge goes in"), 1);
    }

    #[test]
    fn a_row_of_the_wrong_width_leaves_nothing_of_itself_behind() {
        let (_dir, path) = scratch("width.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        let err = app.append_row((30i64,)).expect_err("too narrow");
        assert!(err.to_string().contains("the table takes 2"), "{err}");
        let err = app
            .append_row((30i64, "linus", 1i64))
            .expect_err("too wide");
        assert!(err.to_string().contains("columns"), "{err}");
        // The appender is still usable, and the refused rows left no
        // half of themselves in the buffers: if they had, the flush
        // would be ragged and the ingest would refuse it.
        app.append_row((30i64, "linus")).expect("row");
        assert_eq!(app.buffered(), 1);
        assert_eq!(app.close().expect("close"), 1);
        assert_eq!(people(&mut conn), PEOPLE as i64 + 1);
    }

    #[test]
    fn a_field_of_the_wrong_type_is_refused_by_the_column_it_was_for() {
        let (_dir, path) = scratch("types.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        let err = app.append_row(("linus", 30i64)).expect_err("swapped");
        assert!(err.to_string().contains("an integer"), "{err}");
        app.append_row((30i64, "linus"))
            .expect("the right way round");
        assert_eq!(app.close().expect("close"), 1);
    }

    #[test]
    fn a_flush_with_nothing_buffered_writes_nothing() {
        let (_dir, path) = scratch("empty.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let before = std::fs::metadata(&path).expect("stat").len();
        let mut app = conn.appender("person").expect("appender");
        app.flush().expect("flush");
        app.flush().expect("flush again");
        assert_eq!(app.committed(), 0);
        assert_eq!(app.close().expect("close"), 0);
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            before,
            "an empty flush must not grow the file"
        );
        assert_eq!(people(&mut conn), PEOPLE as i64);
    }

    #[test]
    fn several_flushes_accumulate() {
        let (_dir, path) = scratch("flushes.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        for i in 0..3i64 {
            app.append_row((20 + i, "loader")).expect("row");
            app.flush().expect("flush");
            assert_eq!(app.buffered(), 0);
            assert_eq!(app.committed(), i as u64 + 1);
        }
        assert_eq!(app.close().expect("close"), 3);
        assert_eq!(people(&mut conn), PEOPLE as i64 + 3);
    }

    #[test]
    fn a_read_only_connection_refuses_an_appender() {
        let (_dir, path) = scratch("readonly.zu1");
        let db = Database::open_with(&path, Config::new().read_only(true)).expect("open");
        let mut conn = db.connect().expect("connect");
        let err = conn.appender("person").expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    #[test]
    fn a_table_nothing_declares_is_refused_by_name() {
        let (_dir, path) = scratch("unknown.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let err = conn.appender("cities").expect_err("refused");
        assert!(err.to_string().contains("'cities'"), "{err}");
    }

    #[test]
    fn a_table_that_stores_no_properties_says_so_when_the_appender_opens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bare.zu1");
        let mut file = Zu1File::create(&path).expect("create");
        bulk_load_as(&mut file, "person", "follows", 4, &[(0, 1), (1, 2)]).expect("load");
        drop(file);
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let err = conn.appender("person").expect_err("refused");
        assert!(err.to_string().contains("stores no properties"), "{err}");
    }

    #[test]
    fn rows_committed_survive_a_reopen() {
        let (_dir, path) = scratch("reopen.zu1");
        {
            let db = Database::open(&path).expect("open");
            let mut conn = db.connect().expect("connect");
            let mut app = conn.appender("person").expect("appender");
            app.append_row((30i64, "linus")).expect("row");
            app.close().expect("close");
        }
        let db = Database::open(&path).expect("reopen");
        let mut conn = db.connect().expect("connect");
        assert_eq!(people(&mut conn), PEOPLE as i64 + 1);
    }

    #[test]
    fn a_commit_the_fold_never_reached_is_replayed_at_the_next_writable_open() {
        let (_dir, path) = scratch("replay.zu1");
        // The commit half of a flush and no fold, which is what a
        // crash between the two leaves behind: the segments and the
        // frame naming them are on disk and the base does not know
        // them.
        {
            let mut file = Zu1File::open(&path).expect("open");
            let mut wal = Wal::open(&sidecar(&path)).expect("wal");
            let mut mvcc = recover(&mut file, &mut wal).expect("recover");
            let names: Vec<&[u8]> = vec![b"linus"];
            let table = Catalog::load(&mut file)
                .expect("catalog")
                .node_by_name("person")
                .expect("person")
                .id;
            ingest_nodes(
                &mut file,
                &mut wal,
                &mut mvcc,
                table,
                &[(0, PropValues::Int(&[30])), (1, PropValues::Str(&names))],
            )
            .expect("ingest");
        }
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(
            people(&mut conn),
            PEOPLE as i64 + 1,
            "an open that can write must fold what the log still holds"
        );
    }

    #[test]
    fn a_row_can_be_a_slice_of_fields_rather_than_a_tuple() {
        let (_dir, path) = scratch("slice.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        let row = [Field::Int(30), Field::Str("linus")];
        app.append_row(&row[..]).expect("row");
        app.append_row([Field::Int(28), Field::Str("ken")])
            .expect("row");
        assert_eq!(app.close().expect("close"), 2);
        assert_eq!(people(&mut conn), PEOPLE as i64 + 2);
    }

    #[test]
    fn an_appender_dropped_after_closing_its_buffer_writes_nothing_more() {
        let (_dir, path) = scratch("drop.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        app.append_row((30i64, "linus")).expect("row");
        app.flush().expect("flush");
        drop(app);
        assert_eq!(
            people(&mut conn),
            PEOPLE as i64 + 1,
            "the drop had nothing buffered and must not write the row twice"
        );
    }

    /// The documented footgun, from the side that makes it a feature: a
    /// loader that returns early still has its rows land. What it does
    /// not have is any way to know, which is what close() is for.
    #[test]
    fn an_appender_dropped_with_rows_in_it_writes_them() {
        let (_dir, path) = scratch("drop-buffered.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let mut app = conn.appender("person").expect("appender");
        app.append_row((30i64, "linus")).expect("row");
        app.append_row((28i64, "ken")).expect("row");
        assert_eq!(app.buffered(), 2, "and no flush before the drop");
        drop(app);
        assert_eq!(people(&mut conn), PEOPLE as i64 + 2);
    }
}
