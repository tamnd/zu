//! Meta-block chains: linked lists of 256 KiB blocks carrying a byte
//! payload, used for the catalog, group directories, free list, and stats.
//!
//! Per-block layout: `next: u64 LE` (NULL_BLOCK terminates), `len: u32 LE`
//! bytes of payload in this block, `crc32c: u32 LE` of that payload, then
//! the payload itself. The chain is written back to front so every `next`
//! pointer is known before its block is written.

use zu_common::{Result, ZuError};

use crate::BLOCK_SIZE;
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};

const BLOCK_HEADER: usize = 16;

/// Payload bytes one chain block can carry.
pub const CHAIN_CAPACITY: usize = BLOCK_SIZE as usize - BLOCK_HEADER;

/// Writes `payload` as a chain of freshly allocated blocks and returns the
/// head pointer, or `NULL_BLOCK` for an empty payload.
pub fn write_chain(db: &mut Zu1File, payload: &[u8]) -> Result<BlockPtr> {
    if payload.is_empty() {
        return Ok(NULL_BLOCK);
    }
    let ptrs: Vec<BlockPtr> = payload
        .chunks(CHAIN_CAPACITY)
        .map(|_| db.allocate_block())
        .collect();
    let mut block = vec![0u8; BLOCK_SIZE as usize];
    for (i, part) in payload.chunks(CHAIN_CAPACITY).enumerate() {
        let next = ptrs.get(i + 1).copied().unwrap_or(NULL_BLOCK);
        block[..8].copy_from_slice(&next.to_le_bytes());
        block[8..12].copy_from_slice(&(part.len() as u32).to_le_bytes());
        block[12..16].copy_from_slice(&crc32c::crc32c(part).to_le_bytes());
        block[BLOCK_HEADER..BLOCK_HEADER + part.len()].copy_from_slice(part);
        block[BLOCK_HEADER + part.len()..].fill(0);
        db.write_block(ptrs[i], &block)?;
    }
    Ok(ptrs[0])
}

/// Follows a chain from `head`, validating every block's crc, and calls
/// `visit` with each block pointer and its payload slice. A chain longer
/// than the block count is rejected as a cycle.
fn walk_chain(
    db: &mut Zu1File,
    head: BlockPtr,
    mut visit: impl FnMut(BlockPtr, &[u8]),
) -> Result<()> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "meta chain",
        detail,
    };
    let mut ptr = head;
    let mut hops = 0u64;
    while ptr != NULL_BLOCK {
        hops += 1;
        if hops > db.db_header().block_count {
            return Err(corrupt(format!("cycle detected at block {ptr}")));
        }
        let block = db.read_block(ptr)?;
        let next = u64::from_le_bytes(block[..8].try_into().unwrap());
        let len = u32::from_le_bytes(block[8..12].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(block[12..16].try_into().unwrap());
        if len > CHAIN_CAPACITY {
            return Err(corrupt(format!("block {ptr} claims {len} payload bytes")));
        }
        let part = &block[BLOCK_HEADER..BLOCK_HEADER + len];
        if crc32c::crc32c(part) != stored {
            return Err(corrupt(format!("crc mismatch in block {ptr}")));
        }
        visit(ptr, part);
        ptr = next;
    }
    Ok(())
}

/// Follows a chain from `head` and returns the concatenated payload.
/// `NULL_BLOCK` reads as empty. Every block's crc is verified, and a chain
/// longer than the block count is rejected as a cycle.
pub fn read_chain(db: &mut Zu1File, head: BlockPtr) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    walk_chain(db, head, |_, part| payload.extend_from_slice(part))?;
    Ok(payload)
}

/// Follows a chain from `head` and returns the blocks it occupies, with
/// the same validation as [`read_chain`]. The free path uses this to
/// recycle a chain's own storage.
pub fn chain_blocks(db: &mut Zu1File, head: BlockPtr) -> Result<Vec<BlockPtr>> {
    let mut ptrs = Vec::new();
    walk_chain(db, head, |ptr, _| ptrs.push(ptr))?;
    Ok(ptrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db(dir: &tempfile::TempDir) -> Zu1File {
        Zu1File::create(&dir.path().join("meta.zu1")).unwrap()
    }

    #[test]
    fn empty_chain_is_null() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = fresh_db(&dir);
        assert_eq!(write_chain(&mut db, &[]).unwrap(), NULL_BLOCK);
        assert!(read_chain(&mut db, NULL_BLOCK).unwrap().is_empty());
    }

    #[test]
    fn multi_block_roundtrip_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.zu1");
        let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 253) as u8).collect();
        let head;
        {
            let mut db = Zu1File::create(&path).unwrap();
            head = write_chain(&mut db, &payload).unwrap();
            assert_eq!(db.db_header().block_count, 3, "600 KB needs three blocks");
            db.db_header_mut().catalog_root = head;
            db.checkpoint().unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        assert_eq!(db.db_header().catalog_root, head);
        assert_eq!(read_chain(&mut db, head).unwrap(), payload);
    }

    #[test]
    fn corrupt_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = fresh_db(&dir);
        let head = write_chain(&mut db, &[7u8; 100]).unwrap();
        let mut block = db.read_block(head).unwrap();
        block[BLOCK_HEADER + 5] ^= 0xFF;
        db.write_block(head, &block).unwrap();
        assert!(read_chain(&mut db, head).is_err());
    }

    #[test]
    fn cycle_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = fresh_db(&dir);
        let head = write_chain(&mut db, &vec![1u8; CHAIN_CAPACITY + 10]).unwrap();
        // Point the first block's next at itself.
        let mut block = db.read_block(head).unwrap();
        block[..8].copy_from_slice(&head.to_le_bytes());
        db.write_block(head, &block).unwrap();
        let err = read_chain(&mut db, head).unwrap_err();
        assert!(format!("{err}").contains("cycle"));
    }
}
