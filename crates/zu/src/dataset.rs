//! A directory of node files and rel files, loaded as the graph it
//! describes rather than as the edge list underneath it.
//!
//! The edge list loader builds one node table and one rel table, which
//! is the whole of a SNAP or GAP graph and none of a labelled one. A
//! finbench dataset is three node labels and six rel types, and a query
//! over it asks for a transfer's amount and a person's name, so a load
//! that flattens it answers nothing it was written to answer. What this
//! adds is the tables: one node table per node file, one rel table per
//! rel file, each rel table bound to the two node tables its ends name.
//!
//! Ids are the dataset's own and stay that way. A node file gives every
//! row an id, a rel file names its endpoints by those ids, and the two
//! need not be dense or start at zero or even be disjoint from another
//! label's, because each label is mapped separately. A row of a table is
//! the line it was written on; the id it came in with is kept twice, as
//! the `id` property so `RETURN n.id` answers with it, and as the
//! primary-key index so `{id: $k}` finds the row without a scan.
//!
//! What this does not do is guess. Every node file names its table,
//! every rel file names its table and both of its ends, and an edge
//! whose endpoint no node file declared is an error naming the id
//! rather than a row invented for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use zu_common::{Result, ZuError};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Ends, bulk_load_between_keyed, read_node_csv, read_rel_csv_keyed};
use zu_zu1::props::{OwnedColumn, OwnedValues, store_props_owned, store_rel_props_owned};
use zu_zu1::reorder::load_order;

/// One node file: the table its rows load as, and where to read them.
#[derive(Debug, Clone)]
pub struct NodeFile {
    pub table: String,
    pub path: PathBuf,
}

/// One rel file: the table its edges load as, the node tables its two
/// ends name their rows in, and where to read them.
#[derive(Debug, Clone)]
pub struct RelFile {
    pub table: String,
    pub from: String,
    pub to: String,
    pub path: PathBuf,
    pub undirected: bool,
}

/// What a load put in the file, for the caller that reports it.
#[derive(Debug, Default, Clone)]
pub struct DatasetStats {
    /// Each node table and how many rows it holds, in load order.
    pub nodes: Vec<(String, u64)>,
    /// Each rel table and how many edges it holds, in load order.
    pub rels: Vec<(String, u64)>,
    /// The property columns stored, node ones and edge ones.
    pub node_columns: u64,
    pub rel_columns: u64,
    /// The node tables whose rows carry a primary-key index.
    pub keyed: Vec<String>,
}

/// One node table read whole: the id of every row in row order, the
/// property columns beside them, and the id-to-row map a rel file's
/// endpoints resolve through.
struct NodeTable {
    keys: Vec<u64>,
    columns: Vec<OwnedColumn>,
    rows: HashMap<u64, u32>,
    /// Whether a row's id is its row number for every row, which is the
    /// dense contract the store keeps without an index. A table that
    /// holds it needs no key index and no `id` column, because the
    /// fallback answers both.
    dense: bool,
}

impl NodeTable {
    fn read(file: &NodeFile) -> Result<Self> {
        let named = |e: ZuError| {
            ZuError::InvalidArgument(format!(
                "node file '{}' for table '{}': {e}",
                file.path.display(),
                file.table
            ))
        };
        let (keys, columns) = read_node_csv(&file.path).map_err(named)?;
        if keys.len() > u32::MAX as usize {
            return Err(ZuError::InvalidArgument(format!(
                "node table '{}' holds {} rows, past what a row id says",
                file.table,
                keys.len()
            )));
        }
        let mut rows = HashMap::with_capacity(keys.len());
        for (row, &key) in keys.iter().enumerate() {
            if rows.insert(key, row as u32).is_some() {
                return Err(ZuError::InvalidArgument(format!(
                    "node table '{}' names the id {key} twice",
                    file.table
                )));
            }
        }
        let dense = keys.iter().enumerate().all(|(row, &key)| key == row as u64);
        Ok(NodeTable {
            keys,
            columns,
            rows,
            dense,
        })
    }
}

