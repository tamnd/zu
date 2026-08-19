//! The write side of one file, shared by every connection in this
//! process that has that file open.
//!
//! docs/08 §1 is one line: single writer, many snapshot readers. Until
//! now the single was per connection rather than per file. A
//! [`Writer`] lived inside the session that opened it, so two
//! connections on one path were two writers, two logs open on one
//! sidecar and two handles each flipping the header from its own idea
//! of where the roots are, and nothing anywhere said no. It worked
//! because callers only ever wrote through one of them.
//!
//! This is the thing that says no. One [`FileHandle`] per path per
//! process holds the write side: the file handle that allocates and
//! the writer that logs. A session takes it to write and gives it back
//! when the statement ends, so writers queue instead of overlapping,
//! which is the writer lock docs/08 asks for and what a group commit
//! has to have before it can batch anything.
//!
//! Reading stays parallel, which is the point of doing it this way
//! round. A session reads through a handle of its own forked from the
//! write side, so it shares the block cache and the decoded pools with
//! every other connection on the file, and it never takes the lock to
//! read. What it takes instead is the header: the write side publishes
//! its roots after every commit, and a session picks them up at the
//! start of a statement, which is where a snapshot begins. That is
//! also what makes a commit by one connection visible to the next
//! statement on another, which before this needed a reconnect.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};

use zu_common::Result;

use crate::write::{Patches, Writer, Written};
use crate::zu1::file::{DatabaseHeader, Shared, Zu1File};
use crate::zu1::txn::WriteTxn;
use crate::zu1::vfs::Vfs;
use crate::zu1::wal::Commits;

/// Replays a sidecar WAL that a previous writer left behind.
///
/// A bulk load commits by writing its segments and appending one frame
/// naming them, and folds those segments into the base afterwards. The
/// commit is durable at the frame and the base is what a query reads,
/// so a crash between the two leaves rows that are on disk and that no
/// statement can see. Folding them here is what closes that window:
/// the open that registers a file pays one existence check, and one
/// that finds a log with something in it puts the rows where a reader
/// looks before the first statement runs. The connections after it pay
/// nothing, because the handle they find is the one that recovered.
///
/// A read-only open cannot fold, and does not pretend to. It reads the
/// base as it stands, which is the state the last fold left, and the
/// next writable open recovers the rest.
fn replay_sidecar(db: &mut Zu1File) -> Result<()> {
    if !db.is_writable() {
        return Ok(());
    }
    let path = crate::append::sidecar(db.path());
    if !db.vfs().exists(&path) {
        return Ok(());
    }
    let mut wal = crate::zu1::wal::Wal::open_in(db.vfs(), &path)?;
    if wal.is_empty() {
        return Ok(());
    }
    let mut mvcc = crate::zu1::fold::recover(db, &mut wal)?;
    crate::zu1::fold::checkpoint_fold(db, &mut mvcc, &mut wal)
}

/// What the write side has published for the readers to pick up.
///
/// The header is where the roots are, and a reader that takes it reads
/// the database the writer has reached, staged folds included: data
/// blocks are on the file as they are written, so the header is the
/// only thing a reader is missing. The patches are the commits that
/// have not been folded at all, which live in memory and are read
/// through rather than read.
#[derive(Clone)]
pub struct Published {
    header: DatabaseHeader,
    slot: usize,
    patches: Arc<Patches>,
    /// Bumped whenever either of the two above moves, so a session
    /// asking whether it is behind compares one word.
    version: u64,
    /// Where this state falls in the order the writers staged them.
    ///
    /// A commit stages its state before it waits for the platter and
    /// installs it after, so two of them can be in the air at once and
    /// they can land out of order. This is what says which is which,
    /// and it is not `version`, which counts installs rather than
    /// stages and so cannot tell a stale state from a fresh one.
    staged: u64,
}

impl Published {
    /// The state a session should be reading, or `None` if it already
    /// has this one.
    pub fn newer_than(&self, seen: u64) -> bool {
        self.version != seen
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    pub fn slot(&self) -> usize {
        self.slot
    }

    pub fn patches(&self) -> &Arc<Patches> {
        &self.patches
    }
}

/// The file handle that allocates and the writer that logs, together,
/// because neither is right without the other: a fold moves roots on
/// one and empties the overlay of the other.
pub struct WriteSide {
    file: Zu1File,
    /// Opened on the first write. Opening it costs a log open and a
    /// recovery pass, which a process that only reads should not pay.
    writer: Option<Writer>,
}

impl WriteSide {
    /// The file the writer allocates and folds into.
    pub fn file(&self) -> &Zu1File {
        &self.file
    }

