//! The write side of an open database: the log beside the file, the
//! overlay store above it, and the fold that puts a committed change
//! where the next read will find it.
//!
//! M3 built all three of those and crash tested them, and [`crate::append`]
//! is the first thing on the query side to open one, but an appender is
//! the bulk shape: whole columns at a time, no expression to evaluate,
//! nothing read back. A write statement is the other shape, one
//! transaction of a few rows that has to be visible to the clause after
//! it, and it needs the same three pieces held differently. This is
//! that holding. A [`Writer`] owns the WAL and the [`Mvcc`] store for
//! one open file, recovers whatever the log holds above the file's last
//! checkpoint, and turns a staged transaction into a committed epoch.
//!
//! Most commits fold, and the ones that do not are worth being plain
//! about. A fold rewrites the columns the transaction touched, and the
//! overlay exists precisely so a reader could see a committed row
//! without one, but [`Zu1Graph`] reads the sealed file and nothing
//! else, so a change that is not folded is a change the next `MATCH`
//! cannot see, and a write nobody can read is not a write. So a commit
//! folds unless the read side can be handed what it wrote instead,
//! which is one shape: a value written onto a column of a row that is
//! already there, whether the row is a node or an edge the graph runs
//! through. Those go into a [`CellPatch`] per table,
//! the readers put them over the values their columns hold, and a fold
//! seals them later, at a checkpoint or when the run of them has gone
//! on long enough. It is worth what it costs to keep narrow, because
//! the fold is not the only thing that goes: it is what moves the
//! header epoch, and moving the epoch is what throws away the session's
//! plan cache, its catalog and every decoded chunk it had warm.
//!
//! Not every fold publishes, and that is the part worth reading twice.
//! A fold moves the roots this handle carries; a checkpoint puts them
//! on disk, and on a platform where the only barrier is a full sync it
//! costs two of them, against one for the log frame the commit already
//! synced. So a commit folds and stops, and the file is checkpointed
//! when the folds since the last one have taken as much as
//! [`checkpoint_due`] allows. Nothing is at risk in between: the
//! frame is the durability point, the header on disk still names the
//! epoch the last checkpoint folded through, and a crash replays the
//! log from there back to where the folds had got to. What it costs is
//! file growth, because a block freed by an unpublished fold cannot be
//! handed out again, and that is what the threshold bounds.
//!
//! [`Zu1Graph`]: crate::query::Zu1Graph

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zu_common::{Epoch, IdMap, Result, ZuError};

use crate::append::sidecar;
use crate::deleted::Tombstones;
use crate::zu1::catalog::{Catalog, ElementKind};
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{checkpoint_fold, recover, staged_fold};
use crate::zu1::graph::{Direction, EdgePatch, GraphReader};
use crate::zu1::props::{
    CellPatch, LabelPatch, PropColumn, PropsDirectory, RowPatch, load_props, load_rel_props,
    stored_label_word, zoned,
};
use crate::zu1::txn::{Cell, Deferred, Mvcc, WriteTxn};
use crate::zu1::wal::{Commits, Wal};

/// The most a run of folds may take before one of them checkpoints.
///
/// Every fold in a run takes fresh blocks for what it rewrites and
/// gives back nothing, because until a checkpoint publishes, the header
/// a crash would find still reads what they replaced. So this is file
/// growth, and it is growth that stays: the high-water mark is what the
/// file is on disk and it never comes back down. This is the ceiling
/// rather than the number, and it is here for the store big enough that
/// one fold approaches it on its own.
const CEILING_BYTES: u64 = 64 * 1024 * 1024;

/// The least it may take, for the store too small to have a quarter
/// worth speaking of.
///
/// A checkpoint is two syncs, so the floor is really a statement rate:
/// a small store folds a handful of blocks at a time, which puts a
/// checkpoint every few folds and a few folds is a thousand statements
/// or so. Far enough apart that the syncs land nowhere near the
/// interesting end of the latency distribution, close enough that the
/// file a small store sits in stays about the store.
///
/// It was four megabytes, which was that same rate when a block was
/// 256 KiB and a fold of a small table took a block or two of it. A
/// block is 32 KiB now and a fold takes a block or two of that, so the
/// same four megabytes had become forty folds and six thousand
/// statements of slack, and a 0.7 MB store was entitled to sit in a
/// 4.7 MB file. The unit is still bytes, because what the floor bounds
/// is file growth, but the number has to follow what a fold costs and
/// that fell with the block. One megabyte is thirty two blocks, which
/// at the couple of blocks a small fold takes is back to the every few
/// folds this was always meant to be.
const FLOOR_BYTES: u64 = 1024 * 1024;

/// How much the file may grow before the next commit stops staging and
/// checkpoints, given what the file already is.
///
/// The rule is [`GROWTH_SHARE`] of the file held between
/// [`FLOOR_BYTES`] and [`CEILING_BYTES`]. It is public because it is
/// the bound on how big a file gets between checkpoints, which is a
/// thing a bench checks and an operator sizing a disk wants, and
/// because a copy of the rule written down somewhere else would go
/// stale the first time one of the three numbers moved.
pub fn checkpoint_slack_bytes(file_bytes: u64) -> u64 {
    (file_bytes / GROWTH_SHARE).clamp(FLOOR_BYTES, CEILING_BYTES)
}

/// What the file may grow by between checkpoints, as a fraction of what
/// it already is.
///
/// A big store folds big segments, and a checkpoint per fold would put
/// the two syncs back on the statement that the deferred path exists to
/// take them off. Letting the slack scale with the file keeps the
/// number of folds between checkpoints about the same whatever the
/// store is, and bounds the waste at a quarter rather than at whatever
/// a fixed block count works out to for the store in front of it.
const GROWTH_SHARE: u64 = 4;

/// How many blocks a fold may take before the next commit checkpoints
/// rather than staging.
///
/// See [`CEILING_BYTES`], [`FLOOR_BYTES`] and [`GROWTH_SHARE`]: the
/// slack is a quarter of the file, held between the two of them.
fn checkpoint_due(db: &Zu1File) -> bool {
    let block = u64::from(crate::zu1::BLOCK_SIZE);
    // Floored, not rounded up: a slack of a block and a half is a
    // block, and a fold is due once it has taken that much. Rounding
    // the other way hands out an extra block at every file size where
    // the share does not land on one.
    let slack = checkpoint_slack_bytes(db.db_header().block_count * block);
    db.unpublished_blocks() >= slack / block
}

/// How many commits in a row may go without a fold.
///
/// A deferred commit is a frame in the log and a handful of cells in
/// the overlay, and neither is freed until a fold takes them, so this
/// is what a recovery has to replay and what the patch below has to
/// carry.
///
/// It is also what decides the write tail, and that is the reason it is
/// this large. One statement in this many carries a fold, and a fold is
/// milliseconds where a statement is microseconds, and it holds the
/// write side while it runs, so at eight writers one fold is eight slow
/// statements. The share of statements a fold makes slow is therefore
/// about the width over this number, and a share that is to stay under
/// a percentile has to be under what that percentile leaves: a p99 that
/// is not to contain a fold wants this over a hundred times the widest
/// burst it will see. Four thousand covers thirty two writers with room
/// left.
///
/// What that costs is what the two bounds below hold down, and what
/// made it impossible until now was neither of them: the patch used to
/// be copied whole every time a commit added to it, so a run of this
/// length cost the square of it. See [`Writer::stage_patch`].
const DEFERRED_COMMITS: u32 = 4096;

/// How many of them may leave something behind that the patch copies
/// rather than shares.
///
/// The bound above is what the write tail wants and this is what the
/// rest of the system can afford, and they are two numbers because the
/// changes are of two kinds. A row appended past the end of a table is
/// data the reader was going to produce anyway, in the order it was
/// going to produce it in, so the patch keeps them in sealed chunks
/// that a copy shares and a reader walks once. Everything else is laid
/// over data already there: a cell over a cell, a tombstone over an
/// offset, a label word over the bitset's. Those are copied when the
/// commit after them touches the same table, and worse, every scan
/// between here and the next fold puts them back over the column
/// again. So the cost of the run is the depth of it times the reads
/// through it, and that is a cost the write tail gets nothing for.
///
/// Measured on the sustained SET window, which scans a ten thousand
/// row column a statement: 23.7 us of processor time a statement at a
/// fold every 256 commits against 35.2 at every 1024. Both of those
/// were the same patch, so it is the reads paying, not the writes.
///
/// An added edge is not on this side of the line, though it was until
/// the patch that holds it was split the way the cells are. A read of
/// one was always cheap, because the patch holds the added edges by the
/// node they leave, so a traversal asks about one node's list rather
/// than looking through every edge the run added. What was not cheap
/// was the commit: those lists were copied whole each time one was
/// added to. They sit in a sealed run now with the recent ones beside
/// it, so a commit copies a few dozen of them however many the run has
/// reached, and an edge is bounded by [`DEFERRED_ROWS`] with the
/// appended rows, which is what it is.
///
/// Which leaves the tail. A run of writes over rows that are already
/// there folds as often as it did before any of this, and a run that
/// appends rows gets the whole four thousand. That is the right way
/// round: an append is what a load does and what the commit tail was
/// measured on, and an overwrite is what a scan pays for.
const DEFERRED_COPIED: u32 = 256;

/// How many cells the deferred commits may hold between them.
///
/// A cell written over one the file already holds goes in a map the
/// commit after it copies, so this bounds the per-commit cost of
/// deferring as well as the memory. Rows appended past the end of a
/// table are not copied that way and have a bound of their own below.
///
/// Left where it was while the commit bound went to 4096, and what
/// holds a run of overwrites down now is [`DEFERRED_COPIED`] rather
/// than this. It used to be this one: the cells were copied whole, so
/// bounding how many there were bounded what the copy cost. They are
/// not any more, because they sit in a sorted run with the recent ones
/// in a map beside it and a commit copies a few dozen of them whatever
/// the run has reached. So this is a bound on memory and on the string
/// bytes below it, and a single statement writing a thousand cells
/// still trips it.
const DEFERRED_CELLS: usize = 1024;

/// How many rows the deferred commits may append between them.
///
/// Apart from the cells because the cost is not the same. An appended
/// row is never written into again, so the patch holds them in sealed
/// chunks a copy shares rather than copies, and what a commit pays for
/// the ones already there is a pointer. So this is a bound on memory
/// and on how much a fold has to seal in one go, and not, the way the
/// cell bound is, a bound on what the commit before the fold cost.
const DEFERRED_ROWS: usize = 4096;

/// How many bytes of string the cells among them may hold.
///
/// A word is eight bytes whatever it says, so the count above bounds
/// what an integer write costs on its own. A string is as long as the
/// statement wrote it, and the patch keeps it until the next fold, so
/// this is the other half of the same bound. A quarter of a megabyte
/// is well over what a run of payload updates carries and well under
/// anything a process would notice.
const DEFERRED_BYTES: usize = 256 * 1024;

/// Everything the commits since the last fold left for the readers to
/// read through, which is what makes a deferred commit visible.
///
/// Five shapes, because there are five ways a change can sit above a
/// column nobody has rewritten: a new value for a row the column
/// already holds, a whole new row past the end of it, an edge added or
/// taken away, which is a pair the adjacency reader merges into or out
/// of two lists and, where it was added, a row of the rel table's
/// property columns, a row taken away, which is an offset every read of
/// the table filters by, and a label change, which is a word the reader
/// answers with in place of the bitset's.
#[derive(Debug, Default, Clone)]
pub struct Patches {
    /// New values, by table.
    pub cells: IdMap<u32, Arc<CellPatch>>,
    /// New rows, by table, node tables and rel tables alike. A rel
    /// table's are the property rows of the edges in `edges`, under
    /// the ordinals those took. A node table's are rows of the table
    /// itself, so they move the count every reader is bounded by, and
    /// everything that reads a bound reads it through this.
    pub rows: IdMap<u32, Arc<RowPatch>>,
    /// Edges added and edges taken away, by rel table.
    pub edges: IdMap<u32, Arc<EdgePatch>>,
    /// Rows taken away, by table, ascending. A delete moves nothing:
    /// the offsets stay where they are and the fold writes them into
    /// the table's tombstone chain, so what the patch carries is what
    /// the chain would have held.
    pub gone: Tombstones,
    /// The labels rows are left carrying, by table. A label is not a
    /// column, so there is no cell to lay a value over: what is kept is
    /// the whole word, composed where the commit was taken.
    pub marks: IdMap<u32, Arc<LabelPatch>>,
}

impl Patches {
    pub fn new() -> Self {
        Patches::default()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
            && self.rows.is_empty()
            && self.edges.is_empty()
            && self.gone.is_empty()
            && self.marks.is_empty()
    }

    /// How many cells the parts of this that a commit copies whole hold
    /// between them, which is what the writer bounds when it decides
    /// whether to keep deferring.
    ///
    /// Everything but the appended rows, which are counted on their own
    /// by [`Self::appended`] because they are the one part a commit
    /// does not copy. See [`DEFERRED_CELLS`] and [`DEFERRED_ROWS`].
    fn cells(&self) -> usize {
        self.cells.values().map(|p| p.cells()).sum::<usize>()
            + self.gone.values().map(|rows| rows.len()).sum::<usize>()
            + self.marks.values().map(|p| p.len()).sum::<usize>()
    }

    /// How many edges the deferred commits have added or taken away
    /// between them.
    ///
    /// Counted apart from the cells because the cost is not the same
    /// one. An added edge goes into a sealed run a copy shares, so what
    /// a commit pays for the ones already there is a pointer, and this
    /// bounds the memory and what a fold has to seal rather than what
    /// the commit before it cost. It is bounded with the appended rows
    /// because that is the shape of it: an added edge is a row of the
    /// rel table as well as a pair on two lists.
    fn edges(&self) -> usize {
        self.edges
            .values()
            .map(|p| (p.adds() + p.removed()) as usize)
            .sum()
    }

    /// How many rows the deferred commits have appended between them.
    fn appended(&self) -> usize {
        self.rows.values().map(|p| p.len()).sum()
    }

    /// How many rows a commit has added to `table` and not folded,
    /// which every bound over the table's rows is short by.
    pub fn added_rows(&self, table: u32) -> u64 {
        self.rows.get(&table).map_or(0, |p| p.len() as u64)
    }

    /// The same for the two ends of `rel`, the source first, which is
    /// what its adjacency reader answers rows past its CSR with.
    pub fn grown(&self, catalog: &Catalog, rel: u32) -> [u64; 2] {
        catalog.rel_by_id(rel).map_or([0, 0], |table| {
            [table.from, table.to].map(|end| self.added_rows(end))
        })
    }

    /// How many bytes of string they hold, the other thing it bounds.
    fn bytes(&self) -> usize {
        self.cells.values().map(|p| p.bytes()).sum::<usize>()
            + self.rows.values().map(|p| p.bytes()).sum::<usize>()
    }
}

