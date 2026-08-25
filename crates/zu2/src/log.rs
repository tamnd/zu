//! The hybrid log: an append-only byte stream whose tail is in memory
//! and whose body is on disk.
//!
//! Three boundaries partition the address space, each monotonically
//! increasing:
//!
//! ```text
//! [0 .. head)                stable, on disk only
//! [head .. read_only)        immutable, in memory
//! [read_only .. tail)        mutable, in memory, updates happen here
//! ```
//!
//! The read-only boundary is what makes an in-place update safe. Above
//! it a record is young enough that no reader is entitled to treat it
//! as immutable, so a writer may rewrite the value under the record's
//! seqlock. Below it an update becomes an append with the old address
//! in `previous`, which is FASTER's read-copy-update.
//!
//! Two details are worth reading before the code.
//!
//! A record never straddles a page. When one does not fit, the
//! allocator skips to the next page and the bytes left behind stay
//! zero, so the file offset of a record is exactly its address and no
//! translation table exists to get wrong. The waste is under one record
//! per 4 MiB page.
//!
//! Flushing a partial page is safe without any per-page byte counter,
//! because every session publishes the lowest address it may be writing
//! and the flusher writes below the lowest of those. A session that is
//! only reading publishes nothing and holds nothing up, which is the
//! difference between a commit that costs what the device charges and
//! one that costs whatever the other threads happen to be doing.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::addr::{Address, FIRST, MAX_PAGES, PAGE_SIZE, page_of, page_start};
use crate::epoch::{Epochs, Slotted};
use crate::error::{Error, Result};
use crate::{file, record};

/// How a commit is made durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Durability {
    /// The append is visible at once and nothing waits. A crash loses
    /// everything since the last flush, though what survives is always
    /// a prefix, because the log is the write ahead log and recovery
    /// stops at the first record whose checksum does not hold.
    Async,
    /// The commit does not return until the device has the bytes.
    ///
    /// Concurrent committers share one device write. Whichever thread
    /// takes the flush mutex first becomes the leader and flushes
    /// everything appended so far, so the threads queued behind it find
    /// their own records already durable and return without a second
    /// write. That is Aether's group commit (Johnson, Pandis, Stoica,
    /// Athanassoulis, Ailamaki, PVLDB 3(1), 2010) with the leader
    /// picked by the mutex rather than by a dedicated thread.
    ///
    /// There used to be two modes here, one that woke a background
    /// flusher and waited for it and one that flushed inline. The
    /// handoff cost a scheduler round trip per commit and measured five
    /// times slower than flushing inline, and it bought nothing: both
    /// waited for the same device write. Two modes that differ only in
    /// how a commit gets to the same fsync are one mode.
    #[default]
    Durable,
}

/// The page memory layout, 64 byte aligned so a record's alignment
/// follows from its offset.
fn page_layout() -> Layout {
    Layout::from_size_align(PAGE_SIZE, 64).expect("zu2 page layout")
}

/// The format a file written now carries in the top byte of its marker
/// word.
///
/// One rather than zero because zero is what a file written before
/// there was a format reads back as, and the difference matters: a
/// padless file has no pad records, so recovery has to fall back to
/// reading zeros as page padding and cannot tell a hole from the end of
/// a page. Refusing to open one would have been simpler and it would
/// throw away files that are not wrong, only older.
pub const FORMAT: u8 = 1;

/// A file with no pad records. See [`FORMAT`].
pub const FORMAT_PADLESS: u8 = 0;

/// Where the format sits in the marker word. An address is 48 bits, so
/// the top sixteen were spare.
const FORMAT_SHIFT: u32 = 56;

/// The part of the marker word that is the address.
const ADDRESS_MASK: u64 = (1 << FORMAT_SHIFT) - 1;

/// Where recovery's link repairs live beside a log at this path. Free
/// standing because a database being created has to be able to name its
/// sidecars before it has a log to ask. See [`Log::journal_path`].
pub fn journal_path_beside(path: &Path) -> PathBuf {
    let mut beside = path.to_path_buf().into_os_string();
    beside.push(".relink");
    PathBuf::from(beside)
}

/// The checkpoint beside a log at this path, and the name it is written
/// under before the rename. See [`Log::checkpoint_path`].
pub fn checkpoint_path_beside(path: &Path) -> (PathBuf, PathBuf) {
    let mut beside = path.to_path_buf().into_os_string();
    beside.push(".ckpt");
    let mut writing = beside.clone();
    writing.push(".writing");
    (PathBuf::from(beside), PathBuf::from(writing))
}

/// Pages per chunk of the page table.
///
/// A chunk is 4096 pointers, so 32 KiB of memory covering 16 GiB of
/// log, and the array above it is 16384 pointers, so 128 KiB covering
/// the whole 256 TiB an index entry can address. Both numbers are small
/// enough that neither is worth a decision: the fixed 128 KiB is a
/// thirtieth of one page, and a chunk arrives once per 16 GiB.
const CHUNK: usize = 1 << 12;

/// The bit a page slot carries when its page is a mapping of the file
/// rather than an allocation of the heap.
///
/// The two kinds have to be told apart at exactly one place, the free,
/// where one is a `dealloc` and the other a `munmap`, and the low bit is
/// the cheapest way to carry which. A heap page is 64 byte aligned and a
/// mapping is aligned to whatever the kernel maps at, so the bit is free
/// in both. Everything that reads a page goes through [`Log::page_ptr`],
/// which masks it off, and everything that publishes a mapping tags it
/// there and nowhere else. #757.
const MAPPED: usize = 1;

/// Tags a mapping so the free side knows what it is holding.
#[inline]
fn tag_mapped(at: *mut u8) -> *mut u8 {
    at.map_addr(|address| address | MAPPED)
}

/// Whether a page slot is holding a mapping.
#[inline]
fn is_mapped(at: *mut u8) -> bool {
    at.addr() & MAPPED != 0
}

/// The page a slot points at, whichever kind it is.
#[inline]
fn page_base(at: *mut u8) -> *mut u8 {
    at.map_addr(|address| address & !MAPPED)
}

/// The chunk layout. Written as a slice rather than an array so the
/// free side can rebuild the same `Box<[_]>` it came from.
fn chunk_layout() -> Layout {
    Layout::array::<AtomicPtr<u8>>(CHUNK).expect("zu2 page chunk layout")
}

/// How far past the write frontier the file is kept provisioned by
/// default.
///
/// The size is a trade between two syscalls. Provisioning covers a
/// megabyte at a time, so a workload of small durable commits calls
/// `fallocate` once per few hundred flushes and the rest of them write
/// into blocks the file already owns. Larger chunks would call it less
/// often and reserve more space that a database sitting idle has not
/// used, and the reservation is given back at rest anyway, so a
/// megabyte is the point where the syscall has stopped mattering.
pub const PROVISION_CHUNK: u64 = 1 << 20;

/// How much log the engine's own copies may use above `max_pages`.
///
/// An eighth of the cap, and never less than two pages. Compaction has
/// to take a live record to the tail before it can free the space the
/// record was in, so a cap that binds the pass as tightly as it binds
/// the writers is a cap the log cannot get out from under. The reserve
/// is what a pass copies into, and it comes back the moment the pass
/// reclaims, so the steady state is the cap and not the cap plus this.
/// See [`Log::allocate`]. #566.
fn reserve_pages(max_pages: usize) -> usize {
    (max_pages / 8).max(2)
}

/// What the thread doing the device write owns while it does it.
///
/// Only the leader of a group ever holds this, and leadership is decided
/// under [`Flushing`], so the lock is uncontended by construction. It is
/// a separate lock rather than a field of `Flushing` for one reason: the
/// device write happens with `Flushing` released, so that the threads
/// arriving during it can queue up and be served by the write instead of
/// waiting to start one of their own.
struct Device {
    /// The address up to which bytes have been written to the file but
    /// not necessarily synced.
    written: Address,
    /// The address up to which the file's blocks are allocated, so a
    /// write below it is a write and not also an allocation.
    provisioned: u64,
    /// Cleared when the filesystem refuses to provision, so the log
    /// stops asking. It is slower and not wrong.
    provisions: bool,
}

/// Who is allowed to write to the device, and whether the log is closing.
///
/// Short critical sections only. A committer holds this to find out
/// whether somebody is already writing, and either becomes the leader or
/// waits on `synced` for the leader to publish, which is what makes one
/// device write serve a whole group.
struct Flushing {
    /// Set while a thread is between claiming the device and publishing
    /// what it wrote.
    syncing: bool,
    /// Set when the log is closing, so the flusher stops after a final
    /// pass.
    stopping: bool,
}

