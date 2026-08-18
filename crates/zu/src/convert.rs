//! Engine conversion: the same graph, the other file format.
//!
//! Both engines share the dense row contract, node offsets from zero,
//! so conversion is a catalog walk with no id translation: node tables
//! copy row by row with their typed property columns, rel tables copy
//! their forward adjacency, and the result answers every query the
//! source answers with the same rows. Losslessness is the round trip
//! test's claim to check, not this module's to assume; `zuql` results
//! and raw reads are compared in `tests/convert.rs`.
//!
//! Scope follows the writers that exist today. The zu1 bulk loader
//! binds a rel table to one node table and gives node tables their
//! row domain through a load, so a sqlite rel with two distinct
//! endpoint tables or a node table no rel touches has no zu1 shape
//! yet and converts to a clean error instead of a guess. sqlite text
//! must be UTF-8 and property columns must be dense and uniformly
//! typed, because zu1 columns are.

use std::path::Path;

use crate::list_text::{self, Kind};
use zu_common::{DurationKind, FloatBits, IntBits, LogicalType, Result, ZuError};
use zu_sqlite::{ColumnType, SqliteStore, TableDef, Value};
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction as Zu1Direction, Ends, GraphReader, bulk_load_between};
use zu_zu1::props::{
    ListElement, PropInput, PropValues, PropsReader, list_elements, load_props, load_rel_props,
    store_labels, store_props_nullable, store_rel_props_nullable,
};

/// The staged element kind a stored list's element type names, `None`
/// for a type no staging column declares.
///
/// A stored list is written with its nullability wrapper off, so this
/// only meets whole types. It is narrower than the set a lane holds on
/// purpose: a list of dates has no JSON spelling that says it is dates
/// rather than counts, and inventing one here would be a second place
/// that decides what a count means.
fn list_kind(elem: &LogicalType) -> Option<Kind> {
    Some(match elem {
        LogicalType::Int { .. } => Kind::Int,
        LogicalType::Float { .. } => Kind::Real,
        LogicalType::Str { .. } => Kind::Text,
        LogicalType::Bool => Kind::Bool,
        _ => return None,
    })
}

/// The sqlite storage class a zu1 property column converts to. sqlite
/// has four and zu1 columns are typed finer than that, so the mapping
/// is lossy on the way out by construction: a date and a count both
/// land in INTEGER. The round trip test reads back through zu1, where
/// the type still says which.
///
/// A boolean is the exception, and only because sqlite lets a column
/// be declared BOOLEAN even though the values are the integers 0 and 1.
/// That declaration is enough to bring the column back as a boolean,
/// which matters for the fixture loaders that stage a sqlite file and
/// convert it.
fn sqlite_type(ty: &LogicalType, name: &str) -> Result<ColumnType> {
    Ok(match ty {
        LogicalType::Str { .. } => ColumnType::Text,
        LogicalType::Bytes { .. } => ColumnType::Blob,
        LogicalType::Float { .. } => ColumnType::Real,
        LogicalType::Bool => ColumnType::Boolean,
        LogicalType::Int { .. } => ColumnType::Integer,
        // The temporal columns keep their declaration on the way out,
        // so a file that round trips through sqlite comes back holding
        // dates rather than counts of days.
        LogicalType::Date => ColumnType::Date,
        LogicalType::LocalTime => ColumnType::LocalTime,
        LogicalType::LocalDatetime => ColumnType::LocalDatetime,
        LogicalType::Duration(DurationKind::DayTime) => ColumnType::Duration,
        LogicalType::Duration(DurationKind::YearMonth) => ColumnType::YearMonthDuration,
        // A list column crosses as a JSON array in a text column, and
        // the declaration is what says what the array holds. Only the
        // four element types a list column can be stored with have a
        // declaration, which is the same set going in and coming out.
        LogicalType::List { elem, .. } => match list_kind(elem) {
            Some(Kind::Int) => ColumnType::IntegerList,
            Some(Kind::Real) => ColumnType::RealList,
            Some(Kind::Text) => ColumnType::TextList,
            Some(Kind::Bool) => ColumnType::BooleanList,
            None => {
                return Err(ZuError::InvalidArgument(format!(
                    "column '{name}' holds {ty}, which has no sqlite staging form"
                )));
            }
        },
        other => {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {other}, which has no sqlite storage class"
            )));
        }
    })
}

