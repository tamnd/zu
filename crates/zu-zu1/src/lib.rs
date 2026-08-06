//! The `zu1` native single-file storage engine.
//!
//! Byte-level format is specified in `docs/04-storage-zu1-format.md`.
//! `file` holds the headers, block I/O, and the dual-header checkpoint
//! flip; `meta` the meta-block chains behind every root pointer. Node
//! groups, segments, and CSR build on top of these within M1.

pub mod file;
pub mod graph;
pub mod meta;
pub mod reorder;
pub mod segment;

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
/// headers, each meta-block chain reachable from the committed roots, and
/// every column segment listed in the group directory. Returns the number
/// of payload bytes verified.
pub fn verify(path: &Path) -> Result<u64> {
    let mut db = Zu1File::open(path)?;
    let roots = [
        db.db_header().catalog_root,
        db.db_header().table_index_root,
        db.db_header().free_list_root,
        db.db_header().stats_root,
    ];
    let mut bytes = 0u64;
    for root in roots {
        bytes += meta::read_chain(&mut db, root)?.len() as u64;
    }
    if db.db_header().table_index_root != file::NULL_BLOCK {
        let reader = graph::GraphReader::load(&mut db)?;
        let groups = reader.directory().groups.clone();
        let mut values = Vec::new();
        for group in &groups {
            for seg in [&group.offsets, &group.neighbors] {
                values.clear();
                segment::read_segment(&mut db, seg, &mut values)?;
                bytes += seg.payload_len;
            }
        }
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