/// What one [`Writer::write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written<T> {
    /// Whatever the staging closure returned, which for an insert is
    /// usually how many rows it appended.
    pub value: T,

    /// The epoch the change committed at. A transaction that staged
    /// nothing publishes nothing, so this is the epoch that was
    /// already current when the closure did no work.
    pub epoch: Epoch,

    /// How far the log has to be on the platter for this to have
    /// committed, or `None` when nothing is owed.
    ///
    /// Nothing is owed by a transaction that staged nothing, and
    /// nothing is owed once a checkpoint has sealed the change into the
    /// base file and cut the log, because then what would be waited for
    /// is not there any more and what it said is durable without it.
    ///
    /// The caller waits for this after it has given the write side
    /// back, which is what lets the writer behind it stage into the
    /// same sync. See [`Commits::sync_through`].
    pub owed: Option<u64>,
}

/// The write side of one open database.
///
/// One of these is the single writer: it holds the only handle on its
/// overlay store, and [`Mvcc::begin`] hands out that store's only
/// mutable borrow, so a second transaction cannot exist while one is
/// open. Two writers over one file would be two overlay stores and one
/// log between them, and neither would be right, so there is exactly
/// one place a [`Writer`] lives, which is inside the session that owns
/// the file handle. An appender is the other writer of the same log,
/// and it is kept apart the same way: it borrows the file, and a
/// session hands the file out only after dropping its writer.
pub struct Writer {
    wal: Wal,
    mvcc: Mvcc,
    path: PathBuf,
    /// Everything the commits since the last fold left behind, as a
    /// reader takes it. This is the writer's own copy as well: a
    /// deferred commit writes into it in place where nothing is looking
    /// and clones it where something is, and the read-your-writes
    /// questions [`Writer::defers`] asks are asked of it rather than of
    /// a second copy kept alongside. A reader holds the `Arc` and hands
    /// copies of it to the workers a query forks.
    patches: Arc<Patches>,
    /// How many commits have gone without a fold.
    deferred: u32,
    /// How many folds this writer has run, staged or checkpointed,
    /// since it was opened.
    ///
    /// Nothing in the write path reads it. It is here because a fold is
    /// otherwise only visible as a slow statement, and a bench that
    /// wants to know whether its window held the housekeeping it is
    /// measuring had to draw a line through a latency distribution and
    /// call what was above it folds. That line moves with the machine
    /// and it was calling noise a fold on a shared box, so the count is
    /// kept here where it is exact.
    folds: u64,
    /// How many of them left something the patch copies rather than
    /// shares. See [`DEFERRED_COPIED`].
    copied: u32,
    /// The rows they took away, by table. A set, because a row can be
    /// deleted twice and the second one takes nothing away, and sorted
    /// because that is the order the readers merge it in. Kept apart
    /// from the patch because what the patch publishes is a sorted run
    /// and what a delete wants is somewhere to put an offset.
    graves: IdMap<u32, BTreeSet<u64>>,
    /// Adjacency readers of the rel tables a deferred commit has added
    /// an edge to, which is what says whether the pair is already
    /// there and how many edges the file holds. A fold moves what
    /// these describe, so it empties this.
    readers: IdMap<u32, GraphReader>,
    /// The catalog those readers were loaded through, for the same
    /// stretch and dropped at the same fold.
    catalog: Option<Catalog>,
    /// Props directories of the tables a deferred commit has written
    /// into, which is how a commit is checked against the columns it
    /// names without reading the directory chain per statement. A fold
    /// moves the roots these describe, so it empties this.
    dirs: IdMap<u32, Option<PropsDirectory>>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("path", &self.path)
            .field("epoch", &self.mvcc.epoch())
            .finish()
    }
}

impl Writer {
    /// Opens the write side of `db`, replaying whatever the log holds
    /// above the file's last checkpoint and sealing it.
    ///
    /// The sealing is what makes a reopen after a crash the same as
    /// never having crashed: a crash between a commit and a checkpoint
    /// leaves committed transactions in the log and not in the file,
    /// recovery brings them back as overlays, and an overlay is what no
    /// reader reads. A session replays the sidecar when it opens, so on
    /// that path this usually finds an empty log and folds nothing;
    /// doing it here too is what keeps a [`Writer`] correct for a
    /// caller that did not come through a session.
    ///
    /// A read-only handle is refused here rather than at the first
    /// commit, because opening the write side already creates the log,
    /// and a log beside a database nobody may write to is a file that
    /// only ever confuses the next reader.
    pub fn open(db: &mut Zu1File) -> Result<Writer> {
        writable(db)?;
        let path = sidecar(db.path());
        let mut wal = Wal::open_in(db.vfs(), &path)?;
        let mvcc = recover(db, &mut wal)?;
        let mut writer = Writer {
            wal,
            mvcc,
            path,
            patches: Arc::new(Patches::new()),
            deferred: 0,
            folds: 0,
            copied: 0,
            graves: IdMap::default(),
            readers: IdMap::default(),
            catalog: None,
            dirs: IdMap::default(),
        };
        writer.fold(db)?;
        Ok(writer)
    }

    /// The newest committed epoch.
    pub fn epoch(&self) -> Epoch {
        self.mvcc.epoch()
    }

