//! The `zu1` native single-file storage engine.
//!
//! Byte-level format is specified in `docs/04-storage-zu1-format.md`.
//! `file` holds the headers, block I/O, and the dual-header checkpoint
//! flip; `meta` the meta-block chains behind every root pointer. Node
//! groups, segments, and CSR build on top of these within M1.

pub mod catalog;
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
                match col.ty {
                    props::PropType::Str => {
                        let (mut blob, mut ends) = (Vec::new(), Vec::new());
                        fullzip::read_blob_segment(&mut db, &col.meta, &mut blob, &mut ends)?;
                    }
                    props::PropType::Int => {
                        values.clear();
                        segment::read_segment(&mut db, &col.meta, &mut values)?;
                    }
                }
                bytes += col.meta.payload_len;
                live.extend(col.meta.blocks.iter().copied());
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
        let from = catalog.node_by_id(rel.from).expect("validated on decode");
        if directory.node_count > from.node_count {
            return Err(corrupt(
                "catalog",
                format!(
                    "rel table '{}' spans {} nodes, node table '{}' holds {}",
                    rel.name, directory.node_count, from.name, from.node_count
                ),
            ));
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
        if let Some(keys) = &directory.keys {
            keys::verify_key_index(&mut db, keys, directory.node_count)?;
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
}
