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
//! when the folds since the last one have taken
//! [`THRESHOLD_BLOCKS`] blocks. Nothing is at risk in between: the
//! frame is the durability point, the header on disk still names the
//! epoch the last checkpoint folded through, and a crash replays the
//! log from there back to where the folds had got to. What it costs is
//! file growth, because a block freed by an unpublished fold cannot be
//! handed out again, and that is what the threshold bounds.
//!
//! [`Zu1Graph`]: crate::query::Zu1Graph

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zu_common::{Epoch, Result, ZuError};

use crate::append::sidecar;
use crate::deleted::Tombstones;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{checkpoint_fold, recover, staged_fold};
use crate::zu1::graph::{Direction, EdgePatch, GraphReader};
use crate::zu1::props::{
    CellPatch, PropColumn, PropsDirectory, RowPatch, load_props, load_rel_props,
};
use crate::zu1::txn::{Cell, Deferred, Mvcc, WriteTxn};
use crate::zu1::wal::{Commits, Wal};

/// How much a run of folds may take before one of them checkpoints.
///
/// Every fold in a run takes fresh blocks for what it rewrites, so this
/// is the file growth a writer that never stops is allowed to carry,
/// 16 MiB at the 256 KiB block. A one cell write folds a handful of
/// blocks, so a statement pays the two syncs of a checkpoint about
/// once in a dozen and the log stays about that long, which is also
/// about how much a recovery has to replay.
const THRESHOLD_BLOCKS: u64 = 256;

/// How many commits in a row may go without a fold.
///
/// A deferred commit is a frame in the log and a handful of cells in
/// the overlay, and neither is freed until a fold takes them, so this
/// is what a recovery has to replay and what the patch below has to
/// carry. It is deliberately well short of what the log can hold: the
/// point is to take the fold off the statement, and folding one
/// statement in a few hundred does that whatever the number is.
const DEFERRED_COMMITS: u32 = 256;