    pub fn file_mut(&mut self) -> &mut Zu1File {
        &mut self.file
    }

    /// Whether a writer has been opened on this side yet.
    pub fn has_writer(&self) -> bool {
        self.writer.is_some()
    }

    /// Opens the writer if it is not open already, and answers whether
    /// this call is what opened it. Opening recovers and folds whatever
    /// the log holds, which can move the roots, so the caller that
    /// opened one publishes afterwards.
    pub fn open_writer(&mut self) -> Result<bool> {
        if self.writer.is_some() {
            return Ok(false);
        }
        self.writer = Some(Writer::open(&mut self.file)?);
        Ok(true)
    }

    /// Runs one transaction against this file. See [`Writer::write`].
    pub fn write<T>(
        &mut self,
        stage: impl FnOnce(&mut WriteTxn<'_>) -> Result<T>,
    ) -> Result<Written<T>> {
        self.open_writer()?;
        let writer = self.writer.as_mut().expect("opened just above");
        writer.write(&mut self.file, stage)
    }

    /// What makes this side's commits durable, held past the side
    /// itself going back so the wait happens off the queue. `None`
    /// before a writer has been opened, which is a side with nothing to
    /// make durable.
    pub fn commits(&self) -> Option<Arc<Commits>> {
        self.writer.as_ref().map(Writer::commits)
    }

    /// Whether the log owes nothing, so what this side holds is durable
    /// and may be shown to the readers.
    ///
    /// A writer that has staged its frames and not yet waited for the
    /// platter is in this side, and publishing it would let a reader
    /// act on a commit a crash could take back. Whoever owes that sync
    /// publishes it themselves once it lands, so what this says to skip
    /// is never lost, only left to the writer it belongs to.
    pub fn settled(&self) -> bool {
        self.writer.as_ref().is_none_or(Writer::settled)
    }

    /// The epoch the writer has committed through, or the file's own
    /// when there is no writer yet.
    pub fn epoch(&self) -> u64 {
        match &self.writer {
            Some(writer) => writer.epoch(),
            None => self.file.db_header().epoch,
        }
    }

    /// The cells committed since the last fold, which a reader reads
    /// through.
    pub fn patches(&self) -> Arc<Patches> {
        match &self.writer {
            Some(writer) => Arc::clone(writer.patches()),
            None => Arc::new(Patches::new()),
        }
    }

    /// Takes the log back to `floor` and drops the writer, which is
    /// what a rollback owes: the frames above the floor are the ones
    /// being taken away, and a fresh writer would replay them back on
    /// top of the roots going in.
    pub fn discard_above(&mut self, floor: u64) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.discard_above(floor)?;
        }
        self.writer = None;
        Ok(())
    }

    /// Folds what the writer is holding and drops it, which is what a
    /// caller wanting the raw file has to do first: an appender opens
    /// the same sidecar and can truncate it.
    pub fn fold_writer(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.fold(&mut self.file)?;
        }
        Ok(())
    }

    /// Puts what the patch is carrying on the file, keeping the writer.
    ///
    /// A statement that reads the tables itself rather than through a
    /// reader sees only what the file holds, and a deferred commit is
    /// not on the file. Copying a graph is one, and so is anything else
    /// that walks the storage under the query plane. The writer stays
    /// because the statement is still inside a savepoint that will want
    /// it.
    pub fn fold_patches(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.fold(&mut self.file)?;
        }
        Ok(())
    }
}

