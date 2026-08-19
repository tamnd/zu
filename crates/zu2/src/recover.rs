//! Recovery: a forward scan of the log that rebuilds the index.
//!
//! The record log is the write ahead log, so there is nothing to redo
//! from somewhere else and nothing to undo. Reading the records in the
//! order they were written and installing each one leaves the index
//! naming the newest record for every key, which is the same state the
//! writes left it in.
//!
//! Address order is not quite commit order, and the difference is
//! compaction. A copy is appended before the compare and swap that would
//! publish it, so a copy that loses that race sits on the log above the
//! record that beat it and belongs to nobody. Installing by address
//! alone hands such a key back at the value the loser held (#436), so
//! the scan installs a record only when its version is at least the
//! version of the record the index already reaches for that key.
//! Versions are what compaction copies carry from the records they
//! copy, so the two rules are the same rule.
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
    // Where the last record the scan accepted ended, which is where the
    // next append goes. Not the same as where the scan stops: the file
    // can be longer than the log, either because a page ends in padding
    // or because the write path had provisioned blocks past the tail
    // that the run did not live to use, and appending after those would
    // leave a hole and lose the room.
    let mut end = address;
    let mut version = 0u64;
    let mut records = 0u64;
    while address < len {
        let page = page_of(address);
        let base = core.log.resident(address);
        if base.is_null() {
            break;
        }
        let room = PAGE_SIZE - (address - page_start(page)) as usize;
        if room < record::HEADER {
            // The gap the allocator leaves when the last record of a
            // page does not fit in what is left of it. There is no
            // header here to look at, and looking anyway reads past the
            // end of the page: a heap that happened to hold something
            // other than zeros next to it made the scan stop and lose
            // every record above this page (#438).
            let next = page_start(page + 1);
            if next >= len {
                break;
            }
            address = next;
            continue;
        }
        // SAFETY: the page is resident, the address is 8 byte aligned,
        // and a whole header fits in what is left of the page. The
        // lengths are checked against the page before anything past the
        // header is touched.
        let size = unsafe {
            let header = RecordRef::new(base);
            let key_len = header.key_len();
            let value_len = header.value_len();
            let size = record::size_of(key_len, value_len);
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
                        // A node with no edges yet would otherwise
                        // leave no trace, and the next allocation would
                        // hand its id out again.
                        graph::replay_node(core, header.value_unchecked());
                    }
                    install(core, header, address);
                }
                version = version.max(header.version());
                records += 1;
                Some(size)
            }
        };
        match size {
            Some(size) => {
                address += size as u64;
                end = address;
            }
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
    core.log.resume_at(end);
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
        if let Some(installed) = chain_version(core, index::address_of(entry), key) {
            // Not greater than: a pass that copies a copy leaves two
            // records carrying the same version, and the higher address
            // is the one that is still there after the region below it
            // goes.
            if header.version() >= installed {
                bucket.slots[i].store(
                    index::entry(tag, address, index::is_foreign(entry)),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
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

/// The version of the record this chain holds for `key`, or `None` when
/// it holds none. Follows the pointers the records themselves carry and
/// stops at the log's begin for the same reason the live path does:
/// below it there is nothing left to find.
///
/// The first match is the answer, because a chain runs newest first.
fn chain_version(core: &Core, mut address: Address, key: &[u8]) -> Option<u64> {
    let floor = core.log.begin();
    while address >= floor && address != crate::addr::NULL {
        let base = core.log.resident(address);
        if base.is_null() {
            return None;
        }
        // SAFETY: the whole file was restored into pages before the
        // scan, so any address a record points at is resident.
        let previous = unsafe {
            let r = RecordRef::new(base);
            if r.key() == key {
                return Some(r.version());
            }
            r.previous()
        };
        address = previous;
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::addr::{NULL, PAGE_SIZE, offset_of};
    use crate::db::{Db, Options};
    use crate::record::{HEADER, KIND_VALUE};

    /// A page whose last record leaves less room than a header, which is
    /// the gap the tail allocator makes when the next record does not
    /// fit and it moves on. The scan used to read a header out of that
    /// gap, past the end of the page, and stop the moment the heap next
    /// door held something other than zeros, losing every record above
    /// that page (#438).
    ///
    /// The gap is built rather than hoped for, and the test says so: it
    /// asserts the page really does end eight bytes short before it
    /// closes the database.
    #[test]
    fn a_page_that_ends_short_of_a_header_is_not_the_end_of_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("g.zu2");
        let options = Options {
            durability: crate::log::Durability::Async,
            ..Options::default()
        };
        let below = |i: u32| format!("fill{i:09}").into_bytes();
        let above = |i: u32| format!("over{i:09}").into_bytes();
        let over = 200u32;
        {
            let db = Db::create(&path, options).expect("create");
            let mut s = db.session();
            let mut i = 0u32;
            // Up to the last few kilobytes of the first page, in
            // ordinary thousand byte records.
            while PAGE_SIZE - offset_of(db.core().log.tail()) as usize >= 4096 {
                s.upsert(&below(i), &vec![b'x'; 1000]).expect("fill");
                i += 1;
            }
            // One record sized to land eight bytes short of the end,
            // which is a quarter of a header.
            let room = PAGE_SIZE - offset_of(db.core().log.tail()) as usize;
            let value = vec![b'x'; room - 8 - HEADER - below(i).len()];
            s.upsert(&below(i), &value).expect("short");
            assert_eq!(
                PAGE_SIZE - offset_of(db.core().log.tail()) as usize,
                8,
                "the page did not end short of a header, so this proves nothing"
            );
            // And the records the old scan would have thrown away.
            for i in 0..over {
                s.upsert(&above(i), &vec![b'y'; 1000]).expect("over");
            }
            db.sync().expect("sync");
        }

        let db = Db::open(&path, options).expect("reopen");
        let mut s = db.session();
        let mut out = Vec::new();
        for i in 0..over {
            assert!(
                s.read(&above(i), &mut out).expect("read"),
                "the scan stopped in the gap and lost over{i:09}"
            );
        }
    }

    /// The shape a compaction pass leaves on the log when one of its
    /// copies loses its race: an older version of a key sitting above
    /// the record that beat it, named by nothing. Memory is right either
    /// way because the compare and swap failed, so the only way to see
    /// it is to close and open again, and a replay that went by address
    /// handed back the value the loser held (#436).
    ///
    /// Written by hand rather than raced for, because a race that shows
    /// up in one run out of three is a test that passes for the wrong
    /// reason the other two.
    #[test]
    fn a_replay_takes_the_newest_version_and_not_the_highest_address() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v.zu2");
        {
            let db = Db::create(&path, Options::default()).expect("create");
            let mut s = db.session();
            s.upsert(b"k", b"old").expect("first");
            s.upsert(b"k", b"new").expect("second");
            s.slot.protect();
            let orphan = s
                .core
                .log
                .append(&s.slot, NULL, 1, b"k", b"old", false, KIND_VALUE);
            s.slot.unprotect();
            orphan.expect("orphan");
            db.sync().expect("sync");
        }

        let db = Db::open(&path, Options::default()).expect("reopen");
        let mut s = db.session();
        let mut out = Vec::new();
        assert!(s.read(b"k", &mut out).expect("read"), "the key is gone");
        assert_eq!(
            out,
            b"new".to_vec(),
            "the replay took the copy that lost its race"
        );
    }
}