/// One stored value read back as the sqlite value it stages as, `Null`
/// for a row of a nullable column that holds nothing.
///
/// A row holding nothing holds a placeholder underneath, and writing
/// that out would turn a null into a zero or an empty string on the way
/// back. `owner` names the table for the one error that has to say
/// where a value came from, and `row` is a node id or an edge ordinal
/// depending on which kind of table that is.
#[allow(clippy::too_many_arguments)]
fn staged_value(
    zu: &mut Zu1File,
    reader: &mut PropsReader,
    ci: usize,
    row: u64,
    name: &str,
    ty: &LogicalType,
    owner: &str,
    buf: &mut Vec<u8>,
) -> Result<Value> {
    if reader.is_nullable(ci) && !reader.is_valid(zu, ci, row)? {
        return Ok(Value::Null);
    }
    Ok(match ty {
        LogicalType::Str { .. } => {
            buf.clear();
            reader.read_str(zu, ci, row, buf)?;
            Value::Text(String::from_utf8(buf.clone()).map_err(|_| {
                ZuError::InvalidArgument(format!(
                    "'{owner}' column '{name}' row {row} is not utf-8; sqlite stores text"
                ))
            })?)
        }
        LogicalType::Bytes { .. } => {
            buf.clear();
            reader.read_str(zu, ci, row, buf)?;
            Value::Blob(buf.clone())
        }
        LogicalType::Bool => Value::Int(i64::from(reader.read_int(zu, ci, row)? != 0)),
        LogicalType::List { elem, .. } => {
            buf.clear();
            reader.read_str(zu, ci, row, buf)?;
            Value::Text(list_text::write(&staged_items(elem, buf, name)?))
        }
        LogicalType::Float { bits, .. } => {
            let word = reader.read_int(zu, ci, row)?;
            Value::Real(match bits {
                FloatBits::B32 => f64::from(f32::from_bits(word as u32)),
                _ => f64::from_bits(word),
            })
        }
        // Everything else the lane holds is a count of something: an
        // integer, a day, a nanosecond or a month, and sqlite stores
        // all four the same.
        _ => Value::Int(reader.read_int(zu, ci, row)? as i64),
    })
}