pub struct Log {
    /// One slot per page, filled lazily, in chunks that are themselves
    /// filled lazily. A null slot means the page is not resident, which
    /// is the only test the read path makes: the head boundary can move
    /// while a reader is mid-operation, but the memory it is looking at
    /// cannot go away until its epoch passes.
    ///
    /// The two levels are what stop `max_pages` from being a budget for
    /// every byte the database will ever write. A flat table indexed by
    /// absolute page has to be as long as the highest address the log
    /// will ever reach, and since the tail only goes up and a page
    /// index is never reused, that is the sum of every append rather
    /// than the size of anything (#470). Chunked, the array of chunk
    /// pointers covers the whole address space an index entry can name
    /// for 128 KiB, the chunks under it arrive as the log reaches them
    /// and leave when compaction passes them, and `max_pages` goes back
    /// to bounding the live span the way it reads.
    ///
    /// A ring over the flat table would have been smaller and it is
    /// wrong. Reclamation nulls a slot and defers the page's free to
    /// the epoch, precisely so that a session already walking a chain
    /// down there keeps reading real bytes until it is done. A ring
    /// hands the slot to a new page immediately, so that session reads
    /// the new page's bytes at the old page's offset and gets a
    /// different record with no sign that anything happened.
    chunks: Box<[AtomicPtr<AtomicPtr<u8>>]>,
    /// The live span the log is allowed to reach, in pages.
    max_pages: usize,
    /// The format the file is written in. See [`FORMAT`].
    format: AtomicU32,
    /// Serialises page allocation, which happens once per 4 MiB.
    allocating: Mutex<()>,
    tail: AtomicU64,
    read_only: AtomicU64,
    head: AtomicU64,
    /// Durable up to here.
    flushed: AtomicU64,
    /// The address a flush in progress is going to write up to.
    ///
    /// This is what makes an in-place update safe against the flusher.
    /// The flusher publishes its target before it reads the session
    /// write frontiers, and a session about to rewrite a record
    /// publishes its frontier before it reads the target, so at least
    /// one of the two sees the other: either the flusher waits for the
    /// rewrite or the rewrite stands down and appends instead. Without
    /// it a writer could be editing a record while the flusher was
    /// reading the same bytes, and the file would take a record whose
    /// checksum belongs to neither version.
    flush_target: AtomicU64,
    /// The lowest address any record still lives at.
    ///
    /// Compaction raises it, and everything below it is a hole in the
    /// file. A chain walk stops here rather than at [`NULL`], which is
    /// what lets the front of the log be given back to the filesystem
    /// while the records above it keep the addresses they were written
    /// at. The value is persisted in the first eight bytes of the file,
    /// which no record ever occupies because [`FIRST`] is 8.
    ///
    /// [`NULL`]: crate::addr::NULL
    begin: AtomicU64,
    file: File,
    path: PathBuf,
    flushing: Mutex<Flushing>,
    /// Woken when a group's device write has landed and been published.
    ///
    /// A committer that finds a sync in progress waits here rather than
    /// on the lock. Waking every waiter at once is the point: a leader's
    /// write covers everything appended before it started, so most of
    /// the threads it wakes find themselves already durable and go
    /// straight back to work. Waiting on the lock instead meant they
    /// found that out one at a time, in whatever order the platform
    /// handed the lock out, and a writer that never lost the lock
    /// starved the rest for as long as it kept committing.
    synced: Condvar,
    device: Mutex<Device>,
    /// Woken when there is something to flush.
    dirty: Condvar,
    dirty_lock: Mutex<bool>,
    /// Pages kept in memory above the read-only boundary.
    mutable_pages: usize,
    /// Pages kept in memory at all. `usize::MAX` means never evict.
    memory_pages: usize,
    /// Whether a page that has settled becomes a mapping of the file
    /// instead of staying on the heap. See [`Log::remap_settled`], #757.
    map_settled: bool,
    /// The lowest page [`Log::remap_settled`] has not looked at.
    ///
    /// A cursor and not a scan, because the conversion is one way and a
    /// page only has to be considered once. Without it every call would
    /// walk from the head to the boundary, which with no eviction floor
    /// set is the whole log and grows without bound.
    remap_from: AtomicUsize,
    /// How far past the write frontier the blocks are reserved. Zero
    /// never reserves.
    provision_bytes: u64,
    /// Device writes since the log was opened.
    ///
    /// Against the commits that asked for one, this is what says whether
    /// group commit is grouping. A thousand commits and a thousand syncs
    /// means every thread paid the device on its own and the leader
    /// arrangement bought nothing.
    syncs: AtomicU64,
    /// Durable commits since the log was opened, whether or not they
    /// ended up doing the device write themselves.
    commits: AtomicU64,
    pub(crate) epochs: Epochs,
}

