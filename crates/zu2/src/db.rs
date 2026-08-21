//! The record plane's public surface: a database, a session per
//! worker, and four operations.
//!
//! A session is held for the life of a worker, not taken per call. It
//! owns an epoch slot and the scratch buffer the disk read path uses,
//! so an operation on a warm database allocates nothing.
//!
//! Every mutating operation is the same three steps: append the record,
//! swing the index entry with a compare and swap, make the append
//! durable according to the configured mode. The append happens before
//! the swap so that publishing the address publishes a record that is
//! already whole; a swap that loses simply retries, and the record it
//! wrote becomes unreachable bytes that compaction reclaims later.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use crate::addr::{Address, FIRST, NULL, PAGE_SIZE, page_of, page_start};
use crate::epoch::Slotted;
use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::index::{self, Bucket, Claim, EMPTY, Index, Migration, SLOTS};
use crate::log::{self, Durability, Log};
use crate::record::{self, RecordRef};
use crate::{compact, file, recover};

/// Records a split reads out of one chain before it gives up on
/// splitting that bucket by key and carries it over whole.
///
/// The walk is what turns displaced keys back into keys with entries,
/// and it is bounded because the chain it walks holds every version of
/// every key in the bucket that compaction has not reclaimed yet, not
/// just the live ones. A hot key rewritten ten thousand times between
/// two compaction passes would otherwise make the doubling pay for all
/// ten thousand.
const SPLIT_WALK_LIMIT: usize = 1024;

/// The record a split has settled on for one key.
#[derive(Clone, Copy)]
struct Placed {
    address: Address,
    version: u64,
}

/// How a database is sized and how durable it is.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// What a commit waits for.
    pub durability: Durability,
    /// Buckets in the hash index, rounded up to a power of two. Eight
    /// entries per bucket, and the table should be under half full, so
    /// records / 4 is a reasonable hint.
    pub index_buckets: usize,
    /// Whether the index doubles when it passes half full. On, because a
    /// hint is a hint and a database that outgrows one should not start
    /// paying a log dereference per lookup for it.
    ///
    /// Off pins the table at `index_buckets` however many keys arrive,
    /// which is what a measurement of what crowding costs needs, and
    /// what a caller who knows its key count exactly can use to keep the
    /// pointer indirection off its read path.
    pub grow_index: bool,
    /// The live span the log may reach, in 4 MiB pages, counted from the
    /// compaction floor to the tail. A database that compacts hard
    /// enough to keep its span under this never reaches it, however long
    /// it runs and however much it writes: what is bounded is what the
    /// database is holding and not what it has written.
    ///
    /// That distinction is #470 and it used to go the other way. The
    /// page table was flat and indexed by absolute page, so the ceiling
    /// landed on the highest address the log would ever reach, which is
    /// the sum of every append. One megabyte of live data died
    /// permanently after eighty three megabytes of writes.
    ///
    /// The remaining ceiling is [`crate::addr::MAX_PAGES`], 256 TiB of
    /// appends, which is the 48 bits an index entry has for an address.
    pub max_pages: usize,
    /// Pages above the read-only boundary, which is the window an
    /// update can happen in place in.
    pub mutable_pages: usize,
    /// Pages kept in memory. `usize::MAX` never evicts.
    pub memory_pages: usize,
    /// Concurrent sessions the epoch table has room for.
    pub sessions: usize,
    /// Nodes the graph plane is sized for. Only the array of chunk
    /// pointers is allocated up front, one pointer per 16384 nodes
    /// per direction, so this is cheap to set high.
    pub max_nodes: usize,
    /// The log span below which compaction does not bother. Zero turns
    /// compaction off, which is what a load that is going to be measured
    /// and thrown away wants.
    ///
    /// Space is not free but neither is reclaiming it, and a database
    /// smaller than this is not worth a scan. The default is a hundred
    /// and twenty eight megabytes, which is thirty two pages.
    pub compact_below: u64,
    /// How far past the write frontier the file's blocks are reserved,
    /// so a durable write lands in space the file already owns. Zero
    /// turns the reservation off, which is what a measurement of what
    /// the reservation is worth wants.
    pub provision_bytes: u64,
    /// How many bytes of log to keep per byte of live data, as a
    /// percentage.
    ///
    /// This is the whole space against write amplification trade and it
    /// is one number. At 200 the log settles at about twice the live set
    /// and compaction rewrites roughly one byte for every byte the
    /// workload writes; at 400 the file is twice as big and the rewriting
    /// is a third of that. The default of 200 is the same rule a copying
    /// collector uses, and for the same reason.
    pub space_target_percent: u32,
    /// Opens a file with a hole in it anyway, taking the prefix below
    /// the hole and discarding what is above.
    ///
    /// Off by default, because a hole means records that were
    /// acknowledged and made durable are gone and a database that says
    /// nothing about that is worse than one that will not open. Turning
    /// it on is a decision to lose them, which is the right decision
    /// when the alternative is losing the prefix as well, and it is the
    /// operator's to make rather than the library's. See
    /// [`Error::LogHole`](crate::Error::LogHole).
    pub salvage: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            durability: Durability::Durable,
            index_buckets: 1 << 16,
            grow_index: true,
            max_pages: 1 << 16,
            mutable_pages: 4,
            memory_pages: usize::MAX,
            sessions: 128,
            max_nodes: 1 << 26,
            compact_below: 128 << 20,
            provision_bytes: log::PROVISION_CHUNK,
            space_target_percent: 200,
            salvage: false,
        }
    }
}

/// What compaction has done to a database since it was opened.
#[derive(Debug, Default)]
pub struct Compaction {
    /// Passes run.
    pub passes: AtomicU64,
    /// Bytes of log read.
    pub scanned: AtomicU64,
    /// Bytes written back at the tail.
    pub copied: AtomicU64,
    /// Bytes the filesystem took back.
    pub reclaimed: AtomicU64,
}

impl Compaction {
    fn note(&self, pass: &crate::compact::Compacted) {
        self.passes.fetch_add(1, Ordering::Relaxed);
        self.scanned.fetch_add(pass.scanned, Ordering::Relaxed);
        self.copied.fetch_add(pass.copied, Ordering::Relaxed);
        self.reclaimed.fetch_add(pass.reclaimed, Ordering::Relaxed);
    }
}

/// What the reopen's scan found and what it had to change.
///
/// The last two are what #463 is about. A repaired link is eight bytes
/// of `previous` and four bytes of checksum twenty four bytes later, and
/// the page carrying both goes back to the file whole, so a crash in the
/// middle of that write can leave a record whose checksum does not hold
/// and end the durable prefix there. How big a window that is depends
/// entirely on how much a reopen repairs, which is why it is counted.
#[derive(Debug, Default)]
pub struct Recovered {
    /// Records the scan read.
    pub records: AtomicU64,
    /// Records whose `previous` did not fit the table being filled and
    /// had to be rewritten.
    pub relinked: AtomicU64,
    /// Pages that went back to the file because of those rewrites.
    pub pages: AtomicU64,
    /// Bytes above a hole that `Options::salvage` threw away, and zero
    /// on every open that did not have to throw anything away. A
    /// salvaged database is a short database and this is how short.
    pub discarded: AtomicU64,
}

/// Everything the sessions and the flusher share.
pub struct Core {
    pub(crate) log: Log,
    pub(crate) index: Index,
    pub(crate) graph: Graph,
    version: AtomicU64,
    pub(crate) durability: Durability,
    /// The log span at which the next compaction pass starts. Raised
    /// after every pass to the live set times the space target, so a
    /// database that stops being rewritten stops being compacted.
    compact_at: AtomicU64,
    compaction: Compaction,
    pub(crate) recovered: Recovered,
}