/// Converts a zu1 file into a fresh sqlite store at `db_path`.
pub fn zu1_to_sqlite(zu1_path: &Path, db_path: &Path) -> Result<()> {
    let mut zu = Zu1File::open(zu1_path)?;
    let catalog = Catalog::load(&mut zu)?;
    let mut sq = SqliteStore::open(db_path)?;

    for node in catalog.node_tables().to_vec() {
        let props = load_props(&mut zu, node.id)?;
        let cols: Vec<(String, LogicalType)> = props
            .iter()
            .flat_map(|p| p.columns.iter())
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect();
        let col_refs: Vec<(&str, ColumnType)> = cols
            .iter()
            .map(|(n, t)| Ok((n.as_str(), sqlite_type(t, n)?)))
            .collect::<Result<_>>()?;
        sq.create_node_table(&node.name, &col_refs)?;

        let mut reader = props.map(PropsReader::new);
        let mut buf = Vec::new();
        sq.begin()?;
        for row in 0..node.node_count {
            let mut values = Vec::with_capacity(cols.len());
            if let Some(reader) = reader.as_mut() {
                for (ci, (name, ty)) in cols.iter().enumerate() {
                    values.push(staged_value(
                        &mut zu, reader, ci, row, name, ty, &node.name, &mut buf,
                    )?);
                }
            }
            sq.insert_node_at(&node.name, row as i64, &values)?;
            // The label word carries the table's own bit on every row,
            // and the sqlite side gets what the row carries beyond it,
            // which is what the word says once that bit is cleared.
            if let Some(reader) = reader.as_mut()
                && let Some(word) = reader.label_word(&mut zu, row)?
            {
                let extra = word & !(1u64 << node.primary_label());
                if extra != 0 {
                    let names: Vec<&str> = (0..catalog.labels().len() as u16)
                        .filter(|l| extra & (1 << l) != 0)
                        .map(|l| {
                            catalog.label_name(l).ok_or_else(|| ZuError::Corrupt {
                                what: "label dictionary",
                                detail: format!(
                                    "node table '{}' row {row} carries label {l} and the \
                                     dictionary holds {} names",
                                    node.name,
                                    catalog.labels().len()
                                ),
                            })
                        })
                        .collect::<Result<_>>()?;
                    sq.set_node_labels(&node.name, row as i64, &names)?;
                }
            }
        }
        sq.commit()?;
    }

    for rel in catalog.rel_tables().to_vec() {
        let name_of = |id| {
            catalog
                .node_by_id(id)
                .map(|n| n.name.clone())
                .ok_or_else(|| ZuError::Corrupt {
                    what: "catalog",
                    detail: format!("rel table '{}' names no node table", rel.name),
                })
        };
        let (from, to) = (name_of(rel.from)?, name_of(rel.to)?);
        let props = load_rel_props(&mut zu, rel.id)?;
        let cols: Vec<(String, LogicalType)> = props
            .iter()
            .flat_map(|p| p.columns.iter())
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect();
        let col_refs: Vec<(&str, ColumnType)> = cols
            .iter()
            .map(|(n, t)| Ok((n.as_str(), sqlite_type(t, n)?)))
            .collect::<Result<_>>()?;
        sq.create_rel_table_as(&rel.name, &from, &to, &col_refs, rel.undirected)?;
        let g = GraphReader::load_table(&mut zu, &rel.name)?;
        // The rows of the FROM table this table's edges leave, which is
        // the directory's own domain rather than the node table's: a
        // node table grows to the widest load that touched it, and a
        // source past this table's own domain has no list here to read.
        let src_count = g.directory().from_count;
        let mut reader = props.map(PropsReader::new);
        let mut buf = Vec::new();
        let mut neighbors = Vec::new();
        // The edge ordinal is the place an edge takes when the forward
        // lists are read source after source, which is this walk, so a
        // counter along it is the row of the value without anything
        // being looked up to find it.
        let mut ordinal = 0u64;
        sq.begin()?;
        for src in 0..src_count {
            neighbors.clear();
            g.neighbors_dir_into(&mut zu, src, Zu1Direction::Fwd, &mut neighbors)?;
            for &dst in &neighbors {
                let mut values = Vec::with_capacity(cols.len());
                if let Some(reader) = reader.as_mut() {
                    for (ci, (name, ty)) in cols.iter().enumerate() {
                        values.push(staged_value(
                            &mut zu, reader, ci, ordinal, name, ty, &rel.name, &mut buf,
                        )?);
                    }
                }
                sq.insert_rel(&rel.name, src as i64, dst as i64, &values)?;
                ordinal += 1;
            }
        }
        sq.commit()?;
    }
    Ok(())
}

