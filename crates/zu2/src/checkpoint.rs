//! A checkpoint: the two planes written down so that a reopen does not
//! have to read the whole log to rebuild them.
//!
//! Recovery without one is a forward scan of every record the file
//! holds, and [`crate::recover`] says why that is the right default: the
//! record log is the write ahead log, so a scan is the only thing that
//! has to happen and a wrong checkpoint is worse than no checkpoint.
//! What it is not is a bounded cost. Compaction holds the file at about
//! twice the live set, so the scan is bounded by the live set rather
//! than by everything ever written, and that is still every record: a
//! database of ten million keys pays ten million hashes, ten million
//! installs and a read of every byte before it answers its first query.
//! The scan is the reason a reopen is measured in seconds where an open
//! should be measured in milliseconds.
//!
//! Concurrent Prefix Recovery (Prasaad, Chandramouli, Kossmann, SIGMOD
//! 2019) is the model. Its argument is that the state a checkpoint
//! writes down does not have to be the state at an instant, it has to be
//! a state that some prefix of the operations produces, because a
//! recovery that ends at a prefix is a recovery every client can be told
//! about in one sentence: everything up to here happened. So a
//! checkpoint here is a log address and the two planes as they stand at
//! it. Recovery reads the planes back and replays the records above the
//! address, which is the part of the log that was written after the
//! prefix was decided.
//!
//! CPR gets its prefix without stopping anybody, by moving sessions from
//! one version to the next as they arrive at their own boundaries. This
//! takes the barrier instead, and the reason is the graph plane rather
//! than the record plane. An index entry can be walked back to the
//! newest record below the boundary, so the record plane could be
//! captured live. A neighbourhood cannot: it is a sorted array with one
//! version cell, the edges that built it are not distinguishable inside
//! it, and there is nothing to walk back to. Capturing it live would
//! mean versioning every neighbour, which is the design decision 04
//! spends its length arguing against. So the gate shuts, the sessions
//! already inside an operation are waited out, and what is captured is
//! the state at an instant, which is a prefix as well. The pause is
//! measured rather than assumed: `examples/checkpointing.rs` reports it.
//!
//! The file is written under a second name and renamed into place, so a
//! crash during a capture leaves either the checkpoint before it or none
//! at all, never half of one. A checkpoint whose `begin` is not the
//! log's `begin` is refused, which is what makes compaction safe to run
//! against an old one: a pass that punches a region turns every entry
//! pointing into it into an entry pointing at zeros, and the begin it
//! moved is what says so.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::addr::Address;
use crate::db::Core;
use crate::error::{Error, Result};
use crate::graph::Direction;
use crate::index::{self, EMPTY, SLOTS};
use crate::{file, log};

/// Says the file is a zu2 checkpoint and not something that happens to
/// have the right name.
const MAGIC: u64 = 0x7a75_3263_6b70_7431;

/// The layout the reader was taught. A file stamped with anything else
/// is not read, and a reopen that meets one falls back to the scan.
const FORMAT: u32 = 1;

/// Magic, format, the two log addresses, the version counter, the shape
/// of both planes.
const HEADER: usize = 64;

/// What a capture wrote, which is what a caller measuring one wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpointed {
    /// The log address the checkpoint is a prefix of. Recovery replays
    /// from here rather than from the bottom of the log.
    pub boundary: Address,
    /// Bytes the checkpoint file takes.
    pub bytes: u64,
    /// Index entries written down.
    pub entries: u64,
    /// Nodes the graph plane had.
    pub nodes: u32,
    /// Neighbours written down, both directions, so an edge counts
    /// twice.
    pub edges: u64,
    /// How long the barrier was shut. This is the pause every session
    /// pays, and it is the number that says whether a checkpoint is
    /// something a running database can afford.
    pub pause: Duration,
}