impl Log {
    // Eight settings from `Options`, all of them independent and all of
    // them stored flat. Grouping them into a struct would put a second
    // name on the same fields and buy nothing but a shorter signature.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file: File,
        path: &Path,
        max_pages: usize,
        mutable_pages: usize,
        memory_pages: usize,
        sessions: usize,
        provision_bytes: u64,
        map_settled: bool,
    ) -> Self {
        Self {
            chunks: (0..MAX_PAGES / CHUNK)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            max_pages: max_pages.clamp(1, MAX_PAGES),
            format: AtomicU32::new(u32::from(FORMAT)),
            allocating: Mutex::new(()),
            tail: AtomicU64::new(FIRST),
            read_only: AtomicU64::new(FIRST),
            head: AtomicU64::new(0),
            flushed: AtomicU64::new(FIRST),
            flush_target: AtomicU64::new(FIRST),
            begin: AtomicU64::new(FIRST),
            file,
            path: path.to_path_buf(),
            flushing: Mutex::new(Flushing {
                syncing: false,
                stopping: false,
            }),
            synced: Condvar::new(),
            device: Mutex::new(Device {
                written: FIRST,
                provisioned: 0,
                provisions: provision_bytes > 0,
            }),
            provision_bytes,
            syncs: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            dirty: Condvar::new(),
            dirty_lock: Mutex::new(false),
            // Never so wide that the boundary cannot get past the
            // compaction floor. Opening page p puts the boundary at
            // p minus this, compaction may only take what is below the
            // boundary, and the span cap forbids p from being more than
            // max_pages minus one above the floor. So a window of
            // max_pages minus one or wider is a log that fills once and
            // then cannot be compacted, because the pass it needs is
            // never allowed to see anything. It deadlocked rather than
            // said so. #584.
            //
            // Zero is a real setting and it is what this clamps to at
            // the smallest cap: the window is the page being appended to
            // and nothing older, since the boundary lands on the page
            // that just opened.
            mutable_pages: mutable_pages.min(max_pages.saturating_sub(2)),
            memory_pages: memory_pages
                .max(mutable_pages.min(max_pages.saturating_sub(2)).max(1) + 1),
            map_settled,
            remap_from: AtomicUsize::new(0),
            epochs: Epochs::new(sessions),
        }
    }

    /// Device writes since the log was opened.
    #[inline]
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    /// Durable commits since the log was opened.
    #[inline]
    pub fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn tail(&self) -> Address {
        self.tail.load(Ordering::Acquire)
    }

    #[inline]
    pub fn read_only(&self) -> Address {
        self.read_only.load(Ordering::Acquire)
    }

    #[inline]
    pub fn head(&self) -> Address {
        self.head.load(Ordering::Acquire)
    }

    #[inline]
    pub fn flushed(&self) -> Address {
        self.flushed.load(Ordering::Acquire)
    }

    /// The cap on the span, in pages, which is what an [`Error::LogFull`]
    /// reports as its `max`.
    #[inline]
    pub fn max_pages(&self) -> usize {
        self.max_pages
    }

    /// Pages the span covers right now, which is the same measure
    /// [`Log::allocate`] bounds.
    #[inline]
    pub fn pages(&self) -> usize {
        page_of(self.tail()) - page_of(self.begin()) + 1
    }

    /// Whether the log has room for a group of records of these sizes
    /// without running past `max_pages`, which is what a caller asks
    /// when it has a group to write and no way to take back the ones it
    /// has already written. See [`crate::db::Transaction::commit`].
    /// #571.
    ///
    /// It walks the sizes the way [`Log::allocate`] would rather than
    /// guessing at the padding, because the padding is the whole
    /// difficulty: a record never straddles a page, so one that does not
    /// fit in what is left of the current page skips to the next and
    /// leaves the remainder behind. Guessing generously is not the safe
    /// direction either, since a demand larger than `max_pages` can
    /// never be met and the caller would compact for ever waiting for
    /// it.
    ///
    /// Answers about the log as it is at this instant. A concurrent
    /// writer can take the room between this and the append, which is
    /// what the caller's wedge is for.
    pub fn room_for(&self, sizes: &[usize]) -> bool {
        let tail = self.tail();
        let mut at = tail;
        for &size in sizes {
            let page = page_of(at);
            let offset = (at - page_start(page)) as usize;
            if offset + size > PAGE_SIZE {
                at = page_start(page + 1);
            }
            at += size as u64;
        }
        if at == tail {
            return true;
        }
        if page_of(at - 1) >= MAX_PAGES {
            return false;
        }
        // The span the last of these records would leave, counted the
        // way `allocate` counts it: the pages from the compaction floor
        // to the page the record ends on, both included.
        let span = page_of(at - 1) - page_of(self.begin()) + 1;
        span <= self.max_pages
    }

    /// The lowest address a record can still be at. Everything below is
    /// a hole in the file and a chain walk stops here.
    #[inline]
    pub fn begin(&self) -> Address {
        self.begin.load(Ordering::Acquire)
    }

    /// Addresses the log spans, which is what it would cost on disk if
    /// nothing had been compacted away.
    #[inline]
    pub fn span(&self) -> u64 {
        self.tail().saturating_sub(self.begin())
    }

    /// Bytes the log and its sidecars occupy, holes excluded.
    ///
    /// The checkpoint and the relink journal are part of what the store
    /// costs and they were not counted here until #733. The checkpoint
    /// is sized by the index rather than by the records, so on a store
    /// of many small records it is a much larger share of the total
    /// than the 1.1% it was on the load that found this.
    pub fn disk_bytes(&self) -> Result<u64> {
        let mut bytes = file::disk_bytes(&self.file, &self.path)?;
        let (checkpoint, _) = self.checkpoint_path();
        bytes += file::disk_bytes_at(&checkpoint)?;
        bytes += file::disk_bytes_at(&self.journal_path())?;
        Ok(bytes)
    }

    /// Adopts a begin address read off a file that is being reopened.
    pub fn resume_begin(&self, address: Address) {
        self.begin.fetch_max(address, Ordering::AcqRel);
    }

    /// Reads the persisted begin address, or [`FIRST`] when the file has
    /// never been compacted.
    ///
    /// A file whose first eight bytes are zero has never had a hole
    /// punched in it, because compaction writes the marker before it
    /// punches anything. The other order would leave a reopen scanning
    /// from a page that is no longer there.
    pub fn read_begin(&self) -> Result<Address> {
        Ok(self.read_marker()?.1)
    }

    /// Reads the format the file was written in, which a reopen adopts
    /// before it replays. See [`Log::adopt_format`].
    pub fn read_format(&self) -> Result<u8> {
        Ok(self.read_marker()?.0)
    }

    /// The format the file was written in and where its log starts, out
    /// of the one word at offset zero.
    ///
    /// An address is 48 bits, so the top of that word was spare and the
    /// format goes in it. A file written before there was a format
    /// reads back as zero, which is what [`FORMAT_PADLESS`] names.
    fn read_marker(&self) -> Result<(u8, Address)> {
        if self.file_len()? < FIRST {
            return Ok((FORMAT, FIRST));
        }
        let mut marker = [0u8; 8];
        file::read_exact_at(&self.file, &mut marker, 0)?;
        let word = u64::from_le_bytes(marker);
        Ok((
            (word >> FORMAT_SHIFT) as u8,
            (word & ADDRESS_MASK).max(FIRST),
        ))
    }

    /// The format this log is reading and writing.
    #[inline]
    pub fn format(&self) -> u8 {
        self.format.load(Ordering::Acquire) as u8
    }

    /// Takes on the format the file was written in, which is what an
    /// open does before it replays.
    ///
    /// A compaction on an older file must not stamp it with the current
    /// format. It punches the front and leaves every page above the
    /// floor exactly as it found them, pad records and all or none at
    /// all, so the format is a property of the file and not of the run
    /// that happens to have it open.
    pub fn adopt_format(&self, format: u8) {
        self.format.store(u32::from(format), Ordering::Release);
    }

    /// Writes the marker word: the format in the top byte and the log's
    /// floor under it.
    fn write_marker(&self, begin: Address) -> Result<()> {
        let word = (u64::from(self.format()) << FORMAT_SHIFT) | begin;
        file::write_all_at(&self.file, &word.to_le_bytes(), 0)?;
        Ok(())
    }

    /// Stamps a fresh file with the format it is about to be written in.
    ///
    /// A file that never compacts never writes this word otherwise, so
    /// without a stamp at creation a new database would be
    /// indistinguishable from one written before the format existed,
    /// and would be read back under the older and more forgiving rule.
    pub fn stamp(&self) -> Result<()> {
        self.write_marker(FIRST)?;
        file::sync(&self.file)?;
        Ok(())
    }

    /// Hands the front of the log back to the filesystem.
    ///
    /// The caller has already copied every live record out of
    /// `[begin, upto)` and made the copies durable, so what is left down
    /// there is bytes nobody reaches. `upto` must be a page boundary,
    /// because a hole is punched in whole blocks and a page is the unit
    /// eviction already works in.
    ///
    /// The marker goes down first and is synced before a single block is
    /// released. A crash in between costs nothing: the marker names an
    /// address whose bytes are still on disk, so recovery scans a little
    /// more of the log than it had to. The other order would name bytes
    /// that are gone.
    ///
    /// Returns the bytes the filesystem took back, which is zero when it
    /// has no hole punch. That costs space and not correctness, so it is
    /// reported rather than raised.
    pub fn reclaim_to(&self, upto: Address) -> Result<u64> {
        let from = self.begin();
        debug_assert_eq!(upto % PAGE_SIZE as u64, 0, "reclaim to a page boundary");
        if upto <= from {
            return Ok(0);
        }
        self.write_marker(upto)?;
        file::sync(&self.file)?;
        self.begin.store(upto, Ordering::Release);
        self.head.fetch_max(upto, Ordering::AcqRel);
        // The pages go back after the boundary moves, so a session that
        // was already walking a chain either sees the old boundary and
        // real bytes, or the new one and stops before it asks.
        for page in page_of(from)..page_of(upto) {
            let Some(slot) = self.page_slot(page) else {
                continue;
            };
            self.release_page(slot.swap(std::ptr::null_mut(), Ordering::AcqRel));
        }
        self.release_chunks(page_of(upto));
        self.retire_pages();
        // The boundary moving is not enough on its own. A read that has
        // already passed the floor check is going to pread the file, and
        // between that check and the pread there is nothing stopping the
        // hole from arriving, which hands the reader a page of zeros and
        // a checksum that does not hold (#563). So the punch waits for
        // every session that could still be inside such a read, the same
        // wait a doubling does before it touches the old table. It costs
        // the maintenance thread and nothing on the read path.
        self.epochs.wait_for_quiescence();
        // Never the first block: it holds the marker, and a hole there
        // would zero the very thing that says where the log starts.
        let floor = from.max(file::BLOCK);
        if upto <= floor {
            return Ok(0);
        }
        if file::punch(&self.file, floor, upto - floor) {
            Ok(upto - floor)
        } else {
            Ok(0)
        }
    }

    /// Sets the tail after recovery has replayed the file, so that new
    /// records append past what is already there.
    pub fn resume_at(&self, address: Address) {
        // A recovered log is as compacted as it was when it closed, so
        // the boundary the marker named is where chains stop now.
        let begin = self.begin();
        self.head.fetch_max(begin, Ordering::AcqRel);
        self.tail.store(address, Ordering::Release);
        self.flushed.store(address, Ordering::Release);
        self.flush_target.store(address, Ordering::Release);
        self.read_only.store(address, Ordering::Release);
        let mut state = self.device.lock().expect("zu2 device state");
        state.written = address;
        // Whatever the file already has is provisioned, whether it was
        // reserved by a previous run or written by one. Starting from
        // zero would have the first flush ask the filesystem to allocate
        // a range it has already allocated.
        state.provisioned = self.file_len().unwrap_or(0);
    }

    /// The lowest address an update may still rewrite in place: young
    /// enough that no reader treats it as immutable, and not inside a
    /// flush that is already under way.
    ///
    /// The flush target is read sequenced because a caller that has
    /// already published its frontier is relying on one of the two
    /// sides seeing the other. See [`Epochs::write_floor`].
    ///
    /// [`Epochs::write_floor`]: crate::epoch::Epochs::write_floor
    #[inline]
    pub fn in_place_floor(&self) -> Address {
        self.flush_target
            .load(Ordering::SeqCst)
            .max(self.read_only.load(Ordering::Acquire))
    }

    /// The address below which every record is complete.
    ///
    /// The tail is read first and the frontiers after, because a
    /// session publishes its frontier before it claims tail space: read
    /// the other way round, a claim could land between the two reads
    /// and be missed by both.
    #[inline]
    fn write_floor(&self) -> Address {
        let tail = self.tail();
        self.epochs.write_floor(tail)
    }

    /// Waits until every record below `upto` is complete.
    ///
    /// This is what a flush waits for, and it used to wait for global
    /// epoch quiescence instead, which is a much larger thing: every
    /// session had to leave whatever it was doing, so a commit cost
    /// what the other threads happened to be doing rather than what the
    /// device charges. A reader does not appear here at all, and
    /// neither does a writer working above `upto`.
    fn wait_for_writers(&self, upto: Address) {
        let mut spins = 0u32;
        while self.write_floor() < upto {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    /// The slot a page's pointer lives in, or nothing when the chunk
    /// holding it has never been reached or has been compacted past.
    ///
    /// Two dependent loads on the read path where a flat table had one.
    /// The chunk array is 128 KiB and a walk touches the same handful of
    /// entries in it over and over, so the first load is an L1 hit in
    /// everything but the first touch.
    #[inline]
    fn page_slot(&self, page: usize) -> Option<&AtomicPtr<u8>> {
        let chunk = self.chunks.get(page / CHUNK)?;
        let base = chunk.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        // SAFETY: a non-null chunk points at CHUNK initialised slots and
        // its memory outlives every session that could have loaded the
        // pointer, because it is freed through the epoch.
        Some(unsafe { &*base.add(page % CHUNK) })
    }

    #[inline]
    fn page_ptr(&self, page: usize) -> *mut u8 {
        match self.page_slot(page) {
            // Masked here so that everything above this can treat a
            // mapped page and a heap page as the same thing, which they
            // are for every purpose except the free. See [`MAPPED`].
            Some(slot) => page_base(slot.load(Ordering::Acquire)),
            None => std::ptr::null_mut(),
        }
    }

    /// Hands a page back through the epoch, whichever kind it is.
    ///
    /// Deferred rather than freed: a reader that loaded the pointer
    /// before it left the slot is still inside its epoch and is entitled
    /// to the bytes. That is as true of a mapping as of an allocation,
    /// and a `munmap` under a reader is a segmentation fault where a
    /// `dealloc` under one is merely undefined.
    fn release_page(&self, stale: *mut u8) {
        if stale.is_null() {
            return;
        }
        let mapped = is_mapped(stale);
        let retired = page_base(stale) as usize;
        self.epochs.defer(Box::new(move || {
            // SAFETY: the epoch has passed, so no session that could
            // have loaded this pointer is still running, and the tag
            // says which of the two ways it was made.
            unsafe {
                if mapped {
                    crate::file::unmap(retired as *mut u8, PAGE_SIZE);
                } else {
                    dealloc(retired as *mut u8, page_layout());
                }
            }
        }));
    }

    /// The slot for a page, allocating the chunk under it if this is the
    /// first page anyone reached in that 16 GiB.
    fn ensure_slot(&self, page: usize) -> Result<&AtomicPtr<u8>> {
        if page >= MAX_PAGES {
            return Err(Error::AddressSpaceFull { pages: MAX_PAGES });
        }
        if let Some(slot) = self.page_slot(page) {
            return Ok(slot);
        }
        let _guard = self.allocating.lock().expect("zu2 page allocation");
        if let Some(slot) = self.page_slot(page) {
            return Ok(slot);
        }
        // SAFETY: the layout is non-zero sized, and a null pointer is a
        // valid AtomicPtr, so zeroed memory is an initialised chunk.
        let fresh = unsafe { alloc_zeroed(chunk_layout()) }.cast::<AtomicPtr<u8>>();
        if fresh.is_null() {
            std::alloc::handle_alloc_error(chunk_layout());
        }
        self.chunks[page / CHUNK].store(fresh, Ordering::Release);
        // SAFETY: just allocated with room for CHUNK slots.
        Ok(unsafe { &*fresh.add(page % CHUNK) })
    }

    /// Makes a page resident, allocating it if this is the first byte
    /// anyone claimed in it. Idempotent, and the common case is two
    /// acquire loads.
    fn ensure_page(&self, page: usize) -> Result<*mut u8> {
        let slot = self.ensure_slot(page)?;
        let existing = slot.load(Ordering::Acquire);
        if !existing.is_null() {
            return Ok(existing);
        }
        let _guard = self.allocating.lock().expect("zu2 page allocation");
        let existing = slot.load(Ordering::Acquire);
        if !existing.is_null() {
            return Ok(existing);
        }
        // SAFETY: the layout is non-zero sized and correctly aligned.
        let fresh = unsafe { alloc_zeroed(page_layout()) };
        if fresh.is_null() {
            std::alloc::handle_alloc_error(page_layout());
        }
        slot.store(fresh, Ordering::Release);
        Ok(fresh)
    }

    /// Reserves `size` bytes at the tail and returns where they start.
    ///
    /// The session's frontier goes out before the claim and names the
    /// tail as it stood, which is at or below where the claim lands, so
    /// a flusher that sees the claim also sees a frontier it can trust.
    /// Naming it early is what makes the frontier safe rather than
    /// merely early: too low only holds a flush back for as long as this
    /// takes, and too high would let one through over a half written
    /// record.
    ///
    /// A claim that would land past the end of the page table is refused
    /// before the tail moves rather than after. The tail is what a flush
    /// and a commit both take as their target, so a tail sitting in a
    /// page that cannot exist is not a full log, it is a database that
    /// can no longer be made durable at all.
    /// `reserved` is for the engine's own copies. Compaction takes a
    /// record back to the tail before it can reclaim the space the
    /// record was in, so a log held to exactly `max_pages` for everyone
    /// has no way out of being full: the writers stop, the pass that
    /// would free their space needs room to copy into, and there is
    /// none. The reserve is the way out, and only maintenance sessions
    /// can spend it. #566.
    fn allocate(&self, slot: &Slotted, size: usize, reserved: bool) -> Result<Address> {
        if size > PAGE_SIZE {
            return Err(Error::RecordTooLarge {
                size,
                page: PAGE_SIZE,
            });
        }
        loop {
            let observed = self.tail.load(Ordering::Acquire);
            slot.appending_at(observed);
            let page = page_of(observed);
            let offset = (observed - page_start(page)) as usize;
            let start = if offset + size > PAGE_SIZE {
                page_start(page + 1)
            } else {
                observed
            };
            if page_of(start) >= MAX_PAGES {
                return Err(Error::AddressSpaceFull { pages: MAX_PAGES });
            }
            // The span and not the tail. What `max_pages` bounds is how
            // much log there is between the compaction floor and the
            // tail, which is a size a caller can reason about and act
            // on, rather than a count of every page the database has
            // ever touched (#470).
            let span = page_of(start) - page_of(self.begin()) + 1;
            let cap = if reserved {
                self.max_pages + reserve_pages(self.max_pages)
            } else {
                self.max_pages
            };
            if span > cap {
                return Err(Error::LogFull {
                    span,
                    max: self.max_pages,
                });
            }
            if self
                .tail
                .compare_exchange_weak(
                    observed,
                    start + size as u64,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.ensure_page(page_of(start))?;
                if page_of(start) != page {
                    self.pad_out(observed, page);
                    self.opened_page(page_of(start));
                }
                return Ok(start);
            }
        }
    }

    /// Marks the bytes the allocator stepped over as padding on purpose.
    ///
    /// The gap runs from `at` to the end of `page` and belongs to
    /// whichever thread won the claim that moved past it, so nobody else
    /// can be writing there. The flusher cannot have taken it either:
    /// the session published its write frontier at `at` before it
    /// claimed anything, so a flush is held below the gap until this
    /// returns and `wrote` clears the frontier.
    ///
    /// A gap shorter than a header gets nothing, which is the one case
    /// where zeros still stand for padding. Recovery has the same test
    /// and reaches the same conclusion: too small to hold a record, so
    /// there is nothing down there to have lost.
    fn pad_out(&self, at: Address, page: usize) {
        let room = PAGE_SIZE - (at - page_start(page)) as usize;
        if room < record::HEADER {
            return;
        }
        let base = self.page_ptr(page);
        if base.is_null() {
            // The page went out of memory between the claim and here,
            // which takes an eviction of a page still being appended to
            // and should not happen. Nothing to write to, and a missing
            // pad costs recovery the rest of a page it can no longer
            // read anyway.
            return;
        }
        // SAFETY: the gap is inside the page, 8 byte aligned because
        // every record size is, and owned by this thread.
        unsafe {
            let dst = base.add((at - page_start(page)) as usize);
            record::write_at(dst, crate::addr::NULL, 0, &[], &[], false, record::KIND_PAD);
        }
    }

    /// Called by whichever thread first allocated in a new page. Moves
    /// the read-only boundary up behind the mutable window and evicts
    /// what has fallen out of memory.
    fn opened_page(&self, page: usize) {
        let boundary = page_start(page.saturating_sub(self.mutable_pages));
        self.read_only.fetch_max(boundary, Ordering::AcqRel);
        self.wake_flusher();
        self.evict_behind(page);
        self.retire_pages();
    }

    /// The eviction the write path runs, for a thread that is not
    /// writing.
    ///
    /// A page can only leave memory once its bytes are durable, so a
    /// burst of async appends outruns the flusher and [`opened_page`]
    /// stops at the first page that is not flushed yet. That is fine
    /// while the burst is going on. What is not is that the last thing
    /// to open a page is the last append, so when the flusher catches
    /// up a moment later there is nobody left to finish the job, and a
    /// database that has stopped writing sits above its bound for as
    /// long as it stays open. That is the shape of every run in this
    /// series: a load, and then a read phase that appends nothing.
    /// Measured on the test that goes with this, a bound of two pages
    /// held twenty. So the maintainer calls this after every flush,
    /// which is the thread that just made the pages evictable. #636.
    ///
    /// [`opened_page`]: Log::opened_page
    pub fn evict_settled(&self) {
        // Eviction first, and the order is load bearing rather than
        // arbitrary. `remap_settled` starts at the floor, so with a
        // bound set and the remap running ahead of it every settled
        // page was mapped by one half of this call and unmapped by the
        // other half of the same call: two syscalls a page, a mapping
        // that never served a read, and page cache warmed only to be
        // dropped. Moving the floor first means the remap never looks
        // below it. With no bound `evict_behind` returns at its first
        // line, so this costs the common case nothing.
        let bounded = self.memory_pages != usize::MAX;
        if bounded {
            self.evict_behind(page_of(self.tail()));
        }
        // Retires its own pages if it maps any, so a database with no
        // bound still does no epoch work on a flush that changed
        // nothing.
        self.remap_settled();
        if bounded {
            self.retire_pages();
        }
    }

    /// Turns the pages that have settled from heap into mappings of the
    /// file they are already identical to.
    ///
    /// A page below the read-only boundary and below the flushed
    /// frontier is finished: nothing will write to it again, and its
    /// bytes on the device are its bytes in memory. Holding a private
    /// copy of it is holding anonymous memory the kernel can do nothing
    /// with but swap, and the same bytes are sitting in the page cache
    /// underneath it. Mapping the file over the top gives the copy back
    /// and leaves the read a load rather than a `pread`.
    ///
    /// What this is worth is a fact about the kind of memory and not its
    /// size. Measured on server2 at a million records, zu2 held 1152 MiB
    /// of anonymous memory against lmdb's 17.6, and lmdb's resident set
    /// is page cache the kernel drops when it wants it back. #757.
    ///
    /// Three conditions, and each one is load bearing. Below the
    /// boundary, because a page in the mutable window is still being
    /// appended to and the mapping is read only. Below the flushed
    /// frontier, because a mapping shows what the file says and the file
    /// does not say anything yet about bytes that have not been written.
    /// Already resident, because a null slot is a page somebody chose to
    /// let go of and mapping it would quietly take the memory back.
    ///
    /// Off by default. The read of a mapped page is a fault where the
    /// read of a heap page is a load, and what that costs has not been
    /// measured on any host of record yet.
    fn remap_settled(&self) {
        if !self.map_settled {
            return;
        }
        let boundary = page_of(self.read_only());
        let flushed = self.flushed();
        let mut page = self
            .remap_from
            .load(Ordering::Acquire)
            .max(page_of(self.head()));
        let mut mapped = false;
        while page < boundary && page_start(page + 1) <= flushed {
            // The same lock the allocating path takes, so a page cannot
            // be published by one thread while another is replacing it.
            let guard = self.allocating.lock().expect("zu2 page allocation");
            let Some(slot) = self.page_slot(page) else {
                drop(guard);
                page += 1;
                continue;
            };
            let stale = slot.load(Ordering::Acquire);
            if stale.is_null() || is_mapped(stale) {
                drop(guard);
                page += 1;
                continue;
            }
            match crate::file::map_read(&self.file, page_start(page), PAGE_SIZE) {
                Some(at) => {
                    slot.store(tag_mapped(at), Ordering::Release);
                    drop(guard);
                    self.release_page(stale);
                    mapped = true;
                }
                None => {
                    // The platform has no mapping call or the kernel
                    // refused this one. Neither is an error: the page
                    // stays where it is and the cursor moves on, so a
                    // refusal costs one syscall a page and not a retry
                    // loop.
                    drop(guard);
                }
            }
            page += 1;
        }
        self.remap_from.fetch_max(page, Ordering::AcqRel);
        if mapped {
            self.retire_pages();
        }
    }

    /// Pages held as a mapping of the file rather than as heap.
    ///
    /// The rest of [`Log::resident_pages`] is anonymous memory, so the
    /// difference between the two is what a memory column is actually
    /// measuring.
    pub fn mapped_pages(&self) -> usize {
        let mut total = 0;
        for chunk in &self.chunks {
            let base = chunk.load(Ordering::Acquire);
            if base.is_null() {
                continue;
            }
            for i in 0..CHUNK {
                // SAFETY: a non-null chunk holds CHUNK initialised slots
                // and its memory is freed through the epoch.
                if is_mapped(unsafe { &*base.add(i) }.load(Ordering::Acquire)) {
                    total += 1;
                }
            }
        }
        total
    }

    /// Drops the pages that have fallen out of memory, given the page
    /// the log now ends in.
    fn evict_behind(&self, page: usize) {
        if self.memory_pages == usize::MAX {
            return;
        }
        let keep_from = page.saturating_sub(self.memory_pages);
        while page_of(self.head()) < keep_from {
            let victim = page_of(self.head());
            // A page can only leave memory once its bytes are durable,
            // otherwise a reader that misses in memory would pread a
            // hole.
            if page_start(victim + 1) > self.flushed() {
                break;
            }
            // fetch_max rather than a store, because a writer opening
            // a page and the maintainer finishing the job behind it can
            // be in here at the same time and the slower of two stores
            // would put the floor back down.
            self.head
                .fetch_max(page_start(victim + 1), Ordering::AcqRel);
            // The chunk stays. Eviction only says the page is not in
            // memory, and the addresses in it are still live and still
            // readable off the file, so the slot has to keep being
            // there to say null. Only compaction, which makes the
            // addresses themselves unreachable, gives a chunk back.
            let stale = match self.page_slot(victim) {
                Some(slot) => slot.swap(std::ptr::null_mut(), Ordering::AcqRel),
                None => std::ptr::null_mut(),
            };
            self.release_page(stale);
        }
    }

    /// Checks that the file on disk fits the span `max_pages` allows,
    /// and names the number that would if it does not.
    ///
    /// Called on the open path with the compaction floor already
    /// resumed, so what it measures is the live span and not the file
    /// length: a compacted file is mostly hole and its pages below
    /// `begin` cost nothing.
    ///
    /// The reserve counts here for the same reason it exists on the
    /// write path: a crash freezes the log wherever it was, and where it
    /// was may be inside the reserve, mid pass. Turning that file away
    /// would mean a database could write itself into a state it could
    /// not reopen at the options it was written with (#570). It opens,
    /// and the first write that runs out of room compacts the span back
    /// under `max_pages` the way any other over-full log does.
    pub fn fits_the_file(&self) -> Result<()> {
        let len = self.file_len()?;
        if len <= self.begin() {
            return Ok(());
        }
        let needs = page_of(len - 1) - page_of(self.begin()) + 1;
        if needs > self.max_pages + reserve_pages(self.max_pages) {
            return Err(Error::NeedsPages {
                needs,
                max: self.max_pages,
            });
        }
        Ok(())
    }

    /// Pages holding memory right now, which is what `memory_pages` is
    /// a promise about.
    pub fn resident_pages(&self) -> usize {
        let mut total = 0;
        for chunk in &self.chunks {
            let base = chunk.load(Ordering::Acquire);
            if base.is_null() {
                continue;
            }
            for i in 0..CHUNK {
                // SAFETY: a non-null chunk holds CHUNK initialised slots
                // and its memory is freed through the epoch.
                if !unsafe { &*base.add(i) }.load(Ordering::Acquire).is_null() {
                    total += 1;
                }
            }
        }
        total
    }

    /// Gives back the chunks of the page table that compaction has moved
    /// entirely below the floor.
    ///
    /// Only whole chunks below `begin`, which is the one boundary that
    /// says an address is unreachable rather than merely not in memory.
    /// Every slot in such a chunk is already null, so the chunk carries
    /// no page memory with it, and a reader holding a stale address down
    /// there finds a missing chunk exactly where it used to find a null
    /// slot: it misses, preads the hole, and the walk ends.
    ///
    /// Deferred like a page, and for the same reason: a session may have
    /// loaded the chunk pointer already.
    fn release_chunks(&self, below: usize) {
        for index in 0..below / CHUNK {
            let stale = self.chunks[index].swap(std::ptr::null_mut(), Ordering::AcqRel);
            if stale.is_null() {
                continue;
            }
            let retired = stale as usize;
            self.epochs.defer(Box::new(move || {
                // SAFETY: the epoch has passed, so no session that could
                // have loaded this pointer is still running, and it was
                // allocated with exactly this layout.
                unsafe { dealloc(retired as *mut u8, chunk_layout()) };
            }));
        }
    }

    /// Moves the epoch on and runs whatever that let go of.
    ///
    /// The bump comes after the pointers are retired and not before, and
    /// that order is the whole of it. An action is queued in the epoch
    /// that was current when its page was taken out of the table, and it
    /// runs once every session has announced a later one, so without a
    /// bump behind it there is no later one to announce and the action
    /// waits forever. Both eviction sites used to leave that bump to
    /// whoever came next, and the only site that did one was compaction,
    /// so a database with `memory_pages` set and compaction off evicted
    /// pages out of the table and never gave a single one back.
    fn retire_pages(&self) {
        self.epochs.bump();
        self.epochs.drain();
    }

    /// Appends a record and returns its address. The bytes are complete
    /// before this returns, so publishing the address publishes the
    /// record.
    ///
    /// The session's write frontier is cleared on every path out,
    /// including the failing ones. A frontier left behind names an
    /// address no record will ever finish at, and the flusher would
    /// wait at it for the life of the process.
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &self,
        slot: &Slotted,
        previous: Address,
        version: u64,
        key: &[u8],
        value: &[u8],
        tombstone: bool,
        kind: u32,
    ) -> Result<Address> {
        let size = record::size_of(key.len(), value.len());
        let outcome = (|| -> Result<Address> {
            let address = self.allocate(slot, size, slot.is_engine())?;
            let page = self.ensure_page(page_of(address))?;
            // SAFETY: the range came back from the tail allocator, so no
            // other thread owns it, and it lies inside the page because
            // the allocator never splits a record.
            unsafe {
                let dst = page.add((address - page_start(page_of(address))) as usize);
                record::write_at(dst, previous, version, key, value, tombstone, kind);
            }
            Ok(address)
        })();
        slot.wrote();
        outcome
    }

    /// A pointer to a resident record, or null when the page has been
    /// evicted.
    ///
    /// The head boundary is deliberately not consulted. It can move
    /// while a session is mid-operation; the page pointer cannot become
    /// dangling until that session's epoch has passed, so the pointer
    /// is the honest test and the boundary is not.
    #[inline]
    pub fn resident(&self, address: Address) -> *const u8 {
        let page = self.page_ptr(page_of(address));
        if page.is_null() {
            return std::ptr::null();
        }
        // SAFETY: the offset is inside the page by construction of the
        // address space.
        unsafe { page.add((address - page_start(page_of(address))) as usize) }
    }

    /// Reads an evicted record into an 8 byte aligned buffer.
    ///
    /// The checksum is checked here and nowhere else on the read path,
    /// and the reason is that this is the only place a record arrives
    /// from the device without recovery having read it first. A resident
    /// record was either written by this run or walked by the scan, so
    /// it has been through a checksum already; one that comes off the
    /// file now has not, and under a checkpoint recovery there can be a
    /// great many of those, since a checkpoint's whole purpose is to not
    /// read the log below its boundary. A block the device lost down
    /// there is then a record that reads as zeros, and the alternative
    /// to reporting it is answering that a key the database is holding
    /// an entry for does not exist. The cost is a crc over one record
    /// beside a read syscall that has already happened.
    pub fn load(&self, address: Address, into: &mut Vec<u64>) -> Result<()> {
        // One read that usually gets the whole record, rather than a
        // header read followed by a read of the same offset again. This
        // is the trade #557 made for the cold tier and it is the same
        // trade here for the same reason: the cost of a read off this
        // path is in reaching the device and not in the bytes it hands
        // back, so paying two of them for a record that nearly always
        // fits in the first is wrong. The two paths share the size, so
        // there is one number to argue about rather than two.
        //
        // Bounded by the end of the page. This is a performance guard
        // and not a correctness one, and it is worth saying which:
        // removing the bound passes every test in the crate, because a
        // record never straddles a page so the bytes past the boundary
        // are never used, and a short answer is fine anyway. What the
        // bound buys is not touching the next 4 MiB region at all, which
        // under memory pressure is the same mistake readahead was making
        // one page down. See [`crate::file::advise_random`].
        //
        // Short answers are allowed on their own account: the file's
        // length need not reach the end of the page being appended to, so
        // a speculation over the last record in a page can ask for bytes
        // nobody has written. What comes back is the record, which is
        // what this is for.
        let ceiling = (page_start(page_of(address) + 1) - address) as usize;
        let ask = crate::cold::SPECULATE.min(ceiling).max(record::HEADER);
        into.clear();
        into.resize(ask.div_ceil(8), 0);
        // SAFETY: the buffer is `ask` bytes and 8 byte aligned because it
        // is a Vec<u64>.
        let speculated =
            unsafe { std::slice::from_raw_parts_mut(into.as_mut_ptr().cast::<u8>(), ask) };
        let have = file::read_upto_at(&self.file, speculated, address)?;
        if have < record::HEADER {
            return Err(Error::Malformed {
                address,
                why: "the log ends inside a record header",
            });
        }
        // SAFETY: as above, and the lengths are only used to size a
        // second read.
        let size = unsafe {
            let r = record::RecordRef::new(into.as_ptr().cast());
            record::size_of(r.key_len(), r.value_len())
        };
        if size > PAGE_SIZE {
            return Err(Error::Malformed {
                address,
                why: "record longer than a page",
            });
        }
        if size > have {
            into.resize(size.div_ceil(8), 0);
            // SAFETY: the buffer is size bytes and 8 byte aligned, and
            // the first `have` of them are already the record's.
            let rest = unsafe {
                std::slice::from_raw_parts_mut(
                    into.as_mut_ptr().cast::<u8>().add(have),
                    size - have,
                )
            };
            file::read_exact_at(&self.file, rest, address + have as u64)?;
        }
        // SAFETY: the buffer is at least `size` bytes and holds the
        // record.
        let bytes = unsafe { std::slice::from_raw_parts(into.as_ptr().cast::<u8>(), size) };
        // SAFETY: the buffer holds the whole record, sized from the
        // lengths in its own header and bounded against a page above.
        if !unsafe { record::RecordRef::new(bytes.as_ptr()).intact() } {
            return Err(Error::Malformed {
                address,
                why: "checksum does not hold",
            });
        }
        Ok(())
    }

    /// Reads a whole page off the file into a caller's buffer.
    ///
    /// Compaction scans this way rather than through the resident page
    /// pointer, because a pass runs long enough that an eviction could
    /// take the page out from under it, and because the region it reads
    /// is cold by definition: it is below the read-only boundary and
    /// already on the device.
    pub fn read_page(&self, page: usize, into: &mut [u8]) -> Result<()> {
        file::read_exact_at(&self.file, into, page_start(page))?;
        Ok(())
    }

    /// Reads a page of the file back into memory, which is what
    /// recovery does before it walks the records.
    pub fn restore_page(&self, page: usize, len: usize) -> Result<()> {
        let base = self.ensure_page(page)?;
        // SAFETY: the page is resident and len is bounded by the caller
        // to the page size.
        let bytes = unsafe { std::slice::from_raw_parts_mut(base, len) };
        file::read_exact_at(&self.file, bytes, page_start(page))?;
        Ok(())
    }

    /// Reads a page back into memory while the database is running, for
    /// the thread that warms up what a checkpoint recovery did not read.
    /// Answers whether this call is the one that made it resident.
    ///
    /// [`Log::restore_page`] cannot be used for that. It publishes the
    /// page and then fills it, which is fine when recovery is the only
    /// thread there is and is a page of zeros handed to a reader when it
    /// is not. This fills a page nobody can see and publishes it after,
    /// so a reader sees either no page or the whole page.
    ///
    /// `check` is what says the page is worth publishing, and a page it
    /// turns down is left on the device rather than made resident. That
    /// is how a checkpointed database keeps saying something about
    /// damage: a record read off the file has its checksum checked in
    /// [`Log::load`] and a record read out of a resident page does not,
    /// so warming a page with a bad record in it would turn an error
    /// that names an address into a key that quietly does not exist.
    pub fn warm_page(
        &self,
        page: usize,
        len: usize,
        check: impl Fn(&[u8]) -> bool,
    ) -> Result<bool> {
        let slot = self.ensure_slot(page)?;
        if !slot.load(Ordering::Acquire).is_null() {
            return Ok(false);
        }
        // SAFETY: the layout is non-zero sized and correctly aligned.
        let fresh = unsafe { alloc_zeroed(page_layout()) };
        if fresh.is_null() {
            std::alloc::handle_alloc_error(page_layout());
        }
        // SAFETY: the allocation is a whole page and len is bounded by
        // the caller to the page size.
        let bytes = unsafe { std::slice::from_raw_parts_mut(fresh, len) };
        if let Err(error) = file::read_exact_at(&self.file, bytes, page_start(page)) {
            // SAFETY: allocated here with this layout and never
            // published, so nothing else can hold it.
            unsafe { std::alloc::dealloc(fresh, page_layout()) };
            return Err(error.into());
        }
        // SAFETY: filled by the read above and owned here.
        if !check(unsafe { std::slice::from_raw_parts(fresh, len) }) {
            // SAFETY: as above.
            unsafe { std::alloc::dealloc(fresh, page_layout()) };
            return Ok(false);
        }
        // The same lock the allocating path takes, so that the two
        // cannot both decide the slot is empty and both fill it.
        let _guard = self.allocating.lock().expect("zu2 page allocation");
        if !slot.load(Ordering::Acquire).is_null() {
            // SAFETY: as above.
            unsafe { std::alloc::dealloc(fresh, page_layout()) };
            return Ok(false);
        }
        slot.store(fresh, Ordering::Release);
        Ok(true)
    }

    /// Writes a page of memory back over the file.
    ///
    /// The flusher cannot do this. Its frontier only moves forward, so
    /// a page it has already written is a page it will never look at
    /// again, and a record changed down there would sit in memory until
    /// an eviction dropped it and the old bytes came back off the disk.
    /// Recovery is the only thing that changes a record that has
    /// already been written, and this is how it makes that stick.
    pub fn rewrite_page(&self, page: usize, len: usize) -> Result<()> {
        let base = self.page_ptr(page);
        if base.is_null() {
            return Err(Error::Malformed {
                address: page_start(page),
                why: "page left memory before it was written back",
            });
        }
        // SAFETY: the page is resident and len is bounded by the caller
        // to the page size.
        let bytes = unsafe { std::slice::from_raw_parts(base, len) };
        file::write_all_at(&self.file, bytes, page_start(page))?;
        Ok(())
    }

    /// The file's length, which bounds the recovery scan.
    pub fn file_len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Where recovery keeps the link repairs it is part way through
    /// writing. Beside the log rather than inside it, because the log's
    /// header is eight bytes of `begin` with records starting right
    /// after it and there is nowhere in it to put a slot. See
    /// [`crate::recover`].
    pub fn journal_path(&self) -> PathBuf {
        journal_path_beside(&self.path)
    }

    /// Where the checkpoint of the two planes lives, and the name it is
    /// written under before it is renamed into place. Beside the log for
    /// the same reason the relink journal is: there is nowhere in the
    /// log to put it. See [`crate::checkpoint`].
    pub fn checkpoint_path(&self) -> (PathBuf, PathBuf) {
        checkpoint_path_beside(&self.path)
    }

    /// Commits the log file's bytes to the device outside the flusher's
    /// frontier, which recovery needs after it has written repaired
    /// pages back.
    pub fn sync_file(&self) -> Result<()> {
        file::sync(&self.file)?;
        Ok(())
    }

    fn wake_flusher(&self) {
        let mut pending = self.dirty_lock.lock().expect("zu2 dirty flag");
        *pending = true;
        drop(pending);
        self.dirty.notify_one();
    }

    /// Makes sure the file owns the blocks the next write is going to
    /// land in, so that `fdatasync` commits data rather than data plus
    /// an inode size change plus an extent allocation.
    ///
    /// Only the range above `upto` is touched. The bytes below it hold
    /// records and the flush this is running ahead of is about to put
    /// them on the device, so the most that could be done for them is
    /// the allocation they are about to pay for anyway.
    ///
    /// Called under the flush mutex, which is what makes the watermark
    /// safe to keep as a plain field: the only other thing that moves it
    /// is [`Log::trim_tail`], and that takes the same mutex.
    fn provision(&self, state: &mut Device, upto: Address) {
        // The trigger is the frontier reaching the reservation rather
        // than the reservation being short of where it would ideally
        // be, so this runs once per chunk and not once per flush.
        if !state.provisions || state.provisioned >= upto {
            return;
        }
        // A commit with a chunk's worth of records behind it is a bulk
        // load, not a commit. Growing the file costs it one inode update
        // for a megabyte of data it was going to write anyway, and
        // writing zeros first would double what it puts on the device.
        // The reservation is for the small durable commit, which is the
        // case where the metadata is most of the cost.
        if upto - state.written >= self.provision_bytes {
            return;
        }
        let from = upto.div_ceil(file::BLOCK) * file::BLOCK;
        let want = (upto + self.provision_bytes).div_ceil(file::BLOCK) * file::BLOCK;
        if self.initialise(from, want).is_ok() {
            state.provisioned = want;
        } else {
            // One refusal is the answer for this filesystem, or for a
            // disk that has run out. Trying again per flush would be a
            // failing syscall on the commit path forever, and a log
            // that grows the file the old way is slower and not wrong.
            state.provisions = false;
        }
    }

    /// Puts real blocks under a range of the file.
    ///
    /// Asking for the space is not enough. `fallocate` hands out
    /// unwritten extents, and the first write to one is still a metadata
    /// change that the sync has to commit, which is why the fallocated
    /// shape in the appendcost example sits with the growing file rather
    /// than with the file that was filled: 104 durable writes a second
    /// against 1207 on server2. So the range is written, with zeros,
    /// and synced once. Zeros because that is what page padding already
    /// is and what a hole already reads as, so recovery treats the
    /// unused part of a reservation as the end of the log without
    /// knowing anything about reservations.
    ///
    /// The cost is one megabyte written and one extra sync per megabyte
    /// of log, which is about a quarter of one small durable commit
    /// spread over the two hundred and fifty six that fit in it.
    fn initialise(&self, from: u64, want: u64) -> Result<()> {
        if want <= from {
            return Ok(());
        }
        // Still worth asking: it moves the file's size in one call and
        // gives the filesystem the whole range to find contiguous
        // extents for, rather than making it guess from a stream of
        // writes. A refusal is not an error, the write below is what
        // actually does the work.
        file::preallocate(&self.file, from, want - from);
        let zeros = vec![0u8; (want - from) as usize];
        file::write_all_at(&self.file, &zeros, from)?;
        file::sync(&self.file)?;
        Ok(())
    }

    /// Gives back whatever was provisioned above the written frontier.
    ///
    /// The reservation is a write-path optimisation and not part of what
    /// the database holds, so anything that reports or closes the file
    /// drops it first. Truncating rather than punching because it is the
    /// same call everywhere and because a shorter file is also a shorter
    /// recovery scan.
    pub fn trim_tail(&self) -> Result<()> {
        let mut state = self.device.lock().expect("zu2 device state");
        let frontier = state.written.max(self.tail());
        let keep = frontier.div_ceil(file::BLOCK) * file::BLOCK;
        if state.provisioned <= keep {
            return Ok(());
        }
        file::truncate(&self.file, keep)?;
        state.provisioned = keep;
        Ok(())
    }

    /// Writes and syncs everything below `upto`, which the caller has
    /// already established is quiescent.
    ///
    /// `committing` says whether somebody is waiting for this. Only a
    /// commit provisions, because only a commit is paying the metadata
    /// cost with a thread that is standing still.
    fn write_and_sync(&self, state: &mut Device, upto: Address, committing: bool) -> Result<()> {
        if committing {
            self.provision(state, upto);
        }
        while state.written < upto {
            let page = page_of(state.written);
            let page_end = page_start(page + 1).min(upto);
            let from = state.written;
            let base = self.page_ptr(page);
            if base.is_null() {
                return Err(Error::Malformed {
                    address: from,
                    why: "page left memory before it was flushed",
                });
            }
            // SAFETY: the range is inside a resident page, and the wait
            // for the write frontier means every record in it is
            // complete.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    base.add((from - page_start(page)) as usize),
                    (page_end - from) as usize,
                )
            };
            file::write_all_at(&self.file, bytes, from)?;
            state.written = page_end;
        }
        file::sync(&self.file)?;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.flushed.fetch_max(upto, Ordering::AcqRel);
        Ok(())
    }

    /// One pass of the flusher: claim the device, snapshot, wait for the
    /// writers below the snapshot to finish, write, sync, publish.
    ///
    /// A pass that finds a commit already at the device gives up rather
    /// than queueing behind it. The commit is taking the same range and
    /// the flusher has nothing to add, and a flusher standing in the
    /// group's way would only make the next pass later.
    pub fn flush_once(&self) -> Result<()> {
        let upto = self.tail();
        if upto <= self.flushed() {
            return Ok(());
        }
        {
            let mut state = self.flushing.lock().expect("zu2 flush state");
            if state.syncing || upto <= self.flushed() {
                return Ok(());
            }
            state.syncing = true;
        }
        let outcome = self.sync_range(upto, false);
        self.release_device();
        outcome
    }

    /// The device write itself, done by whichever thread holds the
    /// claim. Everything below `target` goes to the file and the file
    /// goes to the device, and `flushed` moves last.
    ///
    /// The target is published before the wait so that a session
    /// starting later refuses to update anything below it in place, and
    /// the wait then covers the sessions that were already running. Both
    /// halves are needed: without the target a later writer could edit
    /// bytes this thread is about to read, and without the wait an
    /// earlier one could still be mid-record.
    fn sync_range(&self, target: Address, committing: bool) -> Result<()> {
        self.flush_target.fetch_max(target, Ordering::SeqCst);
        self.wait_for_writers(target);
        let mut device = self.device.lock().expect("zu2 device state");
        self.write_and_sync(&mut device, target, committing)
    }

    /// Hands the device back and tells everyone waiting on it, whether
    /// the write worked or not. A leader that failed still has to wake
    /// its followers, or they wait for a group that is never coming.
    fn release_device(&self) {
        let mut state = self.flushing.lock().expect("zu2 flush state");
        state.syncing = false;
        drop(state);
        self.synced.notify_all();
    }

    /// Whether the log has been told to shut down.
    pub fn stopping(&self) -> bool {
        self.flushing.lock().expect("zu2 flush state").stopping
    }

    /// Sleeps until there is something to flush, or a millisecond,
    /// whichever comes first. The timeout is what keeps a log that took
    /// its last write during a flush from sitting unflushed.
    pub fn wait_for_work(&self) {
        let mut pending = self.dirty_lock.lock().expect("zu2 dirty flag");
        if !*pending {
            let (guard, _) = self
                .dirty
                .wait_timeout(pending, std::time::Duration::from_millis(1))
                .expect("zu2 dirty wait");
            pending = guard;
        }
        *pending = false;
    }

    /// Makes everything below `upto` durable according to `mode`.
    ///
    /// One device write per group, not per commit. A thread that finds
    /// its record already durable is done, a thread that finds nobody at
    /// the device claims it, and a thread that finds somebody there
    /// waits for them to publish and then asks again. The leader takes
    /// everything appended so far and not just its own record, so the
    /// threads that were waiting behind it usually wake up durable and
    /// return without touching the device at all.
    pub fn make_durable(&self, upto: Address, mode: Durability) -> Result<()> {
        if mode == Durability::Async {
            return Ok(());
        }
        self.commits.fetch_add(1, Ordering::Relaxed);
        if self.flushed() >= upto {
            return Ok(());
        }
        let mut state = self.flushing.lock().expect("zu2 flush state");
        loop {
            if self.flushed() >= upto || state.stopping {
                // Either a leader took this record to the device while
                // this thread was getting here, which is the whole
                // arrangement working, or the log is closing and the
                // last pass will take it.
                return Ok(());
            }
            if !state.syncing {
                break;
            }
            state = self.synced.wait(state).expect("zu2 sync wait");
        }
        state.syncing = true;
        drop(state);
        // Everything appended so far rather than just this record. The
        // device write costs the same either way, and the threads that
        // queued while the last group was at the device are already
        // inside this range, so they will find themselves durable when
        // this one publishes.
        let target = self.tail().max(upto);
        let outcome = self.sync_range(target, true);
        self.release_device();
        outcome
    }

    /// Stops the flusher after one last pass.
    pub fn stop(&self) {
        self.flushing.lock().expect("zu2 flush state").stopping = true;
        self.wake_flusher();
    }
}