/// One stored list read back as the staged items it goes out as.
///
/// The word arms undo exactly what the lane put there: a float is IEEE
/// bits, everything else is a count, and the element type is what says
/// which, the same reading `word_value` gives a scalar column.
fn staged_items(elem: &LogicalType, bytes: &[u8], name: &str) -> Result<Vec<list_text::Item>> {
    let kind = list_kind(elem).ok_or_else(|| {
        ZuError::InvalidArgument(format!(
            "column '{name}' holds a list of {elem}, which has no sqlite staging form"
        ))
    })?;
    list_elements(elem, bytes)?
        .into_iter()
        .map(|item| match (item, kind) {
            (ListElement::Word(w), Kind::Int) => Ok(list_text::Item::Int(w as i64)),
            (ListElement::Word(w), Kind::Bool) => Ok(list_text::Item::Bool(w != 0)),
            (ListElement::Word(w), Kind::Real) => Ok(list_text::Item::Real(match elem {
                LogicalType::Float {
                    bits: FloatBits::B32,
                    ..
                } => f64::from(f32::from_bits(w as u32)),
                _ => f64::from_bits(w),
            })),
            (ListElement::Blob(b), Kind::Text) => Ok(list_text::Item::Text(
                String::from_utf8(b.to_vec()).map_err(|_| ZuError::Corrupt {
                    what: "props column",
                    detail: format!("an element of '{name}' is not UTF-8"),
                })?,
            )),
            (item, _) => Err(ZuError::Corrupt {
                what: "props column",
                detail: format!("'{name}' holds a list of {elem} and an element is {item:?}"),
            }),
        })
        .collect()
}

/// One node property column read whole, uniformly typed.
enum ColumnData {
    Int(Vec<u64>),
    Bool(Vec<bool>),
    Float(Vec<f64>),
    Str(Vec<Vec<u8>>),
    Bytes(Vec<Vec<u8>>),
    Date(Vec<i32>),
    /// A count of nanoseconds or of months, whichever the declared type
    /// asked for, kept together because the lane is the same.
    Counts(Vec<i64>),
    /// Lists, in the element type the declaration named and the row
    /// format's own shape, with the bytes of a text element owned here
    /// so the borrowed view can be built beside the other columns.
    List(LogicalType, Vec<Vec<OwnedElement>>),
}

impl ColumnData {
    /// An empty column of the kind this declaration asks for, which is
    /// the only thing that says what a column holds when every row of
    /// it is null and there is no value to read the kind off.
    fn empty(ty: ColumnType) -> Self {
        match ty {
            ColumnType::Real => ColumnData::Float(Vec::new()),
            ColumnType::Text
            | ColumnType::IntegerList
            | ColumnType::RealList
            | ColumnType::TextList
            | ColumnType::BooleanList => ColumnData::Str(Vec::new()),
            ColumnType::Blob => ColumnData::Bytes(Vec::new()),
            _ => ColumnData::Int(Vec::new()),
        }
    }

    /// Puts something in the slot of a row that holds nothing.
    ///
    /// The column is dense whether or not a row of it is null, so every
    /// row needs a value here and the mask is what says which of them
    /// mean anything. Nothing reads this one. A list column takes the
    /// empty array rather than the empty string, because the pass that
    /// reads a list out of its staged text reads every row including
    /// this one, and the empty string is not an array.
    fn push_placeholder(&mut self, listed: bool) {
        match self {
            ColumnData::Int(vals) => vals.push(0),
            ColumnData::Bool(vals) => vals.push(false),
            ColumnData::Float(vals) => vals.push(0.0),
            ColumnData::Str(vals) => vals.push(if listed { b"[]".to_vec() } else { Vec::new() }),
            ColumnData::Bytes(vals) => vals.push(Vec::new()),
            ColumnData::Date(vals) => vals.push(0),
            ColumnData::Counts(vals) => vals.push(0),
            ColumnData::List(_, rows) => rows.push(Vec::new()),
        }
    }
}

/// A [`ListElement`] that owns what it points at.
enum OwnedElement {
    Word(u64),
    Blob(Vec<u8>),
}