/// Copies the two planes into a buffer, which is the only part of a
/// checkpoint that has to happen behind the barrier.
///
/// The bytes go to the device afterwards, out of [`publish`], because
/// what the barrier is for is reading the planes without them moving.
/// Once they are copied nothing else about the file is anybody's
/// business but the caller's, and leaving the write and its fsync inside
/// the pause put a device round trip into a stall every session pays:
/// on a database of 1.6 million records it was two thirds of the pause.
///
/// # Panics and preconditions
///
/// The caller holds the barrier: the gate is shut, quiescence has been
/// waited for, and the maintenance lock is held so that no compaction
/// pass and no index doubling can be in flight. [`crate::db::Db::checkpoint`]
/// is the only thing that arranges all three.
pub(crate) fn capture(core: &Core, boundary: Address) -> Result<(Vec<u8>, Checkpointed)> {
    let index = &core.index;
    if index.resizing() {
        return Err(Error::Checkpoint {
            why: "the index was doubling, which a checkpoint cannot describe",
        });
    }
    let table = index.live();
    let graph = &core.graph;
    let nodes = graph.nodes();

    let mut buf = Vec::with_capacity(HEADER + table.len() * 8);
    buf.resize(HEADER, 0);

    // The index, bucket by bucket, as a mask of the slots in use and
    // then those slots. A table under half full is the table this engine
    // is meant to run on, so writing the empty slots would be writing
    // mostly zeros: at the default load factor the mask form is about
    // half the size, and it is the whole difference between a checkpoint
    // that costs less than the log it saves reading and one that does
    // not.
    let mut entries = 0u64;
    for i in 0..table.len() {
        let bucket = table.at(i);
        let mut mask = 0u8;
        let mut held = [EMPTY; SLOTS];
        for (slot, held) in held.iter_mut().enumerate() {
            let entry = bucket.slots[slot].load(Ordering::Acquire);
            if entry == EMPTY {
                continue;
            }
            if index::is_tentative(entry) {
                // A claim is made and resolved inside one operation, so
                // the barrier rules this out. Meeting one anyway means
                // the barrier did not hold, and a checkpoint written
                // over a broken barrier is a database that comes back
                // missing a key it acknowledged. Refusing costs a
                // checkpoint; writing it costs the key.
                return Err(Error::Checkpoint {
                    why: "an index entry was still a claim, so the barrier did not hold",
                });
            }
            mask |= 1 << slot;
            *held = entry;
        }
        buf.push(mask);
        for entry in held.iter().filter(|entry| **entry != EMPTY) {
            buf.extend_from_slice(&entry.to_le_bytes());
            entries += 1;
        }
    }

    // The graph, as degrees then neighbours, one run per direction.
    // Degrees rather than offsets because the two are the same size and
    // a degree is what the restore needs a node at a time, so nothing
    // has to hold a prefix sum of the whole graph in memory to read it
    // back.
    let mut edges = 0u64;
    let mut neighbours = Vec::new();
    for direction in [Direction::Out, Direction::In] {
        for node in 0..nodes {
            // SAFETY: the caller's barrier. Nothing is writing a
            // neighbourhood and nothing is retiring a block.
            let slice = unsafe { graph.quiesced(direction, node) };
            buf.extend_from_slice(&(slice.len() as u32).to_le_bytes());
            neighbours.extend_from_slice(slice);
            edges += slice.len() as u64;
        }
    }
    for neighbour in &neighbours {
        buf.extend_from_slice(&neighbour.to_le_bytes());
    }

    let header = &mut buf[..HEADER];
    header[0..8].copy_from_slice(&MAGIC.to_le_bytes());
    header[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    header[12..16].copy_from_slice(&u32::from(core.log.format()).to_le_bytes());
    header[16..24].copy_from_slice(&core.log.begin().to_le_bytes());
    header[24..32].copy_from_slice(&boundary.to_le_bytes());
    header[32..40].copy_from_slice(&core.version().to_le_bytes());
    header[40..48].copy_from_slice(&(table.len() as u64).to_le_bytes());
    header[48..56].copy_from_slice(&(index.keys() as u64).to_le_bytes());
    header[56..60].copy_from_slice(&nodes.to_le_bytes());

    let crc = crc32c::crc32c(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    let taken = Checkpointed {
        boundary,
        bytes: buf.len() as u64,
        entries,
        nodes,
        edges,
        // Filled in by the caller, which is the only thing that knows
        // when the gate shut and when it lifted.
        pause: Duration::ZERO,
    };
    Ok((buf, taken))
}

/// Puts a captured checkpoint on the device and names it, which is what
/// makes the next open read it.
///
/// The bytes go down under a second name and are renamed into place, so
/// a crash anywhere in here leaves the checkpoint from before it, or
/// none, and never half of one. Nothing in this holds the barrier.
pub(crate) fn publish(core: &Core, buf: &[u8]) -> Result<()> {
    let (path, writing) = core.log.checkpoint_path();
    let handle = file::create_or_replace(&writing)?;
    file::write_all_at(&handle, buf, 0)?;
    file::sync(&handle)?;
    drop(handle);
    std::fs::rename(&writing, &path)?;
    Ok(())
}

/// What a checkpoint said, once it has been put back into the two
/// planes.
pub(crate) struct Restored {
    /// Where the replay picks up.
    pub boundary: Address,
    /// The version counter the capture ran at, which the replay raises
    /// further as it reads records above the boundary.
    pub version: u64,
}

/// Reads the checkpoint beside the log and installs it, or answers
/// `None` when there is none to read, when it does not check out, or
/// when it describes a file this one is no longer.
///
/// Nothing here fails the open. Every reason to turn a checkpoint down
/// is a reason to read the log instead, which is the thing the
/// checkpoint was avoiding rather than the thing it was replacing, so a
/// refusal costs time and never an answer.
pub(crate) fn restore(core: &Core, len: u64) -> Result<Option<Restored>> {
    let (path, _) = core.log.checkpoint_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(None);
    };
    let Some(read) = parse(&bytes) else {
        return Ok(None);
    };

    // A checkpoint names log addresses, so it is only about this log if
    // the log still starts where it did and still holds what it held.
    // A compaction pass since the capture moved `begin` and punched the
    // region below it, which turns an entry pointing down there into an
    // entry pointing at a hole, and a file shorter than the boundary is
    // one whose tail did not survive, which the capture's own sync makes
    // impossible unless the file has been replaced under us.
    if read.begin != core.log.begin() || read.boundary > len || read.format != core.log.format() {
        return Ok(None);
    }
    // Nothing above this point has touched either plane, which is the
    // reason the file is read through twice. Every refusal is a fall
    // back to the scan, and a scan that starts on top of half a restored
    // table is a scan that installs behind entries pointing at records
    // it has not read yet.
    if !core.index.adopt(read.buckets) {
        return Ok(None);
    }
    let table = core.index.live();
    if table.len() != read.buckets {
        return Ok(None);
    }

    let mut at = HEADER;
    for i in 0..read.buckets {
        let mask = bytes[at];
        at += 1;
        let bucket = table.at(i);
        for slot in 0..SLOTS {
            if mask & (1 << slot) == 0 {
                continue;
            }
            bucket.slots[slot].store(word(&bytes, at), Ordering::Relaxed);
            at += 8;
        }
    }

    let nodes = read.nodes as usize;
    let degrees_at = at;
    let mut at = degrees_at + nodes * 2 * 4;
    core.graph.adopt_nodes(read.nodes);
    let mut list = Vec::new();
    for i in 0..nodes * 2 {
        let degree = half(&bytes, degrees_at + i * 4) as usize;
        if degree == 0 {
            continue;
        }
        list.clear();
        for j in 0..degree {
            list.push(half(&bytes, at + j * 4));
        }
        at += degree * 4;
        let direction = if i < nodes {
            Direction::Out
        } else {
            Direction::In
        };
        // A graph opened with less room than it was written with is the
        // same mistake `replay_edge` reports, and it deserves the same
        // answer rather than a silent half of a graph.
        core.graph
            .adopt(direction, (i % nodes) as u32, &list)
            .map_err(|error| match error {
                Error::NodeOutOfRange { node, max } => Error::GraphTooSmall {
                    needs: node as usize + 1,
                    max,
                },
                other => other,
            })?;
    }

    core.index.adopt_keys(read.keys);
    Ok(Some(Restored {
        boundary: read.boundary,
        version: read.version,
    }))
}

/// The header of a checkpoint whose body has been measured and found to
/// be exactly as long as the header says.
struct Read {
    format: u8,
    begin: Address,
    boundary: Address,
    version: u64,
    buckets: usize,
    keys: usize,
    nodes: u32,
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn half(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// Checks a checkpoint over without installing any of it: the magic, the
/// format, the checksum, and then that the sections add up to exactly
/// the bytes there are.
///
/// The last of those is what lets the install index the buffer without
/// bounds tests of its own. The checksum says the bytes are the bytes
/// that were written, and this says those bytes describe a whole
/// checkpoint, so an install that walks the same sections in the same
/// order cannot walk off the end.
fn parse(bytes: &[u8]) -> Option<Read> {
    if bytes.len() < HEADER + 4 {
        return None;
    }
    if word(bytes, 0) != MAGIC || half(bytes, 8) != FORMAT {
        return None;
    }
    let end = bytes.len() - 4;
    if half(bytes, end) != crc32c::crc32c(&bytes[..end]) {
        return None;
    }
    let read = Read {
        format: half(bytes, 12) as u8,
        begin: word(bytes, 16),
        boundary: word(bytes, 24),
        version: word(bytes, 32),
        buckets: word(bytes, 40) as usize,
        keys: word(bytes, 48) as usize,
        nodes: half(bytes, 56),
    };
    let mut at = HEADER;
    for _ in 0..read.buckets {
        if at >= end {
            return None;
        }
        at = at.checked_add(1 + bytes[at].count_ones() as usize * 8)?;
    }
    let degrees_at = at;
    at = at.checked_add((read.nodes as usize).checked_mul(2 * 4)?)?;
    if at > end {
        return None;
    }
    for i in 0..read.nodes as usize * 2 {
        at = at.checked_add(half(bytes, degrees_at + i * 4) as usize * 4)?;
        if at > end {
            return None;
        }
    }
    if at != end {
        return None;
    }
    Some(read)
}

/// Takes the checkpoint away.
///
/// Called when the log's shape has moved out from under it, which today
/// means a compaction pass advanced `begin`. The reader would refuse it
/// anyway, so this is tidiness rather than safety: a file left lying
/// beside a database that will never read it again is a file somebody
/// eventually has to explain.
pub(crate) fn discard(core: &Core) {
    let (path, _) = core.log.checkpoint_path();
    let _ = std::fs::remove_file(path);
}

/// Whether a log format stamp is one a checkpoint may be written for.
///
/// Only the current one. A file written before pad records existed is
/// read under a rule that treats a zero header as padding rather than as
/// damage, and giving it a checkpoint would mean carrying that rule into
/// a second file. There are very few such files and none of them are
/// large. See [`crate::recover`].
pub(crate) fn writable(format: u8) -> bool {
    format != log::FORMAT_PADLESS
}