    /// The log this writer commits to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs one transaction: `stage` describes the change against an
    /// open [`WriteTxn`], and what it returns comes back alongside the
    /// epoch the change committed at.
    ///
    /// A closure that raises is the rollback path, and it costs
    /// nothing: staging is local to the transaction and the log has
    /// not been touched, so dropping it on the way out of the `?` is
    /// the whole of the undo. That is the shape an implicit
    /// transaction wants, where a statement that raises anywhere in
    /// the middle must leave the database as it found it.
    pub fn write<T>(
        &mut self,
        db: &mut Zu1File,
        stage: impl FnOnce(&mut WriteTxn<'_>) -> Result<T>,
    ) -> Result<Written<T>> {
        let mut txn = self.mvcc.begin();
        let value = stage(&mut txn)?;
        let staged = !txn.is_empty();
        // A savepoint holding a state to go back to puts it on the file
        // before this frame reaches the log, because from the log sync
        // on there is a change a recovery would bring back, and
        // something has to say it was meant to be taken away again.
        if staged {
            db.keep_savepoint()?;
        }
        // The frames go to the log, and the wait for the platter does
        // not happen here: the caller does it once it has let go of the
        // write side, so the next writer stages into the same sync.
        let (epoch, mut owed) = txn.stage_commit(&mut self.wal)?;
        if staged {
            let changes = self.mvcc.take_deferred();
            if let Some(changes) = self.defers(db, changes)? {
                self.stage_patch(changes);
                // The frames are in the log and no fold has taken them
                // anywhere, so an open savepoint owes the file its
                // marker before the next commit goes on top.
                db.stage_deferred();
            } else {
                self.folds += 1;
                match checkpoint_due(db) {
                    true => self.fold(db)?,
                    false => {
                        staged_fold(db, &mut self.mvcc, &mut self.wal)?;
                        self.sealed();
                    }
                }
            }
        }
        // A fold that checkpointed put this epoch in the base file,
        // synced it there and cut the log. So the offset above names a
        // byte the log no longer has, and what it was going to make
        // durable is durable already.
        if owed.is_some_and(|need| self.wal.len() < need) {
            owed = None;
        }
        Ok(Written { value, epoch, owed })
    }

    /// How many folds this writer has run since it was opened. See
    /// [`Writer::folds`].
    pub fn fold_count(&self) -> u64 {
        self.folds
    }

    /// What makes this writer's commits durable, held past the write
    /// side going back so the wait happens off the lock.
    pub fn commits(&self) -> Arc<Commits> {
        Arc::clone(self.wal.commits())
    }

    /// Whether the log owes nothing. See [`Commits::settled`].
    pub fn settled(&self) -> bool {
        self.wal.commits().settled()
    }

    /// The cells the commits since the last fold wrote, which a reader
    /// puts over the words its columns hold.
    ///
    /// This is the whole of what a deferred commit hands the read side,
    /// so a reader that has it reads the same database a fold would
    /// have left behind. The `Arc` is the version: it is replaced when
    /// a commit adds to the patch and emptied when a fold seals it, and
    /// a reader that holds the current one is up to date.
    pub fn patches(&self) -> &Arc<Patches> {
        &self.patches
    }

    /// This commit as the patch would hold it, or `None` when it has to
    /// be folded.
    ///
    /// Six shapes need no fold. A value written onto a row that is
    /// already there, because the patch carries exactly that and a
    /// reader reads through it. The same value written onto an edge,
    /// which is the one above once the pair has been turned into the
    /// ordinal it holds, and turning it is why this answers with the
    /// changes rather than about them. An edge added to a rel table,
    /// because the adjacency reader merges it into the two lists it
    /// belongs in and its property values sit under the ordinal it was
    /// given. And a row taken away, because a delete moves nothing: the
    /// offset goes in the patch where a fold would have put it in the
    /// chain, and the readers filter by the two together. And rows
    /// added to a node table, because they go on the end of every
    /// column of it in the order a fold would have appended them in,
    /// and the readers take their bound from the patch as well as from
    /// the file. And an edge taken away, because the pair it runs
    /// between is the whole name of it, and a reader handed that pair
    /// drops it out of the two lists it is in as it reads them.
    /// Anything else, a label above all, folds the way it always did.
    /// On top of the shape
    /// there are five bounds: how many commits may go unfolded, how
    /// many of those may leave a change laid over data already there,
    /// how many cells they may hold, how many bytes of string among
    /// them, and the block growth the checkpoint threshold already
    /// bounds.
    fn defers(
        &mut self,
        db: &mut Zu1File,
        changes: Vec<Deferred>,
    ) -> Result<Option<Vec<Deferred>>> {
        if changes.is_empty() || !self.mvcc.soft() {
            return Ok(None);
        }
        if self.deferred >= DEFERRED_COMMITS
            || self.copied >= DEFERRED_COPIED
            || self.patches.cells() + written_cells(&changes) > DEFERRED_CELLS
            || self.patches.appended() + written_rows(&changes) > DEFERRED_ROWS
            || self.patches.edges() + written_edges(&changes) > DEFERRED_ROWS
            || self.patches.bytes() + written_bytes(&changes) > DEFERRED_BYTES
            || checkpoint_due(db)
        {
            return Ok(None);
        }
        // The edges this commit is taking away, before any of it is
        // taken, because a `DETACH DELETE` stages the row after the
        // edges that ran onto it and the row is only removable once
        // they are gone.
        let dying: Vec<(u32, u64, u64)> = changes
            .iter()
            .filter_map(|change| match *change {
                Deferred::DeadRel(rel, src, dst) => Some((rel, src, dst)),
                _ => None,
            })
            .collect();
        // And the edges it is adding, for the other half of the same
        // question: a row is only removable once nothing is left
        // pointing at it, and an edge written beside the delete points
        // at it as surely as one that was already there.
        let born: Vec<(u32, u64, u64)> = changes
            .iter()
            .filter_map(|change| match *change {
                Deferred::Edge(rel, src, dst, _) => Some((rel, src, dst)),
                _ => None,
            })
            .collect();
        // The words this commit has left on rows so far, by table and
        // offset, for the same reason.
        let mut staged: IdMap<(u32, u64), u64> = IdMap::default();
        // The rows this commit has put on the end of each table so far,
        // for the same reason again: `INSERT (m:Post), (p)-[:LIKES]->(m)`
        // is a row and then an edge onto it, and the edge is asked about
        // while the row it names is still only in here.
        let mut appending: IdMap<u32, u64> = IdMap::default();
        let mut taken = Vec::with_capacity(changes.len());
        for change in changes {
            match change {
                Deferred::Cell((table, row, col, ref value)) => {
                    if !self.patchable(db, table, row, col as usize, value)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::RelCell((rel, src, dst, col, ref value)) => {
                    let Some(run) = self.edge_cells(db, rel, src, dst, col as usize, value)? else {
                        return Ok(None);
                    };
                    // Every copy of the pair, because the fold writes the
                    // value onto every slot the pair holds and the two
                    // paths have to leave the same column behind.
                    taken.extend(run.map(|row| Deferred::Cell((rel, row, col, value.clone()))));
                }
                Deferred::Edge(rel, src, dst, ref cols) => {
                    if !self.edge_patchable(db, rel, src, dst, cols, &appending)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::Gone(table, offset) => {
                    if !self.removable(db, table, offset, &dying, &born, &appending)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::DeadRel(rel, ..) => {
                    if !self.edge_removable(db, rel)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::Rows(table, ref rows) => {
                    if !self.appendable(db, table, rows)? {
                        return Ok(None);
                    }
                    *appending.entry(table).or_default() += rows.len() as u64;
                    taken.push(change);
                }
                Deferred::Labels(table, row, add, remove) => {
                    let Some(word) = self.marked(db, table, row, add, remove, &staged)? else {
                        return Ok(None);
                    };
                    // Two changes of one commit can name one row, and
                    // the second of them goes over what the first left
                    // rather than over what the file holds.
                    staged.insert((table, row), word);
                    taken.push(Deferred::Marks(table, row, word));
                }
                // The composing is here, so nothing else makes one.
                Deferred::Marks(..) => unreachable!("composed where the commit was taken"),
            }
        }
        Ok(Some(taken))
    }

    /// The rows of `rel`'s property columns that `value` written onto
    /// the edges of `src -> dst` lands on, and `None` when the write has
    /// to be folded instead.
    ///
    /// A pair this run of deferred commits added holds its values in
    /// the row the patch appended for it rather than in the column
    /// underneath, and its ordinal is past everything the column has,
    /// so the write goes over the appended row. The reader answers for
    /// that ordinal out of the same row, so what the next `MATCH` gets
    /// is what the `SET` left.
    ///
    /// This is the second half of what a bracketed write does: the
    /// insert that sets the edge up, the write being measured onto it,
    /// and the delete that takes it away, none of them folding. The
    /// insert on its own stopped folding when the delete before it
    /// began leaving a mark [`Self::edge_patchable`] reads; without
    /// this the write in the middle folded instead and the run was no
    /// longer for it.
    fn edge_cells(
        &mut self,
        db: &mut Zu1File,
        rel: u32,
        src: u64,
        dst: u64,
        col: usize,
        value: &Cell,
    ) -> Result<Option<std::ops::Range<u64>>> {
        // The patch first, and on its own where it answers, because the
        // readers here are the file's: they are loaded once for the run
        // and never handed the patch, so what they know of a pair is
        // what the last fold sealed. A pair the patch added runs once,
        // at the ordinal the add gave it.
        let patch = self.patches.edges.get(&rel);
        let held = patch.and_then(|p| p.ordinal(src, dst));
        let (base, count) = match held {
            Some(ord) => (ord, 1),
            None => {
                // A pair the patch took away is one no reader sees, so
                // there is nothing under it to write onto and the file's
                // copies of it are not the answer.
                if patch.is_some_and(|p| p.drops(src, dst, Direction::Fwd)) {
                    return Ok(None);
                }
                self.load_reader(db, rel)?;
                let Some(reader) = self.readers.get_mut(&rel) else {
                    return Ok(None);
                };
                let Some(run) = reader.edge_run(db, src, dst)? else {
                    return Ok(None);
                };
                run
            }
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(rel) {
            slot.insert(load_rel_props(db, rel)?);
        }
        let Some(column) = self.dirs[&rel]
            .as_ref()
            .and_then(|directory| directory.columns.get(col))
        else {
            return Ok(None);
        };
        if !holds(column, value) || column.validity.is_some() {
            return Ok(None);
        }
        // A run is either wholly the file's or wholly the patch's: the
        // patch holds one copy of a pair at most, so a run it answers
        // for is the one edge it added, and a run the file answers for
        // is edges the last fold sealed.
        let patchable = match base < column.meta.value_count {
            true => base + count <= column.meta.value_count,
            false => self.appended_cell(rel, base, col),
        };
        Ok(patchable.then_some(base..base + count))
    }

    /// Whether a reader can be shown this edge without the CSR being
    /// rebuilt around it.
    ///
    /// The pair has to be one the graph does not run through yet. An
    /// edge in the patch takes an ordinal past everything the file
    /// holds, so a second edge over a pair the file already has would
    /// leave a run of copies whose ordinals are not consecutive, and
    /// the whole of the ordinal side is built on their being so. A
    /// table that stores properties refuses the second copy at write
    /// time anyway; this is what keeps the other kind honest.
    ///
    /// A pair the same run of deferred commits already took away is not
    /// one the graph runs through, whatever the file still holds under
    /// it. The patch says the file's copies are gone and every reader
    /// takes them off the list before it merges the patch's own in, so
    /// the added edge is the only one there is and its run is itself.
    /// This is the whole of an add and a delete over one pair going
    /// round without a fold, which is what a bracketed write does: the
    /// delete leaves the mark, the add goes on top of it, and neither
    /// of them rebuilds a CSR.
    ///
    /// Either end may be a row the CSR was never built over, one an
    /// earlier commit of this run appended or one this same commit is
    /// appending, which is what `appending` counts. The CSR holds no
    /// list for such a row and the readers are told how far past it the
    /// two node tables now run, so they answer for one out of the patch
    /// alone. Past that the id is not a row of anything and the edge
    /// folds. The probe below is only for an end the file has a row
    /// for, because an end it does not cannot be in a list it wrote.
    ///
    /// Every column the table stores has to be given a value of its own
    /// kind, because that row is served out of the patch and there is
    /// no column underneath it to fall back on. That is also what the
    /// fold demands of an added edge, so an insert that skips a column
    /// is an error either way and this only decides where it is raised.
    fn edge_patchable(
        &mut self,
        db: &mut Zu1File,
        rel: u32,
        src: u64,
        dst: u64,
        cols: &[(u32, Cell)],
        appending: &IdMap<u32, u64>,
    ) -> Result<bool> {
        let patch = self.patches.edges.get(&rel);
        if patch.is_some_and(|p| p.holds(src, dst)) {
            return Ok(false);
        }
        let dropped = patch.is_some_and(|p| p.drops(src, dst, Direction::Fwd));
        self.load_reader(db, rel)?;
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::load(db)?);
        }
        let Some(reader) = self.readers.get(&rel) else {
            return Ok(false);
        };
        let (from_count, to_count) = {
            let directory = reader.directory();
            (directory.from_count, directory.to_count)
        };
        let catalog = self.catalog.as_ref().expect("loaded with the reader");
        let Some(table) = catalog.rel_by_id(rel) else {
            return Ok(false);
        };
        let (from, to) = (table.from, table.to);
        let [from_grown, to_grown] = self.patches.grown(catalog, rel);
        let room = |count: u64, grown: u64, end: u32| {
            count + grown + appending.get(&end).copied().unwrap_or(0)
        };
        if src >= room(from_count, from_grown, from) || dst >= room(to_count, to_grown, to) {
            return Ok(false);
        }
        if !dropped && src < from_count && reader.has_edge(db, src, dst)? {
            return Ok(false);
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(rel) {
            slot.insert(load_rel_props(db, rel)?);
        }
        let Some(directory) = self.dirs[&rel].as_ref() else {
            return Ok(cols.is_empty());
        };
        Ok(directory.columns.iter().enumerate().all(|(at, column)| {
            match cols.iter().find(|(c, _)| *c as usize == at) {
                Some((_, Cell::Null)) => true,
                Some((_, cell)) => holds(column, cell),
                None => false,
            }
        }))
    }

    /// Whether a reader can be shown this edge as gone without the CSR
    /// being rebuilt without it.
    ///
    /// Nothing is asked but that the table is one a reader can be
    /// handed a patch over. The pair names whatever copies of it the
    /// file holds and no more, so a delete of an edge that is not there
    /// takes nothing away, and the property columns underneath are left
    /// exactly as they were: the ordinals of the edges that stay do not
    /// move, because nothing has been rebuilt around the ones that
    /// went.
    ///
    /// A pair this run of deferred commits added is taken out of the
    /// patch's lists rather than laid over them as gone, which is what
    /// leaves an unfolded write and the delete that follows it costing
    /// nothing between them. The ordinal it took stays spent and the
    /// row of properties it wrote stays where it was, unreachable, so
    /// the run of ordinals nothing has rebuilt is left as dense as it
    /// was.
    fn edge_removable(&mut self, db: &mut Zu1File, rel: u32) -> Result<bool> {
        self.load_reader(db, rel)?;
        Ok(self.readers.contains_key(&rel))
    }

    /// Whether readers can be shown these rows on the end of the table
    /// without a column of it being rewritten.
    ///
    /// Every column the table stores has to be given a value of its
    /// own kind, because the row is served out of the patch and there
    /// is no column underneath it to fall back on. That is what the
    /// fold demands of an appended row as well, so a row that skips a
    /// column is an error either way and this only decides where it is
    /// raised.
    ///
    /// A value that is absent is taken only into a column that already
    /// keeps a validity segment. The patch carries no mask, so what
    /// says a row of it holds nothing is the cell itself, and a reader
    /// only asks that question of a column it has been told can hold a
    /// null. Into a column that cannot, the same row would read as a
    /// zero now and as an absence once the fold had given the column a
    /// mask, and the two have to be the same answer.
    ///
    /// The columns have to end where the table does, because the rows
    /// go on the end of every one of them and a column that stopped
    /// short would take them at the wrong offsets.
    fn appendable(
        &mut self,
        db: &mut Zu1File,
        table: u32,
        rows: &[Vec<(u32, Cell)>],
    ) -> Result<bool> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(table) {
            slot.insert(load_props(db, table)?);
        }
        let Some(directory) = self.dirs[&table].as_ref() else {
            return Ok(false);
        };
        if directory.columns.is_empty()
            || directory
                .columns
                .iter()
                .any(|column| column.meta.value_count != directory.node_count)
        {
            return Ok(false);
        }
        Ok(rows.iter().all(|row| {
            directory.columns.iter().enumerate().all(|(at, column)| {
                match row.iter().find(|(c, _)| *c as usize == at) {
                    Some((_, Cell::Null)) => column.validity.is_some(),
                    Some((_, cell)) => holds(column, cell),
                    None => false,
                }
            })
        }))
    }

    /// Loads the adjacency reader of `rel` if this run of deferred
    /// commits has not needed it yet. A rel table with no directory
    /// entry is one nothing can be added to, and leaves the slot empty
    /// so the caller folds.
    fn load_reader(&mut self, db: &mut Zu1File, rel: u32) -> Result<()> {
        if self.readers.contains_key(&rel) {
            return Ok(());
        }
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::load(db)?);
        }
        let name = self
            .catalog
            .as_ref()
            .expect("just loaded")
            .rel_by_id(rel)
            .map(|table| table.name.clone());
        if let Some(name) = name {
            self.readers
                .insert(rel, GraphReader::load_table(db, &name)?);
        }
        Ok(())
    }

    /// Whether a reader can be shown a value written onto this cell
    /// without the column being rewritten.
    ///
    /// The column has to be one the patch can describe: the kind that
    /// stores what was written, and no validity segment, because a
    /// column that can hold nothing in a row keeps that in a bitmap the
    /// patch does not carry.
    ///
    /// A row the column has a value for is patched by laying a word
    /// over it. A row past the column is one an earlier commit of this
    /// run appended, and there is nothing under it to lay a word over:
    /// its values are the ones the append carried and they sit in the
    /// row patch, so a write onto it goes over them there. That is what
    /// the fold does with the same pair of commits, because it asks the
    /// overlay what each appended row holds rather than taking the
    /// append at its word, so the two paths seal the same column.
    ///
    /// Both are refused past what the patch actually holds, a row this
    /// same commit is appending above all: the append has not been
    /// staged when this is asked, so the row is in neither place yet
    /// and the commit folds.
    fn patchable(
        &mut self,
        db: &mut Zu1File,
        table: u32,
        row: u64,
        col: usize,
        value: &Cell,
    ) -> Result<bool> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(table) {
            slot.insert(load_props(db, table)?);
        }
        let Some(directory) = self.dirs[&table].as_ref() else {
            return Ok(false);
        };
        let Some(column) = directory.columns.get(col) else {
            return Ok(false);
        };
        if !holds(column, value) || column.validity.is_some() {
            return Ok(false);
        }
        Ok(match row < column.meta.value_count {
            true => true,
            false => self.appended_cell(table, row, col),
        })
    }

    /// Whether `row` is one the run appended and its `col` can be
    /// written over where it lies.
    fn appended_cell(&self, table: u32, row: u64, col: usize) -> bool {
        self.patches
            .rows
            .get(&table)
            .is_some_and(|rows| rows.settable(col, row))
    }

    /// Whether a reader can be shown this row as gone without the file
    /// being rewritten around it.
    ///
    /// The row has to be one the table has, and every edge on it has to
    /// be going away with it. Every edge in the file names its
    /// endpoints by offset, so a row that still has one and is gone all
    /// the same is an edge that runs to nothing, which is the state a
    /// fold is careful never to leave: it prunes the edges of a
    /// tombstoned row as it rebuilds the rel table. Nothing here
    /// rebuilds anything, so what a fold would have pruned is what this
    /// wants accounted for, either by an earlier commit of this run or
    /// by `dying`, the pairs this same commit is taking away. That is
    /// what a `DETACH DELETE` is: the edges and then the row, in one
    /// transaction.
    ///
    /// The readers here are the file's, so the file's list and the
    /// patch's are asked separately and between them are the whole list
    /// a fold would have found on the row. The pairs an earlier commit
    /// of this run took away are in the patch, the ones this commit is
    /// taking away are in `dying`, and the ones it is adding are in
    /// `born`, which has to be accounted for as well: an edge written
    /// and a row deleted in one transaction is still an edge that would
    /// run to nothing.
    ///
    /// The row may be one the patch appended rather than one the file
    /// holds, which is what `appending` bounds along with the rows this
    /// same commit has put on the end so far. The file holds no list for
    /// such a row, so only the patch is asked about it.
    fn removable(
        &mut self,
        db: &mut Zu1File,
        table: u32,
        offset: u64,
        dying: &[(u32, u64, u64)],
        born: &[(u32, u64, u64)],
        appending: &IdMap<u32, u64>,
    ) -> Result<bool> {
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::load(db)?);
        }
        let catalog = self.catalog.as_ref().expect("just loaded");
        let Some(node) = catalog.node_by_id(table) else {
            return Ok(false);
        };
        let held = offset < node.node_count;
        let room = node.node_count
            + self.patches.added_rows(table)
            + appending.get(&table).copied().unwrap_or(0);
        if offset >= room {
            return Ok(false);
        }
        let rels: Vec<(u32, bool, bool)> = catalog
            .rel_tables()
            .iter()
            .filter(|rel| rel.from == table || rel.to == table)
            .map(|rel| (rel.id, rel.from == table, rel.to == table))
            .collect();
        let mut list = Vec::new();
        for (rel, out, back) in rels {
            self.load_reader(db, rel)?;
            let Some(reader) = self.readers.get(&rel) else {
                return Ok(false);
            };
            for (dir, walk) in [(Direction::Fwd, out), (Direction::Bwd, back)] {
                if !walk {
                    continue;
                }
                let patched = self.patches.edges.get(&rel).is_some_and(|patch| {
                    patch
                        .adds_on(offset, dir)
                        .any(|other| !self.gone(rel, offset, other, dir, dying))
                });
                if patched || self.arriving(rel, offset, dir, born, dying) {
                    return Ok(false);
                }
                if !held {
                    continue;
                }
                list.clear();
                reader.neighbors_dir_into(db, offset, dir, &mut list)?;
                if list
                    .iter()
                    .any(|&other| !self.gone(rel, offset, other, dir, dying))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Whether this same commit is putting an edge on `node`'s list in
    /// `dir` and leaving it there. An insert and a delete of one pair
    /// inside one transaction cancel, so what is asked of each is
    /// whether `dying` covers it.
    fn arriving(
        &self,
        rel: u32,
        node: u64,
        dir: Direction,
        born: &[(u32, u64, u64)],
        dying: &[(u32, u64, u64)],
    ) -> bool {
        born.iter().any(|&(r, src, dst)| {
            let (end, other) = match dir {
                Direction::Fwd => (src, dst),
                Direction::Bwd => (dst, src),
            };
            r == rel && end == node && !self.gone(rel, node, other, dir, dying)
        })
    }

    /// Whether the edges between `node` and `other` are already going
    /// away, either taken by an earlier commit of this run or by the
    /// commit being decided about now.
    fn gone(
        &self,
        rel: u32,
        node: u64,
        other: u64,
        dir: Direction,
        dying: &[(u32, u64, u64)],
    ) -> bool {
        let (src, dst) = match dir {
            Direction::Fwd => (node, other),
            Direction::Bwd => (other, node),
        };
        self.patches
            .edges
            .get(&rel)
            .is_some_and(|patch| patch.drops(node, other, dir))
            || dying.contains(&(rel, src, dst))
    }

    /// The labels row `offset` of `table` is left carrying, and `None`
    /// when the change has to be folded instead.
    ///
    /// A change is a pair of masks and what a reader wants is a word,
    /// so the word the row had is read and the masks go over it: out of
    /// this run's patch when a commit has already left one there, off
    /// the bitset when the file has one, and out of the catalog when
    /// neither does, a row nobody has renamed carrying the name of its
    /// own table and nothing else. That is the word a fold would have
    /// started from as well.
    ///
    /// The rules a change has to keep are the fold's, and they are
    /// asked here in the same order, because what this does with a
    /// change that breaks one is refuse to defer it. The fold then
    /// raises where it always raised, and there is one place saying
    /// what a label change may do rather than two.
    ///
    /// A table that stores nothing has no props directory to hang a
    /// bitset off, and the fold makes one for the first change that
    /// lands on it, so that one folds.
    fn marked(
        &mut self,
        db: &mut Zu1File,
        table: u32,
        offset: u64,
        add: u64,
        remove: u64,
        staged: &IdMap<(u32, u64), u64>,
    ) -> Result<Option<u64>> {
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::load(db)?);
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(table) {
            slot.insert(load_props(db, table)?);
        }
        let catalog = self.catalog.as_ref().expect("just loaded");
        let Some(node) = catalog.node_by_id(table) else {
            return Ok(None);
        };
        let primary = 1u64 << node.primary_label();
        // What the table has declared its rows may be called, which is
        // what bounds a change, and its own name, which is the one
        // label a row of it cannot be rid of.
        if add & !node.label_mask() != 0 || remove & primary != 0 {
            return Ok(None);
        }
        let graph = node.graph;
        let Some(directory) = self.dirs[&table].as_ref() else {
            return Ok(None);
        };
        let rows =
            directory.node_count + self.patches.rows.get(&table).map_or(0, |p| p.len() as u64);
        if offset >= rows {
            return Ok(None);
        }
        let held = match staged.get(&(table, offset)).copied().or_else(|| {
            self.patches
                .marks
                .get(&table)
                .and_then(|marks| marks.get(offset))
        }) {
            Some(word) => word,
            None => stored_label_word(db, directory, offset)?.unwrap_or(primary),
        };
        let after = (held | add) & !remove;
        // A closed graph type is a promise about every element the
        // graph holds, so it is asked about the word the row ends with,
        // which is this one. An element that would fall out of every
        // element type of it is the fold's refusal to raise.
        if let Some(ty) = catalog.closed_type_of(graph)
            && ty.holder(ElementKind::Node, after).is_none()
        {
            return Ok(None);
        }
        Ok(Some(after))
    }

    /// Adds a deferred commit's cells to the patch and republishes it.
    ///
    /// The patch a reader holds is the one this writes into, through
    /// [`Arc::make_mut`], so what a commit costs is what the commit
    /// changed and not what the run has accumulated. It used to be the
    /// other way around: the writer kept a running copy of everything
    /// and built a fresh patch out of the whole of it whenever a commit
    /// added to it, which is a copy of the run on every commit of the
    /// run, and quadratic in how long the run is allowed to get. That
    /// is what held the deferral bound down, because the bound is how
    /// long the run gets.
    ///
    /// A copy still happens where one is owed, and only there: a reader
    /// holding this patch is holding the database as of the commit it
    /// opened at, so the first write after it looked clones what it is
    /// looking at and leaves it alone. A reader that has moved on, and
    /// a burst with no readers in it at all, is written into in place.
    fn stage_patch(&mut self, changes: Vec<Deferred>) {
        // Borrowed a field at a time rather than through `&mut self`,
        // because the loop reads the readers and the directories while
        // it writes the patch and those are three different fields.
        let (readers, dirs, graves) = (&self.readers, &self.dirs, &mut self.graves);
        // Asked before the loop takes the changes, and the question is
        // the one [`written_cells`] answers: everything but an appended
        // row is something the patch copies.
        let copies = written_cells(&changes) > 0;
        let mut dug: Vec<u32> = Vec::new();
        let patches = Arc::make_mut(&mut self.patches);
        for change in changes {
            match change {
                Deferred::Cell((table, row, col, value)) => {
                    // A row this run appended keeps its values in the
                    // row patch, because no column holds it yet and
                    // there is nothing under it to lay a word over. So
                    // a write onto one goes there instead, which is
                    // what [`Self::patchable`] let the commit through
                    // on. Nothing else puts such a row in the lane
                    // patch, so no read has to choose between them.
                    if let Some(rows) = patches.rows.get_mut(&table)
                        && row >= rows.base()
                    {
                        let wrote = Arc::make_mut(rows).set(col as usize, row, value);
                        debug_assert!(wrote, "taken where the patch could not hold it");
                        continue;
                    }
                    let cells = Arc::make_mut(patches.cells.entry(table).or_default());
                    match value {
                        Cell::Int(word) => cells.set(col as usize, row, word),
                        Cell::Str(bytes) => cells.set_bytes(col as usize, row, bytes),
                        // A value taken away is not one of the shapes
                        // [`Self::defers`] takes, because what it
                        // changes is the validity mask.
                        Cell::Null => unreachable!("refused where the commit was taken"),
                    }
                }
                Deferred::Edge(rel, src, dst, cols) => {
                    let edges = readers[&rel].directory().edge_count;
                    Arc::make_mut(edge_patch(patches, rel, edges)).add(src, dst);
                    // The ordinal the edge took and the row number of
                    // its properties are the same number, because a rel
                    // table's columns are dense over its edges in load
                    // order and an added edge goes on the end of both.
                    if let Some(directory) = dirs.get(&rel).and_then(Option::as_ref) {
                        let width = directory.columns.len();
                        let key = key_col(directory);
                        Arc::make_mut(row_patch(patches, rel, edges, key))
                            .push(row_of(width, &cols));
                    }
                }
                Deferred::Gone(table, offset) => {
                    graves.entry(table).or_default().insert(offset);
                    if !dug.contains(&table) {
                        dug.push(table);
                    }
                }
                Deferred::DeadRel(rel, src, dst) => {
                    let edges = readers[&rel].directory().edge_count;
                    Arc::make_mut(edge_patch(patches, rel, edges)).remove(src, dst);
                }
                Deferred::Rows(table, rows) => {
                    // The base is the count the columns hold, which is
                    // the offset the first added row takes, and it is
                    // read from the props directory rather than the
                    // catalog because that is what the fold checks its
                    // own arithmetic against.
                    let Some(directory) = dirs.get(&table).and_then(Option::as_ref) else {
                        continue;
                    };
                    let (base, width) = (directory.node_count, directory.columns.len());
                    let key = key_col(directory);
                    let patch = Arc::make_mut(row_patch(patches, table, base, key));
                    for row in rows {
                        patch.push(row_of(width, &row));
                    }
                }
                Deferred::Marks(table, row, word) => {
                    Arc::make_mut(patches.marks.entry(table).or_default()).set(row, word);
                }
                // [`Self::defers`] reads the word the row had and puts
                // the masks over it, so what reaches here is the answer.
                Deferred::Labels(..) => unreachable!("composed where the commit was taken"),
                // [`Self::defers`] answers with the ordinals a pair
                // holds, so what reaches here names a row of its own.
                Deferred::RelCell(..) => unreachable!("resolved where the commit was taken"),
            }
        }
        // The tombstones alone are still rebuilt whole, because what a
        // reader wants of them is a sorted run it can binary search and
        // an offset can land anywhere in one. A run of deletes is what
        // that costs, and it is bounded by the same deferral bound as
        // everything else.
        for table in dug {
            patches
                .gone
                .insert(table, graves[&table].iter().copied().collect());
        }
        self.deferred += 1;
        if copies {
            self.copied += 1;
        }
    }

    /// Drops the patch a fold has just sealed into the columns.
    fn sealed(&mut self) {
        self.deferred = 0;
        self.copied = 0;
        self.dirs.clear();
        self.readers.clear();
        self.catalog = None;
        if !self.patches.is_empty() {
            self.graves.clear();
            self.patches = Arc::new(Patches::new());
        }
    }

    /// Seals every committed overlay into the file, publishes it and
    /// truncates the log. On return the file alone answers what the
    /// overlays answered, and the header epoch has moved, so anything
    /// caching the catalog or a decoded layout has to reload.
    pub fn fold(&mut self, db: &mut Zu1File) -> Result<()> {
        checkpoint_fold(db, &mut self.mvcc, &mut self.wal)?;
        self.sealed();
        Ok(())
    }

    /// Cuts the log back to where it stood at `floor`, which is what a
    /// rollback owes a log the folds it is undoing never truncated. A
    /// writer is dropped straight after, because the store it holds
    /// describes epochs that have stopped existing.
    pub fn discard_above(&mut self, floor: Epoch) -> Result<()> {
        self.wal.rollback_above(floor)
    }
}

/// The running edge patch of `rel`, started over a CSR holding `edges`
/// if this is the first change to it since the last fold.
fn edge_patch(patches: &mut Patches, rel: u32, edges: u64) -> &mut Arc<EdgePatch> {
    patches
        .edges
        .entry(rel)
        .or_insert_with(|| Arc::new(EdgePatch::new(edges)))
}

/// The running row patch of `table`, started over columns holding
/// `base` rows the same way.
fn row_patch(
    patches: &mut Patches,
    table: u32,
    base: u64,
    key: Option<usize>,
) -> &mut Arc<RowPatch> {
    patches
        .rows
        .entry(table)
        .or_insert_with(|| Arc::new(RowPatch::new(base, key)))
}

/// The column a key lookup on this table asks about, which is the one
/// the fold takes the key from and is named the same way the reader
/// names it.
fn key_col(directory: &PropsDirectory) -> Option<usize> {
    directory.columns.iter().position(|col| col.name == "id")
}

/// One row of a table `width` columns wide out of the cells a statement
/// named, which are the columns it named and no others. What it did not
/// name it does not hold, and an absent value is [`Cell::Null`].
fn row_of(width: usize, cols: &[(u32, Cell)]) -> Vec<Cell> {
    (0..width)
        .map(|at| {
            cols.iter()
                .find(|(c, _)| *c as usize == at)
                .map_or(Cell::Null, |(_, cell)| cell.clone())
        })
        .collect()
}

/// Whether a column can be handed this value without being rewritten:
/// a word goes over the word a lane column holds, and bytes go over the
/// bytes a blob column holds. Nothing goes over a value that is not
/// there, which is why an absent one is refused here.
///
/// A zoned column takes neither. It rides the lane and its value is two
/// numbers, and a cell is one, so a word written over it would keep the
/// instant and lose the zone. Nothing makes such a cell today, because
/// no statement writes a zoned value, and saying so here is what keeps
/// it that way rather than leaving a silent half write for whichever
/// path reaches it first.
fn holds(column: &PropColumn, value: &Cell) -> bool {
    if zoned(&column.ty) {
        return false;
    }
    match value {
        Cell::Int(_) => column.is_lane(),
        Cell::Str(_) => !column.is_lane(),
        Cell::Null => false,
    }
}

/// How many bytes of string a commit's changes carry, which is what the
/// patch would have to hold onto until the next fold.
fn written_bytes(changes: &[Deferred]) -> usize {
    changes
        .iter()
        .map(|change| match change {
            Deferred::Cell((.., Cell::Str(bytes))) => bytes.len(),
            Deferred::RelCell((.., Cell::Str(bytes))) => bytes.len(),
            Deferred::Edge(_, _, _, cols) => cols
                .iter()
                .map(|(_, cell)| match cell {
                    Cell::Str(bytes) => bytes.len(),
                    _ => 0,
                })
                .sum(),
            Deferred::Rows(_, rows) => rows
                .iter()
                .flatten()
                .map(|(_, cell)| match cell {
                    Cell::Str(bytes) => bytes.len(),
                    _ => 0,
                })
                .sum(),
            _ => 0,
        })
        .sum()
}

/// How many cells a commit's changes would put in the patch, which is
/// the other thing the run of them is bounded by. Most shapes are one
/// cell each; rows added to a node table are as many as there are, and
/// an edge either way is none, because both of those go into parts of
/// the patch a copy shares rather than copies.
fn written_cells(changes: &[Deferred]) -> usize {
    changes
        .iter()
        .map(|change| match change {
            Deferred::Rows(..) | Deferred::Edge(..) | Deferred::DeadRel(..) => 0,
            _ => 1,
        })
        .sum()
}

/// How many edges a commit's changes would put in the patch or take
/// out of it, which is bounded with the appended rows. See
/// [`Patches::edges`].
fn written_edges(changes: &[Deferred]) -> usize {
    changes
        .iter()
        .map(|change| match change {
            Deferred::Edge(..) | Deferred::DeadRel(..) => 1,
            _ => 0,
        })
        .sum()
}

/// How many rows a commit's changes append past the end of a table,
/// which is the other half of the same question and bounded on its own
/// because it costs something else. See [`DEFERRED_ROWS`].
fn written_rows(changes: &[Deferred]) -> usize {
    changes
        .iter()
        .map(|change| match change {
            Deferred::Rows(_, rows) => rows.len(),
            Deferred::Edge(..) => 1,
            _ => 0,
        })
        .sum()
}

/// Refuses a handle nothing may be written through.
///
/// A statement that writes asks this before it works out what it would
/// write, so that a caller holding a read-only connection is told what
/// is wrong with the connection rather than what is wrong with the
/// statement.
pub(crate) fn writable(db: &Zu1File) -> Result<()> {
    if db.is_writable() {
        return Ok(());
    }
    Err(ZuError::InvalidArgument(format!(
        "cannot write to {}, which is open read-only",
        db.path().display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Value;
    use crate::session::Session;
    use crate::zu1::catalog::Catalog;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::{PropValues, store_props};
    use crate::zu1::txn::Cell;

    /// Four people with an age and a name, and three edges between
    /// them. Names are the observable throughout: they are strings, so
    /// they come out of the blob side of the props store, which is the
    /// side a fold has to rebuild rather than overwrite in place.
    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
    }

    /// The same four people, with a year and a note on every edge. The
    /// note is a string, so an edge added to this table has to land a
    /// cell on both sides of the props store: the lane a word goes
    /// straight onto, and the blob a byte string comes out of.
    fn seeded_dated(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
        let notes: Vec<&[u8]> = vec![b"school", b"work", b"club"];
        crate::zu1::props::store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1990, 2000, 2010])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .expect("edge props");
    }

    /// Four people linked in a line and a fifth with nobody, because a
    /// plain `DELETE` takes away an element that has no edges on it and
    /// refuses one that has.
    fn seeded_loner(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 5, &[(0, 1), (1, 2), (2, 3)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy", b"zoe"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40, 50])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
    }

    /// What a run of folds is allowed to add to the file, which is a
    /// share of the file and not a number of blocks.
    ///
    /// It was a number of blocks, a flat 256, and the difference is a
    /// store of 3.9 MB that ran to 71.8 before the first checkpoint
    /// published anything: the threshold had been set when blocks were
    /// coming back mid transaction and it was never what bounded the
    /// file, so nothing noticed it was eighteen times too loose for a
    /// small store. Now it is a quarter, and the floor and the ceiling
    /// are what a quarter has to be held between: a small store folds a
    /// handful of blocks at a time and a quarter of it would checkpoint
    /// every other fold, and a store big enough that one fold is a
    /// quarter of it would checkpoint every fold, which is the two
    /// syncs a statement that the deferred path exists to avoid.
    #[test]
    fn the_checkpoint_threshold_is_a_share_of_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("threshold.zu1");
        let mut db = Zu1File::create(&path).expect("create");

        let block = u64::from(crate::zu1::BLOCK_SIZE);
        let floor_blocks = FLOOR_BYTES.div_ceil(block);
        let ceiling_blocks = CEILING_BYTES.div_ceil(block);

        // A file of nothing. The floor is what answers, because a
        // quarter of nothing would checkpoint on the first block.
        db.db_header_mut().block_count = 0;
        for _ in 0..floor_blocks - 1 {
            db.allocate_block();
            assert!(!checkpoint_due(&db), "under the floor is not due");
        }
        db.allocate_block();
        assert!(checkpoint_due(&db), "the floor is due");

        // A file big enough that the share is what answers, and small
        // enough that it is still under the ceiling.
        let mut db = Zu1File::create(&dir.path().join("big.zu1")).expect("create");
        db.db_header_mut().block_count = floor_blocks * GROWTH_SHARE * 4;
        let mut taken = 0;
        while !checkpoint_due(&db) {
            db.allocate_block();
            taken += 1;
            assert!(taken < 1000, "the share has to fall due somewhere");
        }
        // A block taken past the end of the file is also a block the
        // file is longer by, so the two climb together and meet where a
        // quarter of what the file has become is what has been taken
        // out of it.
        assert_eq!(taken, db.db_header().block_count / GROWTH_SHARE);
        assert!(
            taken > floor_blocks && taken < ceiling_blocks,
            "neither end of the clamp answered this one: {taken}"
        );

        // And a file so big that a quarter of it is more garbage than
        // anything wants to carry, where the ceiling answers instead.
        let mut db = Zu1File::create(&dir.path().join("huge.zu1")).expect("create");
        db.db_header_mut().block_count = 1_000_000;
        for _ in 0..ceiling_blocks - 1 {
            db.allocate_block();
        }
        assert!(!checkpoint_due(&db), "under the ceiling is not due");
        db.allocate_block();
        assert!(checkpoint_due(&db), "the ceiling is due whatever the share");
    }

    fn string(row: &[Value], at: usize) -> String {
        match &row[at] {
            Value::Str(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    fn names(session: &mut Session) -> Vec<String> {
        let r = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        r.rows
            .iter()
            .map(|row| match &row[0] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a string name, got {other:?}"),
            })
            .collect()
    }

    fn person_and_knows(session: &Session) -> (u32, u32) {
        let catalog = session.catalog();
        (
            catalog.node_by_name("person").expect("person").id,
            catalog.rel_by_name("knows").expect("knows").id,
        )
    }

    /// A row committed through the write side is a row the next query
    /// on the same session reads, which is the whole contract this
    /// module exists to hold.
    #[test]
    fn a_committed_row_is_there_for_the_next_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);

        let (person, _) = person_and_knows(&session);
        let rows = session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(50)]),
                        (1, vec![Cell::Str(b"eva".to_vec())]),
                    ],
                )
            })
            .expect("write");
        assert_eq!(rows, 1);
        assert_eq!(names(&mut session), ["ada", "amy", "eva", "joe", "kay"]);
    }

    /// Eight connections writing the same file at once end with every
    /// row they inserted there, once each.
    ///
    /// This is the shape group commit changed. A writer stages its
    /// frames, gives the write side back, and only then waits for the
    /// platter, so the next writer is staging while the sync of the
    /// last one is in the air and one sync commits them both. What that
    /// must not cost is any of this: every commit still lands, and the
    /// state a reader picks up is one a crash could not take back.
    #[test]
    fn eight_connections_writing_one_file_all_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("burst.zu1");
        seeded(&path);

        const WRITERS: u64 = 8;
        const EACH: u64 = 10;
        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let path = path.as_path();
                scope.spawn(move || {
                    let mut session = Session::open(path).expect("open");
                    let (person, _) = person_and_knows(&session);
                    for i in 0..EACH {
                        let age = 1000 + writer * EACH + i;
                        session
                            .write(|txn| {
                                txn.insert_nodes(
                                    person,
                                    vec![
                                        (0, vec![Cell::Int(age)]),
                                        (1, vec![Cell::Str(b"new".to_vec())]),
                                    ],
                                )
                            })
                            .expect("write");
                    }
                });
            }
        });

        let mut session = Session::open(&path).expect("open");
        let rows = session
            .run(
                "MATCH (p:person) WHERE p.age >= 1000 RETURN p.age AS age",
                &[],
            )
            .expect("read")
            .rows;
        let mut ages: Vec<u64> = rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(age) => age as u64,
                ref other => panic!("expected an age, got {other:?}"),
            })
            .collect();
        ages.sort_unstable();
        let want: Vec<u64> = (1000..1000 + WRITERS * EACH).collect();
        assert_eq!(ages, want, "every commit landed exactly once");
    }

    /// An edge committed the same way is an edge a hop walks.
    #[test]
    fn a_committed_edge_is_one_the_next_hop_walks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // The three seeded edges run ada to kay to joe to amy, so
        // every name but ada's is somebody's neighbor. Closing the
        // cycle with amy to ada is what brings ada into the answer.
        let hop = "MATCH (a:person)-[:knows]->(b:person) RETURN b.name AS name ORDER BY name";
        let reached = |session: &mut Session| -> Vec<String> {
            session
                .run(hop, &[])
                .expect("read")
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::Str(s) => s.clone(),
                    other => panic!("expected a string name, got {other:?}"),
                })
                .collect()
        };
        assert_eq!(reached(&mut session), ["amy", "joe", "kay"]);

        let (_, knows) = person_and_knows(&session);
        session
            .write(|txn| {
                txn.insert_rel(knows, 3, 0);
                Ok(())
            })
            .expect("write");
        assert_eq!(reached(&mut session), ["ada", "amy", "joe", "kay"]);
    }

    /// An edge insert is a commit, not a fold: the epoch stays where it
    /// was, and the edge is read out of the overlay, in both
    /// directions, carrying the cells it was written with.
    ///
    /// This is the whole point of the deferral. A fold rebuilds both
    /// adjacency sides and rewrites every edge property column into the
    /// new edge order, which is work linear in the edges already there,
    /// so an insert that folds costs more the bigger the table gets.
    #[test]
    fn an_edge_insert_is_read_before_it_is_folded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-defer.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        // The first write opens the writer, which recovers and folds,
        // so the epoch to hold against is the one after it.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        // amy to ada closes the cycle the three seeded edges leave
        // open, and it is the pair that sorts first in ada's backward
        // list, so the merge has to place it rather than append it.
        session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'amy' AND b.name = 'ada' \
                 INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)",
                &[],
            )
            .expect("write an edge");
        assert_eq!(session.epoch(), before, "an edge insert folded");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, b.name AS to, k.since AS since, k.note AS note \
                 ORDER BY src",
                &[],
            )
            .expect("walk forward");
        let forward: Vec<(String, String, Value, String)> = out
            .rows
            .iter()
            .map(|row| {
                (
                    string(row, 0),
                    string(row, 1),
                    row[2].clone(),
                    string(row, 3),
                )
            })
            .collect();
        assert_eq!(
            forward,
            [
                (
                    "ada".into(),
                    "kay".into(),
                    Value::Int(1990),
                    "school".into()
                ),
                ("amy".into(), "ada".into(), Value::Int(2020), "gym".into()),
                ("joe".into(), "amy".into(), Value::Int(2010), "club".into()),
                ("kay".into(), "joe".into(), Value::Int(2000), "work".into()),
            ]
        );

        // Backward, ada is now somebody's neighbor, and the ordinal the
        // added edge took has to resolve to the cells it was written
        // with rather than to the seeded edge that shares its slot.
        let back = session
            .run(
                "MATCH (a:person)<-[k:knows]-(b:person) WHERE a.name = 'ada' \
                 RETURN b.name AS src, k.since AS since, k.note AS note",
                &[],
            )
            .expect("walk backward");
        assert_eq!(back.rows.len(), 1);
        assert_eq!(string(&back.rows[0], 0), "amy");
        assert_eq!(back.rows[0][1], Value::Int(2020));
        assert_eq!(string(&back.rows[0], 2), "gym");

        // And a degree is the merged one, on both sides.
        let counts = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN count(b) AS n",
                &[],
            )
            .expect("count");
        assert_eq!(counts.rows[0][0], Value::Int(4));
    }

    /// What the overlay answered, the file answers on its own once the
    /// fold has sealed it, at an epoch that has moved.
    #[test]
    fn a_folded_edge_reads_the_same_as_the_deferred_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-fold.zu1");
        seeded_dated(&path);

        let years = |session: &mut Session| -> Vec<(String, i64)> {
            session
                .run(
                    "MATCH (a:person)-[k:knows]->(b:person) \
                     RETURN a.name AS src, k.since AS since ORDER BY src",
                    &[],
                )
                .expect("read")
                .rows
                .iter()
                .map(|row| match row[1] {
                    Value::Int(n) => (string(row, 0), n),
                    ref other => panic!("expected a year, got {other:?}"),
                })
                .collect()
        };

        let mut session = Session::open(&path).expect("open");
        session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'amy' AND b.name = 'ada' \
                 INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)",
                &[],
            )
            .expect("write an edge");
        let deferred = years(&mut session);
        let before = session.epoch();

        // Taking the file folds what the writer was holding.
        session.file_mut().expect("fold");
        assert_eq!(years(&mut session), deferred);
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A year written onto an edge that is already there is the write a
    /// linkbench update of a link's payload is, and it is the one shape
    /// the patch had to learn the ordinal for: a statement names the
    /// edge by the pair it runs between, and the column it writes into
    /// is in edge order.
    #[test]
    fn a_year_written_onto_an_edge_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-set.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        let before = session.epoch();
        session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'ada' SET k.since = 1991",
                &[],
            )
            .expect("write a year");
        assert_eq!(session.epoch(), before, "an edge property set folded");

        // Forward, where the ordinal is the slot the expand walked to,
        // and backward, where it is the one the lookup worked out. The
        // string beside it is untouched, which is what says the write
        // landed on the column it named and not on the row.
        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'ada' \
                 RETURN k.since AS since, k.note AS note",
                &[],
            )
            .expect("read forward");
        assert_eq!(out.rows[0][0], Value::Int(1991));
        assert_eq!(string(&out.rows[0], 1), "school");

        let back = session
            .run(
                "MATCH (a:person)<-[k:knows]-(b:person) WHERE b.name = 'ada' \
                 RETURN k.since AS since",
                &[],
            )
            .expect("read backward");
        assert_eq!(back.rows[0][0], Value::Int(1991));

        // And the other edges kept what they held, which is what says
        // the ordinal was the pair's own and not the first of the table.
        let all = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("read every edge");
        let years: Vec<(String, Value)> = all
            .rows
            .iter()
            .map(|row| (string(row, 0), row[1].clone()))
            .collect();
        assert_eq!(
            years,
            [
                ("ada".into(), Value::Int(1991)),
                ("joe".into(), Value::Int(2010)),
                ("kay".into(), Value::Int(2000)),
            ]
        );

        // What the overlay answered, the file answers on its own.
        session.file_mut().expect("fold");
        let folded = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("read every edge again");
        assert_eq!(
            folded
                .rows
                .iter()
                .map(|row| (string(row, 0), row[1].clone()))
                .collect::<Vec<_>>(),
            years
        );
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A name written onto a row that is already there is the write a
    /// linkbench update of a node's payload is, and it lands on the blob
    /// side of the props store: the value that goes over is as long as
    /// the statement wrote it rather than a word, so the range a scan
    /// reads has to be rebuilt around it and not written into.
    #[test]
    fn a_name_written_onto_a_row_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("name-set.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // The first write opens the writer, which recovers and folds,
        // so the epoch to hold against is the one after it.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        // Longer than the name it goes over, so every name behind kay's
        // in the column moves along the buffer the scan reads.
        session
            .run(
                "MATCH (p:person) WHERE p.age = 20 SET p.name = 'katherine'",
                &[],
            )
            .expect("write a name");
        assert_eq!(session.epoch(), before, "a string write folded");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "katherine"]);

        // Found by a filter as well as returned by a scan, and the age
        // beside it is untouched, which is what says the write landed on
        // the column it named.
        let out = session
            .run(
                "MATCH (p:person) WHERE p.name = 'katherine' RETURN p.age AS age",
                &[],
            )
            .expect("read it back");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], Value::Int(20));

        // What the overlay answered, the file answers on its own.
        session.file_mut().expect("fold");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "katherine"]);
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// The same onto an edge, which is the pair worked into its ordinal
    /// and then the write above.
    #[test]
    fn a_note_written_onto_an_edge_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note-set.zu1");
        seeded_dated(&path);

        let notes = |session: &mut Session| -> Vec<(String, String)> {
            session
                .run(
                    "MATCH (a:person)-[k:knows]->(b:person) \
                     RETURN a.name AS src, k.note AS note ORDER BY src",
                    &[],
                )
                .expect("read")
                .rows
                .iter()
                .map(|row| (string(row, 0), string(row, 1)))
                .collect()
        };

        let mut session = Session::open(&path).expect("open");
        let before = session.epoch();
        session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'ada' \
                 SET k.note = 'university'",
                &[],
            )
            .expect("write a note");
        assert_eq!(session.epoch(), before, "an edge string write folded");
        assert_eq!(
            notes(&mut session),
            [
                ("ada".to_string(), "university".to_string()),
                ("joe".into(), "club".into()),
                ("kay".into(), "work".into()),
            ]
        );

        // The year beside it kept what it held, and so did the other
        // edges, which is what says the ordinal was the pair's own.
        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'ada' \
                 RETURN k.since AS since",
                &[],
            )
            .expect("read the year");
        assert_eq!(out.rows[0][0], Value::Int(1990));

        let deferred = notes(&mut session);
        session.file_mut().expect("fold");
        assert_eq!(notes(&mut session), deferred);
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A string long enough to fill the patch on its own folds, which is
    /// what keeps a run of large payload writes from carrying them all
    /// until the next checkpoint.
    #[test]
    fn a_string_too_long_to_carry_folds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("long-name.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        let long = "z".repeat(DEFERRED_BYTES + 1);
        session
            .run(
                &format!("MATCH (p:person) WHERE p.age = 20 SET p.name = '{long}'"),
                &[],
            )
            .expect("write a long name");
        assert!(session.epoch() > before, "the write did not fold");

        let out = session
            .run(
                "MATCH (p:person) WHERE p.age = 20 RETURN p.name AS name",
                &[],
            )
            .expect("read it back");
        assert_eq!(string(&out.rows[0], 0), long);
    }

    /// A row taken away is gone to every read without the file being
    /// rewritten around it.
    ///
    /// A delete moves nothing: the offsets stay where they are, the
    /// fold writes them into the table's tombstone chain, and every
    /// read filters by that chain. So the patch carries what the chain
    /// would have held and the readers merge the two, which is the same
    /// answer for the cost of a walk of two sorted lists.
    #[test]
    fn a_row_taken_away_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("delete-defer.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        // The first write opens the writer, which recovers and folds,
        // so the epoch to hold against is the one after it.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        session
            .run("MATCH (p:person) WHERE p.age = 50 DELETE p", &[])
            .expect("take zoe away");
        assert_eq!(session.epoch(), before, "a delete folded");

        // Out of a scan of the table, out of a filter over the column
        // the row was found by, and out of the count the table answers.
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
        let out = session
            .run(
                "MATCH (p:person) WHERE p.age = 50 RETURN p.name AS name",
                &[],
            )
            .expect("read the row that went");
        assert!(out.rows.is_empty(), "a deleted row came back");
        let counted = session
            .run("MATCH (p:person) RETURN count(p) AS n", &[])
            .expect("count");
        assert_eq!(counted.rows[0][0], Value::Int(4));

        // And the rows around it kept their offsets, which is what says
        // nothing was compacted underneath.
        let ages = session
            .run("MATCH (p:person) RETURN p.age AS age ORDER BY age", &[])
            .expect("read the ages");
        assert_eq!(
            ages.rows
                .iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>(),
            [
                Value::Int(11),
                Value::Int(20),
                Value::Int(30),
                Value::Int(40)
            ]
        );

        // What the patch answered, the chain answers on its own.
        session.file_mut().expect("fold");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// An edge taken away on its own is read out of the patch: it is
    /// out of the list it was in on both sides, out of the degree the
    /// two ends answer, and the edges it left behind still carry the
    /// values they were loaded with.
    ///
    /// That last part is what says the ordinals did not move. A fold
    /// rebuilds the rel table around the edge it drops and renumbers
    /// everything after it; nothing here rebuilds anything, so an edge
    /// the delete did not name is exactly where it was.
    #[test]
    fn an_edge_taken_away_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-delete.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        // ada to kay is the first edge of the table, so what follows it
        // is what a fold would have renumbered.
        session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'ada' DELETE k",
                &[],
            )
            .expect("take the edge away");
        assert_eq!(session.epoch(), before, "an edge delete folded");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, b.name AS to, k.since AS since, k.note AS note \
                 ORDER BY src",
                &[],
            )
            .expect("walk forward");
        let forward: Vec<(String, String, Value, String)> = out
            .rows
            .iter()
            .map(|row| {
                (
                    string(row, 0),
                    string(row, 1),
                    row[2].clone(),
                    string(row, 3),
                )
            })
            .collect();
        assert_eq!(
            forward,
            [
                ("joe".into(), "amy".into(), Value::Int(2010), "club".into()),
                ("kay".into(), "joe".into(), Value::Int(2000), "work".into()),
            ]
        );

        // Backward, kay has nobody pointing at her any more, which is
        // the other list the pair had to come out of.
        let back = session
            .run(
                "MATCH (a:person)<-[:knows]-(b:person) WHERE a.name = 'kay' \
                 RETURN b.name AS src",
                &[],
            )
            .expect("walk backward");
        assert!(back.rows.is_empty(), "the edge came back backward");

        // And the degrees the two ends answer are the merged ones.
        let counted = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN count(b) AS n",
                &[],
            )
            .expect("count");
        assert_eq!(counted.rows[0][0], Value::Int(2));

        // What the patch answered, the file answers on its own.
        session.file_mut().expect("fold");
        let folded = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("read every edge again");
        assert_eq!(
            folded
                .rows
                .iter()
                .map(|row| (string(row, 0), row[1].clone()))
                .collect::<Vec<_>>(),
            [
                ("joe".into(), Value::Int(2010)),
                ("kay".into(), Value::Int(2000)),
            ]
        );
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A `DETACH DELETE` is the two of them in one transaction: the
    /// edges on the row and then the row, and neither folds.
    ///
    /// The row is only removable once its edges are accounted for, and
    /// they are accounted for by the same commit, so this is what says
    /// the writer looks at the whole transaction rather than at one
    /// change at a time.
    #[test]
    fn a_detach_delete_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("detach-delete.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        // kay is in the middle of the line, so she has an edge on each
        // side and the delete has both lists to take her out of.
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' DETACH DELETE p", &[])
            .expect("detach kay");
        assert_eq!(session.epoch(), before, "a detach delete folded");

        assert_eq!(names(&mut session), ["ada", "amy", "joe"]);
        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, b.name AS to, k.since AS since ORDER BY src",
                &[],
            )
            .expect("walk forward");
        let forward: Vec<(String, String, Value)> = out
            .rows
            .iter()
            .map(|row| (string(row, 0), string(row, 1), row[2].clone()))
            .collect();
        assert_eq!(forward, [("joe".into(), "amy".into(), Value::Int(2010))]);

        // ada pointed at kay and points at nobody now, which is the
        // list the deleted row was the far end of.
        let ada = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) WHERE a.name = 'ada' \
                 RETURN count(b) AS n",
                &[],
            )
            .expect("count ada's edges");
        assert_eq!(ada.rows[0][0], Value::Int(0));

        // What the patch answered, the file answers on its own.
        session.file_mut().expect("fold");
        assert_eq!(names(&mut session), ["ada", "amy", "joe"]);
        let folded = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("read every edge again");
        assert_eq!(
            folded
                .rows
                .iter()
                .map(|row| (string(row, 0), row[1].clone()))
                .collect::<Vec<_>>(),
            [("joe".into(), Value::Int(2010))]
        );
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A row added to a node table is read out of the patch: by a scan
    /// of the table, by a filter over a column, by the count the table
    /// answers, and by a hop that walks past it.
    #[test]
    fn a_row_added_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert-defer.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        session
            .run("INSERT (p:person {age: 60, name: 'eva'})", &[])
            .expect("insert eva");
        assert_eq!(session.epoch(), before, "an insert folded");

        assert_eq!(
            names(&mut session),
            ["ada", "amy", "eva", "joe", "kay", "zoe"]
        );
        let found = session
            .run(
                "MATCH (p:person) WHERE p.age = 60 RETURN p.name AS name",
                &[],
            )
            .expect("read the new row by its age");
        assert_eq!(found.rows.len(), 1);
        assert_eq!(string(&found.rows[0], 0), "eva");
        let counted = session
            .run("MATCH (p:person) RETURN count(p) AS n", &[])
            .expect("count");
        assert_eq!(counted.rows[0][0], Value::Int(6));

        // The row is in no adjacency list, and asking the CSR about a
        // row it was not built over is what the reader has to answer
        // rather than refuse.
        let hops = session
            .run(
                "MATCH (p:person)-[:knows]->(q:person) RETURN count(*) AS n",
                &[],
            )
            .expect("walk");
        assert_eq!(hops.rows[0][0], Value::Int(3));

        // What the patch answered, the columns answer on their own.
        session.file_mut().expect("fold");
        assert_eq!(
            names(&mut session),
            ["ada", "amy", "eva", "joe", "kay", "zoe"]
        );
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// A run of them lands in order and reads back in order, which is
    /// what says the patch appends where the fold would have.
    #[test]
    fn a_run_of_added_rows_keeps_the_order_it_was_written_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert-run.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        for (age, name) in [(60, "eva"), (70, "raj"), (80, "ann")] {
            session
                .run(
                    &format!("INSERT (p:person {{age: {age}, name: '{name}'}})"),
                    &[],
                )
                .expect("insert");
        }
        assert_eq!(session.epoch(), before, "an insert folded");

        let want = [
            Value::Int(11),
            Value::Int(20),
            Value::Int(30),
            Value::Int(40),
            Value::Int(50),
            Value::Int(60),
            Value::Int(70),
            Value::Int(80),
        ];
        let ages = |session: &mut Session| {
            session
                .run("MATCH (p:person) RETURN p.age AS age ORDER BY age", &[])
                .expect("read the ages")
                .rows
                .iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ages(&mut session), want);
        session.file_mut().expect("fold");
        assert_eq!(ages(&mut session), want);
    }

    /// An edge onto a row the patch is carrying is patched too. The CSR
    /// was built over the rows the file held and has nowhere to put a
    /// list for the new row, so the reader is told how far past the CSR
    /// the table now runs and answers for that row out of the patch,
    /// where its only edges are.
    #[test]
    fn an_edge_onto_a_row_the_patch_is_carrying_is_patched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert-then-link.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("INSERT (p:person {age: 60, name: 'eva'})", &[])
            .expect("insert eva");
        let before = session.epoch();

        session
            .run(
                "MATCH (a:person), (e:person) WHERE a.name = 'ada' AND e.name = 'eva' \
                 INSERT (a)-[:knows]->(e)",
                &[],
            )
            .expect("link ada to eva");
        assert_eq!(session.epoch(), before, "the edge folded");

        let hops = session
            .run(
                "MATCH (p:person)-[:knows]->(q:person) WHERE q.name = 'eva' \
                 RETURN p.name AS name",
                &[],
            )
            .expect("walk to eva");
        assert_eq!(hops.rows.len(), 1);
        assert_eq!(string(&hops.rows[0], 0), "ada");

        // Forward off the new row, which is the direction the CSR has no
        // offset for at all, and the degree that goes with it.
        let back = session
            .run(
                "MATCH (q:person)<-[:knows]-(p:person) WHERE q.name = 'eva' \
                 RETURN p.name AS name",
                &[],
            )
            .expect("walk back from eva");
        assert_eq!(back.rows.len(), 1);
        assert_eq!(string(&back.rows[0], 0), "ada");

        // And it survives the fold, laid out where a rebuild puts it.
        let long = "z".repeat(DEFERRED_BYTES + 1);
        session
            .run(
                &format!("MATCH (p:person) WHERE p.name = 'ada' SET p.name = '{long}'"),
                &[],
            )
            .expect("a change that folds");
        let after = session
            .run(
                "MATCH (p)-[:knows]->(q:person) WHERE q.name = 'eva' \
                 RETURN q.age AS age",
                &[],
            )
            .expect("walk to eva after the fold");
        assert_eq!(after.rows.len(), 1);
    }

    /// One statement that writes a row and an edge onto it is patched
    /// whole. The edge is decided about while the row it names is only
    /// in the changes beside it, not in the file and not in the patch,
    /// so what says the row is there is the count this commit is
    /// carrying.
    #[test]
    fn a_row_and_an_edge_onto_it_in_one_statement_are_patched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert-both.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        // A first deferred write, so the run is up and the epoch the
        // assert below reads is a settled one.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        session
            .run(
                "MATCH (a:person) WHERE a.name = 'ada' \
                 INSERT (e:person {age: 60, name: 'eva'}), (a)-[:knows]->(e)",
                &[],
            )
            .expect("insert eva and the edge onto her");
        assert_eq!(session.epoch(), before, "the statement folded");

        let hops = session
            .run(
                "MATCH (p:person)-[:knows]->(q:person) WHERE q.name = 'eva' \
                 RETURN p.name AS name",
                &[],
            )
            .expect("walk to eva");
        assert_eq!(hops.rows.len(), 1);
        assert_eq!(string(&hops.rows[0], 0), "ada");
    }

    /// An edge this run of deferred commits added is taken away without
    /// a fold, and so is the row it ran onto. That is the whole of the
    /// shape a benchmark harness writes in, a scratch row created,
    /// written over and taken away again, and none of the four
    /// statements folds.
    ///
    /// The edge comes back out of the patch's lists rather than being
    /// laid over them as gone, and the row it ran onto is one the patch
    /// appended, so what says either is gone is the patch alone. The
    /// fold at the end is what says the two paths agree.
    #[test]
    fn a_bracket_of_a_row_an_edge_and_both_away_never_folds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bracket.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        for round in 0..3 {
            session
                .run("INSERT (p:person {age: 60, name: 'eva'})", &[])
                .expect("the scratch row");
            session
                .run(
                    "MATCH (a:person), (e:person) WHERE a.name = 'ada' AND e.name = 'eva' \
                     INSERT (a)-[:knows]->(e)",
                    &[],
                )
                .expect("the edge onto it");
            let seen = session
                .run(
                    "MATCH (a:person)-[:knows]->(q:person) WHERE q.name = 'eva' \
                     RETURN count(a) AS n",
                    &[],
                )
                .expect("count the edge");
            assert_eq!(seen.rows[0][0], Value::Int(1), "round {round}");
            session
                .run("MATCH (e:person) WHERE e.name = 'eva' DETACH DELETE e", &[])
                .expect("take both away");
            assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay", "zoe"]);
            assert_eq!(session.epoch(), before, "round {round} folded");
        }

        // ada is where she started, with the one edge the file gave her
        // and nothing the bracket left behind, and the fold agrees.
        let count = |session: &mut Session| {
            session
                .run(
                    "MATCH (a:person)-[:knows]->(b:person) WHERE a.name = 'ada' \
                     RETURN count(b) AS n",
                    &[],
                )
                .expect("count ada's edges")
                .rows[0][0]
                .clone()
        };
        assert_eq!(count(&mut session), Value::Int(1));
        session.file_mut().expect("fold");
        assert_eq!(count(&mut session), Value::Int(1));
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay", "zoe"]);
    }

    /// A row the patch appended and an edge onto it in one transaction,
    /// and the delete of the row in another, with the edge left for the
    /// delete to find. A `DETACH DELETE` stages the edge and the row
    /// together, so this is the same question asked of a patch the
    /// commit before left behind rather than of the commit's own
    /// changes.
    #[test]
    fn a_patched_row_with_a_patched_edge_on_it_is_deleted_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("patched-detach.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        session
            .run(
                "MATCH (a:person) WHERE a.name = 'ada' \
                 INSERT (e:person {age: 60, name: 'eva'}), (a)-[:knows]->(e)",
                &[],
            )
            .expect("the row and the edge");
        session
            .run("MATCH (e:person) WHERE e.name = 'eva' DETACH DELETE e", &[])
            .expect("take both away");
        assert_eq!(session.epoch(), before, "the bracket folded");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay", "zoe"]);
    }

    /// A row the patch appended that still has an edge on it folds
    /// rather than being carried away, the same as one the file holds.
    /// An edge in the file names its ends by offset, so a row that goes
    /// with one still on it is an edge running to nothing.
    #[test]
    fn a_patched_row_with_an_edge_left_on_it_folds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("patched-dangling.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        let (person, _) = person_and_knows(&session);
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        session
            .run(
                "MATCH (a:person) WHERE a.name = 'ada' \
                 INSERT (e:person {age: 60, name: 'eva'}), (a)-[:knows]->(e)",
                &[],
            )
            .expect("the row and the edge");
        let before = session.epoch();

        // Straight at the write side, because a `DELETE` statement is
        // refused a row that still has an edge and a `DETACH DELETE`
        // takes the edge with it, so neither reaches the question.
        session
            .write(|txn| {
                txn.delete(person, 5);
                Ok(())
            })
            .expect("delete the row on its own");
        assert!(
            session.epoch() > before,
            "a row with an edge still on it did not fold"
        );
    }

    /// A row holding nothing in a column that cannot hold nothing
    /// folds. The patch carries a cell per row and no mask, so what
    /// says the row is empty there is the cell, and a reader only asks
    /// that of a column it has been told may hold a null. The fold is
    /// what gives the column the mask, and after it the two answers
    /// agree.
    ///
    /// A statement cannot write this: it is refused a table with a
    /// nullable column and refused a row that skips one, so the path is
    /// the write side's own.
    #[test]
    fn a_row_holding_nothing_in_a_column_that_cannot_folds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("insert-null.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        let (person, _) = person_and_knows(&session);
        session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![(0, vec![Cell::Null]), (1, vec![Cell::Str(b"eva".to_vec())])],
                )
            })
            .expect("insert eva with no age");
        assert!(session.epoch() > before, "a row with a hole in it deferred");
        assert_eq!(
            names(&mut session),
            ["ada", "amy", "eva", "joe", "kay", "zoe"]
        );
        let ages = session
            .run(
                "MATCH (p:person) WHERE p.name = 'eva' RETURN p.age AS age",
                &[],
            )
            .expect("read the age it does not have");
        assert_eq!(ages.rows[0][0], Value::Null);
    }

    /// An element that still has edges folds rather than deferring,
    /// because it does not get as far as the write side: an edge names
    /// its endpoints by offset, so a row that is gone with an edge on it
    /// is an edge that runs to nothing, and a plain `DELETE` refuses it.
    #[test]
    fn a_row_with_an_edge_on_it_is_refused_rather_than_carried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("delete-attached.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        let err = session
            .run("MATCH (p:person) WHERE p.age = 20 DELETE p", &[])
            .expect_err("kay is in the middle of the line");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("G1001"));
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay", "zoe"]);
    }

    /// The part of a statement after a delete reads the row as gone,
    /// whether the delete folded or is still in the patch. An edge onto
    /// it is refused rather than written into the lists, because an edge
    /// to a row nobody can read is one no reader can resolve.
    #[test]
    fn an_edge_onto_a_row_the_same_statement_took_away_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("delete-then-link.zu1");
        seeded_loner(&path);

        let mut session = Session::open(&path).expect("open");
        let err = session
            .run(
                "MATCH (a:person), (z:person) WHERE a.name = 'ada' AND z.name = 'zoe' \
                 DELETE z INSERT (a)-[:knows]->(z)",
                &[],
            )
            .expect_err("the far end is gone");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("G1002"));
        assert!(err.to_string().contains("taken away"), "got: {err}");
    }

    /// A year written onto an edge this same unfolded run added goes
    /// over the row the patch appended for it, and only that column of
    /// it: the note stays what the insert carried.
    #[test]
    fn a_year_written_onto_an_edge_just_added_leaves_the_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-set-fresh.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        let before = session.epoch();
        session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'amy' AND b.name = 'ada' \
                 INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)",
                &[],
            )
            .expect("write an edge");
        assert_eq!(session.epoch(), before, "an edge insert folded");

        session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'amy' SET k.since = 2021",
                &[],
            )
            .expect("write a year onto it");
        assert_eq!(session.epoch(), before, "the write folded");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE a.name = 'amy' \
                 RETURN k.since AS since, k.note AS note",
                &[],
            )
            .expect("read it back");
        assert_eq!(out.rows[0][0], Value::Int(2021));
        assert_eq!(string(&out.rows[0], 1), "gym");
    }

    /// A transaction whose staging raises leaves the database exactly
    /// as it was, including the epoch: nothing partial, nothing
    /// logged, nothing to undo.
    #[test]
    fn a_transaction_that_raises_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("raise.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let (person, _) = person_and_knows(&session);
        let before = session.epoch();

        let err = session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(50)]),
                        (1, vec![Cell::Str(b"eva".to_vec())]),
                    ],
                )?;
                Err::<(), _>(ZuError::InvalidArgument("changed my mind".into()))
            })
            .expect_err("the staging raised");
        assert!(err.to_string().contains("changed my mind"), "got: {err}");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
        assert_eq!(session.epoch(), before, "a rollback published an epoch");
    }

    /// A transaction that stages nothing publishes nothing. It is not
    /// an error to run one, and it must not cost an epoch, or a
    /// statement that turns out to match no rows would churn the file.
    #[test]
    fn a_transaction_that_stages_nothing_publishes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let before = session.epoch();
        session.write(|_| Ok(())).expect("write");
        assert_eq!(session.epoch(), before);
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
    }

    /// How many people the file on disk says there are, read through a
    /// handle of its own, so a session holding the same path with folds
    /// it has not checkpointed does not answer for it.
    fn published_people(path: &Path) -> u64 {
        let mut db = Zu1File::open(path).expect("open");
        Catalog::load(&mut db)
            .expect("catalog")
            .node_by_name("person")
            .expect("person")
            .node_count
    }

    /// The visibility contract holds without the header flip: a row
    /// committed and folded is a row the next query reads, and the file
    /// on disk is still the one from before it, because the fold
    /// stopped short of publishing.
    #[test]
    fn a_fold_that_did_not_publish_is_read_and_is_not_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("staged.zu1");
        seeded(&path);
        assert_eq!(published_people(&path), 4);

        let mut session = Session::open(&path).expect("open");
        let (person, _) = person_and_knows(&session);
        session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(50)]),
                        (1, vec![Cell::Str(b"eva".to_vec())]),
                    ],
                )
            })
            .expect("write");
        assert_eq!(names(&mut session), ["ada", "amy", "eva", "joe", "kay"]);
        assert_eq!(
            published_people(&path),
            4,
            "a statement checkpointed under the threshold"
        );

        // The log is what makes that safe, so a process that goes away
        // here has lost nothing.
        drop(session);
        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(names(&mut session), ["ada", "amy", "eva", "joe", "kay"]);
    }

    /// Closing publishes. Nothing would be lost if it did not, because
    /// the log holds every commit the folds staged, but a process that
    /// only writes would leave a log as long as its life and hand the
    /// next open a replay to match.
    #[test]
    fn a_session_that_closes_leaves_nothing_staged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("close.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let (person, _) = person_and_knows(&session);
        for age in [50u64, 60, 70] {
            session
                .write(|txn| {
                    txn.insert_nodes(
                        person,
                        vec![
                            (0, vec![Cell::Int(age)]),
                            (1, vec![Cell::Str(b"eva".to_vec())]),
                        ],
                    )
                })
                .expect("write");
        }
        drop(session);

        assert_eq!(published_people(&path), 7);
        // The file keeps the room it took, so what says the log is
        // empty is what is in it rather than how long it is.
        let log = crate::zu1::wal::Wal::open(&sidecar(&path)).expect("the log is there");
        assert!(log.is_empty(), "the close left frames in the log");
    }

    /// A statement that writes twice and then raises is undone whole,
    /// and it is undone whole after a crash too. Neither fold published,
    /// so what has to be taken back is the log rather than the header,
    /// and the marker the second commit wrote is what says how far.
    #[test]
    fn a_crash_inside_a_statement_that_wrote_twice_takes_both_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("half.zu1");
        seeded(&path);

        {
            let mut db = Zu1File::open(&path).expect("open");
            let person = Catalog::load(&mut db)
                .expect("catalog")
                .node_by_name("person")
                .expect("person")
                .id;
            let mut writer = Writer::open(&mut db).expect("writer");
            db.begin_savepoint(false, writer.epoch()).expect("hold");
            for name in [b"eva", b"raj"] {
                writer
                    .write(&mut db, |txn| {
                        txn.insert_nodes(
                            person,
                            vec![
                                (0, vec![Cell::Int(50)]),
                                (1, vec![Cell::Str(name.to_vec())]),
                            ],
                        )
                    })
                    .expect("write");
            }
            // The statement raises here and the process dies before it
            // can say so.
        }

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
        drop(session);
        crate::zu1::verify(&path).expect("verify");
    }

    /// A log left beside the file by a process that committed and then
    /// died before checkpointing is sealed when the next session opens
    /// over it, so the reader sees the committed row rather than the
    /// state the writer had already left.
    #[test]
    fn a_log_left_behind_by_a_crash_is_sealed_on_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash.zu1");
        seeded(&path);

        {
            let mut db = Zu1File::open(&path).expect("open");
            let person = Catalog::load(&mut db)
                .expect("catalog")
                .node_by_name("person")
                .expect("person")
                .id;
            let mut wal = Wal::open(&sidecar(&path)).expect("wal");
            let mut mvcc = recover(&mut db, &mut wal).expect("recover");
            let mut txn = mvcc.begin();
            txn.insert_nodes(
                person,
                vec![
                    (0, vec![Cell::Int(60)]),
                    (1, vec![Cell::Str(b"raj".to_vec())]),
                ],
            )
            .expect("stage");
            txn.commit(&mut wal).expect("commit");
            // No fold, no checkpoint: the process dies here.
        }

        let mut session = Session::open(&path).expect("open");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay", "raj"]);
    }

    /// A session hands its file out to an appender, and the log goes
    /// with it: the appender opens the same sidecar, so a writer still
    /// holding one would have two handles on a file one of them is
    /// about to truncate. Writing after an appender has been and gone
    /// has to work, which is what says the writer was dropped and
    /// reopened rather than kept across the borrow.
    #[test]
    fn an_appender_and_a_write_statement_share_one_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("both.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let (person, _) = person_and_knows(&session);
        session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(50)]),
                        (1, vec![Cell::Str(b"eva".to_vec())]),
                    ],
                )
            })
            .expect("write");

        let mut appender =
            crate::append::Appender::open(session.file_mut().expect("publish"), "person")
                .expect("open the appender");
        appender.append_row((70i64, "raj")).expect("append");
        appender.close().expect("close");

        session
            .write(|txn| {
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(80)]),
                        (1, vec![Cell::Str(b"sol".to_vec())]),
                    ],
                )
            })
            .expect("write after the appender");
        assert_eq!(
            names(&mut session),
            ["ada", "amy", "eva", "joe", "kay", "raj", "sol"]
        );
    }

    /// Ages in order, the observable for the writes below: `age` is an
    /// integer column with a value in every row, which is the one shape
    /// a commit can leave for a later fold to seal.
    fn ages(session: &mut Session) -> Vec<i64> {
        let r = session
            .run("MATCH (p:person) RETURN p.age AS age ORDER BY age", &[])
            .expect("read");
        r.rows
            .iter()
            .map(|row| match &row[0] {
                Value::Int(age) => *age,
                other => panic!("expected an integer age, got {other:?}"),
            })
            .collect()
    }

    /// A point write is read by the next statement without a fold
    /// having sealed it. The epoch is what says so: a fold moves it,
    /// and moving it is what throws away the plan cache, the catalog
    /// and every decoded chunk the session was holding.
    #[test]
    fn a_point_write_is_read_before_it_is_folded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("defer.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // The first write opens the writer, which recovers and folds,
        // so the epoch to hold against is the one after it.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        session
            .run("MATCH (p:person) WHERE p.age = 20 SET p.age = 21", &[])
            .expect("second write");
        assert_eq!(session.epoch(), before, "a point write folded");
        assert_eq!(ages(&mut session), [11, 21, 30, 40]);
    }

    /// A written value outside the column's stored bounds is still
    /// found. The zone map is read off the sealed segment and a scan
    /// skips a column it says cannot hold the value, so an unsealed
    /// write has to widen it or the row goes missing.
    #[test]
    fn a_write_outside_the_zone_is_still_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("zone.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 999", &[])
            .expect("write");
        let r = session
            .run(
                "MATCH (p:person) WHERE p.age = 999 RETURN p.name AS name",
                &[],
            )
            .expect("read");
        assert_eq!(r.rows.len(), 1, "the write is outside the stored zone");
        assert_eq!(ages(&mut session), [20, 30, 40, 999]);
    }

    /// A run of added edges longer than [`DEFERRED_COPIED`] does not
    /// fold, because an added edge is bounded with the appended rows
    /// rather than with the overwrites.
    ///
    /// It used to be bounded with the overwrites, and for a reason that
    /// has gone: the patch held its lists as plain maps and copied them
    /// whole every time a commit added to one, so the length of the run
    /// was what the copy cost. The lists are a sealed run with the
    /// recent edges beside them now, so a commit copies a few dozen of
    /// them however many the run has reached. The epoch is what says the
    /// bound moved with it: a fold moves the epoch, and this run is over
    /// the copied bound and under the appended one.
    #[test]
    fn a_run_of_added_edges_outlasts_the_copied_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edges.zu1");
        let nodes = u64::from(DEFERRED_COPIED) + 32;
        {
            let mut db = Zu1File::create(&path).expect("create");
            bulk_load_as(&mut db, "person", "knows", nodes, &[(0, 1)]).expect("load");
            let ages: Vec<u64> = (0..nodes).collect();
            store_props(&mut db, "person", &[("age", PropValues::Int(&ages))]).expect("props");
        }

        let mut session = Session::open(&path).expect("open");
        // The first write opens the writer, which recovers and folds,
        // so the epoch to hold against is the one after it.
        let edge = |to: i64| {
            format!(
                "MATCH (a:person), (b:person) WHERE a.age = 0 AND b.age = {to} \
                 INSERT (a)-[:knows]->(b)"
            )
        };
        session.run(&edge(2), &[]).expect("first write");
        let before = session.epoch();

        let added = (nodes - 3) as usize;
        for to in 3..nodes as i64 {
            session.run(&edge(to), &[]).expect("edge");
        }
        assert!(
            added > DEFERRED_COPIED as usize,
            "{added} edges is not past the copied bound"
        );
        assert_eq!(
            session.epoch(),
            before,
            "a run of {added} added edges folded"
        );

        // And they are all there, which is what says the run was
        // deferred rather than dropped.
        let r = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) WHERE a.age = 0 \
                 RETURN count(b) AS n",
                &[],
            )
            .expect("count");
        assert_eq!(r.rows[0][0], Value::Int(added as i64 + 2));
    }

    /// An edge taken away and put back over and over does not fold,
    /// even over a pair the file itself holds.
    ///
    /// This is the shape a bracketed write has: a benchmark that
    /// measures an insert deletes what it inserted before it measures
    /// the next one, so the same pair goes round for the length of the
    /// run. The delete leaves a mark saying the file's copies of the
    /// pair are gone, and the add that follows used to be refused
    /// because the file still held them, which folded, which sealed the
    /// new edge into the file, which made the next one fold as well. The
    /// mark is what the add reads now.
    #[test]
    fn an_edge_taken_away_and_put_back_does_not_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cycle.zu1");
        {
            let mut db = Zu1File::create(&path).expect("create");
            bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1)]).expect("load");
            let ages: Vec<u64> = (0..4).collect();
            store_props(&mut db, "person", &[("age", PropValues::Int(&ages))]).expect("props");
        }

        let mut session = Session::open(&path).expect("open");
        let drop_it = "MATCH (a:person)-[k:knows]->(b:person) \
                       WHERE a.age = 0 AND b.age = 1 DELETE k";
        let add_it = "MATCH (a:person), (b:person) WHERE a.age = 0 AND b.age = 1 \
                      INSERT (a)-[:knows]->(b)";
        let count = |session: &mut Session| {
            let r = session
                .run(
                    "MATCH (a:person)-[:knows]->(b:person) WHERE a.age = 0 \
                     RETURN count(b) AS n",
                    &[],
                )
                .expect("count");
            r.rows[0][0].clone()
        };

        // The first write opens the writer, which recovers and folds, so
        // the epoch to hold against is the one after it.
        session.run(drop_it, &[]).expect("first delete");
        let before = session.epoch();
        assert_eq!(count(&mut session), Value::Int(0));

        let rounds = 200;
        for _ in 0..rounds {
            session.run(add_it, &[]).expect("add");
            assert_eq!(count(&mut session), Value::Int(1), "the edge is not back");
            session.run(drop_it, &[]).expect("drop");
            assert_eq!(count(&mut session), Value::Int(0), "the edge is not gone");
        }
        assert_eq!(
            session.epoch(),
            before,
            "{rounds} rounds of one edge going away and coming back folded"
        );

        // And the pair ends where the last statement left it rather than
        // where the file has it, which is what says the mark outlived
        // the adds that went on top of it.
        session.run(add_it, &[]).expect("add");
        assert_eq!(count(&mut session), Value::Int(1));
    }

    /// A write onto a row the same unfolded run appended does not fold
    /// either, which is the node side of the same thing.
    #[test]
    fn a_write_onto_a_row_the_run_appended_does_not_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("row-set.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        // Past the chunk the row patch fills, so the writes land on
        // sealed chunks as well as on the one being filled.
        let rows = 100;
        for at in 0..rows {
            session
                .run(
                    "INSERT (:person {age: $a, name: $n})",
                    &[
                        ("a", Value::Int(1000 + at)),
                        ("n", Value::Str(format!("new{at}"))),
                    ],
                )
                .expect("insert");
            session
                .run(
                    "MATCH (p:person) WHERE p.age = $a SET p.age = $b, p.name = $n",
                    &[
                        ("a", Value::Int(1000 + at)),
                        ("b", Value::Int(2000 + at)),
                        ("n", Value::Str(format!("set{at}"))),
                    ],
                )
                .expect("set");
        }
        assert_eq!(
            session.epoch(),
            before,
            "{rows} rows appended and written over folded"
        );

        // What the writes left is what a read gets, and what the fold
        // seals: the appended row carries the value the `SET` put on it
        // rather than the one the `INSERT` did.
        let check = |session: &mut Session| {
            let out = session
                .run(
                    "MATCH (p:person) WHERE p.age >= 2000 \
                     RETURN p.age AS age, p.name AS name ORDER BY age",
                    &[],
                )
                .expect("read");
            let seen: Vec<(Value, String)> = out
                .rows
                .iter()
                .map(|row| (row[0].clone(), string(row, 1)))
                .collect();
            let want: Vec<(Value, String)> = (0..rows)
                .map(|at| (Value::Int(2000 + at), format!("set{at}")))
                .collect();
            assert_eq!(seen, want);
            // And nothing of the rows the file came with moved.
            let out = session
                .run(
                    "MATCH (p:person) WHERE p.age < 100 RETURN p.age AS age ORDER BY age",
                    &[],
                )
                .expect("read the old rows");
            let ages: Vec<Value> = out.rows.iter().map(|row| row[0].clone()).collect();
            assert_eq!(
                ages,
                [
                    Value::Int(11),
                    Value::Int(20),
                    Value::Int(30),
                    Value::Int(40)
                ]
            );
        };
        check(&mut session);
        drop(session);
        let mut session = Session::open(&path).expect("reopen");
        check(&mut session);
    }

    /// A write onto an edge the same unfolded run added does not fold,
    /// and what it wrote is what a read gets and what the fold seals.
    ///
    /// This is the whole of a bracketed write: the insert that sets the
    /// edge up, the write being measured onto it, and the delete that
    /// takes it away, round after round over one pair. The write in the
    /// middle used to fold because the edge's values are in the row the
    /// patch appended rather than in the column, and a value aimed at
    /// the column would have been written where nothing reads it. It
    /// goes over the appended row now.
    #[test]
    fn a_write_onto_an_edge_the_run_added_does_not_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edge-set.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        let add_it = "MATCH (a:person), (b:person) \
                      WHERE a.name = 'amy' AND b.name = 'ada' \
                      INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)";
        let drop_it = "MATCH (a:person)-[k:knows]->(b:person) \
                       WHERE a.name = 'amy' AND b.name = 'ada' DELETE k";
        let read_it = |session: &mut Session| {
            let out = session
                .run(
                    "MATCH (a:person)-[k:knows]->(b:person) \
                     WHERE a.name = 'amy' AND b.name = 'ada' \
                     RETURN k.since AS since, k.note AS note",
                    &[],
                )
                .expect("read the edge");
            assert_eq!(out.rows.len(), 1, "the edge is not there");
            (out.rows[0][0].clone(), string(&out.rows[0], 1))
        };

        // The first write opens the writer, which recovers and folds, so
        // the epoch to hold against is the one after it.
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 11", &[])
            .expect("first write");
        let before = session.epoch();

        let rounds = 40;
        for round in 0..rounds {
            session.run(add_it, &[]).expect("add");
            assert_eq!(read_it(&mut session), (Value::Int(2020), "gym".into()));
            let year = 3000 + round;
            session
                .run(
                    "MATCH (a:person)-[k:knows]->(b:person) \
                     WHERE a.name = 'amy' AND b.name = 'ada' \
                     SET k.since = $y, k.note = $n",
                    &[("y", Value::Int(year)), ("n", Value::Str("pub".into()))],
                )
                .expect("set");
            assert_eq!(read_it(&mut session), (Value::Int(year), "pub".into()));
            session.run(drop_it, &[]).expect("drop");
            session.run(add_it, &[]).expect("add back");
            // Put back with the values the insert carries, not the ones
            // the write left on the copy that went away.
            assert_eq!(read_it(&mut session), (Value::Int(2020), "gym".into()));
            session.run(drop_it, &[]).expect("drop again");
        }
        assert_eq!(
            session.epoch(),
            before,
            "{rounds} rounds of insert, write and delete over one edge folded"
        );

        // And what the last write left is what the fold seals, which
        // reopening is the check on: it reads the file and the log and
        // nothing this session was holding.
        session.run(add_it, &[]).expect("add");
        session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 WHERE a.name = 'amy' AND b.name = 'ada' \
                 SET k.since = 2077, k.note = 'last'",
                &[],
            )
            .expect("set");
        drop(session);
        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(read_it(&mut session), (Value::Int(2077), "last".into()));
        // And the edges the file came with are where they were.
        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("walk");
        let seen: Vec<(String, Value)> = out
            .rows
            .iter()
            .map(|row| (string(row, 0), row[1].clone()))
            .collect();
        assert_eq!(
            seen,
            [
                ("ada".into(), Value::Int(1990)),
                ("amy".into(), Value::Int(2077)),
                ("joe".into(), Value::Int(2010)),
                ("kay".into(), Value::Int(2000)),
            ]
        );
    }

    /// A run of point writes longer than a writer will defer folds
    /// somewhere in the middle, and what it sealed is what the writes
    /// left. Reopening is the check: it reads the file and the log and
    /// nothing this session was holding.
    #[test]
    fn a_long_run_of_point_writes_seals_what_it_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("run.zu1");
        seeded(&path);

        {
            let mut session = Session::open(&path).expect("open");
            let first = session.epoch();
            for i in 0..DEFERRED_COMMITS + 8 {
                let age = 100 + i64::from(i);
                session
                    .run(
                        &format!("MATCH (p:person) WHERE p.name = 'ada' SET p.age = {age}"),
                        &[],
                    )
                    .expect("write");
            }
            assert!(
                session.epoch() > first,
                "a run that long folded nothing at all"
            );
            let last = 100 + i64::from(DEFERRED_COMMITS + 7);
            assert_eq!(ages(&mut session), [20, 30, 40, last]);
        }

        let mut session = Session::open(&path).expect("reopen");
        let last = 100 + i64::from(DEFERRED_COMMITS + 7);
        assert_eq!(ages(&mut session), [20, 30, 40, last]);
    }

    /// A point write is durable before it is sealed, the same as any
    /// other: the frame is synced at commit, so a process that dies
    /// with the fold still owed replays it on the next open.
    #[test]
    fn a_point_write_survives_a_crash_before_the_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-point.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 77", &[])
            .expect("write");
        // The process dies here: no fold, no checkpoint, and no drop
        // either, because dropping the session is what publishes.
        std::mem::forget(session);
        crate::shared::forget(&path);

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(ages(&mut session), [20, 30, 40, 77]);
    }

    /// So is an edge insert. The pair and the cells it carries are in
    /// the frame the commit synced, so a process that dies with the
    /// fold still owed comes back to an edge that is there.
    #[test]
    fn an_edge_insert_survives_a_crash_before_the_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-edge.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'amy' AND b.name = 'ada' \
                 INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)",
                &[],
            )
            .expect("write an edge");
        std::mem::forget(session);
        crate::shared::forget(&path);

        let mut session = Session::open(&path).expect("reopen");
        let out = session
            .run(
                "MATCH (a:person)<-[k:knows]-(b:person) WHERE a.name = 'ada' \
                 RETURN b.name AS src, k.since AS since, k.note AS note",
                &[],
            )
            .expect("read");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(string(&out.rows[0], 0), "amy");
        assert_eq!(out.rows[0][1], Value::Int(2020));
        assert_eq!(string(&out.rows[0], 2), "gym");
    }

    /// And so does the twentieth of them, which is the one that used to
    /// come back to a file that would not open at all.
    ///
    /// Every statement holds the file for the length of itself, and a
    /// statement that folds without publishing leaves the blocks it
    /// replaced free with nothing on disk saying so: the header a crash
    /// finds is the older one and it still reads them. Handing those
    /// back to allocation at the end of the statement put a rebuilt
    /// adjacency segment on top of the table index the durable header
    /// names, so the file read back as corrupt rather than as the graph
    /// the log describes. One statement never showed it because one
    /// statement has nothing to hand back yet.
    #[test]
    fn a_run_of_edge_inserts_survives_a_crash_before_the_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-edges.zu1");
        seeded_loner(&path);

        let people = ["ada", "kay", "joe", "amy", "zoe"];
        let mut session = Session::open(&path).expect("open");
        for i in 0..20 {
            let (from, to) = (people[i % 5], people[(i + 2) % 5]);
            session
                .run(
                    &format!(
                        "MATCH (a:person), (b:person) \
                         WHERE a.name = '{from}' AND b.name = '{to}' \
                         INSERT (a)-[:knows]->(b)"
                    ),
                    &[],
                )
                .expect("write an edge");
        }
        std::mem::forget(session);
        crate::shared::forget(&path);

        let mut session = Session::open(&path).expect("reopen");
        let out = session
            .run(
                "MATCH (:person)-[:knows]->(:person) RETURN count(*) AS edges",
                &[],
            )
            .expect("read");
        // The three the fixture loaded and the twenty the statements
        // wrote, every one of them in a frame its commit synced.
        assert_eq!(out.rows[0][0], Value::Int(23));
    }

    /// And so is a detach delete. The pairs and the row are in the
    /// frame the commit synced, so the fold recovery does on the way
    /// back in leaves the graph the statement left behind.
    #[test]
    fn a_detach_delete_survives_a_crash_before_the_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-detach.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' DETACH DELETE p", &[])
            .expect("detach kay");
        std::mem::forget(session);
        crate::shared::forget(&path);

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(names(&mut session), ["ada", "amy", "joe"]);
        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) \
                 RETURN a.name AS src, k.since AS since ORDER BY src",
                &[],
            )
            .expect("read");
        assert_eq!(
            out.rows
                .iter()
                .map(|row| (string(row, 0), row[1].clone()))
                .collect::<Vec<_>>(),
            [("joe".into(), Value::Int(2010))]
        );
    }

    /// A point write inside a transaction that rolls back goes away,
    /// which is the one thing the readers holding the unsealed cells
    /// must not keep.
    #[test]
    fn a_rolled_back_point_write_goes_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollback-point.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("MATCH (p:person) WHERE p.age = 10 SET p.age = 88", &[])
            .expect("write");
        assert_eq!(ages(&mut session), [20, 30, 40, 88]);
        session.run("ROLLBACK", &[]).expect("roll back");
        assert_eq!(ages(&mut session), [10, 20, 30, 40]);
    }

    /// And an edge insert inside one goes away with it, adjacency and
    /// cells together, which is what says the overlay is dropped rather
    /// than only the log frame.
    #[test]
    fn a_rolled_back_edge_insert_goes_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollback-edge.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        let into_ada = |session: &mut Session| -> usize {
            session
                .run(
                    "MATCH (a:person)<-[:knows]-(b:person) WHERE a.name = 'ada' RETURN b.name",
                    &[],
                )
                .expect("read")
                .rows
                .len()
        };

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'amy' AND b.name = 'ada' \
                 INSERT (a)-[:knows {since: 2020, note: 'gym'}]->(b)",
                &[],
            )
            .expect("write an edge");
        assert_eq!(into_ada(&mut session), 1);
        session.run("ROLLBACK", &[]).expect("roll back");
        assert_eq!(into_ada(&mut session), 0);
    }

    /// And a detach delete inside one comes back whole, the row and the
    /// edges that ran onto it, which is the same thing the other way
    /// round: what the patch was carrying is dropped with the epochs
    /// the rollback took.
    #[test]
    fn a_rolled_back_detach_delete_comes_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollback-detach.zu1");
        seeded_dated(&path);

        let mut session = Session::open(&path).expect("open");
        let hops = |session: &mut Session| -> usize {
            session
                .run("MATCH (a:person)-[:knows]->(b:person) RETURN a.name", &[])
                .expect("read")
                .rows
                .len()
        };

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' DETACH DELETE p", &[])
            .expect("detach kay");
        assert_eq!(names(&mut session), ["ada", "amy", "joe"]);
        assert_eq!(hops(&mut session), 1);
        session.run("ROLLBACK", &[]).expect("roll back");
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);
        assert_eq!(hops(&mut session), 3);
    }

    /// The rows a pattern finds by a label it was given without a fold,
    /// which is the label change read out of the patch.
    fn bots(session: &mut Session) -> Vec<String> {
        let out = session
            .run("MATCH (p:Bot) RETURN p.name AS name ORDER BY name", &[])
            .expect("read the bots");
        out.rows.iter().map(|row| string(row, 0)).collect()
    }

    /// A label put on a row is read out of the patch: the pattern that
    /// names it finds the row, the rows it did not name are not found,
    /// and the properties of all of them are where they were.
    ///
    /// The first change is not the one this is about. A table that has
    /// never been named a label has no bitset and has declared no bit,
    /// so the first one widens the catalog and makes the bitset, and
    /// both of those are folds. What is measured here is the second.
    #[test]
    fn a_label_put_on_a_row_needs_no_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("label-set.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Bot", &[])
            .expect("declare the label and make the bitset");
        let before = session.epoch();

        session
            .run("MATCH (p:person) WHERE p.name = 'joe' SET p:Bot", &[])
            .expect("put it on a second row");
        assert_eq!(session.epoch(), before, "a label set folded");
        assert_eq!(bots(&mut session), ["ada", "joe"]);

        // A label is a bit beside the row rather than anything in a
        // column, so nothing a column holds moved.
        assert_eq!(ages(&mut session), [10, 20, 30, 40]);
        assert_eq!(names(&mut session), ["ada", "amy", "joe", "kay"]);

        // And off again, over the word the patch is already carrying,
        // which is the half of it a second read of the file would get
        // wrong.
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' REMOVE p:Bot", &[])
            .expect("take it off again");
        assert_eq!(session.epoch(), before, "a label remove folded");
        assert_eq!(bots(&mut session), ["joe"]);

        // What the patch answered, the bitset answers on its own.
        session.file_mut().expect("fold");
        assert_eq!(bots(&mut session), ["joe"]);
        assert!(session.epoch() > before, "a fold published no epoch");
    }

    /// Two labels of one row, put on by two statements, both land: the
    /// second composes over the word the first left rather than over
    /// the word the bitset holds.
    #[test]
    fn a_second_label_on_one_row_goes_over_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("label-two.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Bot", &[])
            .expect("the first label of the table");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Admin", &[])
            .expect("the second, which the catalog widens for");
        let before = session.epoch();

        session
            .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Bot", &[])
            .expect("one label on another row");
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Admin", &[])
            .expect("and the other on the same row");
        assert_eq!(session.epoch(), before, "a label set folded");

        assert_eq!(bots(&mut session), ["ada", "kay"]);
        let admins = session
            .run("MATCH (p:Admin) RETURN p.name AS name ORDER BY name", &[])
            .expect("read the admins");
        assert_eq!(
            admins
                .rows
                .iter()
                .map(|row| string(row, 0))
                .collect::<Vec<_>>(),
            ["ada", "kay"],
            "the second label did not take the first off"
        );
    }

    /// A label change is in the log before it is in the patch, so a
    /// crash before the fold brings it back with everything else.
    #[test]
    fn a_label_change_survives_a_crash_before_the_fold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-label.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Bot", &[])
            .expect("declare the label");
        session
            .run("MATCH (p:person) WHERE p.name = 'joe' SET p:Bot", &[])
            .expect("the one that is only in the patch");
        std::mem::forget(session);
        crate::shared::forget(&path);

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(bots(&mut session), ["ada", "joe"]);
    }

    /// And one inside a transaction that rolls back goes away, which is
    /// the word the patch was carrying being dropped with the epoch.
    #[test]
    fn a_rolled_back_label_change_goes_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollback-label.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Bot", &[])
            .expect("declare the label");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Bot", &[])
            .expect("a second bot");
        assert_eq!(bots(&mut session), ["ada", "kay"]);
        session.run("ROLLBACK", &[]).expect("roll back");
        assert_eq!(bots(&mut session), ["ada"]);
    }
}
