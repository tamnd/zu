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

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use crate::addr::{Address, FIRST, NULL, PAGE_SIZE, page_of, page_start};
use crate::epoch::Slotted;
use crate::error::Result;
use crate::graph::Graph;
use crate::index::{self, Bucket, EMPTY, Index, SLOTS};
use crate::log::{self, Durability, Log};
use crate::record::{self, RecordRef};
use crate::{compact, file, recover};

/// How a database is sized and how durable it is.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// What a commit waits for.
    pub durability: Durability,
    /// Buckets in the hash index, rounded up to a power of two. Eight
    /// entries per bucket, and the table should be under half full, so
    /// records / 4 is a reasonable hint.
    pub index_buckets: usize,
    /// Slots in the page table, which caps the log at
    /// `max_pages * 4 MiB`.
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            durability: Durability::Durable,
            index_buckets: 1 << 16,
            max_pages: 1 << 16,
            mutable_pages: 4,
            memory_pages: usize::MAX,
            sessions: 128,
            max_nodes: 1 << 26,
            compact_below: 128 << 20,
            provision_bytes: log::PROVISION_CHUNK,
            space_target_percent: 200,
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
}

impl Core {
    /// A session on a core held behind an `Arc`, which is what the
    /// background thread needs and what [`Db::session`] is built on.
    pub(crate) fn session(&self) -> Session<'_> {
        Session {
            core: self,
            slot: Slotted::new(&self.log.epochs),
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
        Ok(Self::start(Self::assemble(handle, path, options), options))
    }

    /// Opens a database and replays its log into the index.
    pub fn open(path: &Path, options: Options) -> Result<Self> {
        let handle = file::open_rw(path)?;
        let core = Self::assemble(handle, path, options);
        // Before the replay, because it is what says where the replay
        // starts: a compacted file has a hole where its first pages were.
        let begin = core.log.read_begin()?;
        core.log.resume_begin(begin);
        recover::replay(&core)?;
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
            index: Index::new(options.index_buckets),
            graph: Graph::new(options.max_nodes),
            version: AtomicU64::new(0),
            durability: options.durability,
            compact_at: AtomicU64::new(options.compact_below),
            compaction: Compaction::default(),
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
    pub fn session(&self) -> Session<'_> {
        self.core.session()
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
        let mut session = self.core.session();
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

    /// Entries in use, for reporting the load factor a run happened at.
    pub fn index_occupancy(&self) -> usize {
        self.core.index.occupancy()
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
            debug_assert!(false, "zu2 compactor: {error}");
            return;
        }
        core.log.wait_for_work();
    }
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
    let mut session = core.session();
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

impl Session<'_> {
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

    /// Walks a chain looking for `key`, comparing keys at every step
    /// because a chain can hold records of other keys.
    ///
    /// The walk stops at the log's begin address, not at [`NULL`].
    /// Everything below begin has been compacted away, and a chain only
    /// ever points backwards, so a record still live down there would
    /// have been copied to the tail before begin passed it. Reading the
    /// floor once per walk rather than per step is deliberate: begin
    /// only rises, so a stale floor costs at worst a step into a page of
    /// zeros, which ends the walk anyway.
    fn chain_find(&mut self, mut address: Address, key: &[u8]) -> Result<Option<Address>> {
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
            address = found;
        }
        Ok(None)
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
            if let Some(address) = self.chain_find(index::address_of(entry), key)? {
                return Ok(Some((i, address)));
            }
        }
        Ok(None)
    }

    /// Reads the newest value for `key` into `out`.
    pub fn read(&mut self, key: &[u8], out: &mut Vec<u8>) -> Result<bool> {
        let hash = index::hash(key);
        let bucket = self.core.index.bucket(hash);
        let tag = Index::tag(hash);
        self.slot.protect();
        let answer = self.read_protected(bucket, tag, key, out);
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

    /// Removes `key`. Returns whether it was there.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let hash = index::hash(key);
        let bucket = self.core.index.bucket(hash);
        let tag = Index::tag(hash);
        self.slot.protect();
        let existed = self.lookup(bucket, tag, key).map(|f| f.is_some());
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
        let bucket = self.core.index.bucket(hash);
        let tag = Index::tag(hash);
        self.slot.protect();
        let found = self.lookup(bucket, tag, key);
        let mut current = Vec::new();
        let outcome = (|| -> Result<Option<Address>> {
            let found = found?;
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
        let bucket = self.core.index.bucket(hash);
        let tag = Index::tag(hash);
        self.slot.protect();
        let outcome = self.install(bucket, tag, key, value, tombstone, kind);
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
                if let Some(address) = self.chain_find(index::address_of(entry), key)? {
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
        let bucket = self.core.index.bucket(hash);
        let tag = Index::tag(hash);
        self.slot.protect();
        let outcome = (|| -> Result<bool> {
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
                    if let Some(address) = self.chain_find(index::address_of(entry), key)? {
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
    /// insert is the one that finds it full.
    fn one_bucket() -> Options {
        Options {
            durability: Durability::Async,
            index_buckets: 1,
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
        let bucket = core.index.bucket(index::hash(key));
        let mut session = core.session();
        let mut found = Vec::new();
        for i in 0..SLOTS {
            let entry = bucket.slots[i].load(Ordering::Acquire);
            if entry == EMPTY || index::is_tentative(entry) {
                continue;
            }
            if let Some(address) = session
                .chain_find(index::address_of(entry), key)
                .expect("chain find")
            {
                let base = session.locate(address).expect("locate");
                // SAFETY: locate returns a whole record and nothing is
                // writing to this database by the time this runs.
                found.push((i, unsafe { RecordRef::new(base).version() }));
            }
        }
        found
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
        let bucket = db.core.index.bucket(index::hash(&spare));
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