impl Drop for Log {
    fn drop(&mut self) {
        // Nothing is running by the time a log drops, so the deferred
        // frees can retire unconditionally and the resident pages go
        // back directly.
        self.epochs.retire_all();
        for chunk in &self.chunks {
            let base = chunk.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if base.is_null() {
                continue;
            }
            for i in 0..CHUNK {
                // SAFETY: a non-null chunk holds CHUNK initialised
                // slots and nothing is running at drop time.
                let page = unsafe { &*base.add(i) }.swap(std::ptr::null_mut(), Ordering::AcqRel);
                if !page.is_null() {
                    // SAFETY: made by ensure_page or by remap_settled,
                    // the tag says which, and no session can exist at
                    // drop time.
                    unsafe {
                        if is_mapped(page) {
                            crate::file::unmap(page_base(page), PAGE_SIZE);
                        } else {
                            dealloc(page, page_layout());
                        }
                    }
                }
            }
            // SAFETY: allocated by ensure_slot with this layout.
            unsafe { dealloc(base.cast::<u8>(), chunk_layout()) };
        }
    }
}

// SAFETY: every field is either atomic, behind a mutex, or immutable
// after construction. The raw page pointers are only dereferenced under
// epoch protection, which is what keeps them alive.
unsafe impl Send for Log {}
// SAFETY: as above.
unsafe impl Sync for Log {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::PAGE_SIZE;
    use crate::epoch::Slotted;
    use crate::record::RecordRef;
    use std::sync::atomic::AtomicBool;