impl Core {
    /// A session on a core held behind an `Arc`, which is what the
    /// background thread needs and what [`Db::session`] is built on.
    ///
    /// Panics when the host has as many sessions open as it asked for,
    /// because a caller that sized `Options::sessions` for its workers
    /// and then opened one more has a bug in it and not a full table.
    /// [`Core::try_session`] is the same thing for a caller that cannot
    /// say that, which in practice means the C API.
    pub(crate) fn session(&self) -> Session<'_> {
        self.try_session().expect("zu2 session")
    }

    /// A session, or [`Error::NoSessions`] when the host already has as
    /// many open as `Options::sessions` gave it room for.
    pub(crate) fn try_session(&self) -> Result<Session<'_>> {
        let Some(slot) = Slotted::claim(&self.log.epochs) else {
            return Err(Error::NoSessions {
                max: self.log.epochs.sessions(),
            });
        };
        Ok(self.wrap(slot))
    }

    /// A session for the engine's own flushing and compaction, out of
    /// slots the host cannot take. See [`crate::epoch`].
    pub(crate) fn maintenance_session(&self) -> Result<Session<'_>> {
        let Some(slot) = Slotted::reserved(&self.log.epochs) else {
            return Err(Error::NoSessions {
                max: self.log.epochs.sessions(),
            });
        };
        Ok(self.wrap(slot))
    }

    fn wrap<'a>(&'a self, slot: Slotted<'a>) -> Session<'a> {
        Session {
            core: self,
            slot,
            durability: self.durability,
            scratch: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn next_version(&self) -> u64 {
        self.version.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The newest version handed out, which is what a snapshot read
    /// would compare against.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub(crate) fn set_version(&self, version: u64) {
        self.version.store(version, Ordering::Release);
    }

    /// The adjacency. Shared with the record plane rather than sitting
    /// beside it, so an edge and a property change are one transaction
    /// on one log.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// The epoch table, which both planes reclaim through.
    pub(crate) fn epochs(&self) -> &crate::epoch::Epochs {
        &self.log.epochs
    }
}

pub struct Db {
    core: Arc<Core>,
    flusher: Option<JoinHandle<()>>,
}

impl Db {
    /// Creates a database, failing if the file is already there.
    pub fn create(path: &Path, options: Options) -> Result<Self> {
        let handle = file::create_new(path)?;
        let core = Self::assemble(handle, path, options);
        // A file that never compacts never writes its marker word
        // otherwise, and a marker of zeros is how a file written before
        // pad records existed is recognised. Without this a brand new
        // database would be read back under that older and more
        // forgiving rule, which is the rule #472 is about.
        core.log.stamp()?;
        Ok(Self::start(core, options))
    }

    /// Opens a database and replays its log into the index.
    pub fn open(path: &Path, options: Options) -> Result<Self> {
        let handle = file::open_rw(path)?;
        let core = Self::assemble(handle, path, options);
        // Before the replay, because it is what says where the replay
        // starts: a compacted file has a hole where its first pages were.
        let begin = core.log.read_begin()?;
        core.log.resume_begin(begin);
        // And what rule to read it under. An older file has no pad
        // records in it, so a zero header there is padding and not
        // damage, and reading it strictly would refuse most of a log
        // that is perfectly good (#472).
        core.log.adopt_format(core.log.read_format()?);
        // Before the replay rather than during it. A file longer than
        // the options left room for is a sizing mistake and not a
        // corrupt log, so it gets the number that would open it. The
        // replay would otherwise reach the first page past the ceiling
        // and report a full log, which is the same words for a
        // different problem and no help at all (#470).
        core.log.fits_the_file()?;
        recover::replay(&core, options.salvage)?;
        Ok(Self::start(core, options))
    }

    /// Creates the file if it is missing and replays it if it is not,
    /// which is what a benchmark harness wants.
    pub fn open_or_create(path: &Path, options: Options) -> Result<Self> {
        if path.exists() {
            Self::open(path, options)
        } else {
            Self::create(path, options)
        }
    }

    fn assemble(handle: std::fs::File, path: &Path, options: Options) -> Arc<Core> {
        Arc::new(Core {
            log: Log::new(
                handle,
                path,
                options.max_pages,
                options.mutable_pages,
                options.memory_pages,
                options.sessions,
                options.provision_bytes,
            ),
            index: Index::new(options.index_buckets, !options.grow_index),
            graph: Graph::new(options.max_nodes),
            version: AtomicU64::new(0),
            durability: options.durability,
            compact_at: AtomicU64::new(options.compact_below),
            compaction: Compaction::default(),
            recovered: Recovered::default(),
        })
    }

    fn start(core: Arc<Core>, options: Options) -> Self {
        // Async never waits on the flusher, but it still wants one:
        // without it the log would grow in memory forever and eviction
        // would have nothing durable to evict.
        let background = Arc::clone(&core);
        let flusher = std::thread::Builder::new()
            .name("zu2-flush".into())
            .spawn(move || maintain(&background, options))
            .expect("zu2 flusher thread");
        Self {
            core,
            flusher: Some(flusher),
        }
    }

    /// A worker's handle. Hold one per thread for the whole run.
    ///
    /// Panics when `Options::sessions` has run out. A session is a
    /// worker's for the length of the run, so the count is something the
    /// host knows before it starts and running out is a bug in the host.
    /// [`Db::try_session`] is for a host that cannot panic, which in
    /// practice means one on the other side of the C API.
    pub fn session(&self) -> Session<'_> {
        self.core.session()
    }

    /// A worker's handle, or [`Error::NoSessions`].
    pub fn try_session(&self) -> Result<Session<'_>> {
        self.core.try_session()
    }

    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }

    /// Addresses the log has spent, which is what it would cost on disk
    /// if nothing had ever been compacted away.
    pub fn log_bytes(&self) -> u64 {
        self.core.log.tail()
    }

    /// Addresses the log still spans, tail minus begin.
    pub fn log_span(&self) -> u64 {
        self.core.log.span()
    }

    /// Log pages holding memory right now, each 4 MiB. The memory side
    /// of what [`Db::disk_bytes`] answers for the filesystem.
    pub fn resident_pages(&self) -> usize {
        self.core.log.resident_pages()
    }

    /// Bytes the file occupies on the device, holes excluded. This is
    /// the honest storage number: a compacted log keeps its addresses
    /// but not its blocks.
    ///
    /// The blocks the write path has provisioned past the tail go back
    /// first. They are a reservation the log makes so that a durable
    /// commit does not have to allocate on its way to the device, and
    /// counting them would report the database as costing up to a
    /// megabyte more than it holds. The next commit provisions again.
    pub fn disk_bytes(&self) -> Result<u64> {
        self.core.log.trim_tail()?;
        self.core.log.disk_bytes()
    }

    /// Pushes everything appended so far to the device and waits for it.
    ///
    /// A session in [`Durability::Async`] does not wait for anything, so
    /// after a load there is a tail the file does not have yet. This is
    /// how a caller gets it there: a loader that is done, or anything
    /// about to measure what the database costs on disk.
    pub fn sync(&self) -> Result<()> {
        let tail = self.core.log.tail();
        self.core.log.make_durable(tail, Durability::Durable)
    }

    /// Device writes since this database was opened, which against the
    /// commits that asked for one says how far group commit is
    /// grouping.
    pub fn syncs(&self) -> u64 {
        self.core.log.syncs()
    }

    /// Durable commits since this database was opened. Most of them do
    /// no device write of their own, or that is the intent.
    pub fn commits(&self) -> u64 {
        self.core.log.commits()
    }

    /// What compaction has done since this database was opened.
    pub fn compaction(&self) -> &Compaction {
        &self.core.compaction
    }

    /// What the reopen's scan read and what it had to rewrite. All zeros
    /// on a database that was created rather than opened.
    pub fn recovered(&self) -> &Recovered {
        &self.core.recovered
    }

    /// Runs compaction until another pass would not pay for itself, and
    /// returns the bytes the filesystem took back.
    ///
    /// The background thread does this on its own schedule. This is for
    /// a caller that wants the space now: a loader that has finished, or
    /// a benchmark about to measure the file.
    ///
    /// Nothing above the tail as it stands when this is called belongs to
    /// this call. It is either a writer's record or one of the loop's own
    /// copies, and a pass that reaches into the copies reads the live set
    /// a second time and writes it a third. So the ceiling is clamped
    /// there, the region only ever shrinks from the bottom, and the loop
    /// ends when `begin` has walked up to meet it.
    ///
    /// That is also what stops it running forever. The clamp used to be a
    /// test on the pass instead: a pass that found everything it read
    /// still live was taken as done, because its copies were then the
    /// oldest thing in the log and the next pass would copy them again.
    /// It terminated, and it cost. Any pass that straddled the join
    /// between the original records and the copies had one dead record in
    /// it, so the test did not fire and the loop went round again, and on
    /// a 381 MiB log with a 266 MiB live set it took twenty one passes
    /// and spent 5793 MiB of addresses to save 115 MiB. It also stopped
    /// early on the other side, because a first pass over an all live
    /// region ended the loop with the dead records above it untouched.
    pub fn compact(&self) -> Result<u64> {
        let mut session = self.core.maintenance_session()?;
        let mut reclaimed = 0;
        let started_at = page_start(page_of(self.core.log.tail()));
        loop {
            let upto = compact::ceiling(&session).min(started_at);
            let pass = compact::compact(&mut session, upto)?;
            self.core.compaction.note(&pass);
            reclaimed += pass.reclaimed;
            if pass.scanned == 0 {
                return Ok(reclaimed);
            }
        }
    }

    /// Slots in use, for reporting the load factor a run happened at.
    ///
    /// Slots and not keys, since a displaced key shares the slot of the
    /// one that took its place. [`Db::index_foreign`] is how many slots
    /// that is true of.
    ///
    /// Under an epoch because a doubling retires the table it grew out
    /// of, and a caller counting slots is walking one.
    pub fn index_occupancy(&self) -> usize {
        self.counted(|index| index.occupancy())
    }

    /// Slots naming a chain of more than one key, which is the crowding
    /// a lookup pays for rather than the crowding the load factor
    /// implies.
    pub fn index_foreign(&self) -> usize {
        self.counted(|index| index.foreign())
    }

    fn counted(&self, count: impl Fn(&Index) -> usize) -> usize {
        let Ok(session) = self.core.maintenance_session() else {
            return 0;
        };
        session.slot.protect();
        let n = count(&self.core.index);
        session.slot.unprotect();
        n
    }

    /// Buckets in the index as it stands, which is the size it was
    /// opened with doubled once per [`Db::index_grows`].
    pub fn index_buckets(&self) -> usize {
        let Ok(session) = self.core.maintenance_session() else {
            return 0;
        };
        session.slot.protect();
        let buckets = self.core.index.buckets();
        session.slot.unprotect();
        buckets
    }

    /// Times the index has doubled since the database was opened. A run
    /// that reports more than a couple of these was sized well short of
    /// what it loaded.
    pub fn index_grows(&self) -> u64 {
        self.core.index.grows()
    }

    /// Whether a doubling is in flight right now. A caller that wants a
    /// count of anything in the index wants this to be false first, or it
    /// is counting a table halfway through being replaced.
    ///
    /// No epoch here, unlike its neighbours: this reads the pointer and
    /// does not make a reference out of it.
    pub fn index_resizing(&self) -> bool {
        self.core.index.resizing()
    }
}

