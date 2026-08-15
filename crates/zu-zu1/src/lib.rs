//! The `zu1` native single-file storage engine.
//!
//! Byte-level format is specified in `docs/04-storage-zu1-format.md`.
//! `file` holds the headers, block I/O, and the dual-header checkpoint
//! flip; `meta` the meta-block chains behind every root pointer. Node
//! groups, segments, and CSR build on top of these within M1.

pub mod algo;
pub mod cache;
pub mod catalog;
pub mod colors;
pub mod epoch;
pub mod file;
pub mod fold;
pub mod fullzip;
pub mod graph;
pub mod ingest;
pub mod keys;
pub mod meta;
#[cfg(feature = "arrow")]
pub mod parquet;
pub mod props;
pub mod reorder;
pub mod segment;
pub mod stats;
pub mod txn;
pub mod vfs;
pub mod wal;

use std::path::Path;

use zu_common::Result;

use crate::file::Zu1File;

/// File magic: UTF-8 図 followed by `ZU1\0\n`.
pub const MAGIC: [u8; 8] = [0xE5, 0x9B, 0xB3, b'Z', b'U', b'1', 0x00, 0x0A];

/// Fixed block size in bytes.
pub const BLOCK_SIZE: u32 = 262_144;

/// Current format version.
pub const FORMAT_VERSION: u16 = 1;

/// Oldest reader version that can open files we write.
pub const MIN_READER_VERSION: u16 = 1;