/// A statement's claim on the epoch it is reading.
///
/// A checkpoint lists as free the blocks the epoch it supersedes
/// reaches, and would hand them straight back to allocation. That is
/// right with one connection, which cannot be reading and writing at
/// once, and wrong with several: a statement on another connection is
/// reading those blocks, and the next write puts a column of somebody
/// else's rows in the middle of them. The lease says which epoch a
/// statement is on, so the write side knows what it may not reuse yet.
///
/// It lives for the statement rather than for the connection, because
/// a connection sitting idle on an old epoch would otherwise hold
/// every block the writer frees after it and grow the file by the
/// churn of everyone else.
pub struct Lease {
    handle: Arc<FileHandle>,
    epoch: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut readers = self.handle.readers.lock().expect("readers");
        if let Some(count) = readers.get_mut(&self.epoch) {
            *count -= 1;
            if *count == 0 {
                readers.remove(&self.epoch);
            }
        }
    }
}

/// One process-wide write side per open file.
///
/// A session takes the side to write and gives it back afterwards, so
/// at most one connection is writing a file at a time and the rest
/// queue in the order they asked. Everything else about a connection,
/// the caches, the plans, the reading handle, stays where it was.
pub struct FileHandle {
    key: Key,
    gate: Mutex<Gate>,
    /// Woken when the side goes back, which is also when a ticket
    /// ahead of the waiters is served.
    freed: Condvar,
    published: RwLock<Published>,
    /// How many statements are reading each epoch. Taken with the
    /// published state in one lock, so a reader is counted before it
    /// can read anything the write side might reclaim.
    readers: Mutex<BTreeMap<u64, usize>>,
    /// The caches every handle on this file reads through, kept here
    /// so a reader that has to open its own descriptor takes them up
    /// rather than starting a second cache nothing invalidates.
    shared: Shared,
    /// Where the file came from, kept for the same reason the key is:
    /// a reader that arrives while somebody is writing opens its own
    /// descriptor, and the path alone does not say what to open it
    /// through when the database is not on disk.
    vfs: Arc<dyn Vfs>,
    /// Hands out the order writers staged their published states in,
    /// so a state that lands after a newer one can be recognized as
    /// stale and dropped. See [`FileHandle::stage`].
    staged: AtomicU64,
}

/// The queue in front of the write side.
///
/// Tickets rather than a bare mutex, because a condvar wakes whoever
/// the operating system feels like and docs/08 §1 asks for a queue in
/// order. A writer draws a number on the way in and waits for it to
/// come up, so a session that has been waiting is served before one
/// that has just arrived.
struct Gate {
    side: Option<WriteSide>,
    next: u64,
    serving: u64,
}

/// What two connections have to agree on to be sharing a write side:
/// the same file, opened the same way. A read-only open holds a
/// descriptor the operating system refuses writes on, so it cannot
/// stand in for a writable one, and a writable one handed to a caller
/// that asked for read-only would break the promise that call made.
type Key = (PathBuf, bool);

fn registry() -> &'static Mutex<HashMap<Key, Weak<FileHandle>>> {
    static OPEN: OnceLock<Mutex<HashMap<Key, Weak<FileHandle>>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The key a file is registered under. The path is canonicalized so
/// that two callers naming one file by different routes find each
/// other, and left as given when it cannot be, which is a path the
/// open would have failed on anyway.
fn key_of(file: &Zu1File) -> Key {
    let path = std::fs::canonicalize(file.path()).unwrap_or_else(|_| file.path().to_path_buf());
    (path, file.is_writable())
}

