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
//! that address is the durable prefix. A torn write inside a page fails
//! its checksum and ends the scan there, which is the behaviour that
//! matters: everything before it was acknowledged and everything after
//! it was not.
//!
//! Padding at the end of a page used to be zeros and nothing else, so
//! the scan read a zero header as padding and moved to the next page.
//! That is also what a block the device lost reads as, which made the
//! two indistinguishable: a hole cost the rest of its page and then the
//! scan carried on above it, and what came back was a prefix with a
//! suffix stapled on rather than a prefix (#472). The allocator now
//! leaves a pad record where it skipped, so padding says so and carries
//! a checksum, and zeros mean either the end of the log or damage. The
//! scan stops on both, which is right for both: at the end there is
//! nothing above to lose, and at a hole what is above is unreachable
//! rather than lost.
//!
//! A file written before pad records existed says so in its marker word
//! and gets the old reading, since it holds nothing the new conclusion
//! could be drawn from.
//!
//! A scan is what runs when there is nothing else to go on, and it is
//! still the whole of the durability argument: the log is the write
//! ahead log and reading it is always enough. What it is not is bounded
//! by anything but the size of the live set. A checkpoint beside the
//! file bounds it by the writes since the checkpoint was taken instead,
//! and [`from_checkpoint`] is the path that uses one. Everything below
//! it is what happens when there is not one, or when it does not check
//! out, which are the same thing: a wrong checkpoint is worse than no
//! checkpoint, so every doubt about one ends here. See
//! [`crate::checkpoint`].

use std::collections::BTreeSet;

use crate::addr::{Address, FIRST, NULL, PAGE_SIZE, page_of, page_start};
use crate::cold;
use crate::db::{Core, restore_pages, restore_pages_from};
use crate::error::{Error, Result};
use crate::graph;
use crate::index::{self, EMPTY, Index, SLOTS};
use crate::log;
use crate::record::{self, RecordRef};

/// Pages the scan changed a record in, which have to go back to the
/// file before anything can evict them. See [`install`].
type Rewritten = BTreeSet<usize>;

/// Every link the scan moved, as the record and where it now points.
///
/// Kept as well as the pages, because the pages are what has to be
/// written and this is what has to be written down first. See
/// [`journal`].
type Repairs = Vec<(Address, Address)>;