/// Walks the whole file checking every crc: file header, database
/// headers, the catalog, the table index, and each rel table's group
/// directory with every column segment it lists, the primary-key index
/// included when the table carries one. Cross-checks the three
/// against each other (every rel table has a directory, every index
/// entry belongs to a catalog table, node entries decode as props
/// directories with row-aligned columns, counts agree), then decodes
/// the free list and
/// rejects a file whose free list claims a block a live chain or segment
/// uses, since allocating such a block would overwrite live data.
/// Returns the number of payload bytes verified.
pub fn verify(path: &Path) -> Result<u64> {
    let corrupt = |what, detail| zu_common::ZuError::Corrupt { what, detail };
    let mut db = Zu1File::open(path)?;
    let mut bytes = 0u64;
    let mut live: std::collections::HashSet<file::BlockPtr> = std::collections::HashSet::new();
    for root in [
        db.db_header().catalog_root,
        db.db_header().table_index_root,
        db.db_header().stats_root,
    ] {
        bytes += meta::read_chain(&mut db, root)?.len() as u64;
        live.extend(meta::chain_blocks(&mut db, root)?);
    }
    let catalog = catalog::Catalog::load(&mut db)?;
    let index = catalog::TableIndex::load(&mut db)?;
    for rel in catalog.rel_tables() {
        if index.get(rel.id).is_none() {
            return Err(corrupt(
                "table index",
                format!("rel table '{}' has no directory entry", rel.name),
            ));
        }
    }
    let mut values = Vec::new();
    for &(id, root) in index.entries() {
        let chain = meta::read_chain(&mut db, root)?;
        bytes += chain.len() as u64;
        live.extend(meta::chain_blocks(&mut db, root)?);
        // A node table's entry is its props directory (the M2 column
        // slice), a rel table's entry is its group directory, and a
        // reserved key carries a node table's persisted tombstones.
        if id & fold::TOMBSTONE_KEY != 0 {
            let table = id & !fold::TOMBSTONE_KEY;
            let node = catalog.node_by_id(table).ok_or_else(|| {
                corrupt(
                    "table index",
                    format!("tombstone entry {id} names no node table"),
                )
            })?;
            let offsets = fold::decode_tombstones(&chain)?;
            if let Some(&offset) = offsets.iter().find(|&&o| o >= node.node_count) {
                return Err(corrupt(
                    "tombstone chain",
                    format!(
                        "'{}' tombstones row {offset} beyond its {} rows",
                        node.name, node.node_count
                    ),
                ));
            }
            continue;
        }
        if let Some(node) = catalog.node_by_id(id) {
            let props = props::PropsDirectory::decode(&chain)?;
            if props.node_count > node.node_count {
                return Err(corrupt(
                    "props directory",
                    format!(
                        "'{}' props span {} rows, table holds {}",
                        node.name, props.node_count, node.node_count
                    ),
                ));
            }
            for col in &props.columns {
                if col.meta.value_count != props.node_count {
                    return Err(corrupt(
                        "props directory",
                        format!(
                            "column '{}' holds {} values over {} rows",
                            col.name, col.meta.value_count, props.node_count
                        ),
                    ));
                }
                if col.is_lane() {
                    values.clear();
                    segment::read_segment(&mut db, &col.meta, &mut values)?;
                } else {
                    let (mut blob, mut ends) = (Vec::new(), Vec::new());
                    fullzip::read_blob_segment(&mut db, &col.meta, &mut blob, &mut ends)?;
                }
                bytes += col.meta.payload_len;
                live.extend(col.meta.blocks.iter().copied());
                if let Some(meta) = &col.validity {
                    values.clear();
                    segment::read_segment(&mut db, meta, &mut values)?;
                    bytes += meta.payload_len;
                    live.extend(meta.blocks.iter().copied());
                }
            }
            // The label bitset is checked against the catalog, which is
            // the only place that says what a bit means: a row may carry
            // a label its table declares, and it carries the table's own
            // label whatever else it holds.
            if let Some(meta) = &props.labels {
                if meta.value_count != props.node_count {
                    return Err(corrupt(
                        "props directory",
                        format!(
                            "'{}' labels hold {} words over {} rows",
                            node.name, meta.value_count, props.node_count
                        ),
                    ));
                }
                values.clear();
                segment::read_segment(&mut db, meta, &mut values)?;
                let declared = node.label_mask();
                let primary = 1u64 << node.primary_label();
                if let Some((row, word)) = values
                    .iter()
                    .enumerate()
                    .find(|(_, w)| *w & !declared != 0 || *w & primary == 0)
                {
                    return Err(corrupt(
                        "props directory",
                        format!(
                            "'{}' row {row} carries labels {word:#x} against a declared \
                             {declared:#x}",
                            node.name
                        ),
                    ));
                }
                bytes += meta.payload_len;
                live.extend(meta.blocks.iter().copied());
            }
            continue;
        }
        let rel = catalog
            .rel_by_id(id)
            .ok_or_else(|| corrupt("table index", format!("entry {id} names no catalog table")))?;
        let directory = graph::Directory::decode(&chain)?;
        if directory.edge_count != rel.edge_count {
            return Err(corrupt(
                "catalog",
                format!(
                    "rel table '{}' claims {} edges, directory holds {}",
                    rel.name, rel.edge_count, directory.edge_count
                ),
            ));
        }
        // Each end of the rel table is checked against the node table it
        // names, which are two different tables when the edges run
        // between labels and the same one twice when they do not.
        for (end, id, spans) in [
            ("source", rel.from, directory.from_count),
            ("destination", rel.to, directory.to_count),
        ] {
            let table = catalog.node_by_id(id).expect("validated on decode");
            if spans > table.node_count {
                return Err(corrupt(
                    "catalog",
                    format!(
                        "rel table '{}' spans {spans} nodes at its {end} end, node table '{}' holds {}",
                        rel.name, table.name, table.node_count
                    ),
                ));
            }
        }
        for group in &directory.groups {
            for seg in [
                &group.fwd.offsets,
                &group.fwd.neighbors,
                &group.bwd.offsets,
                &group.bwd.neighbors,
            ] {
                values.clear();
                segment::read_segment(&mut db, seg, &mut values)?;
                bytes += seg.payload_len;
                live.extend(seg.blocks.iter().copied());
            }
        }
        if directory.props != file::NULL_BLOCK {
            let chain = meta::read_chain(&mut db, directory.props)?;
            bytes += chain.len() as u64;
            live.extend(meta::chain_blocks(&mut db, directory.props)?);
            let props = props::PropsDirectory::decode(&chain)?;
            // Edge columns are row-aligned with the edges the way node
            // columns are with the rows, and the alignment is the whole
            // of what ties a value to an edge, so a column that has
            // drifted from the edge count is corruption and not a
            // column with a gap in it.
            if props.node_count != directory.edge_count {
                return Err(corrupt(
                    "props directory",
                    format!(
                        "rel table '{}' props span {} edges, directory holds {}",
                        rel.name, props.node_count, directory.edge_count
                    ),
                ));
            }
            for col in &props.columns {
                if col.meta.value_count != props.node_count {
                    return Err(corrupt(
                        "props directory",
                        format!(
                            "column '{}' holds {} values over {} edges",
                            col.name, col.meta.value_count, props.node_count
                        ),
                    ));
                }
                if col.is_lane() {
                    values.clear();
                    segment::read_segment(&mut db, &col.meta, &mut values)?;
                } else {
                    let (mut blob, mut ends) = (Vec::new(), Vec::new());
                    fullzip::read_blob_segment(&mut db, &col.meta, &mut blob, &mut ends)?;
                }
                bytes += col.meta.payload_len;
                live.extend(col.meta.blocks.iter().copied());
                if let Some(meta) = &col.validity {
                    values.clear();
                    segment::read_segment(&mut db, meta, &mut values)?;
                    bytes += meta.payload_len;
                    live.extend(meta.blocks.iter().copied());
                }
            }
        }
        if let Some(keys) = &directory.keys {
            keys::verify_key_index(&mut db, keys, directory.from_count)?;
            for seg in [&keys.keys, &keys.rows] {
                bytes += seg.payload_len;
                live.extend(seg.blocks.iter().copied());
            }
        }
    }
    let free_root = db.db_header().free_list_root;
    let free_bytes = meta::read_chain(&mut db, free_root)?;
    bytes += free_bytes.len() as u64;
    let free = file::decode_free_list(&free_bytes, db.db_header().block_count)?;
    if let Some(ptr) = free.iter().find(|ptr| live.contains(ptr)) {
        return Err(corrupt(
            "free list",
            format!("block {ptr} is listed free but the graph uses it"),
        ));
    }
    Ok(bytes)
}