/// The background thread: flush, then compact if the log has outgrown
/// its budget, then sleep until there is more to do.
///
/// One thread does both because they are the same job seen twice. A
/// flush is what makes a page eligible to be compacted, and a compaction
/// is what makes the pages a flush has written worth keeping.
fn maintain(core: &Core, options: Options) {
    loop {
        let stopping = core.log.stopping();
        if let Err(error) = core.log.flush_once() {
            // A flush that cannot proceed is not something a background
            // thread can resolve. Committers waiting on durability find
            // out because `flushed` stops moving, and the next
            // foreground operation reports it.
            debug_assert!(false, "zu2 flusher: {error}");
            return;
        }
        if stopping {
            return;
        }
        if options.compact_below > 0
            && core.log.span() >= core.compact_at.load(Ordering::Acquire)
            && let Err(error) = compact_slice(core, options)
        {
            // Every slot in use is a pass this thread does not get to
            // make, not a reason to stop making them. The reserved slots
            // mean it takes a host holding all of its own sessions and a
            // foreground compaction at the same time, and the next pass
            // round the loop will find one.
            if !matches!(error, Error::NoSessions { .. }) {
                debug_assert!(false, "zu2 compactor: {error}");
                return;
            }
        }
        if let Err(error) = resize_index(core)
            && !matches!(error, Error::NoSessions { .. })
        {
            debug_assert!(false, "zu2 index: {error}");
            return;
        }
        core.log.wait_for_work();
    }
}

/// Doubles the index when it has passed the load factor it grows at,
/// and finishes off a doubling that the traffic left half done.
///
/// The grow happens here rather than on the write path that noticed,
/// because it waits for every operation that is running and a writer
/// that waited for its peers would be waiting inside its own epoch. This
/// thread holds none, which is what makes the wait terminate.
///
/// Finishing is not just tidying. A migration holds the whole old table
/// until its last bucket is drained, and a key nobody touches never
/// drains itself, so a table that doubled once against a workload with a
/// cold half would hold that half forever and never be allowed to double
/// again.
fn resize_index(core: &Core) -> Result<()> {
    if core.index.wants_growth() {
        core.index.grow(core.epochs());
    }
    if !core.index.resizing() {
        return Ok(());
    }
    let mut session = core.maintenance_session()?;
    session.drain_index()?;
    // Outside the epoch this time, so the table the last drain retired
    // can actually go back.
    core.epochs().drain();
    Ok(())
}

/// One compaction pass plus the decision about when the next one runs.
///
/// The next threshold comes from what this pass found. A quarter of the
/// old log that was `density` live says the whole log is about that live,
/// so the target span is that estimate times the space target. A pass
/// that found everything live raises the threshold above the current
/// span, which is what stops a database nobody is rewriting from being
/// scanned over and over for nothing.
fn compact_slice(core: &Core, options: Options) -> Result<()> {
    let mut session = core.maintenance_session()?;
    let upto = compact::slice(&session);
    let pass = compact::compact(&mut session, upto)?;
    core.compaction.note(&pass);
    if pass.scanned == 0 {
        // Nothing was compactable, which happens when the whole log is
        // still inside the mutable window. Try again a page later
        // rather than on every flush.
        core.compact_at
            .fetch_max(core.log.span() + PAGE_SIZE as u64, Ordering::AcqRel);
        return Ok(());
    }
    let live = (core.log.span() as u128 * pass.copied as u128 / pass.scanned as u128) as u64;
    let target = live.saturating_mul(u64::from(options.space_target_percent)) / 100;
    core.compact_at
        .store(target.max(options.compact_below), Ordering::Release);
    Ok(())
}

impl Drop for Db {
    fn drop(&mut self) {
        self.core.log.stop();
        if let Some(handle) = self.flusher.take() {
            let _ = handle.join();
        }
        // After the last flush, so the frontier it trims back to is the
        // final one. A file that closes carrying its reservation would
        // report a size nobody wrote and hand the next run a scan over
        // blocks with nothing in them.
        let _ = self.core.log.trim_tail();
    }
}

/// One worker's view of the database.
pub struct Session<'a> {
    pub(crate) core: &'a Core,
    pub(crate) slot: Slotted<'a>,
    /// How far this session waits before it acknowledges a write. It
    /// starts at what the options asked for and a caller can change it,
    /// which is the same arrangement as sqlite's `synchronous`: the
    /// setting belongs to the connection, not to the file. A loader that
    /// can rebuild its input has no reason to wait for the device while
    /// the queries that follow it on the same database do.
    durability: Durability,
    /// 8 byte aligned buffer for records that have left memory.
    scratch: Vec<u64>,
}

impl<'a> Session<'a> {
    /// What this session is attached to. The graph plane reaches the
    /// log and the adjacency through here.
    pub fn core_ref(&self) -> &Core {
        self.core
    }

    /// Changes how far this session waits before acknowledging a write.
    /// Writes already acknowledged keep the guarantee they were given.
    pub fn set_durability(&mut self, durability: Durability) {
        self.durability = durability;
    }

    /// What this session waits for today.
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Appends a record of a kind other than a value, which today means
    /// an edge change, and returns the address just past it. Nothing
    /// goes in the hash index, because the record is not keyed: it is
    /// replayed into the graph instead.
    pub(crate) fn append_untracked(&mut self, kind: u32, payload: &[u8]) -> Result<Address> {
        let version = self.core.next_version();
        let address = self
            .core
            .log
            .append(&self.slot, NULL, version, &[], payload, false, kind)?;
        Ok(address + record::size_of(0, payload.len()) as u64)
    }