/// The element type and staged element kind a list declaration names,
/// `None` for a declaration that is not a list at all.
fn staged_list(ty: ColumnType) -> Option<(LogicalType, Kind)> {
    Some(match ty {
        ColumnType::IntegerList => (
            LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            },
            Kind::Int,
        ),
        ColumnType::RealList => (
            LogicalType::Float {
                bits: FloatBits::B64,
                precision: None,
            },
            Kind::Real,
        ),
        ColumnType::TextList => (
            LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
            Kind::Text,
        ),
        ColumnType::BooleanList => (LogicalType::Bool, Kind::Bool),
        _ => return None,
    })
}

/// What a list declaration is called, for the error that has to name it.
fn list_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::IntegerList => "INTEGERLIST",
        ColumnType::RealList => "REALLIST",
        ColumnType::TextList => "TEXTLIST",
        _ => "BOOLEANLIST",
    }
}

/// What a temporal declaration is called, for the one error that has to
/// name it.
fn temporal_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Date => "DATE",
        ColumnType::LocalTime => "LOCALTIME",
        ColumnType::LocalDatetime => "LOCALDATETIME",
        ColumnType::YearMonthDuration => "YEARMONTHDURATION",
        _ => "DURATION",
    }
}

/// One table's property columns, read whole out of sqlite and narrowed
/// to what the zu1 row format holds, with one validity bit per row.
///
/// Nodes and edges stage the same way. What differs is where the values
/// come from and what the row number means: a node's row is its dense
/// id, an edge's row is its edge ordinal, its place in a load sorted by
/// source and then destination. Both are dense counts from zero, which
/// is the whole of what this needs to know about either.
struct Staged<'a> {
    declared: &'a [(String, ColumnType)],
    data: Vec<ColumnData>,
    masks: Vec<Vec<u64>>,
}