/// How a store's blocks divide between what the schema costs and what
/// the graph in it costs.
///
/// This exists because a store's size on its own does not divide by a
/// graph. Every engine writes something before it holds anything, and
/// zu writes rather a lot of it: the header block, the catalog, the
/// table index and the statistics are four blocks of 256 KiB before a
/// single node exists. A tool dividing 1 MiB by three edges gets a
/// number in the millions of bits per edge and publishes it as an
/// encoding, which happened, repeatedly, to a benchmark harness that
/// had no way to ask this question.
///
/// The split is drawn where it can be drawn honestly. The four schema
/// structures are fixed by the shape of the database and do not know
/// how many rows are under them. Everything else, node groups, column
/// segments, adjacency, key indexes and the per-table directories that
/// name them, grows with the graph and is data. The free list is
/// neither: it is space this file owns and is not currently using, so
/// it is counted apart from both rather than charged to the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bytes per block. Every count below is in blocks; this is what
    /// turns one into bytes.
    pub block_size: u64,
    /// Every block the file spans, the header block included.
    pub blocks: u64,
    /// The header block plus the catalog, table index and statistics
    /// chains: what this database would weigh holding nothing.
    pub schema_blocks: u64,
    /// Blocks the free list names, plus the blocks the free list is
    /// itself written in.
    pub free_blocks: u64,
    /// What is left, which is the graph.
    pub data_blocks: u64,
}

impl Layout {
    /// Total size, as the block count implies it.
    pub fn bytes(&self) -> u64 {
        self.blocks * self.block_size
    }

    /// What the schema costs, which is the figure to subtract before
    /// dividing a store by the graph in it.
    pub fn schema_bytes(&self) -> u64 {
        self.schema_blocks * self.block_size
    }

    /// What the free list is holding on to.
    pub fn free_bytes(&self) -> u64 {
        self.free_blocks * self.block_size
    }

