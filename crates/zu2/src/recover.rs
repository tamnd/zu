//! Recovery: a forward scan of the log that rebuilds the index.
//!
//! The record log is the write ahead log, so there is nothing to redo
//! from somewhere else and nothing to undo. Reading the records in the
//! order they were written and installing each one leaves the index
//! naming the newest record for every key, which is the same state the
//! writes left it in.
//!
//! The scan stops at the first record whose checksum does not hold, and
//! that address is the durable prefix. Padding at the end of a page is
//! zeros, so a scan that finds a zero header there moves to the next
//! page rather than stopping, and stops only when the file has no more
//! pages. A torn write inside a page fails its checksum and ends the
//! scan there, which is the behaviour that matters: everything before
//! it was acknowledged and everything after it was not.
//!
//! This is deliberately not a checkpoint. Concurrent Prefix Recovery
//! (Prasaad, Chandramouli, Kossmann, SIGMOD 2019) is the model for one,
//! and until it exists a scan of a young log is fast and a wrong
//! checkpoint is worse than no checkpoint.

use crate::addr::{Address, FIRST, PAGE_SIZE, page_of, page_start};
use crate::db::{Core, restore_pages};
use crate::error::Result;
use crate::graph;
use crate::index::{self, EMPTY, Index, SLOTS};
use crate::record::{self, RecordRef};

/// Rebuilds the index from the file and leaves the log ready to append.
pub fn replay(core: &Core) -> Result<()> {
    let len = restore_pages(core)?;
    // A compacted file starts where its begin marker says, because the
    // pages before that are a hole. Starting at FIRST would still work,
    // since a hole reads as zeros and a page of zeros is skipped the
    // same way page padding is, but it would restore every punched page
    // into memory to learn that.
    let mut address = core.log.begin().max(FIRST);
    let mut version = 0u64;
    let mut records = 0u64;
    while address < len {
        let page = page_of(address);
        let base = core.log.resident(address);
        if base.is_null() {
            break;
        }
        // SAFETY: the page is resident and the address is 8 byte
        // aligned, so the header words are readable. The lengths are
        // checked against the page before anything past the header is
        // touched.
        let size = unsafe {
            let header = RecordRef::new(base);
            let key_len = header.key_len();
            let value_len = header.value_len();
            let size = record::size_of(key_len, value_len);
            let room = PAGE_SIZE - (address - page_start(page)) as usize;
            if key_len == 0 && value_len == 0 {
                // Either page padding or the end of the log.
                None
            } else if size > room || !header.intact() {
                break;
            } else {
                if header.kind() == record::KIND_EDGE {
                    // Edge records are not keyed, so nothing goes in the
                    // index. A remove that replays after the add it
                    // cancels lands the same way it did the first time,
                    // because the log is in commit order.
                    graph::replay_edge(core, header.value_unchecked())?;
                } else {
                    if header.kind() == record::KIND_VERTEX {
                        // A vertex with no edges yet would otherwise
                        // leave no trace, and the next allocation would
                        // hand its id out again.
                        graph::replay_vertex(core, header.value_unchecked());
                    }
                    install(core, header, address);
                }
                version = version.max(header.version());
                records += 1;
                Some(size)
            }
        };
        match size {
            Some(size) => address += size as u64,
            None => {
                let next = page_start(page + 1);
                if next >= len {
                    break;
                }
                address = next;
            }
        }
    }
    let _ = records;
    core.log.resume_at(address);
    core.set_version(version);
    Ok(())
}

/// Puts one record's address into the index, by the same rules the
/// write path uses so that a recovered table behaves like a written
/// one. Single threaded, so no compare and swap and no tentative bit.
fn install(core: &Core, header: RecordRef<'_>, address: Address) {
    let key = header.key();
    let hash = index::hash(key);
    let tag = Index::tag(hash);
    let bucket = core.index.bucket(hash);
    let mut empty = None;
    for i in 0..SLOTS {
        let entry = bucket.slots[i].load(std::sync::atomic::Ordering::Relaxed);
        if entry == EMPTY {
            if empty.is_none() {
                empty = Some(i);
            }
            continue;
        }
        if index::tag_of(entry) != tag && !index::is_foreign(entry) {
            continue;
        }
        if chain_holds(core, index::address_of(entry), key) {
            bucket.slots[i].store(
                index::entry(tag, address, index::is_foreign(entry)),
                std::sync::atomic::Ordering::Relaxed,
            );
            return;
        }
    }
    if let Some(i) = empty {
        bucket.slots[i].store(
            index::entry(tag, address, false),
            std::sync::atomic::Ordering::Relaxed,
        );
        return;
    }
    // The bucket was full when this record was written too, so it
    // displaced an entry then and it displaces one now.
    let i = tag as usize % SLOTS;
    bucket.slots[i].store(
        index::entry(tag, address, true),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Whether a chain contains `key`, following the pointers the records
/// themselves carry. Stops at the log's begin for the same reason the
/// live path does: below it there is nothing left to find.
fn chain_holds(core: &Core, mut address: Address, key: &[u8]) -> bool {
    let floor = core.log.begin();
    while address >= floor && address != crate::addr::NULL {
        let base = core.log.resident(address);
        if base.is_null() {
            return false;
        }
        // SAFETY: the whole file was restored into pages before the
        // scan, so any address a record points at is resident.
        let previous = unsafe {
            let r = RecordRef::new(base);
            if r.key() == key {
                return true;
            }
            r.previous()
        };
        address = previous;
    }
    false
}