    /// Waits for the configured durability on everything up to `end`.
    pub(crate) fn make_durable(&self, end: Address) -> Result<()> {
        self.core.log.make_durable(end, self.durability)
    }

    /// Points at a record, using the scratch buffer when the page has
    /// been evicted.
    ///
    /// The returned pointer is good until the next call on this session
    /// or the end of the epoch, whichever comes first.
    fn locate(&mut self, address: Address) -> Result<*const u8> {
        let resident = self.core.log.resident(address);
        if !resident.is_null() {
            return Ok(resident);
        }
        self.core.log.load(address, &mut self.scratch)?;
        Ok(self.scratch.as_ptr().cast())
    }

    /// What an entry has to say about `key`: the address of its newest
    /// record, or nothing.
    ///
    /// The foreign bit is what says how far to look. An entry that has
    /// it walks its whole chain, comparing keys at every step, because
    /// displacement put keys under it that have nowhere else to be
    /// found. An entry without it answers for the key at its head record
    /// and for nothing else, and the records below that head are the
    /// same key's older versions as far as this entry is concerned.
    ///
    /// That is not the same as saying the chain holds one key. A split
    /// names a record out of a chain it is taking apart, and the records
    /// under that one stay where they were, so a placed entry very often
    /// has other keys below it. Those keys got entries of their own from
    /// the same split, which is why this may stop at the head and why it
    /// has to: walking on would let a lookup answer for a key out of an
    /// entry that is not the key's own, and the write path swings the
    /// entry it found the key through. Two keys with the same fourteen
    /// bit tag in one bucket was enough to make an update to one of them
    /// take the other's entry over and bury it, and the next split then
    /// read the cleared foreign bit as licence to stop at the head and
    /// never saw the buried key again (#466).
    ///
    /// The walk stops at the log's begin address, not at [`NULL`].
    /// Everything below begin has been compacted away, and a chain only
    /// ever points backwards, so a record still live down there would
    /// have been copied to the tail before begin passed it. Reading the
    /// floor once per walk rather than per step is deliberate: begin
    /// only rises, so a stale floor costs at worst a step into a page of
    /// zeros, which ends the walk anyway.
    fn chain_find(&mut self, entry: u64, key: &[u8]) -> Result<Option<Address>> {
        let mut address = index::address_of(entry);
        let foreign = index::is_foreign(entry);
        let floor = self.core.log.begin();
        while address >= floor && address != NULL {
            let base = self.locate(address)?;
            // SAFETY: locate returns a whole record, 8 byte aligned,
            // valid until the next call, and nothing below moves on
            // before it is done with this one.
            let found = unsafe {
                let r = RecordRef::new(base);
                if r.key() == key {
                    return Ok(Some(address));
                }
                r.previous()
            };
            if !foreign {
                return Ok(None);
            }
            address = found;
        }
        Ok(None)
    }

