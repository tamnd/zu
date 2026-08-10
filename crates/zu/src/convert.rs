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

use zu_common::{Result, ZuError};
use zu_sqlite::{ColumnType, SqliteStore, TableDef, Value};
use zu_storage::Direction;
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction as Zu1Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropType, PropValues, PropsReader, load_props, store_props};

/// Converts a zu1 file into a fresh sqlite store at `db_path`.
pub fn zu1_to_sqlite(zu1_path: &Path, db_path: &Path) -> Result<()> {
    let mut zu = Zu1File::open(zu1_path)?;
    let catalog = Catalog::load(&mut zu)?;
    let mut sq = SqliteStore::open(db_path)?;

    for node in catalog.node_tables().to_vec() {
        let props = load_props(&mut zu, node.id)?;
        let cols: Vec<(String, PropType)> = props
            .iter()
            .flat_map(|p| p.columns.iter())
            .map(|c| (c.name.clone(), c.ty))
            .collect();
        let col_refs: Vec<(&str, ColumnType)> = cols
            .iter()
            .map(|(n, t)| {
                let ty = match t {
                    PropType::Int => ColumnType::Integer,
                    PropType::Str => ColumnType::Text,
                };
                (n.as_str(), ty)
            })
            .collect();
        sq.create_node_table(&node.name, &col_refs)?;

        let mut reader = props.map(PropsReader::new);
        let mut buf = Vec::new();
        sq.begin()?;
        for row in 0..node.node_count {
            let mut values = Vec::with_capacity(cols.len());
            if let Some(reader) = reader.as_mut() {
                for (ci, (name, ty)) in cols.iter().enumerate() {
                    values.push(match ty {
                        PropType::Int => Value::Int(reader.read_int(&mut zu, ci, row)? as i64),
                        PropType::Str => {
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
    Str(Vec<Vec<u8>>),
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
        let cols = sq.node_columns(&node.name)?;
        if cols.is_empty() {
            continue;
        }
        let count = sq.node_count(&node.name)?;
        let mut data = Vec::with_capacity(cols.len());
        for col in &cols {
            let mut column: Option<ColumnData> = None;
            for row in 0..count {
                let bad = |what: &str| {
                    ZuError::InvalidArgument(format!(
                        "'{}' column '{col}' row {row} {what}; zu1 property columns \
                         are dense and uniformly int or string",
                        node.name
                    ))
                };
                match (sq.read_node_prop(&node.name, row, col)?, &mut column) {
                    (Value::Int(v), None) => column = Some(ColumnData::Int(vec![v as u64])),
                    (Value::Int(v), Some(ColumnData::Int(vals))) => vals.push(v as u64),
                    (Value::Text(v), None) => {
                        column = Some(ColumnData::Str(vec![v.into_bytes()]));
                    }
                    (Value::Text(v), Some(ColumnData::Str(vals))) => vals.push(v.into_bytes()),
                    (Value::Null, _) => return Err(bad("is null")),
                    (Value::Int(_) | Value::Text(_), Some(_)) => {
                        return Err(bad("changes type"));
                    }
                    (_, _) => return Err(bad("is neither int nor text")),
                }
            }
            data.push(column.unwrap_or(ColumnData::Int(Vec::new())));
        }
        let str_refs: Vec<Vec<&[u8]>> = data
            .iter()
            .map(|c| match c {
                ColumnData::Str(vals) => vals.iter().map(|v| v.as_slice()).collect(),
                ColumnData::Int(_) => Vec::new(),
            })
            .collect();
        let columns: Vec<(&str, PropValues)> = cols
            .iter()
            .zip(&data)
            .zip(&str_refs)
            .map(|((name, c), refs)| {
                let values = match c {
                    ColumnData::Int(vals) => PropValues::Int(vals),
                    ColumnData::Str(_) => PropValues::Str(refs),
                };
                (name.as_str(), values)
            })
            .collect();
        store_props(&mut zu, &node.name, &columns)?;
    }
    Ok(())
}