impl FileHandle {
    /// The handle for the file `open` opens, opening it only if this
    /// process does not already hold one.
    ///
    /// The closure is what keeps the common case honest: a second
    /// connection to a file that is already open costs a lock and a
    /// map lookup rather than an open, a header read and a free list
    /// walk.
    pub fn attach(
        path: &Path,
        read_only: bool,
        open: impl FnOnce() -> Result<Zu1File>,
    ) -> Result<Arc<FileHandle>> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let key = (canonical, !read_only);
        {
            let open_files = registry().lock().expect("registry");
            if let Some(handle) = open_files.get(&key).and_then(Weak::upgrade) {
                return Ok(handle);
            }
        }
        FileHandle::attach_to(open()?)
    }

    /// The handle for a file the caller has already opened.
    ///
    /// The file is dropped when this process holds a handle on it
    /// already, because the one already registered is the one every
    /// other connection is reading through and a second would be a
    /// second writer of the same log.
    pub fn attach_to(mut file: Zu1File) -> Result<Arc<FileHandle>> {
        let key = key_of(&file);
        let mut open_files = registry().lock().expect("registry");
        if let Some(handle) = open_files.get(&key).and_then(Weak::upgrade) {
            drop(file);
            return Ok(handle);
        }
        replay_sidecar(&mut file)?;
        // This handle is the one connections read behind, so the blocks
        // its checkpoints free wait for [`FileHandle::reclaim`] and the
        // count of who is reading what.
        file.defer_reclaim(true);
        let published = Published {
            header: file.db_header().clone(),
            slot: file.active_slot(),
            patches: Arc::new(Patches::new()),
            version: 1,
            staged: 0,
        };
        let shared = file.shared();
        let vfs = Arc::clone(file.vfs());
        let handle = Arc::new(FileHandle {
            key: key.clone(),
            shared,
            vfs,
            gate: Mutex::new(Gate {
                side: Some(WriteSide { file, writer: None }),
                next: 0,
                serving: 0,
            }),
            freed: Condvar::new(),
            published: RwLock::new(published),
            readers: Mutex::new(BTreeMap::new()),
            staged: AtomicU64::new(0),
        });
        open_files.insert(key, Arc::downgrade(&handle));
        Ok(handle)
    }

    /// Takes the write side, waiting for whoever has it. The caller
    /// gives it back with [`Self::put`], and a caller that does not is
    /// a caller nobody else on this file can write past, so the two go
    /// in the same scope.
    pub fn take(&self) -> WriteSide {
        let mut gate = self.gate.lock().expect("write gate");
        let ticket = gate.next;
        gate.next += 1;
        loop {
            if gate.serving == ticket && gate.side.is_some() {
                return gate.side.take().expect("held just above");
            }
            gate = self.freed.wait(gate).expect("write gate");
        }
    }

    /// Gives the write side back and serves the next ticket.
    pub fn put(&self, side: WriteSide) {
        let mut gate = self.gate.lock().expect("write gate");
        gate.side = Some(side);
        gate.serving += 1;
        drop(gate);
        self.freed.notify_all();
    }

    /// A reading handle on this file: its own descriptor, the roots the
    /// write side is at, and the block cache and decoded pools every
    /// other handle on the file is sharing.
    pub fn reader(&self) -> Result<Zu1File> {
        let gate = self.gate.lock().expect("write gate");
        match gate.side.as_ref() {
            Some(side) => side.file.reopen(),
            // Somebody is writing. Forking off their handle would mean
            // waiting for the statement, and what a reader wants is a
            // descriptor and the roots, so it opens its own and takes
            // the roots from the published state.
            None => {
                drop(gate);
                let published = self.published();
                let mut file = match self.key.1 {
                    true => Zu1File::open_in(Arc::clone(&self.vfs), &self.key.0)?,
                    false => Zu1File::open_read_only_in(Arc::clone(&self.vfs), &self.key.0)?,
                };
                file.adopt(&self.shared);
                file.follow(published.header(), published.slot());
                Ok(file)
            }
        }
    }

    /// What the write side last published.
    pub fn published(&self) -> Published {
        self.published.read().expect("published").clone()
    }

    /// The published state, and a lease on the epoch it is at.
    ///
    /// The two come together under one lock, and [`Self::reclaim`]
    /// takes the same one, which is the whole of why a reader never
    /// reads a block that has been handed back. Either a statement is
    /// counted before the reclaim runs, and the reclaim leaves its
    /// epoch alone, or it arrives after, and what it is handed is the
    /// epoch the reclaim published rather than the one it superseded.
    pub fn observe(self: &Arc<Self>) -> (Published, Lease) {
        let mut readers = self.readers.lock().expect("readers");
        let published = self.published.read().expect("published").clone();
        let epoch = published.header().epoch;
        *readers.entry(epoch).or_insert(0) += 1;
        drop(readers);
        (
            published,
            Lease {
                handle: Arc::clone(self),
                epoch,
            },
        )
    }

    /// Gives back for allocation the blocks the last checkpoint freed
    /// that nothing is reading any more.
    ///
    /// Called by the writer after it has published, so a statement
    /// starting from here on reads the epoch that owns those blocks
    /// rather than the one that let go of them.
    pub fn reclaim(&self, side: &mut WriteSide) {
        let readers = self.readers.lock().expect("readers");
        let floor = readers.keys().next().copied().unwrap_or(u64::MAX);
        side.file_mut().release_retained(floor);
    }

    /// Publishes where the write side has got to, for the readers to
    /// pick up at the start of their next statement.
    pub fn publish(&self, side: &WriteSide) {
        let mut published = self.published.write().expect("published");
        published.header = side.file.db_header().clone();
        published.slot = side.file.active_slot();
        published.patches = side.patches();
        published.version += 1;
        published.staged = self.staged.load(Ordering::Relaxed);
    }

    /// What [`Self::publish`] would install, without installing it,
    /// stamped with where it falls in the order writers staged.
    ///
    /// This is the first half of a group commit. The state is taken
    /// while the write side is still held, which is the only moment it
    /// can be read consistently, and put in with [`Self::publish_staged`]
    /// once the log behind it is on the platter. Between the two the
    /// write side is back with the next writer, which is the whole
    /// point: their frames are staged while this one's sync is in the
    /// air, so one sync covers them both.
    pub fn stage(&self, side: &WriteSide) -> Published {
        Published {
            header: side.file.db_header().clone(),
            slot: side.file.active_slot(),
            patches: side.patches(),
            version: 0,
            staged: self.staged.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// Installs a state whose log is now durable.
    ///
    /// A writer behind this one may have staged, synced and published
    /// while this one was waiting, and then this state is stale: what
    /// it describes the newer one describes too, because they are
    /// cumulative. So the older one is dropped rather than put in,
    /// which would take the readers backwards.
    pub fn publish_staged(&self, next: Published) {
        let mut published = self.published.write().expect("published");
        if next.staged <= published.staged {
            return;
        }
        let version = published.version + 1;
        *published = Published { version, ..next };
    }
}

/// Takes a file out of the registry without closing anything, which is
/// how a test is a process that stopped.
///
/// A session left where its drop cannot reach it used to be the whole
/// of that, because everything it held died with it. Now the process
/// holds the file too, and a handle in the registry is a handle the
/// next open finds, so being the crash means letting go of that as
/// well. What is left behind stays open and unreachable, which is what
/// a killed process leaves the operating system to clear up.
#[cfg(test)]
pub fn forget(path: &Path) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut open_files = registry().lock().expect("registry");
    open_files.remove(&(canonical.clone(), true));
    open_files.remove(&(canonical, false));
}

