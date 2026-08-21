//! The columns no buffer covers, walked one value at a time.
//!
//! Nodes, rels, paths, lists and records have no fixed width cell, so
//! the engine hands them over as the values themselves and this is where
//! they become structs and lists. Everything below the top level reaches
//! here always, because a list item and a record field are values
//! wherever they sit, which is why the flat types appear here a second
//! time in their slow form.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryArray, BooleanArray, Date32Array, DurationNanosecondArray, Float64Array,
    Int64Array, IntervalMonthDayNanoArray, ListArray, NullArray, StringArray, StructArray,
    Time64NanosecondArray, TimestampNanosecondArray, UInt64Array,
};
use arrow::buffer::{NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::DataType;

use zu_common::{DurationKind, Temporal};
use zu_query::column::ColumnType;
use zu_query::exec::Value;

use crate::{
    Error, Tables, data_type, item, mismatch, months, node_fields, path_fields, rel_fields,
    unsupported, zone,
};

/// One column's array, walked out of the values in it.
pub fn build<T: Tables + ?Sized>(
    name: &str,
    ty: &ColumnType,
    values: &[&Value],
    tables: &T,
) -> Result<ArrayRef, Error> {
    Ok(match ty {
        ColumnType::Null => Arc::new(NullArray::new(values.len())),
        ColumnType::Bool => Arc::new(
            values
                .iter()
                .map(|value| match value {
                    Value::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect::<BooleanArray>(),
        ),
        ColumnType::Int => Arc::new(
            values
                .iter()
                .map(|value| match value {
                    Value::Int(n) => Some(*n),
                    _ => None,
                })
                .collect::<Int64Array>(),
        ),
        ColumnType::Float => Arc::new(
            values
                .iter()
                .map(|value| match value {
                    Value::Float(f) => Some(*f),
                    // Widened where the column holds both, which is the
                    // only place an integer reaches a float column.
                    Value::Int(n) => Some(*n as f64),
                    _ => None,
                })
                .collect::<Float64Array>(),
        ),
        ColumnType::Str => Arc::new(
            values
                .iter()
                .map(|value| match value {
                    Value::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect::<StringArray>(),
        ),
        ColumnType::Bytes => Arc::new(
            values
                .iter()
                .map(|value| match value {
                    Value::Bytes(b) => Some(b.as_slice()),
                    _ => None,
                })
                .collect::<BinaryArray>(),
        ),
        ColumnType::Date => Arc::new(
            temporals(values)
                .map(|temporal| match temporal {
                    Some(Temporal::Date(days)) => Some(*days),
                    _ => None,
                })
                .collect::<Date32Array>(),
        ),
        ColumnType::LocalTime => Arc::new(
            temporals(values)
                .map(|temporal| match temporal {
                    Some(Temporal::LocalTime(nanos)) => Some(*nanos),
                    _ => None,
                })
                .collect::<Time64NanosecondArray>(),
        ),
        ColumnType::LocalDatetime => Arc::new(
            temporals(values)
                .map(|temporal| match temporal {
                    Some(Temporal::LocalDatetime(nanos)) => Some(*nanos),
                    _ => None,
                })
                .collect::<TimestampNanosecondArray>(),
        ),
        ColumnType::ZonedDatetime { offset } => Arc::new(
            temporals(values)
                .map(|temporal| match temporal {
                    Some(Temporal::ZonedDatetime { nanos, .. }) => Some(*nanos),
                    _ => None,
                })
                .collect::<TimestampNanosecondArray>()
                .with_timezone(zone(*offset)),
        ),
        ColumnType::YearMonth => {
            let mut counts = Vec::with_capacity(values.len());
            let mut valid = Vec::with_capacity(values.len());
            for temporal in temporals(values) {
                match temporal {
                    Some(Temporal::Duration(DurationKind::YearMonth, count)) => {
                        counts.push(*count);
                        valid.push(true);
                    }
                    _ => {
                        counts.push(0);
                        valid.push(false);
                    }
                }
            }
            Arc::new(IntervalMonthDayNanoArray::new(
                ScalarBuffer::from(months(name, &counts)?),
                Some(NullBuffer::from(valid)),
            ))
        }
        ColumnType::DayTime => Arc::new(
            temporals(values)
                .map(|temporal| match temporal {
                    Some(Temporal::Duration(DurationKind::DayTime, nanos)) => Some(*nanos),
                    _ => None,
                })
                .collect::<DurationNanosecondArray>(),
        ),
        ColumnType::Node => nodes(values, tables)?,
        ColumnType::Rel => rels(values, tables)?,
        ColumnType::Path => paths(name, values, tables)?,
        ColumnType::List(of) => {
            let mut offsets = Vec::with_capacity(values.len() + 1);
            let mut flat: Vec<&Value> = Vec::new();
            let mut valid = Vec::with_capacity(values.len());
            offsets.push(0i32);
            for value in values {
                if let Value::List(items) = value {
                    flat.extend(items.iter());
                    valid.push(true);
                } else {
                    valid.push(false);
                }
                offsets.push(flat.len() as i32);
            }
            Arc::new(ListArray::try_new(
                item(data_type(name, of)?),
                OffsetBuffer::new(offsets.into()),
                build(name, of, &flat, tables)?,
                Some(NullBuffer::from(valid)),
            )?)
        }
        ColumnType::Record(fields) => {
            let mut children: Vec<ArrayRef> = Vec::with_capacity(fields.len());
            for (at, (_, ty)) in fields.iter().enumerate() {
                let column: Vec<&Value> = values
                    .iter()
                    .map(|value| match value {
                        Value::Record(held) => &held[at].1,
                        _ => &Value::Null,
                    })
                    .collect();
                children.push(build(name, ty, &column, tables)?);
            }
            Arc::new(StructArray::try_new(
                match data_type(name, ty)? {
                    DataType::Struct(fields) => fields,
                    _ => return Err(mismatch(name, ty)),
                },
                children,
                Some(present(values)),
            )?)
        }
        ColumnType::ZonedTime { .. } | ColumnType::Graph | ColumnType::BindingTable => {
            return Err(unsupported(name, ty));
        }
    })
}

/// The temporal each value holds, or `None` for a value that is not one
/// and for a null.
fn temporals<'a>(values: &'a [&'a Value]) -> impl Iterator<Item = Option<&'a Temporal>> {
    values.iter().map(|value| match value {
        Value::Temporal(temporal) => Some(temporal),
        _ => None,
    })
}

/// Which rows of a struct column are there at all.
fn present(values: &[&Value]) -> NullBuffer {
    NullBuffer::from(
        values
            .iter()
            .map(|value| !matches!(value, Value::Null))
            .collect::<Vec<bool>>(),
    )
}

fn nodes<T: Tables + ?Sized>(values: &[&Value], tables: &T) -> Result<ArrayRef, Error> {
    let table = names(
        values,
        |value| match value {
            Value::Node { table, .. } => Some(*table),
            _ => None,
        },
        |id| tables.node(id),
    );
    let offset: UInt64Array = values
        .iter()
        .map(|value| match value {
            Value::Node { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect();
    Ok(Arc::new(StructArray::try_new(
        node_fields(),
        vec![Arc::new(table), Arc::new(offset)],
        Some(present(values)),
    )?))
}

fn rels<T: Tables + ?Sized>(values: &[&Value], tables: &T) -> Result<ArrayRef, Error> {
    let table = names(
        values,
        |value| match value {
            Value::Rel { table, .. } => Some(*table),
            _ => None,
        },
        |id| tables.rel(id),
    );
    let end = |pick: fn(&Value) -> Option<u64>| -> UInt64Array {
        values.iter().map(|value| pick(value)).collect()
    };
    Ok(Arc::new(StructArray::try_new(
        rel_fields(),
        vec![
            Arc::new(table),
            Arc::new(end(|value| match value {
                Value::Rel { src, .. } => Some(*src),
                _ => None,
            })),
            Arc::new(end(|value| match value {
                Value::Rel { dst, .. } => Some(*dst),
                _ => None,
            })),
            Arc::new(end(|value| match value {
                Value::Rel { ord, .. } => Some(*ord),
                _ => None,
            })),
        ],
        Some(present(values)),
    )?))
}

/// The table name of every row, borrowed rather than copied.
///
/// The client owns the names and a column holds as many rows as the
/// result does, so the names go in by reference and the only string
/// built here is the stand-in for a table the catalog no longer has,
/// which is one per missing table rather than one per row.
fn names<'a>(
    values: &[&Value],
    id_of: impl Fn(&Value) -> Option<u32>,
    name_of: impl Fn(u32) -> Option<&'a str>,
) -> StringArray {
    let mut gone: HashMap<u32, String> = HashMap::new();
    for value in values {
        if let Some(id) = id_of(value)
            && name_of(id).is_none()
        {
            gone.entry(id).or_insert_with(|| format!("#{id}"));
        }
    }
    values
        .iter()
        .map(|value| id_of(value).map(|id| name_of(id).unwrap_or_else(|| gone[&id].as_str())))
        .collect()
}

/// A path column, as the two lists a walk is.
///
/// A path is nodes and edges alternating, and Arrow has no type for a
/// list whose elements alternate between two structs. Two lists say the
/// same thing without a union in the middle of it: the nodes in the order
/// the walk visits them, the edges in the order it crosses them, and one
/// more node than edge.
fn paths<T: Tables + ?Sized>(name: &str, values: &[&Value], tables: &T) -> Result<ArrayRef, Error> {
    let mut node_offsets = vec![0i32];
    let mut rel_offsets = vec![0i32];
    let mut walked_nodes: Vec<&Value> = Vec::new();
    let mut walked_rels: Vec<&Value> = Vec::new();
    for (row, value) in values.iter().enumerate() {
        match value {
            Value::Path(elements) => {
                walked_nodes.extend(elements.iter().step_by(2));
                walked_rels.extend(elements.iter().skip(1).step_by(2));
            }
            // Never in a result: the executor settles a chain into its
            // edges before the rows leave the pipeline.
            Value::Chain(_) => {
                return Err(Error::Type(format!(
                    "row {row} of column '{name}' is a path chain, which is internal to the executor"
                )));
            }
            _ => {}
        }
        node_offsets.push(walked_nodes.len() as i32);
        rel_offsets.push(walked_rels.len() as i32);
    }
    let walked = ListArray::try_new(
        item(DataType::Struct(node_fields())),
        OffsetBuffer::new(node_offsets.into()),
        nodes(&walked_nodes, tables)?,
        Some(present(values)),
    )?;
    let crossed = ListArray::try_new(
        item(DataType::Struct(rel_fields())),
        OffsetBuffer::new(rel_offsets.into()),
        rels(&walked_rels, tables)?,
        Some(present(values)),
    )?;
    Ok(Arc::new(StructArray::try_new(
        path_fields(),
        vec![Arc::new(walked), Arc::new(crossed)],
        Some(present(values)),
    )?))
}