    /// The bucket `hash` belongs in, with the old table drained into it
    /// first when the index is doubling.
    ///
    /// The epoch has to be held before this is called, and this is what
    /// every operation calls first. Both of those matter. The epoch is
    /// what keeps the table alive under the reference this hands back,
    /// and being first is what makes it safe for this to stand the epoch
    /// down and take it again: the operation has read nothing yet, so
    /// there is nothing for it to have to reread.
    fn bucket_of(&mut self, hash: u64) -> Result<&'a Bucket> {
        let core = self.core;
        loop {
            // The live table is read before the migration and not after.
            // A grow publishes the migration first, so a caller that saw
            // the new table sees the migration too, and a caller that
            // sees no migration is holding a table nothing is draining.
            let live = core.index.live();
            let Some(migration) = core.index.pending() else {
                return Ok(live.bucket(hash));
            };
            if !migration.open() {
                // A grow has published a table it has not started
                // filling, and is waiting for the operations that were
                // already running before it lets anyone fill it. This
                // one might be one of those, so it stands down rather
                // than spinning inside the epoch the grower is waiting
                // on, which would be a wait on itself.
                self.slot.unprotect();
                std::hint::spin_loop();
                self.slot.protect();
                continue;
            }
            self.drain_bucket(migration, migration.source(hash))?;
            // Not `live`: this may have been the operation that saw the
            // old table, and after the drain the new one is the answer.
            let filled = core.index.live();
            match core.index.pending() {
                // The same doubling is still running, so the table it is
                // filling is the live one and this key's bucket in it has
                // just been drained.
                Some(pending) if std::ptr::eq(pending, migration) => {
                    return Ok(filled.bucket(hash));
                }
                // It finished, and nothing has started since.
                None => return Ok(filled.bucket(hash)),
                // It finished and the next one started, which is what a
                // table several doublings behind its key set does: the
                // drain that ends one doubling is what lets the next one
                // begin, and this operation may be the one that ended it.
                // The bucket this was about to hand back is in the table
                // that is now being drained, and a write into it lands
                // where the split has already been.
                Some(_) => continue,
            }
        }
    }

    /// Splits whatever of the old table the traffic has not, one bucket
    /// per epoch so a reader never waits on the whole of it.
    pub(crate) fn drain_index(&mut self) -> Result<()> {
        let mut from = 0;
        loop {
            self.slot.protect();
            let more = (|| -> Result<bool> {
                let Some(migration) = self.core.index.pending() else {
                    return Ok(false);
                };
                if !migration.open() {
                    return Ok(false);
                }
                let Some(source) = migration.unfinished(from) else {
                    return Ok(false);
                };
                from = source + 1;
                self.drain_bucket(migration, source)?;
                Ok(true)
            })();
            self.slot.unprotect();
            if !more? {
                return Ok(());
            }
        }
    }

    /// Makes sure one old bucket has been split into the two new ones it
    /// feeds, doing it here if nobody else has.
    fn drain_bucket(&mut self, migration: &Migration, source: usize) -> Result<()> {
        match migration.claim(source) {
            Claim::Done => return Ok(()),
            Claim::Busy => {
                migration.wait(source);
                return Ok(());
            }
            Claim::Mine => {}
        }
        match self.split_bucket(migration, source) {
            Ok(()) => {
                if migration.finish(source) {
                    self.core.index.retire(self.core.epochs(), migration);
                }
                Ok(())
            }
            Err(error) => {
                // The claim comes off and the bucket goes back on the
                // list. Nothing was written, so a later caller sees the
                // bucket exactly as this one found it.
                migration.release(source);
                Err(error)
            }
        }
    }

    /// Splits one old bucket into the two new ones.
    ///
    /// The caller owns the bucket, and owning it means owning both of
    /// its destinations too, because no other hash reaches them. So the
    /// installs are plain stores into slots nobody else can be looking
    /// at until the bucket is published as done.
    ///
    /// Every read happens before every write. A log read can fail, an
    /// entry can have left memory and have to come back off the device,
    /// and a half split bucket is not something a retry could sort out:
    /// the entries already installed have no mark on them saying so, and
    /// installing them twice would put one key in the index twice. So
    /// the whole answer is worked out first and written after, which
    /// leaves a failure looking like nothing happened.
    ///
    /// The bit the split turns on is a low bit of the hash and the tag
    /// is the top fourteen, so an entry cannot say which side it goes to
    /// and the split has to read the log. A chain with one key in it
    /// costs one dereference. A chain with several, which is what the
    /// foreign bit marks, is walked, and the keys in it come out with an
    /// entry each rather than staying displaced. That is the whole point
    /// of doing it this way: a bucket that was crowded enough to
    /// displace is exactly the bucket a doubling is supposed to fix, and
    /// carrying the chain over whole would leave it crowded in two
    /// places instead of one.
    ///
    /// Placed entries are not foreign, and that is a statement about the
    /// entry rather than about the chain: the records under a placed one
    /// stay where they were, other keys and all, and what the cleared
    /// bit says is that this entry answers for the key at its head and
    /// for nothing else. Every key that was under it got an entry of its
    /// own from this same pass, so nothing is left only reachable
    /// through somebody else's head. [`Session::chain_find`] is the
    /// other half of that and #466 is what it cost when the two halves
    /// disagreed.
    fn split_bucket(&mut self, migration: &Migration, source: usize) -> Result<()> {
        let old = migration.old().at(source);
        let floor = self.core.log.begin();
        let mut entries = Vec::with_capacity(SLOTS);
        // Key to the record for it this bucket is going to name.
        let mut keys: HashMap<Vec<u8>, Placed> = HashMap::new();
        // Key, address and version of one chain, head first. Kept across
        // the eight slots so a doubling allocates per bucket rather than
        // per chain.
        let mut chain: Vec<(Vec<u8>, Address, u64)> = Vec::new();
        let mut whole = true;
        for i in 0..SLOTS {
            let entry = old.slots[i].load(Ordering::Acquire);
            // A tentative claim cannot be here. It is set and cleared
            // inside one protected region, and the grower waited for
            // every one of those to end before it opened the migration.
            if entry == EMPTY || index::is_tentative(entry) {
                continue;
            }
            let address = index::address_of(entry);
            if address < floor {
                // Compaction has passed this entry by, so a lookup
                // reaching it stops at the floor and reads nothing. It
                // is already invisible and carrying it over would only
                // take a slot in the new table.
                continue;
            }
            entries.push(entry);
            if !whole {
                // The bucket is going over as it stands, so the rest of
                // the reads would only be to fill a list nothing will
                // look at. The entries still have to be collected, which
                // is why this carries on round the loop.
                continue;
            }
            chain.clear();
            let mut at = address;
            while at >= floor && at != NULL {
                if chain.len() == SPLIT_WALK_LIMIT {
                    // A chain long enough to cost more than the split is
                    // worth. What has been read so far cannot be used,
                    // because the split only gets to drop a key when it
                    // knows the key is somewhere else, so this bucket
                    // goes over whole instead.
                    whole = false;
                    break;
                }
                let base = self.locate(at)?;
                // SAFETY: locate returns a whole record, valid until the
                // next call on this session, which is why the key is
                // copied rather than borrowed.
                let (key, previous, version) = unsafe {
                    let r = RecordRef::new(base);
                    (r.key().to_vec(), r.previous(), r.version())
                };
                chain.push((key, at, version));
                at = previous;
                if !index::is_foreign(entry) {
                    // This entry answers for its head record only, so
                    // whatever is under it belongs to entries of its
                    // own and this walk will reach it through those.
                    break;
                }
            }
            if !whole {
                continue;
            }
            for (key, address, version) in chain.drain(..) {
                if migration.source(index::hash(&key)) != source {
                    continue;
                }
                let placed = Placed { address, version };
                let held = keys.entry(key).or_insert(placed);
                // A key can be in more than one chain here, an older
                // version of it displaced under somebody else while its
                // own entry names the newest. The version says which is
                // which; the address breaks the tie a compaction copy
                // leaves, and it breaks it towards the copy.
                if (placed.version, placed.address) > (held.version, held.address) {
                    *held = placed;
                }
            }
        }

        // Keys from another bucket are in here whenever an earlier split
        // had to carry a chain over whole, and they are dropped rather
        // than placed: the bucket they belong to has them too, and two
        // buckets naming one key is the one thing the index may not do.
        let mut low = Vec::with_capacity(keys.len());
        let mut high = Vec::with_capacity(keys.len());
        for (key, held) in &keys {
            let hash = index::hash(key);
            let placed = index::entry(Index::tag(hash), held.address, false);
            if migration.low_side(hash) {
                low.push(placed);
            } else {
                high.push(placed);
            }
        }

        let live = self.core.index.live();
        self.fill(live.at(source), &low, &entries, whole);
        self.fill(
            live.at(source + migration.old().len()),
            &high,
            &entries,
            whole,
        );
        Ok(())
    }

    /// Puts one side of a split into its bucket, either as an entry per
    /// key or, when that will not fit, as the old bucket copied over.
    ///
    /// The copy is always correct and never helps. Every key the old
    /// bucket reached is still reachable through it, which is why it is
    /// the fallback, and both sides get one, which is why it leaves the
    /// crowding where it was. It happens when a side has more distinct
    /// keys than a bucket has slots, and the next doubling halves that,
    /// so a table that starts far too small works its way out over a few
    /// passes rather than being stuck.
    fn fill(&self, bucket: &Bucket, placed: &[u64], entries: &[u64], whole: bool) {
        let source = if whole && placed.len() <= SLOTS {
            placed
        } else {
            entries
        };
        for (slot, entry) in source.iter().enumerate() {
            bucket.slots[slot].store(*entry, Ordering::Release);
        }
    }

    /// Finds the newest record for `key`, and which entry named it.
    fn lookup(
        &mut self,
        bucket: &Bucket,
        tag: u64,
        key: &[u8],
    ) -> Result<Option<(usize, Address)>> {
        for i in 0..SLOTS {
            let entry = bucket.slots[i].load(Ordering::Acquire);
            if entry == EMPTY || index::is_tentative(entry) {
                continue;
            }
            if index::tag_of(entry) != tag && !index::is_foreign(entry) {
                continue;
            }
            if let Some(address) = self.chain_find(entry, key)? {
                return Ok(Some((i, address)));
            }
        }
        Ok(None)
    }

    /// Reads the newest value for `key` into `out`.
    pub fn read(&mut self, key: &[u8], out: &mut Vec<u8>) -> Result<bool> {
        let hash = index::hash(key);
        let tag = Index::tag(hash);
        self.slot.protect();
        let answer = self
            .bucket_of(hash)
            .and_then(|bucket| self.read_protected(bucket, tag, key, out));
        self.slot.unprotect();
        answer
    }

    fn read_protected(
        &mut self,
        bucket: &Bucket,
        tag: u64,
        key: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<bool> {
        let Some((_, address)) = self.lookup(bucket, tag, key)? else {
            return Ok(false);
        };
        let base = self.locate(address)?;
        // SAFETY: as in chain_find.
        let live = unsafe {
            let r = RecordRef::new(base);
            if r.tombstone() {
                false
            } else {
                r.read_value(out);
                true
            }
        };
        Ok(live)
    }

    /// Writes `value` under `key`, whether or not it was there.
    pub fn upsert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(key, value, false, record::KIND_VALUE)
    }

    /// Writes every pair, waiting for durability once at the end
    /// rather than once per pair.
    ///
    /// The wait is the cost of a durable write here, not the write: an
    /// append is a memcpy into a mapped page, and the device is what
    /// takes the microseconds. A loader that calls
    /// [`upsert`](Self::upsert) in a loop pays that wait per record for
    /// a guarantee it did not ask for, since what it wants is the batch
    /// on disk, not each record on disk before the next one starts.
    /// Waiting once gives the batch exactly what its last record would
    /// have had alone. What it gives up is per-record acknowledgement
    /// inside the batch, and a caller who wanted that would not have
    /// handed over a batch.
    ///
    /// Returns how many pairs were written, which on an error is where
    /// it stopped. That prefix is in the log and is as durable as this
    /// session asks for, error or not, so a caller can retry from the
    /// count rather than from the start.
    pub fn upsert_many(&mut self, pairs: &[(&[u8], &[u8])]) -> (usize, Result<()>) {
        let mut end = None;
        for (i, &(key, value)) in pairs.iter().enumerate() {
            let hash = index::hash(key);
            let tag = Index::tag(hash);
            self.slot.protect();
            let outcome = self.bucket_of(hash).and_then(|bucket| {
                self.install(bucket, tag, key, value, false, record::KIND_VALUE)
            });
            self.slot.unprotect();
            match outcome {
                // An update that happened inside a record already in the
                // log adds nothing to wait for, so it leaves the address
                // where the last append put it.
                Ok(next) => end = next.or(end),
                Err(error) => return (i, self.finish(end).and(Err(error))),
            }
        }
        (pairs.len(), self.finish(end))
    }

    /// Removes `key`. Returns whether it was there.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let hash = index::hash(key);
        let tag = Index::tag(hash);
        self.slot.protect();
        let existed = self
            .bucket_of(hash)
            .and_then(|bucket| self.lookup(bucket, tag, key))
            .map(|f| f.is_some());
        self.slot.unprotect();
        let existed = existed?;
        self.write(key, &[], true, record::KIND_VALUE)?;
        Ok(existed)
    }

    /// Reads the current value, computes a new one, and writes it back.
    /// The in-place path applies when the new value is the same length
    /// as the old and the record is still in the mutable region, which
    /// is what makes a hot key cost no log growth at all.
    pub fn rmw(
        &mut self,
        key: &[u8],
        scratch: &mut Vec<u8>,
        mut make: impl FnMut(Option<&[u8]>, &mut Vec<u8>),
    ) -> Result<()> {
        let hash = index::hash(key);
        let tag = Index::tag(hash);
        self.slot.protect();
        let entered = self.bucket_of(hash);
        let mut current = Vec::new();
        let outcome = (|| -> Result<Option<Address>> {
            let bucket = entered?;
            let found = self.lookup(bucket, tag, key)?;
            let present = match found {
                Some((_, address)) => {
                    let base = self.locate(address)?;
                    // SAFETY: as in chain_find.
                    unsafe {
                        let r = RecordRef::new(base);
                        if r.tombstone() {
                            false
                        } else {
                            r.read_value(&mut current);
                            true
                        }
                    }
                }
                None => false,
            };
            scratch.clear();
            make(present.then_some(current.as_slice()), scratch);
            self.install(bucket, tag, key, scratch, false, record::KIND_VALUE)
        })();
        self.slot.unprotect();
        let end = outcome?;
        self.finish(end)
    }

    pub(crate) fn write(
        &mut self,
        key: &[u8],
        value: &[u8],
        tombstone: bool,
        kind: u32,
    ) -> Result<()> {
        let hash = index::hash(key);
        let tag = Index::tag(hash);
        self.slot.protect();
        let outcome = self
            .bucket_of(hash)
            .and_then(|bucket| self.install(bucket, tag, key, value, tombstone, kind));
        self.slot.unprotect();
        let end = outcome?;
        self.finish(end)
    }

    fn finish(&self, end: Option<Address>) -> Result<()> {
        match end {
            Some(end) => self.core.log.make_durable(end, self.durability),
            None => Ok(()),
        }
    }

    /// The write path proper. Returns the address just past the record
    /// that has to become durable, or `None` when the update happened
    /// in place inside an already-appended record.
    #[allow(clippy::too_many_arguments)]
    fn install(
        &mut self,
        bucket: &Bucket,
        tag: u64,
        key: &[u8],
        value: &[u8],
        tombstone: bool,
        kind: u32,
    ) -> Result<Option<Address>> {
        let size = record::size_of(key.len(), value.len()) as u64;
        loop {
            let mut empty = None;
            let mut found = None;
            let mut claimed = false;
            for i in 0..SLOTS {
                let entry = bucket.slots[i].load(Ordering::Acquire);
                if entry == EMPTY {
                    if empty.is_none() {
                        empty = Some(i);
                    }
                    continue;
                }
                if index::is_tentative(entry) {
                    claimed = true;
                    continue;
                }
                if index::tag_of(entry) != tag && !index::is_foreign(entry) {
                    continue;
                }
                if let Some(address) = self.chain_find(entry, key)? {
                    found = Some((i, entry, address));
                    break;
                }
            }

            // A scan that walked past a claim did not see the key that
            // claim is for, because a claim names no address yet, so its
            // answer of "not here" is only good if the claim is still
            // there when it is acted on. It usually is not: the claimer
            // finishes, the scan goes on believing the key is missing,
            // and puts a second entry for it in the bucket. Two entries
            // for one key is not a lost record, since both chains hold
            // it, but a lookup answers from the lower slot while a replay
            // answers from the higher version, and those are not always
            // the same record. 258 duplicates in 20000 racing pairs, 6 of
            // them reopening a version behind what memory had just said
            // (#454). Reading the key is fine, since the claim's key was
            // not there when the read started either. Creating an entry
            // is not, so that goes round again.
            if found.is_none() && claimed {
                std::hint::spin_loop();
                continue;
            }

            if let Some((i, entry, address)) = found {
                if !tombstone && let Some(end) = self.update_in_place(address, value) {
                    return Ok(Some(end));
                }
                let version = self.core.next_version();
                // The new record chains to whatever the entry names,
                // not to the record this key was found at. Those are
                // the same address in the ordinary case. They differ
                // when the entry is the head of a chain of several
                // keys, and chaining to the found record there would
                // drop every key above it out of the index. The cost of
                // getting it right is that the chain keeps the stale
                // version too, which compaction is what removes.
                let fresh = self.core.log.append(
                    &self.slot,
                    index::address_of(entry),
                    version,
                    key,
                    value,
                    tombstone,
                    kind,
                )?;
                let replacement = index::entry(tag, fresh, index::is_foreign(entry));
                if bucket.slots[i]
                    .compare_exchange(entry, replacement, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(Some(fresh + size));
                }
                // Someone else swung the entry first. The record just
                // written is unreachable and the retry reads the entry
                // again, which is the only correct thing to do: its
                // previous pointer is now stale.
                continue;
            }

            if let Some(i) = empty {
                // Claim the slot before appending, so that two inserts
                // of the same key into an empty bucket cannot both
                // believe they own it.
                if bucket.slots[i]
                    .compare_exchange(
                        EMPTY,
                        index::tentative(tag),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                if self.claim_lost(bucket, tag, i) {
                    bucket.slots[i].store(EMPTY, Ordering::Release);
                    std::hint::spin_loop();
                    continue;
                }
                let version = self.core.next_version();
                let fresh = match self
                    .core
                    .log
                    .append(&self.slot, NULL, version, key, value, tombstone, kind)
                {
                    Ok(fresh) => fresh,
                    // The claim has to come off on the way out. A slot
                    // left tentative is a slot nothing can ever look
                    // through and nothing can ever reuse, and every
                    // insert for that tag would wait on a claim that is
                    // never going to resolve.
                    Err(error) => {
                        bucket.slots[i].store(EMPTY, Ordering::Release);
                        return Err(error);
                    }
                };
                bucket.slots[i].store(index::entry(tag, fresh, false), Ordering::Release);
                self.core.index.note_key();
                return Ok(Some(fresh + size));
            }

            // The bucket is full, so the new record takes over an entry
            // and adopts what it was holding as its chain.
            //
            let i = tag as usize % SLOTS;
            let entry = bucket.slots[i].load(Ordering::Acquire);
            if index::is_tentative(entry) {
                std::hint::spin_loop();
                continue;
            }
            let version = self.core.next_version();
            let fresh = self.core.log.append(
                &self.slot,
                index::address_of(entry),
                version,
                key,
                value,
                tombstone,
                kind,
            )?;
            let replacement = index::entry(tag, fresh, true);
            if bucket.slots[i]
                .compare_exchange(entry, replacement, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.core.index.note_key();
                return Ok(Some(fresh + size));
            }
        }
    }

    /// Writes a record again at the tail, but only if the copy is of the
    /// version the index still reaches. Returns whether it copied.
    ///
    /// This is the one operation compaction needs and the reason it can
    /// run beside the workload rather than instead of it. The find and
    /// the swing are the same compare and swap the write path uses, so a
    /// key that somebody updates between the scan reading it and this
    /// running loses the race and the old version is simply dropped,
    /// which is the right answer: it was dead by then.
    ///
    /// The copy chains to what the index entry holds rather than to the
    /// record it copies, for the same reason an update does. That is
    /// also what makes the region safe to drop afterwards: the copy is
    /// above the region and everything the entry could reach through it
    /// is still reachable through the copy.
    ///
    /// `version` is the version of the record being copied and not a new
    /// one, and that is the whole of what makes a lost race safe. The
    /// copy is appended before the compare and swap that would make it
    /// the index entry, so a copy that loses stays on the log with
    /// nobody pointing at it, at an address above the newer record that
    /// beat it. A replay in address order would install that copy last
    /// and hand back the value the loser held, which is what #436 was:
    /// every key correct in memory and thousands of them a round or two
    /// behind after a reopen. Carrying the original version means the
    /// replay can tell which of the two records is the newer one without
    /// knowing anything about compaction.
    pub(crate) fn copy_forward(
        &mut self,
        key: &[u8],
        value: &[u8],
        tombstone: bool,
        kind: u32,
        from: Address,
        version: u64,
    ) -> Result<bool> {
        let hash = index::hash(key);
        let tag = Index::tag(hash);
        self.slot.protect();
        let entered = self.bucket_of(hash);
        let outcome = (|| -> Result<bool> {
            let bucket = entered?;
            loop {
                let mut found = None;
                for i in 0..SLOTS {
                    let entry = bucket.slots[i].load(Ordering::Acquire);
                    if entry == EMPTY || index::is_tentative(entry) {
                        continue;
                    }
                    if index::tag_of(entry) != tag && !index::is_foreign(entry) {
                        continue;
                    }
                    if let Some(address) = self.chain_find(entry, key)? {
                        found = Some((i, entry, address));
                        break;
                    }
                }
                let Some((i, entry, address)) = found else {
                    return Ok(false);
                };
                if address != from {
                    // Somebody has written a newer version, so this one
                    // is a version and not the record.
                    return Ok(false);
                }
                let fresh = self.core.log.append(
                    &self.slot,
                    index::address_of(entry),
                    version,
                    key,
                    value,
                    tombstone,
                    kind,
                )?;
                if bucket.slots[i]
                    .compare_exchange(
                        entry,
                        index::entry(tag, fresh, index::is_foreign(entry)),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(true);
                }
            }
        })();
        self.slot.unprotect();
        outcome
    }

    /// Whether another insert for the same tag claimed a lower slot,
    /// in which case this one stands down. Lowest index wins, so two
    /// racing inserts cannot both back off.
    fn claim_lost(&self, bucket: &Bucket, tag: u64, mine: usize) -> bool {
        (0..mine).any(|j| {
            let entry = bucket.slots[j].load(Ordering::Acquire);
            index::is_tentative(entry) && index::tag_of(entry) == tag
        })
    }

    /// Rewrites a value inside an existing record when it fits and the
    /// record is young enough. `None` means the caller should append
    /// instead, which is the answer whenever the value changed length,
    /// the record has already gone read-only, or a flush has claimed
    /// the bytes.
    fn update_in_place(&mut self, address: Address, value: &[u8]) -> Option<Address> {
        if address < self.core.log.in_place_floor() {
            return None;
        }
        let base = self.core.log.resident(address);
        if base.is_null() {
            return None;
        }
        // SAFETY: the record is resident, the epoch is held, and the
        // length is checked before anything is written.
        let end = unsafe {
            let r = RecordRef::new(base);
            if r.tombstone() || r.value_len() != value.len() {
                return None;
            }
            // Claim the bytes, then read the floor a second time. A
            // flush that starts from here on will see the claim and
            // wait; a flush that started before it will already have
            // raised the target, and this reads it and stands down.
            // Ordering is in Epochs::write_floor.
            self.slot.updating_at(address);
            if address < self.core.log.in_place_floor() {
                self.slot.wrote();
                return None;
            }
            let version = self.core.next_version();
            if r.write_value_in_place(value, version) {
                // The bytes are inside a record the flusher has not
                // claimed, so the end of that record is what has to
                // become durable, not a fresh append.
                Some(address + r.size() as u64)
            } else {
                None
            }
        };
        self.slot.wrote();
        end
    }
}

