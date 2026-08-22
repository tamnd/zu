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
//! Four things make it safe to then declare the whole region gone.
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
//! A copy carries the version of the record it copies. The alternative,
//! a fresh version, is what made the log disagree with memory: the copy
//! is written before the compare and swap that would publish it, so a
//! copy that loses the race to a concurrent update is left on the log
//! above the update with nobody pointing at it, and a replay that took
//! the highest address as the newest version installed the loser. With
//! the original version on it the replay can order the two records the
//! way the workload did (#436).
//!
//! The region is scanned in address order, so a record that is only
//! reachable through another record in the same region has already been
//! copied by the time its chain link is dropped.
//!
//! Edges are not keyed, so the index cannot answer for them. The
//! adjacency itself does: an add is live when the edge is in the
//! adjacency now. A remove is never live, and the reason is the same
//! prefix property everything else here rests on.
//!
//! A region is always a prefix. The scan starts at `begin`, `reclaim_to`
//! writes the new `begin` durably before it releases a page, and a
//! replay starts its scan there. So a record above the region is a
//! record a replay applies *after* everything in the region, and a
//! remove in the region followed by an add above it replays as an edge
//! that is present whether the remove is kept or not.
//!
//! Work the pair's history through. If the adjacency has the edge, the
//! remove is stale and goes. If it does not, then the last edge record
//! for that pair on the whole log is a remove, because an add as the
//! last record would have left the edge present. That last remove is
//! either this one or one above the region. One above the region does
//! the job on its own. This one being the last means the pair has no
//! add above the region at all and the rest of its history is inside the
//! region about to be punched, so dropping it leaves the pair with no
//! record anywhere, which replays as absent, which is what the adjacency
//! says. Either way the remove is redundant.
//!
//! Keeping them instead was #452, and it was not the cheap safety margin
//! it reads as. A removed edge stays removed, so its remove record
//! matched on every pass and was copied forward on every pass: 48 bytes
//! per deleted edge that never came back and that every future pass paid
//! to copy again.
//!
//! That question can go stale, which was #437. An edge writer puts its
//! record on the log before it touches the adjacency, because an edge in
//! memory that is not on the log would be a lost write, so a pass
//! reading between the two halves is told an edge is present, appends an
//! add copy above a remove record that is already down, and leaves a
//! replay that brings the edge back. The version rule that fixes it for
//! keyed records does not reach here, because a replay has no per edge
//! version to compare against and building one would need a map the size
//! of the edge set on a path whose point is that it carries no state.
//! What fixes it instead is closing the window: the writer holds
//! [`crate::graph::Graph::order_edges`] across its append and its apply,
//! the pass holds the same one across its question and its copy, and
//! neither can see the other half done.

use crate::addr::{Address, PAGE_SIZE, page_of, page_start};
use crate::db::{Carried, Placement, Session};
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
    /// Bytes it moved to the cold tier, which is the other half of that
    /// amplification and the half that is only paid once. See
    /// [`crate::cold`].
    pub migrated: u64,
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

    // A record still live in the oldest region went a whole lap of the
    // log without anybody touching it, which is the definition of cold
    // this engine uses, so the survivors of a pass go to the tier rather
    // than to the tail. See [`crate::cold`].
    // A file written before pad records existed is read under a rule
    // that treats zeros as padding, and the tier's scan is not written
    // for that rule. Such a file keeps the engine it had.
    let cold =
        session.core_ref().cold.is_some() && crate::cold::usable(session.core_ref().log.format());

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
            let (size, kind, tombstone, version) = unsafe {
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
                (size, r.kind(), r.tombstone(), r.version())
            };
            done.records += 1;
            done.scanned += size as u64;
            let kept = match kind {
                record::KIND_EDGE => keep_edge(session, &value).map(|kept| {
                    if kept {
                        Placement::Hot
                    } else {
                        Placement::Dead
                    }
                }),
                // A marker's whole job is to say whether the records
                // between it and its partner were committed, and a pass
                // can only reach a region whose transactions have all
                // finished one way or the other, so by the time one is
                // read it has nothing left to say. Dropping it is also
                // what keeps it out of `copy_forward`, which would look
                // it up under the empty key.
                record::KIND_BEGIN | record::KIND_END => Ok(Placement::Dead),
                _ => session.copy_forward(
                    &Carried {
                        key: &key,
                        value: &value,
                        tombstone,
                        // A copy is not provisional. Its transaction
                        // committed long before a pass could reach it,
                        // and a copy written as one would sit above the
                        // end marker that would have released it and be
                        // dropped by the next replay. See
                        // [`crate::record::KIND_TXN`].
                        kind: if kind == record::KIND_TXN {
                            record::KIND_VALUE
                        } else {
                            kind
                        },
                        version,
                    },
                    address,
                    cold,
                ),
            };
            match kept {
                Ok(Placement::Hot) => {
                    done.live += 1;
                    done.copied += size as u64;
                }
                Ok(Placement::Cold) => {
                    done.live += 1;
                    done.migrated += size as u64;
                }
                Ok(Placement::Dead) => {}
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
    // block of the region is released. The tier goes first because a
    // record that moved there is no longer anywhere else.
    if let Some(tier) = &session.core_ref().cold {
        tier.sync()?;
    }
    let tail = session.core_ref().log.tail();
    session
        .core_ref()
        .log
        .make_durable(tail, Durability::Durable)?;
    done.reclaimed = session.core_ref().log.reclaim_to(upto)?;
    Ok(done)
}

