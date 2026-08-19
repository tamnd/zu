//! Compaction: giving the front of the log back to the filesystem.
//!
//! An append-only log with in-place updates only in its youngest pages
//! has one cost, and it is space. Every update outside the mutable
//! window writes a new record and leaves the old one where it was, so a
//! workload that rewrites the same keys grows the file without bound
//! while the live set stays the same size. That is the defect this file
//! exists to remove.
//!
//! The method is F2's lookup-based compaction (Kanellis, Chandramouli,
//! Hart, Venkataraman, PVLDB 18(12):4910-4923, 2025). Read the oldest
//! region of the log record by record and ask the hash index one
//! question about each: is this address still the one the index reaches
//! for this key? If it is, the record is live and gets appended again at
//! the tail. If it is not, it is a version somebody has already
//! replaced, and it goes. No mark phase, no reference counts, no second
//! index: the index that answers reads answers this too.
//!
//! Three things make it safe to then declare the whole region gone.
//!
//! A chain only ever points backwards, so the path from an index entry
//! to a live record above the region never dips below it. That is what
//! lets the walk stop at `begin` instead of at `NULL`: anything the walk
//! would have found down there is either dead or has a copy up here.
//!
//! A copy chains to whatever the index entry holds at the moment it is
//! made, not to the record it copies. So a copy is reachable the instant
//! its entry swings, and every key that was reachable through that entry
//! stays reachable through the copy.
//!
//! The region is scanned in address order, so a record that is only
//! reachable through another record in the same region has already been
//! copied by the time its chain link is dropped.
//!
//! Edges are not keyed, so the index cannot answer for them. The
//! adjacency itself does: an add is live when the edge is in the
//! adjacency now, and a remove is live when it is not. Keeping a remove
//! whose add lies above the region is what stops a replay from bringing
//! a deleted edge back, and it is why removes are the one thing here
//! that is kept conservatively rather than exactly.

use crate::addr::{Address, PAGE_SIZE, page_of, page_start};
use crate::db::Session;
use crate::error::Result;
use crate::graph;
use crate::graph::Direction;
use crate::log::Durability;
use crate::record::{self, RecordRef};

/// What one pass did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Compacted {
    /// Bytes of log the pass read.
    pub scanned: u64,
    /// Bytes it wrote back at the tail, which is the write amplification
    /// compaction costs.
    pub copied: u64,
    /// Records it read.
    pub records: u64,
    /// Records it found still live.
    pub live: u64,
    /// Bytes the filesystem took back, which is zero where there is no
    /// hole punch.
    pub reclaimed: u64,
}

/// The highest address a pass may safely take.
///
/// Two limits. The region has to be on disk, because the scan preads it
/// rather than holding pointers into pages an eviction could take away
/// mid-pass. And it has to be below the read-only boundary, because a
/// record above that one can still be rewritten in place and a copy of
/// it would be a copy of a value in motion.
pub fn ceiling(session: &Session<'_>) -> Address {
    let log = &session.core_ref().log;
    let safe = log.flushed().min(log.read_only());
    page_start(page_of(safe))
}

/// Compacts `[begin, upto)`, which must be a page boundary at or below
/// [`ceiling`].
///
/// Returns what it did. Compacting nothing is not an error: a log whose
/// oldest page is still inside the mutable window has nothing this can
/// take, and the caller tries again later.
pub fn compact(session: &mut Session<'_>, upto: Address) -> Result<Compacted> {
    let mut done = Compacted::default();
    let begin = session.core_ref().log.begin();
    let upto = upto.min(ceiling(session));
    if upto <= begin {
        return Ok(done);
    }
    debug_assert_eq!(upto % PAGE_SIZE as u64, 0, "compact to a page boundary");

    // The copies do not each wait for the device. One barrier at the end
    // covers all of them, and nothing below is allowed to move until it
    // has returned.
    let restore = session.durability();
    session.set_durability(Durability::Async);

    let mut page = vec![0u64; PAGE_SIZE / 8];
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut outcome = Ok(());
    'pages: for number in page_of(begin)..page_of(upto) {
        // SAFETY: the buffer is PAGE_SIZE bytes and 8 byte aligned
        // because it is a Vec<u64>, which is what a record header needs.
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(page.as_mut_ptr().cast::<u8>(), PAGE_SIZE) };
        if let Err(error) = session.core_ref().log.read_page(number, bytes) {
            outcome = Err(error);
            break 'pages;
        }
        let mut offset = if number == page_of(begin) {
            (begin - page_start(number)) as usize
        } else {
            0
        };
        while offset + record::HEADER <= PAGE_SIZE {
            let address = page_start(number) + offset as u64;
            if address >= upto {
                break 'pages;
            }
            // SAFETY: the buffer holds a whole page of the log and the
            // offset is 8 byte aligned, so the header words are there.
            // Nothing past the header is touched until the lengths have
            // been checked against what is left of the page.
            let (size, kind, tombstone) = unsafe {
                let r = RecordRef::new(page.as_ptr().cast::<u8>().add(offset));
                let key_len = r.key_len();
                let value_len = r.value_len();
                if key_len == 0 && value_len == 0 {
                    // Page padding, so the rest of this page is nothing.
                    break;
                }
                let size = record::size_of(key_len, value_len);
                if offset + size > PAGE_SIZE {
                    break;
                }
                key.clear();
                key.extend_from_slice(r.key());
                value.clear();
                value.extend_from_slice(r.value_unchecked());
                (size, r.kind(), r.tombstone())
            };
            done.records += 1;
            done.scanned += size as u64;
            let kept = match kind {
                record::KIND_EDGE => keep_edge(session, &value),
                _ => session.copy_forward(&key, &value, tombstone, kind, address),
            };
            match kept {
                Ok(true) => {
                    done.live += 1;
                    done.copied += size as u64;
                }
                Ok(false) => {}
                Err(error) => {
                    outcome = Err(error);
                    break 'pages;
                }
            }
            offset += size;
        }
    }
    session.set_durability(restore);
    outcome?;

    // Everything live is at the tail and on the device before a single
    // block of the region is released.
    let tail = session.core_ref().log.tail();
    session
        .core_ref()
        .log
        .make_durable(tail, Durability::Durable)?;
    done.reclaimed = session.core_ref().log.reclaim_to(upto)?;
    Ok(done)
}

/// Decides an edge record and re-appends it when it still counts.
///
/// An add is live when the adjacency has the edge, because a replay that
/// skipped it would come up short. A remove is live when the adjacency
/// does not, because the add it cancels may sit above this region and a
/// replay without the remove would bring the edge back. That keeps a few
/// removes longer than strictly needed, which is the safe direction.
fn keep_edge(session: &mut Session<'_>, payload: &[u8]) -> Result<bool> {
    let Some((add, src, dst)) = graph::decode_edge(payload) else {
        return Ok(false);
    };
    let present = session.neighbours(Direction::Out, src, |n| n.binary_search(&dst).is_ok());
    if present != add {
        return Ok(false);
    }
    session.append_untracked(record::KIND_EDGE, payload)?;
    Ok(true)
}

/// How far a pass should reach, given how much of the log is old.
///
/// A quarter of what is safely compactable at a time. Taking the whole
/// thing would stall the flusher behind one long copy, and taking a
/// single page would spend a scan on too little.
pub fn slice(session: &Session<'_>) -> Address {
    let log = &session.core_ref().log;
    let begin = log.begin();
    let ceiling = ceiling(session);
    let quarter = begin + ceiling.saturating_sub(begin) / 4;
    page_start(page_of(quarter)).max(page_start(page_of(begin) + 1))
}