/// Reads the file back into memory pages, which recovery does before it
/// walks the records. Lives here because it needs both the log and the
/// page arithmetic.
pub(crate) fn restore_pages(core: &Core) -> Result<u64> {
    let len = core.log.file_len()?;
    if len <= FIRST {
        return Ok(FIRST);
    }
    let last = page_of(len.saturating_sub(1));
    // From the begin marker's page, not from zero. What is below it is
    // a hole, and restoring a hole would allocate 4 MiB of memory to
    // hold zeros.
    for page in page_of(core.log.begin())..=last {
        let bytes = (len - page_start(page)).min(PAGE_SIZE as u64) as usize;
        core.log.restore_page(page, bytes)?;
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One bucket, so every key in the test lands in it and the eighth
    /// insert is the one that finds it full. Growth off, because a table
    /// that doubles is a table that stops being one bucket, and what
    /// these are about is what happens inside one.
    fn one_bucket() -> Options {
        Options {
            durability: Durability::Async,
            index_buckets: 1,
            grow_index: false,
            max_pages: 8,
            max_nodes: 1 << 10,
            compact_below: 0,
            mutable_pages: 1,
            ..Options::default()
        }
    }

    /// Which slots of a bucket reach `key`, and the version of the
    /// record each of them reaches it at.
    fn reachers(core: &Core, key: &[u8]) -> Vec<(usize, u64)> {
        let bucket = core.index.live().bucket(index::hash(key));
        let mut session = core.session();
        let mut found = Vec::new();
        for i in 0..SLOTS {
            let entry = bucket.slots[i].load(Ordering::Acquire);
            if entry == EMPTY || index::is_tentative(entry) {
                continue;
            }
            if let Some(address) = session.chain_find(entry, key).expect("chain find") {
                let base = session.locate(address).expect("locate");
                // SAFETY: locate returns a whole record and nothing is
                // writing to this database by the time this runs.
                found.push((i, unsafe { RecordRef::new(base).version() }));
            }
        }
        found
    }

    /// Z14. A batch waits once for all of it, where the loop that
    /// writes the same records waits once each.
    ///
    /// Commits and not syncs, because commits is what the caller does
    /// and syncs is what the device sees, and between the two sits the
    /// background flusher: it can take a record to the device before
    /// the thread that wrote it asks, so the device count is a number
    /// this test does not own. Syncs still gets an inequality, since
    /// the point of waiting once is that the device is touched once.
    #[test]
    fn a_batch_waits_for_the_device_once() {
        const N: usize = 64;
        let dir = tempfile::tempdir().expect("tempdir");
        let options = Options {
            durability: Durability::Durable,
            ..Options::default()
        };
        let db = Db::create(&dir.path().join("batch.zu2"), options).expect("create");
        let mut session = db.session();
        for i in 0..N {
            session
                .upsert(format!("one{i}").as_bytes(), b"v")
                .expect("upsert");
        }
        let (loop_commits, loop_syncs) = (db.commits(), db.syncs());
        assert_eq!(loop_commits, N as u64, "one wait per record");
        // A single writer appends the next record only after the last
        // one is durable, so no device write covers two of them and
        // there are at least as many writes as records.
        assert!(
            loop_syncs >= N as u64,
            "{loop_syncs} device writes for {N} records written one at a time"
        );

        let keys: Vec<String> = (0..N).map(|i| format!("many{i}")).collect();
        let pairs: Vec<(&[u8], &[u8])> = keys.iter().map(|k| (k.as_bytes(), &b"v"[..])).collect();
        let (written, outcome) = session.upsert_many(&pairs);
        outcome.expect("batch");
        assert_eq!(written, N);
        assert_eq!(db.commits() - loop_commits, 1, "the batch waits once");
        let batch_syncs = db.syncs() - loop_syncs;
        assert!(
            batch_syncs * 4 <= loop_syncs,
            "{batch_syncs} device writes for a batch of {N} against {loop_syncs} for the loop"
        );

        let mut value = Vec::new();
        for key in &keys {
            assert!(
                session.read(key.as_bytes(), &mut value).expect("read"),
                "{key} went in with the batch"
            );
        }
    }

    /// #486. A slot in use is not a key, and the difference is the
    /// keys a full bucket displaced onto the chain under a slot that
    /// was already taken. Printed against a record count the occupancy
    /// reads as records having gone missing, which is what this pins
    /// apart: fewer slots than keys, foreign says how many slots carry
    /// the rest, and every key is still there.
    #[test]
    fn a_crowded_bucket_holds_more_keys_than_it_has_slots_in_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::create(&dir.path().join("crowded.zu2"), one_bucket()).expect("create");
        const KEYS: usize = 20;
        {
            let mut session = db.session();
            for i in 0..KEYS {
                session
                    .upsert(format!("k{i}").as_bytes(), &[b'x'; 64])
                    .expect("upsert");
            }
        }

        // One bucket of eight, so twelve of the twenty had to displace.
        assert_eq!(db.index_buckets(), 1);
        assert_eq!(db.index_occupancy(), SLOTS);
        assert!(
            db.index_foreign() > 0,
            "a bucket this crowded carries chains of more than one key"
        );
        assert!(db.index_foreign() <= db.index_occupancy());

        // And the count that looked like loss was not: every key reads
        // back its own value through the chain it was displaced onto.
        let mut session = db.session();
        for i in 0..KEYS {
            let key = format!("k{i}");
            let mut value = Vec::new();
            assert!(
                session.read(key.as_bytes(), &mut value).expect("read"),
                "{key} is in a bucket with {} slots in use",
                db.index_occupancy()
            );
            assert_eq!(value, [b'x'; 64]);
        }
    }

    #[test]
    fn a_racing_insert_never_names_one_key_twice_in_a_bucket() {
        // #454. A scan cannot tell which key a tentative claim is for,
        // because a claim names no address, so it walks past one and
        // reports the key missing. Acting on that report after the claim
        // has resolved puts a second entry for the same key in the
        // bucket, and then a lookup answers from the lower slot while a
        // replay answers from the higher version. Before the fix this
        // shape gave 258 duplicates in 20000 pairs and 6 of them left
        // memory a version behind what a reopen would say.
        //
        // Two racers rather than a crowd, because the divergence needs
        // the racing pair to be the last write to the key: with more
        // threads the writes that follow go through the lower slot and
        // put the newest version there, which hides it.
        for trial in 0..2000u64 {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = Arc::new(Db::create(&dir.path().join("r.zu2"), one_bucket()).expect("create"));
            {
                let mut session = db.session();
                // Seven keys, so the bucket has exactly one free slot
                // when the racers arrive and one of them has to take
                // over a slot that is already spoken for.
                for i in 0..7u64 {
                    session
                        .upsert(format!("fill{trial}-{i}").as_bytes(), &[b'x'; 64])
                        .expect("fill");
                }
            }
            let key = format!("hot{trial}");
            std::thread::scope(|scope| {
                for who in 0..2u64 {
                    let db = Arc::clone(&db);
                    let key = key.clone();
                    scope.spawn(move || {
                        let mut session = db.session();
                        session
                            .upsert(key.as_bytes(), &[b'a' + who as u8; 64])
                            .expect("hot");
                    });
                }
            });

            let found = reachers(&db.core, key.as_bytes());
            assert_eq!(
                found.len(),
                1,
                "trial {trial}: slots {found:?} all reach one key, and a lookup would take the \
                 first of them while a replay would take the newest"
            );
        }
    }

    #[test]
    fn an_update_leaves_the_entry_of_another_key_alone() {
        // #466. A split gives every key it finds an entry of its own and
        // clears the foreign bit on all of them, which says that each
        // one answers for the key at its head record and for nothing
        // else. Two of those keys can share a fourteen bit tag, and then
        // a scan looking for the lower one meets the upper one's entry
        // first. Walking through that entry found the key, and the write
        // path swings whatever entry it found the key through, so the
        // update took the other key's slot over and buried it under a
        // head that is not its own. The next split then read the cleared
        // bit as licence to stop at the head and never saw the buried
        // key again, which is a silent loss and the reason the read path
        // stops at the head too.
        //
        // The bucket here is written by hand into the shape a split
        // leaves, because reaching it through a real doubling needs the
        // two keys to survive together to the same split and that is a
        // matter of luck rather than of arrangement.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::create(&dir.path().join("t.zu2"), one_bucket()).expect("create");

        // Two keys with one tag, and eight more with tags of their own
        // to fill the bucket before either of them arrives.
        let mut seen: HashMap<u64, usize> = HashMap::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut pair = None;
        for i in 0..100_000u64 {
            let key = format!("k{i:09}").into_bytes();
            let tag = Index::tag(index::hash(&key));
            if let Some(&first) = seen.get(&tag) {
                pair = Some((keys[first].clone(), key));
                break;
            }
            seen.insert(tag, keys.len());
            keys.push(key);
        }
        let (under, over) = pair.expect("two keys with one tag");
        let tag = Index::tag(index::hash(&under));
        let fill: Vec<Vec<u8>> = keys
            .iter()
            .filter(|key| Index::tag(index::hash(key)) != tag)
            .take(SLOTS)
            .cloned()
            .collect();
        assert_eq!(fill.len(), SLOTS, "not enough keys before the collision");

        let mut session = db.session();
        for key in &fill {
            session.upsert(key, &[b'x'; 64]).expect("fill");
        }
        // Both of the pair arrive at a full bucket, so each takes over
        // slot tag % SLOTS and chains behind what it found there. That
        // puts over on top of under on top of a filler.
        session.upsert(&under, b"under").expect("under");
        session.upsert(&over, b"over").expect("over");

        let bucket = db.core.index.live().bucket(index::hash(&under));
        let crowded = bucket.slots[tag as usize % SLOTS].load(Ordering::Acquire);
        let over_at = session
            .chain_find(crowded, &over)
            .expect("chain find")
            .expect("over is in the chain");
        let under_at = session
            .chain_find(crowded, &under)
            .expect("chain find")
            .expect("under is in the chain");

        // The bucket as a split leaves it: one entry per key, naming
        // that key's newest record, none of them foreign. The fillers go
        // to the other side of the split, which is why their slots go
        // empty. What matters is that over's entry sits below under's,
        // so a scan for under meets over's first.
        for i in 0..SLOTS {
            bucket.slots[i].store(EMPTY, Ordering::Release);
        }
        bucket.slots[0].store(index::entry(tag, over_at, false), Ordering::Release);
        bucket.slots[1].store(index::entry(tag, under_at, false), Ordering::Release);

        // A value of a different length, so the update has to write a
        // record and swing an entry rather than settle in place.
        session.upsert(&under, b"under again").expect("update");
        drop(session);

        assert_eq!(
            index::address_of(bucket.slots[0].load(Ordering::Acquire)),
            over_at,
            "the update took over the entry of the other key with the same tag"
        );
        assert_eq!(
            reachers(&db.core, &over).len(),
            1,
            "over is not reachable through an entry of its own any more"
        );
        assert_eq!(
            reachers(&db.core, &under).len(),
            1,
            "under came out of the update with two entries"
        );

        let mut session = db.session();
        let mut out = Vec::new();
        assert!(session.read(&over, &mut out).expect("read"), "over is gone");
        assert_eq!(out, b"over".to_vec());
        assert!(
            session.read(&under, &mut out).expect("read"),
            "under is gone"
        );
        assert_eq!(out, b"under again".to_vec());
    }

    #[test]
    fn a_failed_append_gives_the_slot_back() {
        // The other half of #454. An append between claiming a slot and
        // storing the entry can fail, and a slot left tentative is one
        // nothing can look through and nothing can reuse, which after
        // the fix above is also one every insert for that tag waits on.
        //
        // Two buckets, a log of one page, and the fill aimed at one of
        // them. The insert that has to fail is then the first one into
        // the other bucket, which is the empty path, which is the path
        // that claims a slot. Aiming by hash rather than by hope,
        // because with the fill in the same bucket the failing append is
        // a take over and takes no claim at all.
        let options = Options {
            index_buckets: 2,
            max_pages: 1,
            ..one_bucket()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::create(&dir.path().join("f.zu2"), options).expect("create");
        let mut session = db.session();
        let mut fill = Vec::new();
        let mut spare = Vec::new();
        for i in 0..100_000u64 {
            let key = format!("k{i:09}").into_bytes();
            let side = index::hash(&key) as usize & 1;
            if side == 0 && fill.len() < PAGE_SIZE / 1000 {
                fill.push(key);
            } else if side == 1 && spare.is_empty() {
                spare.push(key);
            }
        }
        let spare = spare.pop().expect("a key on the other side");
        let mut failed = false;
        for key in &fill {
            if session.upsert(key, &[b'x'; 1000]).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "the log took the whole fill, so nothing failed");
        assert!(
            session.upsert(&spare, &[b'x'; 1000]).is_err(),
            "the log had room after all"
        );
        let bucket = db.core.index.live().bucket(index::hash(&spare));
        for i in 0..SLOTS {
            let entry = bucket.slots[i].load(Ordering::Acquire);
            assert!(
                !index::is_tentative(entry),
                "slot {i} was left claimed by an append that failed"
            );
        }
        // And the slot is usable again once there is room, which is what
        // being left claimed would have cost.
        db.compact().expect("compact");
    }
}