impl<'a> Staged<'a> {
    /// Reads `count` rows of every declared column through `get`, which
    /// answers with one row of one column by its index in `declared`.
    fn read(
        owner: &'a str,
        declared: &'a [(String, ColumnType)],
        count: u64,
        mut get: impl FnMut(u64, usize) -> Result<Value>,
    ) -> Result<Self> {
        let mut data = Vec::with_capacity(declared.len());
        let mut masks = Vec::with_capacity(declared.len());
        for (ci, (col, declared_ty)) in declared.iter().enumerate() {
            let listed = staged_list(*declared_ty).is_some();
            let mut column: Option<ColumnData> = None;
            // One bit per row, set where the row holds a value. A null
            // row before the first value has nowhere to put its
            // placeholder yet, because the kind of the column is read
            // off the first value there is, so those are counted and
            // filled in once there is one.
            let mut mask = vec![0u64; (count as usize).div_ceil(64)];
            let mut leading = 0usize;
            for row in 0..count {
                let bad = |what: &str| {
                    ZuError::InvalidArgument(format!(
                        "'{owner}' column '{col}' row {row} {what}; zu1 property columns \
                         are dense and uniformly typed"
                    ))
                };
                let value = get(row, ci)?;
                if matches!(value, Value::Null) {
                    match &mut column {
                        Some(data) => data.push_placeholder(listed),
                        None => leading += 1,
                    }
                    continue;
                }
                if column.is_none() {
                    let mut fresh = match &value {
                        Value::Real(_) => ColumnData::Float(Vec::new()),
                        Value::Text(_) => ColumnData::Str(Vec::new()),
                        Value::Blob(_) => ColumnData::Bytes(Vec::new()),
                        _ => ColumnData::Int(Vec::new()),
                    };
                    for _ in 0..leading {
                        fresh.push_placeholder(listed);
                    }
                    column = Some(fresh);
                }
                match (value, column.as_mut().expect("just set")) {
                    (Value::Int(v), ColumnData::Int(vals)) => vals.push(v as u64),
                    (Value::Real(v), ColumnData::Float(vals)) => vals.push(v),
                    (Value::Text(v), ColumnData::Str(vals)) => vals.push(v.into_bytes()),
                    (Value::Blob(v), ColumnData::Bytes(vals)) => vals.push(v),
                    _ => return Err(bad("changes type")),
                }
                mask[row as usize / 64] |= 1u64 << (row % 64);
            }
            data.push(column.unwrap_or_else(|| {
                // Every row is null, so the declaration is the only
                // thing left that says what the column holds.
                let mut empty = ColumnData::empty(*declared_ty);
                for _ in 0..leading {
                    empty.push_placeholder(listed);
                }
                empty
            }));
            masks.push(mask);
        }

        // The temporal columns arrive as integers too, and like a
        // boolean they are told apart by the declaration and nothing
        // else. A date is narrowed here rather than at store time so a
        // count of days no date can name is refused with the column
        // that holds it named.
        for ((name, ty), column) in declared.iter().zip(&mut data) {
            let want = match ty {
                ColumnType::Date
                | ColumnType::LocalTime
                | ColumnType::LocalDatetime
                | ColumnType::Duration
                | ColumnType::YearMonthDuration => *ty,
                _ => continue,
            };
            let ColumnData::Int(vals) = column else {
                return Err(ZuError::InvalidArgument(format!(
                    "'{owner}' column '{name}' is declared {} and does not hold integers",
                    temporal_name(want)
                )));
            };
            let counts: Vec<i64> = vals.iter().map(|&v| v as i64).collect();
            *column = if want == ColumnType::Date {
                ColumnData::Date(
                    counts
                        .iter()
                        .map(|&v| {
                            i32::try_from(v).map_err(|_| {
                                ZuError::InvalidArgument(format!(
                                    "'{owner}' column '{name}' holds {v}, which is no day \
                                     of any date"
                                ))
                            })
                        })
                        .collect::<Result<Vec<i32>>>()?,
                )
            } else {
                ColumnData::Counts(counts)
            };
        }

        // A boolean arrives as integers, because that is all sqlite has
        // to store one in, and the declaration is what says the column
        // was meant as truth values. Anything outside 0 and 1 under that
        // declaration is a store written by something that did not mean
        // it, and is refused rather than folded to true.
        for ((name, ty), column) in declared.iter().zip(&mut data) {
            if *ty != ColumnType::Boolean {
                continue;
            }
            let ColumnData::Int(vals) = column else {
                return Err(ZuError::InvalidArgument(format!(
                    "'{owner}' column '{name}' is declared BOOLEAN and does not hold integers"
                )));
            };
            let bits = vals
                .iter()
                .map(|&v| match v {
                    0 => Ok(false),
                    1 => Ok(true),
                    other => Err(ZuError::InvalidArgument(format!(
                        "'{owner}' column '{name}' is declared BOOLEAN and holds {}",
                        other as i64
                    ))),
                })
                .collect::<Result<Vec<bool>>>()?;
            *column = ColumnData::Bool(bits);
        }

        // A list arrives as the JSON array text sqlite had to hold it
        // as, and like a boolean it is the declaration and nothing else
        // that says so. The elements are reduced here to the words and
        // bytes the row format keeps, which is also where a text that
        // is not the array its column promised is refused.
        for ((name, ty), column) in declared.iter().zip(&mut data) {
            let Some((elem, kind)) = staged_list(*ty) else {
                continue;
            };
            let ColumnData::Str(vals) = column else {
                return Err(ZuError::InvalidArgument(format!(
                    "'{owner}' column '{name}' is declared {} and does not hold text",
                    list_name(*ty)
                )));
            };
            let mut rows = Vec::with_capacity(vals.len());
            for raw in vals.iter() {
                let text = std::str::from_utf8(raw).map_err(|_| {
                    ZuError::InvalidArgument(format!(
                        "'{owner}' column '{name}' holds text that is not UTF-8"
                    ))
                })?;
                rows.push(
                    list_text::parse(kind, text)?
                        .into_iter()
                        .map(|item| match item {
                            list_text::Item::Int(v) => OwnedElement::Word(v as u64),
                            list_text::Item::Bool(v) => OwnedElement::Word(u64::from(v)),
                            list_text::Item::Real(v) => OwnedElement::Word(v.to_bits()),
                            list_text::Item::Text(v) => OwnedElement::Blob(v.into_bytes()),
                        })
                        .collect(),
                );
            }
            *column = ColumnData::List(elem, rows);
        }

        Ok(Staged {
            declared,
            data,
            masks,
        })
    }