    fn log(memory_pages: usize) -> (tempfile::TempDir, Log) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("z.log");
        let file = file::create_new(&path).expect("create");
        let log = Log::new(
            file,
            &path,
            4096,
            2,
            memory_pages,
            8,
            PROVISION_CHUNK,
            false,
        );
        (dir, log)
    }

    #[test]
    fn a_record_never_straddles_a_page() {
        let (_dir, log) = log(usize::MAX);
        let session = Slotted::claim(&log.epochs).expect("slot");
        let value = vec![0u8; 4000];
        let mut last = 0;
        // 4032 bytes a record over 4 MiB pages, so this crosses four
        // page boundaries and lands the tail in the fifth.
        for i in 0..5000u32 {
            let a = log
                .append(
                    &session,
                    last,
                    u64::from(i) + 1,
                    &i.to_be_bytes(),
                    &value,
                    false,
                    record::KIND_VALUE,
                )
                .expect("append");
            let size = record::size_of(4, value.len());
            assert_eq!(
                page_of(a),
                page_of(a + size as u64 - 1),
                "record at {a} straddles a page"
            );
            last = a;
        }
        assert!(page_of(log.tail()) >= 4, "did not cross enough pages");
    }

    #[test]
    fn the_read_only_boundary_trails_the_tail_by_the_mutable_window() {
        let (_dir, log) = log(usize::MAX);
        let session = Slotted::claim(&log.epochs).expect("slot");
        let value = vec![7u8; 8192];
        for i in 0..2000u32 {
            log.append(
                &session,
                0,
                u64::from(i) + 1,
                &i.to_be_bytes(),
                &value,
                false,
                record::KIND_VALUE,
            )
            .expect("append");
        }
        let tail_page = page_of(log.tail());
        assert!(tail_page >= 3, "need several pages for the window");
        assert_eq!(
            page_of(log.read_only()),
            tail_page - 2,
            "the mutable window is two pages"
        );
    }

    /// A window as wide as the cap is a log that fills once and then
    /// cannot be compacted: compaction may only take what is below the
    /// boundary, opening a page puts the boundary that many pages back,
    /// and the cap forbids the tail from getting further than that from
    /// the floor, so the boundary never rises above the floor and a pass
    /// is never allowed to see anything. That deadlocked rather than
    /// said so, and #584 is the clamp that makes it impossible to ask
    /// for.
    #[test]
    fn a_mutable_window_leaves_room_for_a_compaction_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("w.zu2");
        for max_pages in 1..8usize {
            for asked in 0..8usize {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .expect("open");
                let log = Log::new(file, &path, max_pages, asked, 8, 8, PROVISION_CHUNK, false);
                assert!(
                    log.mutable_pages + 2 <= max_pages.max(2),
                    "a cap of {max_pages} pages took a window of {} from an asked for {asked}",
                    log.mutable_pages
                );
                assert!(
                    log.mutable_pages <= asked,
                    "the window widened from {asked} to {}",
                    log.mutable_pages
                );
            }
        }
    }

    #[test]
    fn an_evicted_record_comes_back_off_disk_with_its_bytes() {
        let (_dir, log) = log(3);
        let session = Slotted::claim(&log.epochs).expect("slot");
        let value = vec![0xABu8; 8192];
        let mut first = 0;
        // A page holds 509 of these, and nothing is evicted until the
        // tail is memory_pages past the head, so this has to reach the
        // fifth page before the first one can go.
        for i in 0..3000u32 {
            let a = log
                .append(
                    &session,
                    0,
                    u64::from(i) + 1,
                    &i.to_be_bytes(),
                    &value,
                    false,
                    record::KIND_VALUE,
                )
                .expect("append");
            if i == 0 {
                first = a;
            }
            // The flusher is not running in this test, so drive the
            // durability the eviction path insists on by hand.
            session.protect();
            session.unprotect();
            log.flush_once().expect("flush");
            log.opened_page(page_of(log.tail()));
        }
        assert!(log.head() > first, "nothing was evicted");
        assert!(log.resident(first).is_null(), "the first page is still in");
        let mut scratch = Vec::new();
        log.load(first, &mut scratch).expect("load");
        // SAFETY: the buffer holds the whole record and is aligned.
        let r = unsafe { RecordRef::new(scratch.as_ptr().cast()) };
        assert_eq!(r.key(), 0u32.to_be_bytes());
        assert_eq!(r.version(), 1);
        assert!(r.intact(), "the record did not survive the round trip");
    }

    /// Records of every size around the speculation boundary come back
    /// off disk with their bytes.
    ///
    /// The test above uses an 8 KiB value, which is always longer than
    /// the speculation and so only ever walks the second read. This walks
    /// the other side: a value shorter than the speculation, one that
    /// makes the record land within a few bytes of it either way, and one
    /// longer, all in the same log so they are read back through the same
    /// path. The sizes are picked off [`crate::cold::SPECULATE`] rather
    /// than written out, so moving that constant moves the test with it
    /// instead of quietly leaving it testing the wrong boundary.
    #[test]
    fn an_evicted_record_of_any_size_comes_back_with_its_bytes() {
        let (_dir, log) = log(3);
        let session = Slotted::claim(&log.epochs).expect("slot");
        // The record is the value plus a header and a four byte key, so
        // these put its size below, astride and above the speculation.
        let speculate = crate::cold::SPECULATE;
        let sizes = [
            1usize,
            64,
            speculate - record::HEADER - 4 - 1,
            speculate - record::HEADER - 4,
            speculate - record::HEADER - 4 + 1,
            speculate,
            8192,
        ];
        let mut wrote = Vec::new();
        for (i, size) in (0u32..).zip(sizes) {
            // A distinct byte per record, so a read that returned the
            // wrong record's bytes fails on the contents and not only on
            // the key.
            let value = vec![(i as u8).wrapping_add(1); size];
            let at = log
                .append(
                    &session,
                    0,
                    u64::from(i) + 1,
                    &i.to_be_bytes(),
                    &value,
                    false,
                    record::KIND_VALUE,
                )
                .expect("append");
            wrote.push((at, value));
            session.protect();
            session.unprotect();
            log.flush_once().expect("flush");
            log.opened_page(page_of(log.tail()));
        }
        // Push the tail far enough past them that every one is out of the
        // resident window and the only copy is the file.
        let filler = vec![0xCDu8; 8192];
        for i in 1000u32..4000 {
            log.append(
                &session,
                0,
                u64::from(i) + 1,
                &i.to_be_bytes(),
                &filler,
                false,
                record::KIND_VALUE,
            )
            .expect("append");
            session.protect();
            session.unprotect();
            log.flush_once().expect("flush");
            log.opened_page(page_of(log.tail()));
        }

        let mut scratch = Vec::new();
        for (i, (at, value)) in (0u32..).zip(&wrote) {
            assert!(
                log.resident(*at).is_null(),
                "record {i} is still resident, so this read never reached the file"
            );
            log.load(*at, &mut scratch).expect("load");
            // SAFETY: `load` sized the buffer to hold the whole record
            // and it is a Vec<u64>, so it is aligned.
            let r = unsafe { RecordRef::new(scratch.as_ptr().cast()) };
            assert!(r.intact(), "record {i} did not survive the round trip");
            assert_eq!(r.key(), i.to_be_bytes(), "record {i} came back as another");
            // SAFETY: the buffer holds the whole record, so the value
            // bytes its header names are inside it.
            let bytes = unsafe { r.value_unchecked() };
            assert_eq!(bytes, &value[..], "record {i} came back with wrong bytes");
        }
    }

    /// The point of the write frontier, stated as a test that hangs
    /// rather than fails if it regresses: the reader here does not stand
    /// down until the commit has already returned, so a commit that
    /// waited for the epoch to turn over would never return at all.
    /// `memory_pages` is a promise about memory and not about the page
    /// table. Taking a page out of the table and leaving its four
    /// megabytes queued behind an epoch that never moves keeps the
    /// promise on paper and breaks it everywhere it matters.
    #[test]
    fn an_evicted_page_gives_its_memory_back() {
        let (_dir, log) = log(3);
        let session = Slotted::claim(&log.epochs).expect("slot");
        let value = vec![0xABu8; 8192];
        for i in 0..3000u32 {
            log.append(
                &session,
                0,
                u64::from(i) + 1,
                &i.to_be_bytes(),
                &value,
                false,
                record::KIND_VALUE,
            )
            .expect("append");
            session.protect();
            session.unprotect();
            log.flush_once().expect("flush");
            log.opened_page(page_of(log.tail()));
        }
        let resident = log.resident_pages();
        assert!(page_of(log.head()) > 0, "nothing was evicted");
        assert_eq!(
            log.epochs.pending(),
            0,
            "{} pages left the table and {resident} are still in it, but their memory was never freed",
            page_of(log.head())
        );
        assert!(
            resident <= 4,
            "the page table holds {resident} pages against a memory budget of 3"
        );
    }

    #[test]
    fn a_durable_commit_does_not_wait_for_a_reader() {
        let (_dir, log) = log(usize::MAX);
        let holding = AtomicBool::new(false);
        let release = AtomicBool::new(false);
        let mut end = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let reader = Slotted::claim(&log.epochs).expect("slot");
                reader.protect();
                holding.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                reader.unprotect();
            });
            while !holding.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            let session = Slotted::claim(&log.epochs).expect("slot");
            let a = log
                .append(&session, 0, 1, b"k", b"v", false, record::KIND_VALUE)
                .expect("append");
            end = a + record::size_of(1, 1) as u64;
            log.make_durable(end, Durability::Durable).expect("durable");
            release.store(true, Ordering::Release);
        });
        assert!(log.flushed() >= end, "the commit did not reach the device");
    }

    #[test]
    fn a_record_larger_than_a_page_is_refused_rather_than_split() {
        let (_dir, log) = log(usize::MAX);
        let session = Slotted::claim(&log.epochs).expect("slot");
        let value = vec![0u8; PAGE_SIZE];
        let error = log
            .append(&session, 0, 1, b"k", &value, false, record::KIND_VALUE)
            .expect_err("too big");
        assert!(matches!(error, Error::RecordTooLarge { .. }), "{error}");
    }
}
