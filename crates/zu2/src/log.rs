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
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crate::addr::{Address, FIRST, PAGE_SIZE, page_of, page_start};
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
    /// One slot per page, filled lazily. A null slot means the page is
    /// not resident, which is the only test the read path makes: the
    /// head boundary can move while a reader is mid-operation, but the
    /// memory it is looking at cannot go away until its epoch passes.
    pages: Box<[AtomicPtr<u8>]>,
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
    pub fn new(
        file: File,
        path: &Path,
        max_pages: usize,
        mutable_pages: usize,
        memory_pages: usize,
        sessions: usize,
        provision_bytes: u64,
    ) -> Self {
        Self {
            pages: (0..max_pages)
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
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
            mutable_pages: mutable_pages.max(1),
            memory_pages: memory_pages.max(mutable_pages.max(1) + 1),
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

    /// Bytes the file occupies, holes excluded.
    pub fn disk_bytes(&self) -> Result<u64> {
        Ok(file::disk_bytes(&self.file, &self.path)?)
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
        if self.file_len()? < FIRST {
            return Ok(FIRST);
        }
        let mut marker = [0u8; 8];
        file::read_exact_at(&self.file, &mut marker, 0)?;
        Ok(u64::from_le_bytes(marker).max(FIRST))
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
        file::write_all_at(&self.file, &upto.to_le_bytes(), 0)?;
        file::sync(&self.file)?;
        self.begin.store(upto, Ordering::Release);
        self.head.fetch_max(upto, Ordering::AcqRel);
        // The pages go back after the boundary moves, so a session that
        // was already walking a chain either sees the old boundary and
        // real bytes, or the new one and stops before it asks.
        for page in page_of(from)..page_of(upto) {
            let stale = self.pages[page].swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !stale.is_null() {
                let retired = stale as usize;
                self.epochs.defer(Box::new(move || {
                    // SAFETY: the epoch has passed, so no session that
                    // could have loaded this pointer is still running,
                    // and it was allocated with exactly this layout.
                    unsafe { dealloc(retired as *mut u8, page_layout()) };
                }));
            }
        }
        self.retire_pages();
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

    fn page_ptr(&self, page: usize) -> *mut u8 {
        match self.pages.get(page) {
            Some(slot) => slot.load(Ordering::Acquire),
            None => std::ptr::null_mut(),
        }
    }

    /// Makes a page resident, allocating it if this is the first byte
    /// anyone claimed in it. Idempotent, and the common case is one
    /// acquire load.
    fn ensure_page(&self, page: usize) -> Result<*mut u8> {
        if page >= self.pages.len() {
            return Err(Error::LogFull {
                pages: self.pages.len(),
            });
        }
        let existing = self.pages[page].load(Ordering::Acquire);
        if !existing.is_null() {
            return Ok(existing);
        }
        let _guard = self.allocating.lock().expect("zu2 page allocation");
        let existing = self.pages[page].load(Ordering::Acquire);
        if !existing.is_null() {
            return Ok(existing);
        }
        // SAFETY: the layout is non-zero sized and correctly aligned.
        let fresh = unsafe { alloc_zeroed(page_layout()) };
        if fresh.is_null() {
            std::alloc::handle_alloc_error(page_layout());
        }
        self.pages[page].store(fresh, Ordering::Release);
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
    fn allocate(&self, slot: &Slotted, size: usize) -> Result<Address> {
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
            if page_of(start) >= self.pages.len() {
                return Err(Error::LogFull {
                    pages: self.pages.len(),
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
                    self.opened_page(page_of(start));
                }
                return Ok(start);
            }
        }
    }

    /// Called by whichever thread first allocated in a new page. Moves
    /// the read-only boundary up behind the mutable window and evicts
    /// what has fallen out of memory.
    fn opened_page(&self, page: usize) {
        let boundary = page_start(page.saturating_sub(self.mutable_pages));
        self.read_only.fetch_max(boundary, Ordering::AcqRel);
        self.wake_flusher();
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
            self.head.store(page_start(victim + 1), Ordering::Release);
            let stale = self.pages[victim].swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !stale.is_null() {
                // The pointer is retired rather than freed: a reader
                // that loaded it before the swap is still inside its
                // epoch and is entitled to the bytes.
                let retired = stale as usize;
                self.epochs.defer(Box::new(move || {
                    // SAFETY: the epoch has passed, so no session that
                    // could have loaded this pointer is still running,
                    // and it was allocated with exactly this layout.
                    unsafe { dealloc(retired as *mut u8, page_layout()) };
                }));
            }
        }
        self.retire_pages();
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
            let address = self.allocate(slot, size)?;
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
    pub fn load(&self, address: Address, into: &mut Vec<u64>) -> Result<()> {
        let mut header = [0u8; record::HEADER];
        file::read_exact_at(&self.file, &mut header, address)?;
        let lengths = u64::from_le_bytes(header[16..24].try_into().expect("lengths word"));
        let key_len = (lengths as u32 & !record::TOMBSTONE) as usize;
        let value_len = (lengths >> 32) as usize;
        let size = record::size_of(key_len, value_len);
        if size > PAGE_SIZE {
            return Err(Error::Malformed {
                address,
                why: "record longer than a page",
            });
        }
        into.clear();
        into.resize(size.div_ceil(8), 0);
        // SAFETY: the buffer holds size bytes and is 8 byte aligned
        // because it is a Vec<u64>.
        let bytes = unsafe { std::slice::from_raw_parts_mut(into.as_mut_ptr().cast::<u8>(), size) };
        file::read_exact_at(&self.file, bytes, address)?;
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
        let mut path = self.path.clone().into_os_string();
        path.push(".relink");
        PathBuf::from(path)
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
        for slot in &self.pages {
            let page = slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !page.is_null() {
                // SAFETY: allocated by ensure_page with this layout,
                // and no session can exist at drop time.
                unsafe { dealloc(page, page_layout()) };
            }
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
        let log = Log::new(file, &path, 4096, 2, memory_pages, 8, PROVISION_CHUNK);
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
        let resident = log
            .pages
            .iter()
            .filter(|p| !p.load(Ordering::Acquire).is_null())
            .count();
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