    /// Hands the columns to `store` as the borrowed view a store call
    /// takes. The borrowed elements of a list column and the bytes of a
    /// text one are built here and live only as long as the call, which
    /// is why this passes them to a closure rather than returning them.
    fn with_inputs<T>(&self, store: impl FnOnce(&[PropInput]) -> Result<T>) -> Result<T> {
        let list_items: Vec<Vec<Vec<ListElement>>> = self
            .data
            .iter()
            .map(|c| match c {
                ColumnData::List(_, rows) => rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|e| match e {
                                OwnedElement::Word(w) => ListElement::Word(*w),
                                OwnedElement::Blob(b) => ListElement::Blob(b),
                            })
                            .collect()
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let list_rows: Vec<Vec<&[ListElement]>> = list_items
            .iter()
            .map(|rows| rows.iter().map(|r| r.as_slice()).collect())
            .collect();
        let str_refs: Vec<Vec<&[u8]>> = self
            .data
            .iter()
            .map(|c| match c {
                ColumnData::Str(vals) | ColumnData::Bytes(vals) => {
                    vals.iter().map(|v| v.as_slice()).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        let columns: Vec<PropInput> = self
            .declared
            .iter()
            .zip(&self.data)
            .zip(&str_refs)
            .zip(&list_rows)
            .zip(&self.masks)
            .map(|(((((name, ty), c), refs), lists), mask)| {
                let values = match c {
                    ColumnData::Int(vals) => PropValues::Int(vals),
                    ColumnData::Bool(vals) => PropValues::Bool(vals),
                    ColumnData::Float(vals) => PropValues::Float(vals),
                    ColumnData::Str(_) => PropValues::Str(refs),
                    ColumnData::Bytes(_) => PropValues::Bytes(refs),
                    ColumnData::Date(vals) => PropValues::Date(vals),
                    ColumnData::List(elem, _) => PropValues::List { elem, rows: lists },
                    ColumnData::Counts(vals) => match ty {
                        ColumnType::LocalTime => PropValues::LocalTime(vals),
                        ColumnType::LocalDatetime => PropValues::LocalDatetime(vals),
                        ColumnType::YearMonthDuration => {
                            PropValues::Duration(DurationKind::YearMonth, vals)
                        }
                        _ => PropValues::Duration(DurationKind::DayTime, vals),
                    },
                };
                // The mask goes over whether or not the column has a
                // null in it; a mask with every bit set is not written,
                // so a column with no null is stored the way it was.
                PropInput {
                    name: name.as_str(),
                    values,
                    validity: Some(mask.as_slice()),
                }
            })
            .collect();
        store(&columns)
    }
}

/// Converts a sqlite store into a fresh zu1 file at `zu1_path`.
pub fn sqlite_to_zu1(db_path: &Path, zu1_path: &Path) -> Result<()> {
    let sq = SqliteStore::open(db_path)?;
    let tables = sq.tables()?;
    let nodes: Vec<&TableDef> = tables.iter().filter(|t| t.kind == "node").collect();
    let rels: Vec<&TableDef> = tables.iter().filter(|t| t.kind == "rel").collect();
    for node in &nodes {
        if !rels.iter().any(|r| {
            r.src_table.as_deref() == Some(&node.name) || r.dst_table.as_deref() == Some(&node.name)
        }) {
            return Err(ZuError::Unsupported {
                what: "converting a node table no rel table touches; the zu1 bulk \
                       loader gives a node table its row domain through a load",
                id: node.id,
            });
        }
    }

    let mut zu = Zu1File::create(zu1_path)?;
    for rel in &rels {
        let (src, dst) = match (&rel.src_table, &rel.dst_table) {
            (Some(s), Some(d)) => (s, d),
            _ => {
                return Err(ZuError::InvalidArgument(format!(
                    "rel table '{}' has no recorded endpoints; recreate it",
                    rel.name
                )));
            }
        };
        let (src_count, dst_count) = (sq.node_count(src)?, sq.node_count(dst)?);
        let declared = sq.rel_column_types(&rel.name)?;
        let names: Vec<String> = declared.iter().map(|(n, _)| n.clone()).collect();
        // The scan comes back sorted by source and then destination,
        // which is the order the bulk loader wants an edge list in and
        // the order an edge ordinal counts, so one read answers both
        // and neither can drift from the other.
        let rows = sq.rel_rows(&rel.name, &names)?;
        let mut edges: Vec<(u32, u32)> = Vec::with_capacity(rows.len());
        for (s, d, _) in &rows {
            // Each end is checked against its own table, which are two
            // tables when the edges run between labels, so a row id
            // that belongs to neither is named rather than loaded.
            for (end, id, table, count) in [
                ("source", *s, src, src_count),
                ("destination", *d, dst, dst_count),
            ] {
                if id < 0 || id >= count {
                    return Err(ZuError::InvalidArgument(format!(
                        "rel table '{}' has an edge whose {end} is row {id}, and \
                         '{table}' holds {count} rows",
                        rel.name
                    )));
                }
            }
            edges.push((*s as u32, *d as u32));
        }
        bulk_load_between(
            &mut zu,
            Ends::between((src, src_count as u64), (dst, dst_count as u64)),
            &rel.name,
            &edges,
            rel.undirected,
        )?;
        if !declared.is_empty() {
            // After the load and not before: a load writes the group
            // directory whole, and the edge property root hangs off it.
            let staged = Staged::read(&rel.name, &declared, rows.len() as u64, |row, ci| {
                Ok(rows[row as usize].2[ci].clone())
            })?;
            staged.with_inputs(|columns| {
                store_rel_props_nullable(&mut zu, &rel.name, columns)?;
                Ok(())
            })?;
        }
    }

    for node in &nodes {
        let declared = sq.node_column_types(&node.name)?;
        if !declared.is_empty() {
            let names: Vec<String> = declared.iter().map(|(n, _)| n.clone()).collect();
            let count = sq.node_count(&node.name)? as u64;
            // One scan rather than a lookup per node. The staging pass
            // below walks the table column by column, and asking SQLite
            // for a single value each time it does costs a prepared
            // statement and an index descent per node per column, which
            // is what the per node ingest cost was made of.
            //
            // Each value is moved out rather than copied, which is why
            // the cells are options: a string property read a second
            // time would find the hole the first read left, and saying
            // so is better than handing back a null that reads as an
            // absent property.
            let mut columns = sq.node_columns_values(&node.name, &names)?;
            let mut columns: Vec<Vec<Option<Value>>> = columns
                .drain(..)
                .map(|col| col.into_iter().map(Some).collect())
                .collect();
            let staged = Staged::read(&node.name, &declared, count, |row, ci| {
                columns[ci][row as usize].take().ok_or_else(|| {
                    ZuError::InvalidArgument(format!(
                        "'{}' column '{}' row {row} was read twice",
                        node.name, names[ci]
                    ))
                })
            })?;
            staged.with_inputs(|columns| {
                store_props_nullable(&mut zu, &node.name, columns)?;
                Ok(())
            })?;
        }
        // After the columns and not before, because both hang off the
        // same directory and the label store is the one that keeps
        // what is already there.
        let labels = sq.node_labels(&node.name)?;
        if !labels.is_empty() {
            store_labels(&mut zu, &node.name, &labels)?;
        }
    }
    Ok(())
}