/// Compacts `[cold.begin(), upto)` of the cold tier, which must be a
/// page boundary.
///
/// The same lookup-based rule as the log's pass, and it has to be: the
/// index is what says whether a record is live and it does not care
/// which tier the record is in. What is different is where the
/// survivors go, which is straight back to the cold tail. A record that
/// has already settled once and is still settled is not evidence that it
/// is about to be written to.
///
/// Nothing in this tier is an edge record, because edges are not keyed
/// and only a keyed record is ever moved here, so there is no adjacency
/// question to ask.
pub fn compact_cold(session: &mut Session<'_>, upto: Address) -> Result<Compacted> {
    let mut done = Compacted::default();
    let Some(tier) = &session.core_ref().cold else {
        return Ok(done);
    };
    let begin = tier.begin();
    if upto <= begin {
        return Ok(done);
    }
    tier.sync()?;

    // Read out first and place after, one record at a time, because the
    // walk holds a page buffer that the placing would otherwise be
    // borrowing the session through.
    let mut batch = Vec::new();
    let stopped = tier.walk(begin, upto, |header, address| {
        // SAFETY: the walk hands over a whole record inside its own page
        // buffer, and everything is copied out before it moves on.
        unsafe {
            batch.push((
                address,
                header.key().to_vec(),
                header.value_unchecked().to_vec(),
                header.tombstone(),
                header.kind(),
                header.version(),
            ));
        }
        Ok(())
    })?;
    for (address, key, value, tombstone, kind, version) in batch {
        let size = record::size_of(key.len(), value.len()) as u64;
        done.records += 1;
        done.scanned += size;
        let carried = Carried {
            key: &key,
            value: &value,
            tombstone,
            kind,
            version,
        };
        match session.copy_forward(&carried, address, true)? {
            Placement::Cold => {
                done.live += 1;
                done.migrated += size;
            }
            Placement::Hot => {
                done.live += 1;
                done.copied += size;
            }
            Placement::Dead => {}
        }
    }

    let tier = session.core_ref().cold.as_ref().expect("zu2 cold tier");
    tier.sync()?;
    done.reclaimed = tier.reclaim_to(stopped.min(upto), session.core_ref().epochs())?;
    Ok(done)
}

/// How far a cold pass should reach. A quarter of the span at a time,
/// for the same reason the log's slice is a quarter: a whole pass would
/// hold the maintenance lock for as long as the tier is big.
pub fn cold_slice(session: &Session<'_>) -> Address {
    let Some(tier) = &session.core_ref().cold else {
        return 0;
    };
    let begin = tier.begin();
    let quarter = begin + tier.span() / 4;
    let page = page_start(page_of(quarter)).max(page_start(page_of(begin) + 1));
    page.min(page_start(page_of(tier.tail())))
}