/// A handle that goes away takes its registry entry with it, so a file
/// closed and reopened does not find the state of the handle that is
/// already gone.
///
/// It publishes on the way out, which is the last connection to a file
/// closing it. Nothing would be lost if it did not, because the log
/// holds every commit the folds staged, but a process that only writes
/// would leave a log as long as its life and hand the next open a
/// replay to match. Nothing here can report a failure to do it, which
/// is the reason a caller with something to say about it checkpoints
/// itself before letting go.
impl Drop for FileHandle {
    fn drop(&mut self) {
        if let Some(side) = self.gate.get_mut().expect("write gate").side.as_mut() {
            let _ = side.fold_writer();
        }
        let mut open_files = registry().lock().expect("registry");
        // Only if it is still ours: an entry replaced since is another
        // handle's, and the weak pointer in the map is the way to tell.
        if open_files
            .get(&self.key)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            open_files.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn one_handle_per_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "one.zu1");
        drop(Zu1File::create(&file).expect("create"));

        let a = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");
        let b = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");
        assert!(Arc::ptr_eq(&a, &b), "one file is one write side");
    }

    #[test]
    fn a_read_only_open_is_its_own_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "ro.zu1");
        drop(Zu1File::create(&file).expect("create"));

        let rw = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");
        let ro =
            FileHandle::attach(&file, true, || Zu1File::open_read_only(&file)).expect("attach");
        assert!(
            !Arc::ptr_eq(&rw, &ro),
            "a read-only handle cannot stand in for a writable one"
        );
    }

    #[test]
    fn the_handle_leaves_the_registry_with_the_last_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "gone.zu1");
        drop(Zu1File::create(&file).expect("create"));

        let first = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");
        let key = first.key.clone();
        drop(first);
        assert!(
            !registry().lock().expect("registry").contains_key(&key),
            "a handle nobody holds leaves no entry behind"
        );
        let second = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");
        assert_eq!(second.key, key, "and the next open registers it again");
    }

    #[test]
    fn writers_queue_in_the_order_they_asked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "queue.zu1");
        drop(Zu1File::create(&file).expect("create"));
        let handle = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");

        let held = handle.take();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut waiting = Vec::new();
        for id in 0..4 {
            let handle = Arc::clone(&handle);
            let tx = tx.clone();
            waiting.push(std::thread::spawn(move || {
                let side = handle.take();
                tx.send(id).expect("send");
                handle.put(side);
            }));
            // Each thread has to be in the queue before the next one
            // draws a ticket, or the order under test is the order the
            // threads happened to start in.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        handle.put(held);
        let mut served = Vec::new();
        for _ in 0..4 {
            served.push(rx.recv().expect("recv"));
        }
        for thread in waiting {
            thread.join().expect("join");
        }
        assert_eq!(served, vec![0, 1, 2, 3], "served in the order they asked");
    }

    /// A reader that had to open its own descriptor, because somebody
    /// was writing when it asked for one, reads through the same block
    /// cache as the writer. A cache of its own would hold whatever it
    /// last saw in a block, and the writer can only invalidate the
    /// cache it can reach, so a block freed and written over would read
    /// back as the thing that used to be in it.
    #[test]
    fn a_reader_opened_over_a_writer_shares_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "cache.zu1");
        drop(Zu1File::create(&file).expect("create"));
        let handle = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");

        let mut side = handle.take();
        let block = |fill: u8| vec![fill; crate::zu1::BLOCK_SIZE as usize];
        let watched = side.file_mut().allocate_block();
        let other = side.file_mut().allocate_block();
        side.file_mut()
            .write_block(watched, &block(0xA1))
            .expect("write");
        side.file_mut()
            .write_block(other, &block(0xC3))
            .expect("write");
        side.file_mut().checkpoint().expect("checkpoint");
        handle.publish(&side);

        // The side is out, so this is the branch that opens a
        // descriptor rather than forking the writer's.
        let mut reader = handle.reader().expect("reader");
        assert_eq!(reader.read_block(watched).expect("read"), block(0xA1));
        // The second read is what moves the handle's own memo of the
        // last block off the one under test, so what answers below is
        // the shared cache.
        assert_eq!(reader.read_block(other).expect("read"), block(0xC3));

        side.file_mut()
            .write_block(watched, &block(0xB2))
            .expect("write");
        assert_eq!(
            reader.read_block(watched).expect("read"),
            block(0xB2),
            "the write dropped the frame this reader was reading through"
        );
        handle.put(side);
    }

    /// The floor a reclaim works to is the oldest epoch anything holds
    /// a lease on, and a lease that has been dropped holds nothing.
    #[test]
    fn a_lease_is_what_holds_an_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "lease.zu1");
        drop(Zu1File::create(&file).expect("create"));
        let handle = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");

        let floor = |handle: &FileHandle| {
            let readers = handle.readers.lock().expect("readers");
            readers.keys().next().copied().unwrap_or(u64::MAX)
        };
        assert_eq!(floor(&handle), u64::MAX, "nobody is reading");

        let (published, first) = handle.observe();
        let epoch = published.header().epoch;
        assert_eq!(floor(&handle), epoch);

        // A second statement on the same epoch, so the first one
        // leaving is not the epoch being let go of.
        let (_, second) = handle.observe();
        drop(first);
        assert_eq!(floor(&handle), epoch, "the other one is still reading");
        drop(second);
        assert_eq!(floor(&handle), u64::MAX);
    }

    #[test]
    fn a_reader_follows_what_the_write_side_published() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = path(&dir, "follow.zu1");
        drop(Zu1File::create(&file).expect("create"));
        let handle = FileHandle::attach(&file, false, || Zu1File::open(&file)).expect("attach");

        let before = handle.published().version();
        let mut side = handle.take();
        side.file_mut().db_header_mut().epoch += 1;
        let epoch = side.file().db_header().epoch;
        handle.publish(&side);
        handle.put(side);

        let published = handle.published();
        assert!(published.newer_than(before), "publishing moves the version");
        let mut reader = handle.reader().expect("reader");
        reader.follow(published.header(), published.slot());
        assert_eq!(reader.db_header().epoch, epoch);
    }
}