/// Rebuilds the index from the file and leaves the log ready to append.
///
/// `salvage` is [`crate::Options::salvage`]: what to do about a file
/// that has records above where the scan stopped. See [`hole_above`].
pub fn replay(core: &Core, salvage: bool) -> Result<()> {
    if from_checkpoint(core, salvage)? {
        return Ok(());
    }
    // A checkpoint that was refused is a checkpoint nobody will ever
    // accept, and leaving it beside the file is not merely untidy. The
    // scan below repairs the links in the log to fit the table it is
    // filling, which is a different table from the one the checkpoint
    // holds, since a table of the same size would have been adopted. So
    // the records the checkpoint's entries point at end up chained for
    // somebody else's table, and an entry that walks its chain walks
    // into keys that are no longer under it. A pinned reopen at another
    // size followed by an ordinary one lost ten keys out of twenty
    // thousand exactly that way.
    crate::checkpoint::discard(core);
    let len = restore_pages(core)?;
    // Before anything reads a record, because a journal that is still
    // there is a reopen that died part way through writing its repairs
    // back and the file may hold a record whose checksum does not hold.
    // The scan would stop at that record and call it the end of the
    // durable prefix.
    journal::apply(core, len)?;
    // A compacted file starts where its begin marker says, because the
    // pages before that are a hole. Starting at FIRST would still work,
    // since a hole reads as zeros and a page of zeros is skipped the
    // same way page padding is, but it would restore every punched page
    // into memory to learn that.
    let from = core.log.begin().max(FIRST);
    // What the file holds, before anything is built out of it. Records
    // are an upper bound on keys, and a table sized against them is a
    // table the scan can put nearly every record straight into. See
    // [`install`] for what a table of the wrong size costs, and
    // [`crate::index::Index::presize`] for what this leaves alone.
    let mut records = 0usize;
    let stopped = walk(core, from, len, |_, _| {
        records += 1;
        Ok(())
    })?;
    // Before the second walk, because the second walk is the expensive
    // one and a file that is not going to open should not pay for it.
    if let Some(above) = hole_above(core, stopped, len) {
        if !salvage {
            return Err(Error::LogHole { at: stopped, above });
        }
        core.recovered
            .discarded
            .store(len - stopped, std::sync::atomic::Ordering::Relaxed);
    }
    // The tier's records are keys too, and a table sized without them is
    // a table the scan has to displace into. Counting means reading the
    // cold file twice on this path, which is the same bargain the log
    // makes above and for the same reason: a table of the wrong size
    // costs every lookup for the life of the database.
    let cold_records = cold_count(core)?;
    core.index
        .presize((records + cold_records as usize).div_ceil(4));

    let mut rewritten = Rewritten::new();
    let mut repairs = Repairs::new();
    let mut scratch = Vec::new();
    let mut version = 0u64;
    let end = walk(core, from, len, |header, address| {
        if header.kind() == record::KIND_EDGE {
            // Edge records are not keyed, so nothing goes in the index.
            // A remove that replays after the add it cancels lands the
            // same way it did the first time, because the log is in
            // commit order.
            // SAFETY: the walk bounded the lengths against the page and
            // nothing else is running.
            graph::replay_edge(core, unsafe { header.value_unchecked() })?;
        } else {
            if header.kind() == record::KIND_VERTEX {
                // A node with no edges yet would otherwise leave no
                // trace, and the next allocation would hand its id out
                // again.
                // SAFETY: as above.
                graph::replay_node(core, unsafe { header.value_unchecked() });
            }
            install(
                core,
                header,
                address,
                &mut rewritten,
                &mut repairs,
                &mut scratch,
            );
        }
        version = version.max(header.version());
        Ok(())
    })?;
    core.log.resume_at(end);
    core.set_version(version);
    // After the log and not before it, so that a key with a record in
    // both tiers is decided by the version rule with both records in
    // hand, and so that rehoming has a log to append to. See [`cold`].
    let cold_read = cold_scan(core, None, &mut rewritten, &mut repairs, &mut scratch)?;
    core.recovered.records.store(
        records as u64 + cold_read,
        std::sync::atomic::Ordering::Relaxed,
    );
    core.recovered
        .pages
        .store(rewritten.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if rewritten.is_empty() {
        return Ok(());
    }
    // The journal goes down and is committed before a single page moves,
    // so that whatever a crash leaves in the file can be finished from
    // it. Then the pages, then a sync, then the journal comes off.
    journal::write(core, &repairs)?;
    for page in rewritten {
        let bytes = (len - page_start(page)).min(PAGE_SIZE as u64) as usize;
        core.log.rewrite_page(page, bytes)?;
    }
    core.log.sync_file()?;
    journal::clear(core);
    Ok(())
}

/// Recovery from a checkpoint: install both planes as they were at the
/// boundary, then walk the records above it. Answers false when there is
/// no checkpoint to read or it is not one this file can use, and then
/// the scan runs as it always has.
///
/// The scan above the boundary is the ordinary one, doing the ordinary
/// things: an install there can still find that the record's chain
/// pointer belongs to a table of another size and repair it. What it
/// cannot do is repair a link in a record below the boundary, and it
/// never wants to. The links down there were built under the table the
/// checkpoint just restored, which is the table they were written for.
/// That is the second thing a checkpoint is worth, after the reading:
/// the reopen that repaired up to a third of the records in a compacted
/// file (#463) has nothing left to repair.
///
/// A relink journal beside the file turns the checkpoint down. It means
/// the run before this one died part way through writing repairs back,
/// so the file may hold a record whose checksum does not hold, and the
/// repairs it names are anywhere in the log rather than above the
/// boundary. Applying it means having those pages in memory, which is
/// the thing this path exists not to do. It is also the rarest case
/// there is: a crash inside one page write of one reopen.
fn from_checkpoint(core: &Core, salvage: bool) -> Result<bool> {
    if journal::present(core) {
        return Ok(false);
    }
    let len = core.log.file_len()?;
    if len <= FIRST {
        return Ok(false);
    }
    let Some(restored) = crate::checkpoint::restore(core, len)? else {
        return Ok(false);
    };
    restore_pages_from(core, restored.boundary)?;
    // What was not restored, for the thread that reads it back while the
    // database serves. See [`crate::db::warm`].
    core.warm_upto
        .store(restored.boundary, std::sync::atomic::Ordering::Release);
    let mut version = restored.version;
    let mut rewritten = Rewritten::new();
    let mut repairs = Repairs::new();
    let mut scratch = Vec::new();
    let mut records = 0u64;
    let end = walk(core, restored.boundary, len, |header, address| {
        records += 1;
        if header.kind() == record::KIND_EDGE {
            // SAFETY: the walk bounded the lengths against the page and
            // nothing else is running.
            graph::replay_edge(core, unsafe { header.value_unchecked() })?;
        } else {
            if header.kind() == record::KIND_VERTEX {
                // SAFETY: as above.
                graph::replay_node(core, unsafe { header.value_unchecked() });
            }
            install(
                core,
                header,
                address,
                &mut rewritten,
                &mut repairs,
                &mut scratch,
            );
        }
        version = version.max(header.version());
        Ok(())
    })?;
    // The same test the full scan makes, over the same records: a stop
    // with live records above it is a hole rather than a torn tail, and
    // what is above it is unreachable. There is nothing to check below
    // the boundary, because the checkpoint is what says those records
    // were read.
    if let Some(above) = hole_above(core, end, len) {
        if !salvage {
            return Err(Error::LogHole { at: end, above });
        }
        core.recovered
            .discarded
            .store(len - end, std::sync::atomic::Ordering::Relaxed);
    }
    core.log.resume_at(end);
    core.set_version(version);
    // Only what the tier grew by since the checkpoint was taken. The
    // planes the checkpoint restored already name every cold record
    // that existed at the boundary.
    let cold_read = cold_scan(
        core,
        Some(restored.cold_tail),
        &mut rewritten,
        &mut repairs,
        &mut scratch,
    )?;
    core.recovered
        .records
        .store(records + cold_read, std::sync::atomic::Ordering::Relaxed);
    core.recovered
        .pages
        .store(rewritten.len() as u64, std::sync::atomic::Ordering::Relaxed);
    if rewritten.is_empty() {
        return Ok(true);
    }
    journal::write(core, &repairs)?;
    for page in rewritten {
        let bytes = (len - page_start(page)).min(PAGE_SIZE as u64) as usize;
        core.log.rewrite_page(page, bytes)?;
    }
    core.log.sync_file()?;
    journal::clear(core);
    Ok(true)
}

/// Reads the cold tier back into the index.
///
/// `from` is where to start: `None` for a full scan of the file, and the
/// checkpoint's cold tail when there is one, since everything below that
/// is already in the planes the checkpoint restored.
///
/// A cold record has no `previous` and is the whole of what its entry
/// answers for, so this is [`install`] without the relinking, and it can
/// only put a record under an entry whose foreign bit is clear (#466).
/// Whether it can is not something the tier decides: the table this
/// reopen is filling may be a different size from the one the record was
/// migrated under, and then the bucket the key lands in can be full, or
/// can already be somebody else's chain. There is nowhere down here to
/// put such a record, so it goes back to the hot log, which is the one
/// place a record with a chain pointer belongs. Rare by construction and
/// bounded by the bucket, but not impossible, and losing the key is not
/// an option.
fn cold_scan(
    core: &Core,
    from: Option<Address>,
    rewritten: &mut Rewritten,
    repairs: &mut Repairs,
    scratch: &mut Vec<u64>,
) -> Result<u64> {
    let Some(tier) = &core.cold else {
        return Ok(0);
    };
    let end = tier.end()?;
    let from = from.unwrap_or_else(|| tier.begin()).max(tier.begin());
    if from >= end {
        // Nothing to read, but the tail still has to be where the run
        // before this one left it. A tier reopened at its begin reports
        // an empty span and calls every address it holds outside itself.
        tier.resume_at(from.min(end.max(tier.begin())));
        return Ok(0);
    }
    // Before the walk, because a chain the walk follows can point at a
    // record the walk has already installed, and the tier will not read
    // an address above its own tail.
    tier.resume_at(end);

    let mut rehome = Vec::new();
    let mut records = 0u64;
    let mut version = core.version();
    let stopped = tier.walk(from, end, |header, address| {
        records += 1;
        version = version.max(header.version());
        if !install_cold(core, header, address, scratch) {
            // SAFETY: the walk hands over a whole record in its page
            // buffer, and it is copied out before the walk moves on.
            unsafe {
                rehome.push((
                    header.key().to_vec(),
                    header.value_unchecked().to_vec(),
                    header.tombstone(),
                    header.kind(),
                    header.version(),
                ));
            }
        }
        Ok(())
    })?;
    tier.resume_at(stopped);
    core.set_version(version);

    if rehome.is_empty() {
        return Ok(records);
    }
    // Recovery is alone with the database, so every slot is free and a
    // reserved one is the right one: this is maintenance and not a host
    // session. See [`crate::epoch`].
    let slot = crate::epoch::Slotted::reserved(&core.log.epochs).expect("zu2 recovery slot");
    for (key, value, tombstone, kind, version) in rehome {
        let at = core
            .log
            .append(&slot, NULL, version, &key, &value, tombstone, kind)?;
        let base = core.log.resident(at);
        debug_assert!(!base.is_null(), "a record just appended is resident");
        // SAFETY: the address came back from the append that wrote the
        // record, and the page it went in is resident.
        let header = unsafe { RecordRef::new(base) };
        install(core, header, at, rewritten, repairs, scratch);
        core.recovered
            .rehomed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(records)
}

/// Records the cold tier holds, read before anything is built out of
/// them so that the table can be sized against the whole key set.
fn cold_count(core: &Core) -> Result<u64> {
    let Some(tier) = &core.cold else {
        return Ok(0);
    };
    let end = tier.end()?;
    let begin = tier.begin();
    if begin >= end {
        return Ok(0);
    }
    let mut records = 0u64;
    tier.walk(begin, end, |_, _| {
        records += 1;
        Ok(())
    })?;
    Ok(records)
}

/// One cold record into the index. Answers false when there is no entry
/// this record can go under, and then the caller moves it to the hot log.
fn install_cold(
    core: &Core,
    header: RecordRef<'_>,
    address: Address,
    scratch: &mut Vec<u64>,
) -> bool {
    let key = header.key();
    let hash = index::hash(key);
    let tag = Index::tag(hash);
    let bucket = core.index.live().bucket(hash);
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
        let Some(installed) = chain_version(core, entry, key, scratch) else {
            continue;
        };
        if header.version() < installed {
            // The hot log holds something newer for this key, which is
            // the ordinary case for a key that was written again after
            // it settled. The cold record is garbage waiting for a pass.
            return true;
        }
        if index::is_foreign(entry) {
            // Putting a record with no chain pointer at the head of a
            // chain would cut off every key behind it.
            return false;
        }
        bucket.slots[i].store(
            index::entry(tag, address, false),
            std::sync::atomic::Ordering::Relaxed,
        );
        return true;
    }
    core.index.note_key();
    match empty {
        Some(i) => {
            bucket.slots[i].store(
                index::entry(tag, address, false),
                std::sync::atomic::Ordering::Relaxed,
            );
            true
        }
        // A full bucket means taking an entry over, which means carrying
        // what it held, which is the one thing a cold record cannot do.
        None => false,
    }
}

/// Whether every record in a page of the log parses and holds its
/// checksum, which is what the warming thread asks before it makes a
/// page resident. See [`crate::log::Log::warm_page`].
///
/// This is the scan's own reading of a page, minus the installing: a
/// record start at the top of the page, records end to end after it,
/// and either a pad record or a gap shorter than a header where the
/// allocator moved on. Anything else is either damage or a page that
/// was never fully written, and both are answered the same way, by
/// leaving the page where it is so that whoever reads a record out of it
/// reads it off the device and finds out.
///
/// A zeroed header is where this is stricter than the scan. The scan
/// reads one as the end of the page and moves to the next, which it can
/// afford because it goes on to look for a record above where it
/// stopped. There is nothing above a page here, so zeros that are not a
/// pad record are treated as a page not worth trusting, and a padless
/// file, which has no pad records to tell padding from a hole, warms
/// only its full pages.
pub(crate) fn page_intact(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    while at + record::HEADER <= bytes.len() {
        // SAFETY: a whole header fits in what is left of the buffer, and
        // the lengths are bounded against it before anything past the
        // header is touched.
        let step = unsafe {
            let header = RecordRef::new(bytes.as_ptr().add(at));
            let key_len = header.key_len();
            let value_len = header.value_len();
            let size = record::size_of(key_len, value_len);
            if key_len == 0 && value_len == 0 {
                // A pad record is the end of the page and says so with a
                // checksum. Anything else that reads as empty is a hole.
                return header.kind() == record::KIND_PAD && header.intact();
            }
            if at + size > bytes.len() || !header.intact() {
                return false;
            }
            size
        };
        at += step;
    }
    true
}

/// Where the file still holds a record above the address the scan
/// stopped at, or `None` when there is nothing up there.
///
/// A torn tail and a hole look the same from below: the scan stops and
/// what it read is a prefix. They are not the same thing. A torn tail is
/// a write that was in flight when the machine went down, so nothing
/// above it was ever acknowledged and there is nothing above it to find.
/// A hole is bytes the device lost out of the middle of a log that was
/// written and acknowledged, and the records above it are real and are
/// now unreachable, because the log is read in order and the order is
/// broken there.
///
/// Looking is cheap and only page starts have to be looked at. A record
/// never straddles a page, so the allocator starts every page with one,
/// and a page that holds anything at all holds it at its first byte.
/// Provisioned blocks past the tail read as zeros, so a clean stop at
/// the end of the log finds nothing, which is the case this runs in
/// nearly every time.
fn hole_above(core: &Core, stopped: Address, len: u64) -> Option<Address> {
    let mut address = page_start(page_of(stopped) + 1);
    while address < len {
        let base = core.log.resident(address);
        if base.is_null() {
            return None;
        }
        // SAFETY: the page is resident and a page start has a whole
        // header in it, since a page is far larger than a header. The
        // lengths are bounded against the page before the checksum
        // reads a byte past the header, because damage is exactly what
        // this is looking at and damage puts any number in a length
        // field (#438).
        let live = unsafe {
            let header = RecordRef::new(base);
            let key_len = header.key_len();
            let value_len = header.value_len();
            let empty = key_len == 0 && value_len == 0;
            record::size_of(key_len, value_len) <= PAGE_SIZE
                && (!empty || header.kind() == record::KIND_PAD)
                && header.intact()
        };
        if live {
            return Some(address);
        }
        address = page_start(page_of(address) + 1);
    }
    None
}

/// Reads the records the file holds, in the order they were written,
/// and hands each one to `visit`. Returns where the last accepted record
/// ended, which is where the next append goes.
///
/// That is not the same as where the walk stops. The file can be longer
/// than the log, either because a page ends in padding or because the
/// write path had provisioned blocks past the tail that the run did not
/// live to use, and appending after those would leave a hole and lose
/// the room.
fn walk(
    core: &Core,
    from: Address,
    len: u64,
    mut visit: impl FnMut(RecordRef<'_>, Address) -> Result<()>,
) -> Result<Address> {
    let mut address = from;
    let mut end = from;
    // Read once. The format is a property of the file and it cannot
    // change while the file is being replayed.
    let padless = core.log.format() == log::FORMAT_PADLESS;
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
                // Zeros used to mean two things and the walk could not
                // tell them apart, so a block the device lost read as
                // page padding, the rest of the page went with it, and
                // the scan carried on above the hole installing records
                // that were never a prefix of anything (#472).
                //
                // A file written with pad records says which it is. A
                // pad is a bare header with a real checksum, and zeros
                // are neither, so zeros are now either the end of the
                // log or damage and the walk stops on both. Stopping is
                // the safe answer to both: at the end there is nothing
                // above to lose, and at a hole what is above is
                // unreachable rather than lost.
                //
                // A file written before pad records existed gets the
                // old reading, because it has nothing in it to reach
                // the new conclusion from. It keeps the old exposure
                // too, which is the honest trade and is why the format
                // is stamped rather than assumed.
                let padding = padless || (header.kind() == record::KIND_PAD && header.intact());
                if padding {
                    None
                } else {
                    break;
                }
            } else if size > room || !header.intact() {
                break;
            } else {
                visit(header, address)?;
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
    Ok(end)
}

/// Puts one record's address into the index, by the same rules the
/// write path uses so that a recovered table behaves like a written one.
/// Single threaded, so no compare and swap and no tentative bit.
///
/// The one thing it does that the write path does not is move a link.
/// An entry names the head of a chain, and everything that entry can
/// reach is reached through it, so a record that takes a slot over has
/// to point at what the slot held or the keys behind it stop existing.
/// The write path gets that for free: it appends the record after
/// reading the entry, so the link is right by construction. The scan
/// has records that were written under whatever table that run had, and
/// the table it is filling is a different one whenever the run grew or
/// the reopen asked for a different `index_buckets`. A link that points
/// somewhere else is therefore not a rare case, and installing on top of
/// it was silent loss: keys that were acknowledged and made durable came
/// back missing, with nothing in the file wrong (#462).
///
/// So the scan repairs the link instead, which it may do because it is
/// alone with the file and because a record's chain pointer means
/// nothing outside the table it was built for. Nearly every record still
/// points where it should, `presize` is what keeps it that way, and a
/// record that already points at the right place is left untouched.
///
/// The pages that did change go back to the file at the end of the scan,
/// because a repair that only happened in memory lasts until the first
/// eviction and then the old bytes come back off the device. That write
/// is torn write safe through [`journal`], which writes the repairs down
/// and commits them before any of them is applied (#463).
fn install(
    core: &Core,
    header: RecordRef<'_>,
    address: Address,
    rewritten: &mut Rewritten,
    repairs: &mut Repairs,
    scratch: &mut Vec<u64>,
) {
    let key = header.key();
    let hash = index::hash(key);
    let tag = Index::tag(hash);
    // No migration can be in flight: the flusher that would start one is
    // not running yet, and this is the only thread there is.
    let bucket = core.index.live().bucket(hash);
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
        if let Some(installed) = chain_version(core, entry, key, scratch) {
            // Not greater than: a pass that copies a copy leaves two
            // records carrying the same version, and the higher address
            // is the one that is still there after the region below it
            // goes.
            if header.version() >= installed {
                relink(
                    core,
                    header,
                    address,
                    index::address_of(entry),
                    rewritten,
                    repairs,
                );
                bucket.slots[i].store(
                    index::entry(tag, address, index::is_foreign(entry)),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            return;
        }
    }
    // Past the loop is a key the table has not seen, whichever way it
    // goes in, and the count is what the table sizes itself against once
    // the flusher starts.
    core.index.note_key();
    if let Some(i) = empty {
        // Nothing was reachable through an empty slot, so the record
        // starts a chain rather than extending one.
        relink(core, header, address, NULL, rewritten, repairs);
        bucket.slots[i].store(
            index::entry(tag, address, false),
            std::sync::atomic::Ordering::Relaxed,
        );
        return;
    }
    // A full bucket, so the record takes an entry over and carries what
    // it held. The slot is the one the write path would have picked, not
    // because it has to be but because a scan that lands where the
    // writes landed leaves less to repair.
    let i = tag as usize % SLOTS;
    let victim = index::address_of(bucket.slots[i].load(std::sync::atomic::Ordering::Relaxed));
    relink(core, header, address, victim, rewritten, repairs);
    bucket.slots[i].store(
        index::entry(tag, address, true),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Points a record at what the entry it is taking over holds, unless it
/// points there already, and remembers the page so that it goes back to
/// the file before anything can evict it.
///
/// A record that starts a chain is asked to point at [`NULL`], and a
/// record already pointing below the log's begin is left alone instead.
/// Every walk in the engine stops on either, because below begin there
/// is nothing left to find, so the two links say the same thing and
/// rewriting one into the other buys nothing. It is not a rare case: a
/// compaction copy chains to whatever the entry held at copy time, which
/// is in the region the pass is about to drop, so after one pass every
/// live record in the file points below begin. Repairing all of them
/// took a reopen of a compacted database from rewriting nothing to
/// rewriting 99.9% of its records, which is the difference between #463
/// being a narrow window and being the whole file.
fn relink(
    core: &Core,
    header: RecordRef<'_>,
    address: Address,
    previous: Address,
    rewritten: &mut Rewritten,
    repairs: &mut Repairs,
) {
    if header.previous() == previous {
        return;
    }
    if previous == NULL && header.previous() < core.log.begin() {
        return;
    }
    // SAFETY: recovery is single threaded and the record was read out of
    // a resident page, which is a page that can be written back.
    unsafe { header.relink(previous) };
    core.recovered
        .relinked
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    rewritten.insert(page_of(address));
    repairs.push((address, previous));
}

/// The version of the record this chain holds for `key`, or `None` when
/// it holds none. Follows the pointers the records themselves carry and
/// stops at the log's begin for the same reason the live path does:
/// below it there is nothing left to find.
///
/// The first match is the answer, because a chain runs newest first.
///
/// A record that is not in memory is read off the device, the way
/// [`crate::Session::locate`] reads one. The full scan never meets that
/// case, since it restores the whole file first, but a recovery that
/// read a checkpoint restores only the pages above the boundary and
/// every entry the checkpoint installed points below it. Answering
/// `None` there would say the table has never seen a key it is holding
/// an entry for, and the install would put a second entry in the bucket
/// for it and let a later lookup answer out of the stale one.
fn chain_version(core: &Core, entry: u64, key: &[u8], scratch: &mut Vec<u64>) -> Option<u64> {
    let mut address = index::address_of(entry);
    let foreign = index::is_foreign(entry);
    while address != crate::addr::NULL && address >= core.floor_of(address) {
        let mut base = if cold::is_cold(address) {
            // A hot record may point into the tier, so a chain the scan
            // is reading can cross once and has to be followed across.
            // See [`crate::cold`].
            core.cold.as_ref()?.load(address, scratch).ok()?;
            scratch.as_ptr().cast()
        } else {
            core.log.resident(address)
        };
        if base.is_null() {
            core.log.load(address, scratch).ok()?;
            base = scratch.as_ptr().cast();
        }
        // SAFETY: the record is either in a restored page or in the
        // scratch buffer, which `load` sized to hold the whole of it and
        // which is a Vec<u64> and so is 8 byte aligned.
        let previous = unsafe {
            let r = RecordRef::new(base);
            if r.key() == key {
                return Some(r.version());
            }
            r.previous()
        };
        if !foreign {
            // The same rule the read path uses: an entry without the
            // bit answers for the key at its head and the scan has to
            // read it the way a lookup will.
            return None;
        }
        address = previous;
    }
    None
}

/// The side file that makes the link repair torn write safe.
///
/// A repair changes eight bytes of `previous` and four bytes of checksum
/// twenty four bytes later, and the page carrying both goes back to the
/// file whole. A crash inside that write can leave the file with one of
/// the two updated and not the other, and then the next scan finds a
/// record whose checksum does not hold, calls it the end of the durable
/// prefix, and stops there. Everything above it is gone, and it is not a
/// small amount, because the repairs are near the bottom of the log
/// where the oldest pages are. `examples/relink.rs` counts how much a
/// reopen actually repairs and the answer is up to a third of the
/// records, so this is a hole worth closing rather than a corner.
///
/// So the repairs are written down before any of them is applied, the
/// way InnoDB's double write buffer works, but by record rather than by
/// page: sixteen bytes each instead of four megabytes. The order is
/// journal, sync, pages, sync, remove. A reopen that finds a journal
/// left behind applies it before the scan reads anything, which puts the
/// file back into the image the interrupted reopen was writing.
///
/// Reapplying is safe whether the crash landed before, during or after
/// the page writes, because the only bytes that differ between the two
/// images are the ones the journal names. Every other byte of those
/// records is the same in both, so setting `previous` and recomputing
/// the checksum reconstructs the intended record exactly, and doing it
/// to a record that already has it changes nothing.
///
/// A journal beside the file rather than a slot inside it, because the
/// log's header is eight bytes of `begin` with records starting right
/// after it. Z10's index checkpoint removes the need for any of this,
/// since a recovery that reads the table back has no links to repair.
mod journal {
    use super::{Address, Core, PAGE_SIZE, Rewritten, page_of, page_start};
    use crate::addr::NULL;
    use crate::error::Result;
    use crate::record::RecordRef;

    /// Says the file is a zu2 relink journal and not something that
    /// happens to have the right name.
    const MAGIC: u64 = 0x7a75_3272_656c_6e6b;

    /// Magic, count, and the checksum after the entries.
    const HEADER: usize = 16;
    const ENTRY: usize = 16;

    /// Writes the repairs down and commits them, before any of them is
    /// applied to the log file.
    pub fn write(core: &Core, repairs: &[(Address, Address)]) -> Result<()> {
        let buf = encode(repairs);
        let path = core.log.journal_path();
        let file = crate::file::create_or_replace(&path)?;
        crate::file::write_all_at(&file, &buf, 0)?;
        crate::file::sync(&file)?;
        Ok(())
    }

    /// A journal's bytes. Its own function so that a test can make one
    /// without a database to hang it on.
    pub fn encode(repairs: &[(Address, Address)]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER + repairs.len() * ENTRY + 4);
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&(repairs.len() as u64).to_le_bytes());
        for (address, previous) in repairs {
            buf.extend_from_slice(&address.to_le_bytes());
            buf.extend_from_slice(&previous.to_le_bytes());
        }
        let crc = crc32c::crc32c(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Whether there is a journal beside the file at all, which is the
    /// one question the checkpoint path asks before it decides whether
    /// it may skip the pages a journal would need.
    pub fn present(core: &Core) -> bool {
        core.log.journal_path().exists()
    }

    /// Takes the journal away, which is what says the repairs are all in
    /// the log file now.
    ///
    /// A remove that fails leaves a journal that the next reopen applies
    /// again, which changes nothing, so there is nothing to report.
    pub fn clear(core: &Core) {
        let _ = std::fs::remove_file(core.log.journal_path());
    }

    /// Finishes a reopen that died part way through writing its repairs,
    /// if there is one to finish.
    pub fn apply(core: &Core, len: u64) -> Result<()> {
        let path = core.log.journal_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(());
        };
        let Some(repairs) = parse(&bytes) else {
            // A journal that does not check out was never committed, so
            // the page writes had not started and the file is whatever
            // the run before this one left. Nothing to finish.
            let _ = std::fs::remove_file(&path);
            return Ok(());
        };
        let mut rewritten = Rewritten::new();
        for (address, previous) in repairs {
            if address >= len {
                continue;
            }
            let base = core.log.resident(address);
            if base.is_null() {
                continue;
            }
            // SAFETY: nothing else is running, the address came out of a
            // committed journal so it names a record this file wrote,
            // and the only bytes a torn write can have left wrong are
            // the ones about to be rewritten.
            unsafe {
                let record = RecordRef::new(base);
                if record.previous() == previous {
                    continue;
                }
                record.relink(previous);
            }
            rewritten.insert(page_of(address));
        }
        for page in rewritten {
            let bytes = (len - page_start(page)).min(PAGE_SIZE as u64) as usize;
            core.log.rewrite_page(page, bytes)?;
        }
        core.log.sync_file()?;
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// The repairs a journal holds, or nothing when it does not hold a
    /// whole committed one.
    fn parse(bytes: &[u8]) -> Option<Vec<(Address, Address)>> {
        if bytes.len() < HEADER + 4 {
            return None;
        }
        if u64::from_le_bytes(bytes[0..8].try_into().ok()?) != MAGIC {
            return None;
        }
        let count = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
        let end = HEADER.checked_add(count.checked_mul(ENTRY)?)?;
        if bytes.len() != end + 4 {
            return None;
        }
        if u32::from_le_bytes(bytes[end..end + 4].try_into().ok()?) != crc32c::crc32c(&bytes[..end])
        {
            return None;
        }
        let mut repairs = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER + i * ENTRY;
            let address = u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
            let previous = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().ok()?);
            if address == NULL {
                return None;
            }
            repairs.push((address, previous));
        }
        Some(repairs)
    }
}

#[cfg(test)]
mod tests {
    use crate::addr::{Address, NULL, PAGE_SIZE, offset_of};
    use crate::db::{Db, Options};
    use crate::record::{HEADER, KIND_VALUE};

    /// A reopen that died part way through writing its link repairs
    /// back, which #463 is about. A repair changes `previous` and the
    /// checksum twenty four bytes later, the page goes back whole, and a
    /// crash inside that write can leave one of the two updated and not
    /// the other. The scan then finds a record whose checksum does not
    /// hold, calls it the end of the durable prefix, and loses every
    /// record above it.
    ///
    /// Built rather than raced, because the window is one page write
    /// wide. The tear is written by hand into a copy of a good file:
    /// `previous` moved, checksum left alone. The negative control is
    /// the same file with no journal beside it, which has to lose the
    /// records above the tear, because a positive result means nothing
    /// unless the damage was real.
    #[test]
    fn a_reopen_that_died_writing_its_repairs_back_finishes_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = Options {
            durability: crate::log::Durability::Async,
            ..Options::default()
        };
        let key = |i: u32| format!("k{i:04}").into_bytes();
        let records = 10u32;
        let torn = 5u32;

        // A good file, and the address of the record the tear lands in.
        let good = dir.path().join("good.zu2");
        let victim: Address;
        {
            let db = Db::create(&good, options).expect("create");
            let mut s = db.session();
            let mut at = 0u64;
            for i in 0..records {
                if i == torn {
                    at = db.core().log.tail();
                }
                s.upsert(&key(i), &[b'x'; 100]).expect("upsert");
            }
            drop(s);
            db.sync().expect("sync");
            victim = at;
        }
        assert!(victim > 0, "the victim record was never written");

        // The tear: eight bytes of `previous` moved, the checksum
        // twenty four bytes later left as it was.
        let tear = |to: &std::path::Path| {
            std::fs::copy(&good, to).expect("copy");
            let file = crate::file::open_rw(to).expect("open");
            let mut was = [0u8; 8];
            crate::file::read_exact_at(&file, &mut was, victim).expect("read");
            crate::file::write_all_at(&file, &0xdeadu64.to_le_bytes(), victim).expect("write");
            crate::file::sync(&file).expect("sync");
            u64::from_le_bytes(was)
        };

        // Without a journal there is nothing to finish, and the file is
        // as bad as it looks.
        let alone = dir.path().join("alone.zu2");
        tear(&alone);
        {
            let db = Db::open(&alone, options).expect("reopen");
            let mut s = db.session();
            let mut out = Vec::new();
            assert!(
                !s.read(&key(records - 1), &mut out).expect("read"),
                "the tear did not stop the scan, so the journal is not what saves the next one"
            );
        }

        // With one, the reopen puts the record back before the scan
        // reads a byte.
        let saved = dir.path().join("saved.zu2");
        let was = tear(&saved);
        let mut journal = saved.clone().into_os_string();
        journal.push(".relink");
        std::fs::write(&journal, super::journal::encode(&[(victim, was)])).expect("journal");

        let db = Db::open(&saved, options).expect("reopen");
        let mut s = db.session();
        let mut out = Vec::new();
        for i in 0..records {
            assert!(
                s.read(&key(i), &mut out).expect("read"),
                "the journal did not put the file back, and k{i:04} is gone"
            );
            assert_eq!(out, vec![b'x'; 100], "k{i:04} came back wrong");
        }
        drop(s);
        assert!(
            !std::path::Path::new(&journal).exists(),
            "the journal is still there after the reopen that applied it"
        );
    }

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