    /// What the graph costs.
    pub fn data_bytes(&self) -> u64 {
        self.data_blocks * self.block_size
    }
}

/// Reads a file's [`Layout`].
///
/// It follows four chains and decodes the free list, which is a handful
/// of block reads and no scan of the graph, so it is cheap enough to
/// run after every load. It validates every crc it touches on the way,
/// because [`meta::chain_blocks`] does, but it is not [`verify`]: a
/// file whose segments are corrupt still reports a layout.
pub fn layout(path: &Path) -> Result<Layout> {
    let mut db = Zu1File::open(path)?;
    let block_size = u64::from(db.file_header().block_size);
    let block_count = db.db_header().block_count;
    // Block 0 holds the file header and both database header slots, and
    // the pointers the roots use start at 1.
    let mut schema_blocks = 1;
    for root in [
        db.db_header().catalog_root,
        db.db_header().table_index_root,
        db.db_header().stats_root,
    ] {
        schema_blocks += meta::chain_blocks(&mut db, root)?.len() as u64;
    }
    let free_root = db.db_header().free_list_root;
    let free_chain = meta::chain_blocks(&mut db, free_root)?.len() as u64;
    let listed = file::decode_free_list(&meta::read_chain(&mut db, free_root)?, block_count)?.len();
    let free_blocks = free_chain + listed as u64;
    let blocks = block_count + 1;
    Ok(Layout {
        block_size,
        blocks,
        schema_blocks,
        free_blocks,
        // Saturating rather than asserting: a file with blocks reachable
        // from no root leaks them, which VACUUM cleans up and which is
        // not this function's business to refuse a number over.
        data_blocks: blocks.saturating_sub(schema_blocks + free_blocks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_spells_zu() {
        assert_eq!(std::str::from_utf8(&MAGIC[0..3]).unwrap(), "図");
        assert_eq!(&MAGIC[3..6], b"ZU1");
    }

    #[test]
    fn block_size_is_power_of_two() {
        assert!(BLOCK_SIZE.is_power_of_two());
        assert_eq!(BLOCK_SIZE, 256 * 1024);
    }

    /// A store of `edges` edges over `nodes` nodes, laid out the
    /// ordinary way.
    fn loaded(path: &Path, nodes: u32, edges: u32) -> Layout {
        let mut db = Zu1File::create(path).expect("create");
        let mut list: Vec<(u32, u32)> = (0..edges)
            .map(|i| (i % nodes, (i.wrapping_mul(2_654_435_761)) % nodes))
            .collect();
        list.sort_unstable();
        list.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", nodes.into(), &list).expect("load");
        drop(db);
        layout(path).expect("layout")
    }

    #[test]
    fn the_three_parts_of_a_store_add_up_to_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let small = loaded(&dir.path().join("small.zu1"), 97, 400);
        assert_eq!(
            small.schema_blocks + small.free_blocks + small.data_blocks,
            small.blocks
        );
        assert_eq!(small.bytes(), small.blocks * u64::from(BLOCK_SIZE));
        // The schema is a real cost and not the whole file, which is
        // the only reason subtracting it is worth doing.
        assert!(small.schema_bytes() > 0, "{small:?}");
        assert!(small.schema_bytes() < small.bytes(), "{small:?}");
    }

    #[test]
    fn a_bigger_graph_costs_data_blocks_and_not_schema_blocks() {
        // This is the property the figure is for. Two stores of the same
        // shape holding different numbers of edges pay the same schema,
        // so a harness that subtracts it is left with something that
        // divides by the graph.
        let dir = tempfile::tempdir().expect("tempdir");
        let small = loaded(&dir.path().join("small.zu1"), 97, 400);
        let large = loaded(&dir.path().join("large.zu1"), 60_000, 400_000);
        assert_eq!(
            small.schema_blocks, large.schema_blocks,
            "the schema moved with the row count: {small:?} against {large:?}"
        );
        assert!(
            large.data_blocks > small.data_blocks,
            "a hundred times the edges cost no more data: {small:?} against {large:?}"
        );
    }
}
