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

use zu_common::{DurationKind, FloatBits, LogicalType, Result, ZuError};
use zu_sqlite::{ColumnType, SqliteStore, TableDef, Value};
use zu_storage::Direction;
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction as Zu1Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropValues, PropsReader, load_props, store_props};

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
        other => {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {other}, which has no sqlite storage class"
            )));
        }
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
                    values.push(match ty {
                        LogicalType::Str { .. } => {
                            buf.clear();
                            reader.read_str(&mut zu, ci, row, &mut buf)?;
                            Value::Text(String::from_utf8(buf.clone()).map_err(|_| {
                                ZuError::InvalidArgument(format!(
                                    "'{}' column '{name}' row {row} is not utf-8; \
                                     sqlite stores text",
                                    node.name
                                ))
                            })?)
                        }
                        LogicalType::Bytes { .. } => {
                            buf.clear();
                            reader.read_str(&mut zu, ci, row, &mut buf)?;
                            Value::Blob(buf.clone())
                        }
                        LogicalType::Bool => {
                            Value::Int(i64::from(reader.read_int(&mut zu, ci, row)? != 0))
                        }
                        LogicalType::Float { bits, .. } => {
                            let word = reader.read_int(&mut zu, ci, row)?;
                            Value::Real(match bits {
                                FloatBits::B32 => f64::from(f32::from_bits(word as u32)),
                                _ => f64::from_bits(word),
                            })
                        }
                        // Everything else the lane holds is a count of
                        // something: an integer, a day, a nanosecond or
                        // a month, and sqlite stores all four the same.
                        _ => Value::Int(reader.read_int(&mut zu, ci, row)? as i64),
                    });
                }
            }
            sq.insert_node_at(&node.name, row as i64, &values)?;
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
        sq.create_rel_table(&rel.name, &from, &to, &[])?;
        let src_count = catalog
            .node_by_id(rel.from)
            .expect("resolved above")
            .node_count;
        let mut g = GraphReader::load_table(&mut zu, &rel.name)?;
        sq.begin()?;
        for src in 0..src_count {
            for &dst in g.neighbors_dir(&mut zu, src, Zu1Direction::Fwd)? {
                sq.insert_rel(&rel.name, src as i64, dst as i64, &[])?;
            }
        }
        sq.commit()?;
    }
    Ok(())
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
        if src != dst {
            return Err(ZuError::Unsupported {
                what: "converting a rel table with two distinct endpoint tables; \
                       the zu1 bulk loader binds a rel to one node table",
                id: rel.id,
            });
        }
        let count = sq.node_count(src)?;
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for s in 0..count {
            for d in sq.neighbors(&rel.name, s, Direction::Fwd)? {
                edges.push((s as u32, d as u32));
            }
        }
        edges.sort_unstable();
        bulk_load_as(&mut zu, src, &rel.name, count as u64, &edges)?;
    }

    for node in &nodes {
        let declared = sq.node_column_types(&node.name)?;
        if declared.is_empty() {
            continue;
        }
        let cols: Vec<String> = declared.iter().map(|(n, _)| n.clone()).collect();
        let count = sq.node_count(&node.name)?;
        let mut data = Vec::with_capacity(cols.len());
        for col in &cols {
            let mut column: Option<ColumnData> = None;
            for row in 0..count {
                let bad = |what: &str| {
                    ZuError::InvalidArgument(format!(
                        "'{}' column '{col}' row {row} {what}; zu1 property columns \
                         are dense and uniformly typed",
                        node.name
                    ))
                };
                match (sq.read_node_prop(&node.name, row, col)?, &mut column) {
                    (Value::Int(v), None) => column = Some(ColumnData::Int(vec![v as u64])),
                    (Value::Int(v), Some(ColumnData::Int(vals))) => vals.push(v as u64),
                    (Value::Real(v), None) => column = Some(ColumnData::Float(vec![v])),
                    (Value::Real(v), Some(ColumnData::Float(vals))) => vals.push(v),
                    (Value::Text(v), None) => {
                        column = Some(ColumnData::Str(vec![v.into_bytes()]));
                    }
                    (Value::Text(v), Some(ColumnData::Str(vals))) => vals.push(v.into_bytes()),
                    (Value::Blob(v), None) => column = Some(ColumnData::Bytes(vec![v])),
                    (Value::Blob(v), Some(ColumnData::Bytes(vals))) => vals.push(v),
                    (Value::Null, _) => return Err(bad("is null")),
                    (_, Some(_)) => return Err(bad("changes type")),
                }
            }
            data.push(column.unwrap_or(ColumnData::Int(Vec::new())));
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
                    "'{}' column '{name}' is declared {} and does not hold integers",
                    node.name,
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
                                    "'{}' column '{name}' holds {v}, which is no day of any date",
                                    node.name
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
                    "'{}' column '{name}' is declared BOOLEAN and does not hold integers",
                    node.name
                )));
            };
            let bits = vals
                .iter()
                .map(|&v| match v {
                    0 => Ok(false),
                    1 => Ok(true),
                    other => Err(ZuError::InvalidArgument(format!(
                        "'{}' column '{name}' is declared BOOLEAN and holds {}",
                        node.name, other as i64
                    ))),
                })
                .collect::<Result<Vec<bool>>>()?;
            *column = ColumnData::Bool(bits);
        }
        let str_refs: Vec<Vec<&[u8]>> = data
            .iter()
            .map(|c| match c {
                ColumnData::Str(vals) | ColumnData::Bytes(vals) => {
                    vals.iter().map(|v| v.as_slice()).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        let columns: Vec<(&str, PropValues)> = cols
            .iter()
            .zip(&data)
            .zip(&str_refs)
            .zip(&declared)
            .map(|(((name, c), refs), (_, ty))| {
                let values = match c {
                    ColumnData::Int(vals) => PropValues::Int(vals),
                    ColumnData::Bool(vals) => PropValues::Bool(vals),
                    ColumnData::Float(vals) => PropValues::Float(vals),
                    ColumnData::Str(_) => PropValues::Str(refs),
                    ColumnData::Bytes(_) => PropValues::Bytes(refs),
                    ColumnData::Date(vals) => PropValues::Date(vals),
                    ColumnData::Counts(vals) => match ty {
                        ColumnType::LocalTime => PropValues::LocalTime(vals),
                        ColumnType::LocalDatetime => PropValues::LocalDatetime(vals),
                        ColumnType::YearMonthDuration => {
                            PropValues::Duration(DurationKind::YearMonth, vals)
                        }
                        _ => PropValues::Duration(DurationKind::DayTime, vals),
                    },
                };
                (name.as_str(), values)
            })
            .collect();
        store_props(&mut zu, &node.name, &columns)?;
    }
    Ok(())
}
