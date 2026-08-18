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
//! which is one shape and only one: a word written onto a column of a
//! row that is already there. Those go into a [`LanePatch`] per table,
//! the readers put them over the words their columns hold, and a fold
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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use zu_common::{Epoch, Result, ZuError};

use crate::append::sidecar;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{checkpoint_fold, recover, staged_fold};
use crate::zu1::props::{LanePatch, PropsDirectory, load_props};
use crate::zu1::txn::{LaneWrite, Mvcc, WriteTxn};
use crate::zu1::wal::Wal;

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

/// The unsealed cells of every table that has any, which is what a
/// reader is handed so that it reads a deferred commit.
pub type Patches = HashMap<u32, Arc<LanePatch>>;

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
    /// The cells the commits since the last fold wrote, by table and
    /// then by column, which is the running form the patch below is
    /// built from.
    pending: HashMap<u32, BTreeMap<usize, BTreeMap<u64, u64>>>,
    /// The same cells as a reader takes them. Rebuilt whenever a
    /// deferred commit adds to `pending`, and shared from there: a
    /// reader holds the `Arc` and hands copies of it to the workers a
    /// query forks.
    patches: Arc<Patches>,
    /// How many commits have gone without a fold.
    deferred: u32,
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
            patches: Arc::new(Patches::new()),
            deferred: 0,
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
        let epoch = txn.commit(&mut self.wal)?;
        if staged {
            let lanes = self.mvcc.take_lanes();
            if self.defers(db, &lanes)? {
                self.stage_patch(lanes);
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
        Ok(Written { value, epoch })
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

    /// Whether this commit can be left in the overlay for a later fold
    /// to seal.
    ///
    /// A commit that only wrote words onto rows that are already there
    /// can, because the patch carries exactly that and a reader reads
    /// through it. Anything else, a new row, a deleted one, a string, a
    /// null, a label, an edge, folds the way it always did. On top of
    /// the shape there are three bounds: how many commits may go
    /// unfolded, how many cells they may hold, and the block growth the
    /// checkpoint threshold already bounds.
    fn defers(&mut self, db: &mut Zu1File, lanes: &[LaneWrite]) -> Result<bool> {
        if lanes.is_empty() || !self.mvcc.lane_only() {
            return Ok(false);
        }
        if self.deferred >= DEFERRED_COMMITS
            || self.patches.values().map(|p| p.cells()).sum::<usize>() + lanes.len()
                > DEFERRED_CELLS
            || db.unpublished_blocks() >= THRESHOLD_BLOCKS
        {
            return Ok(false);
        }
        for &(table, row, col, _) in lanes {
            if !self.patchable(db, table, row, col as usize)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a reader can be shown a word written onto this cell
    /// without the column being rewritten.
    ///
    /// The column has to be one the patch can describe: fixed width, so
    /// the new value is a word that goes straight over the old one, and
    /// no validity segment, because a column that can hold nothing in a
    /// row keeps that in a bitmap the patch does not carry. The row has
    /// to be one the column already has a word for, which is what says
    /// the write is an update rather than part of an append.
    fn patchable(&mut self, db: &mut Zu1File, table: u32, row: u64, col: usize) -> Result<bool> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.dirs.entry(table) {
            slot.insert(load_props(db, table)?);
        }
        let Some(directory) = self.dirs[&table].as_ref() else {
            return Ok(false);
        };
        let Some(column) = directory.columns.get(col) else {
            return Ok(false);
        };
        Ok(column.is_lane() && column.validity.is_none() && row < column.meta.value_count)
    }

    /// Adds a deferred commit's cells to the patch and republishes it.
    ///
    /// Only the tables this commit wrote into are rebuilt. The others
    /// keep the `Arc` they already had, so a reader that has read one
    /// of them is not made to look at it again.
    fn stage_patch(&mut self, lanes: Vec<LaneWrite>) {
        let mut patches = Patches::clone(&self.patches);
        let mut touched: Vec<u32> = Vec::new();
        for (table, row, col, word) in lanes {
            self.pending
                .entry(table)
                .or_default()
                .entry(col as usize)
                .or_default()
                .insert(row, word);
            if !touched.contains(&table) {
                touched.push(table);
            }
        }
        for table in touched {
            let cols = self.pending[&table]
                .iter()
                .map(|(&col, rows)| (col, rows.iter().map(|(&r, &w)| (r, w)).collect()))
                .collect();
            patches.insert(table, Arc::new(LanePatch::new(cols)));
        }
        self.patches = Arc::new(patches);
        self.deferred += 1;
    }

    /// Drops the patch a fold has just sealed into the columns.
    fn sealed(&mut self) {
        self.deferred = 0;
        self.dirs.clear();
        if !self.pending.is_empty() {
            self.pending.clear();
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
        let log = std::fs::metadata(sidecar(&path)).expect("the log is there");
        assert_eq!(log.len(), 0, "the close left frames in the log");
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

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(ages(&mut session), [20, 30, 40, 77]);
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
}