/// How many cells the deferred commits may hold between them.
///
/// The patch is rebuilt when a commit adds to it, so this bounds the
/// per-commit cost of deferring as well as the memory: a few hundred
/// cells is a copy of a few kilobytes, against the column rewrite and
/// the two segment writes a fold costs.
const DEFERRED_CELLS: usize = 1024;

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
/// Four shapes, because there are four ways a change can sit above a
/// column nobody has rewritten: a new value for a row the column
/// already holds, a whole new row past the end of it, a new edge,
/// which is a row of the rel table's property columns and a pair the
/// adjacency reader has to merge into two lists, and a row taken away,
/// which is an offset every read of the table filters by.
#[derive(Debug, Default)]
pub struct Patches {
    /// New values, by table.
    pub cells: HashMap<u32, Arc<CellPatch>>,
    /// New rows, by table, node tables and rel tables alike. A rel
    /// table's are the property rows of the edges in `edges`, under
    /// the ordinals those took. A node table's are rows of the table
    /// itself, so they move the count every reader is bounded by, and
    /// everything that reads a bound reads it through this.
    pub rows: HashMap<u32, Arc<RowPatch>>,
    /// New edges, by rel table.
    pub edges: HashMap<u32, Arc<EdgePatch>>,
    /// Rows taken away, by table, ascending. A delete moves nothing:
    /// the offsets stay where they are and the fold writes them into
    /// the table's tombstone chain, so what the patch carries is what
    /// the chain would have held.
    pub gone: Tombstones,
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
    }

    /// How many cells everything in here holds between them, which is
    /// what a writer bounds when it decides whether to keep deferring.
    fn cells(&self) -> usize {
        self.cells.values().map(|p| p.cells()).sum::<usize>()
            + self.rows.values().map(|p| p.len()).sum::<usize>()
            + self.edges.values().map(|p| p.len() as usize).sum::<usize>()
            + self.gone.values().map(|rows| rows.len()).sum::<usize>()
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
    /// The words the commits since the last fold wrote, by table and
    /// then by column, which is the running form the patch below is
    /// built from.
    pending: HashMap<u32, BTreeMap<usize, BTreeMap<u64, u64>>>,
    /// The strings they wrote, the same way. Apart from the words
    /// because the patch keeps them apart, and the two never name the
    /// same column: a column stores one kind or the other.
    strings: HashMap<u32, BTreeMap<usize, BTreeMap<u64, Vec<u8>>>>,
    /// The same cells as a reader takes them. Rebuilt whenever a
    /// deferred commit adds to `pending`, and shared from there: a
    /// reader holds the `Arc` and hands copies of it to the workers a
    /// query forks.
    patches: Arc<Patches>,
    /// How many commits have gone without a fold.
    deferred: u32,
    /// The rows the deferred commits added, by table, in the running
    /// form the patch above is built from.
    added: HashMap<u32, RowPatch>,
    /// The edges they added, by rel table, the same way.
    fresh: HashMap<u32, EdgePatch>,
    /// The rows they took away, by table. A set, because a row can be
    /// deleted twice and the second one takes nothing away, and sorted
    /// because that is the order the readers merge it in.
    graves: HashMap<u32, BTreeSet<u64>>,
    /// Adjacency readers of the rel tables a deferred commit has added
    /// an edge to, which is what says whether the pair is already
    /// there and how many edges the file holds. A fold moves what
    /// these describe, so it empties this.
    readers: HashMap<u32, GraphReader>,
    /// The catalog those readers were loaded through, for the same
    /// stretch and dropped at the same fold.
    catalog: Option<Catalog>,
    /// Props directories of the tables a deferred commit has written
    /// into, which is how a commit is checked against the columns it
    /// names without reading the directory chain per statement. A fold
    /// moves the roots these describe, so it empties this.
    dirs: HashMap<u32, Option<PropsDirectory>>,
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
        let mut wal = Wal::open(&path)?;
        let mvcc = recover(db, &mut wal)?;
        let mut writer = Writer {
            wal,
            mvcc,
            path,
            pending: HashMap::new(),
            strings: HashMap::new(),
            patches: Arc::new(Patches::new()),
            deferred: 0,
            added: HashMap::new(),
            fresh: HashMap::new(),
            graves: HashMap::new(),
            readers: HashMap::new(),
            catalog: None,
            dirs: HashMap::new(),
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
                match db.unpublished_blocks() >= THRESHOLD_BLOCKS {
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
    /// Five shapes need no fold. A value written onto a row that is
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
    /// the file. Anything else, a label, an edge taken away, folds the
    /// way it always did. On top of the shape
    /// there are four bounds: how many commits may go unfolded, how
    /// many cells they may hold, how many bytes of string among them,
    /// and the block growth the checkpoint threshold already bounds.
    fn defers(
        &mut self,
        db: &mut Zu1File,
        changes: Vec<Deferred>,
    ) -> Result<Option<Vec<Deferred>>> {
        if changes.is_empty() || !self.mvcc.soft() {
            return Ok(None);
        }
        if self.deferred >= DEFERRED_COMMITS
            || self.patches.cells() + written_cells(&changes) > DEFERRED_CELLS
            || self.patches.bytes() + written_bytes(&changes) > DEFERRED_BYTES
            || db.unpublished_blocks() >= THRESHOLD_BLOCKS
        {
            return Ok(None);
        }
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
                    if !self.edge_patchable(db, rel, src, dst, cols)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::Gone(table, offset) => {
                    if !self.removable(db, table, offset)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
                Deferred::Rows(table, ref rows) => {
                    if !self.appendable(db, table, rows)? {
                        return Ok(None);
                    }
                    taken.push(change);
                }
            }
        }
        Ok(Some(taken))
    }

    /// The rows of `rel`'s property columns that `value` written onto
    /// the edges of `src -> dst` lands on, and `None` when the write has
    /// to be folded instead.
    ///
    /// A pair this run of deferred commits added is turned away. Its
    /// values are in the rows the patch appended rather than in the
    /// column underneath, so a value aimed at the column would be
    /// written where nothing reads it, and folding is the cheap answer
    /// to a write onto an edge that has not been sealed yet.
    fn edge_cells(
        &mut self,
        db: &mut Zu1File,
        rel: u32,
        src: u64,
        dst: u64,
        col: usize,
        value: &Cell,
    ) -> Result<Option<std::ops::Range<u64>>> {
        if self.fresh.get(&rel).is_some_and(|p| p.holds(src, dst)) {
            return Ok(None);
        }
        self.load_reader(db, rel)?;
        let Some(reader) = self.readers.get_mut(&rel) else {
            return Ok(None);
        };
        let Some((base, count)) = reader.edge_run(db, src, dst)? else {
            return Ok(None);
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(rel) {
            slot.insert(load_rel_props(db, rel)?);
        }
        let patchable = self.dirs[&rel]
            .as_ref()
            .and_then(|directory| directory.columns.get(col))
            .is_some_and(|column| {
                holds(column, value)
                    && column.validity.is_none()
                    && base + count <= column.meta.value_count
            });
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
    ) -> Result<bool> {
        if self.fresh.get(&rel).is_some_and(|p| p.holds(src, dst)) {
            return Ok(false);
        }
        self.load_reader(db, rel)?;
        let Some(reader) = self.readers.get(&rel) else {
            return Ok(false);
        };
        let directory = reader.directory();
        if src >= directory.from_count || dst >= directory.to_count {
            return Ok(false);
        }
        if reader.has_edge(db, src, dst)? {
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
    /// patch does not carry. The row has to be one the column already
    /// has a value for, which is what says the write is an update
    /// rather than part of an append.
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
        Ok(holds(column, value) && column.validity.is_none() && row < column.meta.value_count)
    }

    /// Whether a reader can be shown this row as gone without the file
    /// being rewritten around it.
    ///
    /// The row has to be one the table has, and it has to have no edges
    /// on it. Every edge in the file names its endpoints by offset, so
    /// a row that still has one and is gone all the same is an edge
    /// that runs to nothing, which is the state a fold is careful never
    /// to leave: it prunes the edges of a tombstoned row as it rebuilds
    /// the rel table. Nothing here rebuilds anything, so what a fold
    /// would have pruned is what this refuses. A `DELETE` says the same
    /// thing one layer up and raises G1001 rather than folding, and
    /// this is what keeps a caller that staged the op itself honest.
    ///
    /// An edge this run of deferred commits added counts, and it is
    /// counted by turning the whole table away: the readers here are
    /// the file's and the patch is the writer's, and a delete on a
    /// table something has just been linked into is rare enough to
    /// fold.
    fn removable(&mut self, db: &mut Zu1File, table: u32, offset: u64) -> Result<bool> {
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::load(db)?);
        }
        let catalog = self.catalog.as_ref().expect("just loaded");
        let Some(node) = catalog.node_by_id(table) else {
            return Ok(false);
        };
        if offset >= node.node_count {
            return Ok(false);
        }
        let rels: Vec<(u32, bool, bool)> = catalog
            .rel_tables()
            .iter()
            .filter(|rel| rel.from == table || rel.to == table)
            .map(|rel| (rel.id, rel.from == table, rel.to == table))
            .collect();
        for (rel, out, back) in rels {
            if self.fresh.contains_key(&rel) {
                return Ok(false);
            }
            self.load_reader(db, rel)?;
            let Some(reader) = self.readers.get(&rel) else {
                return Ok(false);
            };
            let mut edges = 0;
            if out {
                edges += reader.degree_of(db, offset, Direction::Fwd)?;
            }
            if back {
                edges += reader.degree_of(db, offset, Direction::Bwd)?;
            }
            if edges > 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Adds a deferred commit's cells to the patch and republishes it.
    ///
    /// Only the tables this commit wrote into are rebuilt. The others
    /// keep the `Arc` they already had, so a reader that has read one
    /// of them is not made to look at it again.
    fn stage_patch(&mut self, changes: Vec<Deferred>) {
        let mut cells: Vec<u32> = Vec::new();
        let mut rels: Vec<u32> = Vec::new();
        let mut graves: Vec<u32> = Vec::new();
        let mut grown: Vec<u32> = Vec::new();
        for change in changes {
            match change {
                Deferred::Cell((table, row, col, value)) => {
                    match value {
                        Cell::Int(word) => {
                            self.pending
                                .entry(table)
                                .or_default()
                                .entry(col as usize)
                                .or_default()
                                .insert(row, word);
                        }
                        Cell::Str(bytes) => {
                            self.strings
                                .entry(table)
                                .or_default()
                                .entry(col as usize)
                                .or_default()
                                .insert(row, bytes);
                        }
                        // A value taken away is not one of the shapes
                        // [`Self::defers`] takes, because what it
                        // changes is the validity mask.
                        Cell::Null => unreachable!("refused where the commit was taken"),
                    }
                    if !cells.contains(&table) {
                        cells.push(table);
                    }
                }
                Deferred::Edge(rel, src, dst, cols) => {
                    self.stage_edge(rel, src, dst, cols);
                    if !rels.contains(&rel) {
                        rels.push(rel);
                    }
                }
                Deferred::Gone(table, offset) => {
                    self.graves.entry(table).or_default().insert(offset);
                    if !graves.contains(&table) {
                        graves.push(table);
                    }
                }
                Deferred::Rows(table, rows) => {
                    self.stage_rows(table, rows);
                    if !grown.contains(&table) {
                        grown.push(table);
                    }
                }
                // [`Self::defers`] answers with the ordinals a pair
                // holds, so what reaches here names a row of its own.
                Deferred::RelCell(..) => unreachable!("resolved where the commit was taken"),
            }
        }
        let mut patches = Patches {
            cells: self.patches.cells.clone(),
            rows: self.patches.rows.clone(),
            edges: self.patches.edges.clone(),
            gone: self.patches.gone.clone(),
        };
        for table in cells {
            let words = self.pending.get(&table).map_or_else(BTreeMap::new, |cols| {
                cols.iter()
                    .map(|(&col, rows)| (col, rows.iter().map(|(&r, &w)| (r, w)).collect()))
                    .collect()
            });
            let strs = self.strings.get(&table).map_or_else(BTreeMap::new, |cols| {
                cols.iter()
                    .map(|(&col, rows)| (col, rows.iter().map(|(&r, b)| (r, b.clone())).collect()))
                    .collect()
            });
            patches
                .cells
                .insert(table, Arc::new(CellPatch::new(words, strs)));
        }
        for rel in rels {
            patches
                .edges
                .insert(rel, Arc::new(self.fresh[&rel].clone()));
            if let Some(rows) = self.added.get(&rel) {
                patches.rows.insert(rel, Arc::new(rows.clone()));
            }
        }
        for table in graves {
            patches
                .gone
                .insert(table, self.graves[&table].iter().copied().collect());
        }
        for table in grown {
            patches
                .rows
                .insert(table, Arc::new(self.added[&table].clone()));
        }
        self.patches = Arc::new(patches);
        self.deferred += 1;
    }

    /// Puts one edge in the running patch of its table: the pair in the
    /// two adjacency lists, and a row of cells under the ordinal that
    /// hands it, one per column the table stores.
    ///
    /// The ordinal and the row number are the same number, because a
    /// rel table's property columns are dense over its edges in load
    /// order and an added edge goes on the end of both.
    fn stage_edge(&mut self, rel: u32, src: u64, dst: u64, cols: Vec<(u32, Cell)>) {
        let edges = self.readers[&rel].directory().edge_count;
        self.fresh
            .entry(rel)
            .or_insert_with(|| EdgePatch::new(edges))
            .add(src, dst);
        let Some(directory) = self.dirs.get(&rel).and_then(Option::as_ref) else {
            return;
        };
        let row = (0..directory.columns.len())
            .map(|at| {
                cols.iter()
                    .find(|(c, _)| *c as usize == at)
                    .map_or(Cell::Null, |(_, cell)| cell.clone())
            })
            .collect();
        self.added
            .entry(rel)
            .or_insert_with(|| RowPatch::new(edges))
            .push(row);
    }

    /// Puts rows a commit added to a node table in that table's running
    /// patch, one cell per column of it, in the order the fold would
    /// have appended them in.
    ///
    /// The base is the count the columns hold, which is the offset the
    /// first of them takes, and it is read from the props directory
    /// rather than the catalog because that is what the fold checks its
    /// own arithmetic against.
    fn stage_rows(&mut self, table: u32, rows: Vec<Vec<(u32, Cell)>>) {
        let Some(directory) = self.dirs.get(&table).and_then(Option::as_ref) else {
            return;
        };
        let (base, width) = (directory.node_count, directory.columns.len());
        let patch = self
            .added
            .entry(table)
            .or_insert_with(|| RowPatch::new(base));
        for row in rows {
            patch.push(
                (0..width)
                    .map(|at| {
                        row.iter()
                            .find(|(c, _)| *c as usize == at)
                            .map_or(Cell::Null, |(_, cell)| cell.clone())
                    })
                    .collect(),
            );
        }
    }

    /// Drops the patch a fold has just sealed into the columns.
    fn sealed(&mut self) {
        self.deferred = 0;
        self.dirs.clear();
        self.readers.clear();
        self.catalog = None;
        if !self.patches.is_empty() {
            self.pending.clear();
            self.strings.clear();
            self.added.clear();
            self.fresh.clear();
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

/// Whether a column can be handed this value without being rewritten:
/// a word goes over the word a lane column holds, and bytes go over the
/// bytes a blob column holds. Nothing goes over a value that is not
/// there, which is why an absent one is refused here.
fn holds(column: &PropColumn, value: &Cell) -> bool {
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
/// cell each; rows added to a node table are as many as there are.
fn written_cells(changes: &[Deferred]) -> usize {
    changes
        .iter()
        .map(|change| match change {
            Deferred::Rows(_, rows) => rows.len(),
            _ => 1,
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
                 RETURN a.name AS from, b.name AS to, k.since AS since, k.note AS note \
                 ORDER BY from",
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
                 RETURN b.name AS from, k.since AS since, k.note AS note",
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
                     RETURN a.name AS from, k.since AS since ORDER BY from",
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
                 RETURN a.name AS from, k.since AS since ORDER BY from",
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
                 RETURN a.name AS from, k.since AS since ORDER BY from",
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
                     RETURN a.name AS from, k.note AS note ORDER BY from",
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

    /// An edge onto a row the patch is carrying folds. The CSR was
    /// built over the rows the file held, so a list for the new row is
    /// one the adjacency reader has nowhere to put.
    #[test]
    fn an_edge_onto_a_row_the_patch_is_carrying_folds() {
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
        assert!(session.epoch() > before, "the edge did not fold");

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

    /// A write onto an edge this same unfolded run added folds instead.
    /// Its values are in the rows the patch appended and not in the
    /// column underneath, so a word aimed at the column would land where
    /// nothing reads it.
    #[test]
    fn a_year_written_onto_an_edge_just_added_folds() {
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
        assert!(session.epoch() > before, "the write did not fold");

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
                 RETURN b.name AS from, k.since AS since, k.note AS note",
                &[],
            )
            .expect("read");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(string(&out.rows[0], 0), "amy");
        assert_eq!(out.rows[0][1], Value::Int(2020));
        assert_eq!(string(&out.rows[0], 2), "gym");
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
}
