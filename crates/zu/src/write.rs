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
//! Every commit that changed something folds, and that is worth being
//! plain about, because it is not where this ends up. A fold rewrites
//! every column of every table the transaction touched, so paying one
//! per statement is the wrong shape for a write workload, and the
//! overlay exists precisely so a reader can see a committed row
//! without one. But [`Zu1Graph`] reads the sealed file and nothing
//! else, so today a change that is not folded is a change the next
//! `MATCH` cannot see, and a write nobody can read is not a write.
//! Folding here keeps the visibility contract honest while the read
//! path catches up; once it consults overlays this becomes a policy
//! and a background checkpoint, and the only thing that changes here
//! is when [`Writer::fold`] gets called.
//!
//! [`Zu1Graph`]: crate::query::Zu1Graph

use std::path::{Path, PathBuf};

use zu_common::{Epoch, Result, ZuError};

use crate::append::sidecar;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{checkpoint_fold, recover};
use crate::zu1::txn::{Mvcc, WriteTxn};
use crate::zu1::wal::Wal;

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
        if !db.is_writable() {
            return Err(ZuError::InvalidArgument(format!(
                "cannot write to {}, which is open read-only",
                db.path().display()
            )));
        }
        let path = sidecar(db.path());
        let wal = Wal::open(&path)?;
        let mvcc = recover(db, &wal)?;
        let mut writer = Writer { wal, mvcc, path };
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
        let epoch = txn.commit(&mut self.wal)?;
        if staged {
            self.fold(db)?;
        }
        Ok(Written { value, epoch })
    }

    /// Seals every committed overlay into the file and truncates the
    /// log. On return the file alone answers what the overlays
    /// answered, and the header epoch has moved, so anything caching
    /// the catalog or a decoded layout has to reload.
    pub fn fold(&mut self, db: &mut Zu1File) -> Result<()> {
        checkpoint_fold(db, &mut self.mvcc, &mut self.wal)
    }
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
            let mut mvcc = recover(&mut db, &wal).expect("recover");
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
            crate::append::Appender::open(session.file_mut(), "person").expect("open the appender");
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
}