/// Decides an edge record and re-appends it when it still counts.
///
/// An add is live when the adjacency has the edge, because a replay that
/// skipped it would come up short. A remove is never live: the module
/// comment has the argument, and the short of it is that a region is a
/// prefix, so anything a remove in it could still be cancelling is
/// either gone with the region or answered by a record above it (#452).
///
/// The question and the copy are one step, under the same order an edge
/// writer takes, because an answer read between a writer's append and
/// its apply is an answer that is already wrong. #437 has what that
/// costs when the two are apart, and it is also what makes dropping a
/// remove safe rather than merely cheaper: a copy goes to the tail,
/// above everything, so the pass and a concurrent writer must not be
/// able to interleave. Under the order they cannot, and taking records
/// out cannot invent an interleaving that putting them in did not have.
fn keep_edge(session: &mut Session<'_>, payload: &[u8]) -> Result<bool> {
    let Some((add, src, dst)) = graph::decode_edge(payload) else {
        return Ok(false);
    };
    if !add {
        return Ok(false);
    }
    let _order = session.core.graph().order_edges(src);
    if !session.neighbours(Direction::Out, src, |n| n.binary_search(&dst).is_ok()) {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::addr::PAGE_SIZE;
    use crate::db::{Db, Options};
    use crate::graph::{Direction, encode_edge};
    use crate::log::Durability;
    use crate::record::KIND_EDGE;

    /// A pass cannot be told an edge is present by an adjacency that a
    /// writer has already superseded on the log (#437).
    ///
    /// The window this is about is one instruction wide in a real run,
    /// so the test builds it rather than racing for it. It puts a remove
    /// record on the log by hand, leaves the adjacency alone, and only
    /// then lets a pass look, which is exactly the state an edge writer
    /// is in between its append and its apply. Without the order the two
    /// of them share, the pass reads the adjacency, is told the edge is
    /// there, copies the add above the remove, and the reopened graph
    /// has an edge the writers deleted.
    ///
    /// The sleep is not what makes it correct, only what makes it
    /// discriminating: with the order in place the answer is right
    /// whether the pass arrives during the window or after it, and
    /// without it the pass has to arrive during the window to be wrong.
    #[test]
    fn a_pass_cannot_copy_an_edge_a_writer_has_already_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("e.zu2");
        let options = Options {
            durability: Durability::Async,
            index_buckets: 1 << 14,
            max_pages: 64,
            max_nodes: 1 << 10,
            compact_below: 0,
            // One page rather than four, so the edge record below is out
            // of the mutable window and a pass is allowed to reach it.
            mutable_pages: 1,
            ..Options::default()
        };
        {
            let db = Db::create(&path, options).expect("create");
            let mut s = db.session();
            for i in 0..4u32 {
                s.add_node(format!("n{i}").as_bytes()).expect("node");
            }
            // The record the pass will have to make its mind up about,
            // in the first page and nowhere near the tail by the end.
            s.add_edge(0, 1).expect("edge");
            // Five pages of ordinary records above it, so the first page
            // is both flushed and below the read-only boundary.
            for i in 0..20_000u32 {
                s.upsert(format!("k{i:09}").as_bytes(), &vec![b'x'; 1000])
                    .expect("fill");
            }
            s.set_durability(Durability::Durable);
            s.upsert(b"flush", b"x").expect("flush");
            s.set_durability(Durability::Async);

            let core = db.core().clone();
            let mut writer = db.session();
            // Half an edge writer: the remove is on the log and the
            // adjacency has not heard about it yet.
            let order = core.graph().order_edges(0);
            writer.slot.protect();
            writer
                .append_untracked(KIND_EDGE, &encode_edge(false, 0, 1))
                .expect("remove record");
            writer.slot.unprotect();

            std::thread::scope(|scope| {
                let pass = scope.spawn(|| db.compact().expect("compact"));
                std::thread::sleep(Duration::from_millis(200));
                // The other half of the writer, and then the pass is
                // free to look.
                core.graph()
                    .apply(core.epochs(), false, 0, 1)
                    .expect("apply");
                drop(order);
                pass.join().expect("pass");
            });
            assert!(
                db.core().log.begin() >= PAGE_SIZE as u64,
                "the pass never reached the page the edge record is in"
            );
            db.sync().expect("sync");
        }

        let db = Db::open(&path, options).expect("reopen");
        let mut s = db.session();
        let out = s.neighbours(Direction::Out, 0, |n| n.to_vec());
        assert!(
            !out.contains(&1),
            "the removed edge came back, so the pass copied an add above the remove"
        );
    }
}