/// Loads a dataset of node files and rel files into a fresh zu1 file at
/// `out`.
///
/// Every rel file's two ends must name node files given here, and every
/// node file must be an end of some rel file, because a node table's row
/// domain is declared by the loads that touch it and a table no load
/// touches would come out of this holding nothing.
pub fn load_dataset(nodes: &[NodeFile], rels: &[RelFile], out: &Path) -> Result<DatasetStats> {
    if nodes.is_empty() {
        return Err(ZuError::InvalidArgument(
            "a dataset needs at least one node file".into(),
        ));
    }
    let mut tables: HashMap<&str, NodeTable> = HashMap::with_capacity(nodes.len());
    for file in nodes {
        if tables
            .insert(file.table.as_str(), NodeTable::read(file)?)
            .is_some()
        {
            return Err(ZuError::InvalidArgument(format!(
                "two node files load as the table '{}'",
                file.table
            )));
        }
    }
    let count = |table: &str| -> Result<u64> {
        tables
            .get(table)
            .map(|t| t.keys.len() as u64)
            .ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "no node file loads as the table '{table}', which a rel file names as an end"
                ))
            })
    };
    for file in rels {
        count(&file.from)?;
        count(&file.to)?;
    }
    for file in nodes {
        if !rels
            .iter()
            .any(|r| r.from == file.table || r.to == file.table)
        {
            return Err(ZuError::InvalidArgument(format!(
                "node table '{}' is an end of no rel file; a node table gets its row \
                 domain from the loads that touch it",
                file.table
            )));
        }
    }

    let mut stats = DatasetStats::default();
    let mut db = Zu1File::create(out)?;
    // The primary-key index lives on a rel table's directory and is
    // sized to that table's FROM domain, so a node table's ids ride
    // along on the first rel table that leaves it. Writing it on the
    // second one too would be the same map stored twice.
    let mut keyed: Vec<&str> = Vec::new();
    for file in rels {
        let (from_count, to_count) = (count(&file.from)?, count(&file.to)?);
        let named = |e: ZuError| {
            ZuError::InvalidArgument(format!(
                "rel file '{}' for table '{}': {e}",
                file.path.display(),
                file.table
            ))
        };
        let (keyed_edges, columns) = read_rel_csv_keyed(&file.path).map_err(named)?;
        let mut edges: Vec<(u32, u32)> = Vec::with_capacity(keyed_edges.len());
        for (line, &(src, dst)) in keyed_edges.iter().enumerate() {
            let row = |end: &str, table: &str, id: u64| -> Result<u32> {
                tables[table].rows.get(&id).copied().ok_or_else(|| {
                    ZuError::InvalidArgument(format!(
                        "rel table '{}' edge {} has the {end} id {id}, and no row of \
                         '{table}' carries it",
                        file.table,
                        line + 1
                    ))
                })
            };
            edges.push((
                row("source", &file.from, src)?,
                row("destination", &file.to, dst)?,
            ));
        }
        drop(keyed_edges);
        // Load order is source and then destination, and an edge
        // property column is addressed by nothing but the ordinal that
        // order gives, so the columns move by the same permutation.
        let columns: Vec<OwnedColumn> = if columns.is_empty() {
            edges.sort_unstable();
            Vec::new()
        } else {
            let order = load_order(&mut edges);
            columns
                .into_iter()
                .map(|c| OwnedColumn {
                    values: c.values.permuted(&order),
                    name: c.name,
                })
                .collect()
        };
        let from = tables.get(file.from.as_str()).expect("checked above");
        let carry =
            (!from.dense && !keyed.contains(&file.from.as_str())).then(|| from.keys.clone());
        bulk_load_between_keyed(
            &mut db,
            Ends::between((&file.from, from_count), (&file.to, to_count)),
            &file.table,
            &edges,
            carry.as_deref(),
            file.undirected,
        )?;
        if carry.is_some() {
            keyed.push(file.from.as_str());
            stats.keyed.push(file.from.clone());
        }
        stats.rels.push((file.table.clone(), edges.len() as u64));
        if !columns.is_empty() {
            // After the load and not before: a load writes the group
            // directory whole, and the edge property root hangs off it.
            stats.rel_columns += columns.len() as u64;
            store_rel_props_owned(&mut db, &file.table, &columns)?;
        }
    }

    for file in nodes {
        let table = tables.get_mut(file.table.as_str()).expect("read above");
        // The id column is what `RETURN n.id` reads. Without one the
        // property path falls back to the row offset, which is the right
        // answer only for a table whose ids were already its row
        // numbers, and this loader maps every label into its own dense
        // space, so nearly every table needs it stored.
        if !table.dense && !table.columns.iter().any(|c| c.name == "id") {
            table.columns.push(OwnedColumn {
                name: "id".to_string(),
                values: OwnedValues::Int(table.keys.clone()),
            });
        }
        stats
            .nodes
            .push((file.table.clone(), table.keys.len() as u64));
        if !table.columns.is_empty() {
            stats.node_columns += table.columns.len() as u64;
            store_props_owned(&mut db, &file.table, &table.columns)?;
        }
    }
    Ok(stats)
}
