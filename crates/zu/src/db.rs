//! The public Rust API: a [`Database`] you open once and share, and a
//! [`Connection`] you take one of per thread.
//!
//! [`crate::session::Session`] is the engine's own entry point and it
//! is one object doing both jobs, which works exactly as long as a
//! process wants one. The moment two threads want to read the same
//! graph, or a pool wants to hand a connection out and take it back,
//! the two jobs come apart: the database is the thing that is shared
//! and immutable, and the connection is the thing that is cheap,
//! serial, and owned by whoever is using it. Every binding on the C ABI
//! inherits this split (`dx/02` §3), so it is the Rust API that has to
//! have it first.
//!
//! ```no_run
//! use zu::Database;
//!
//! let db = Database::open("social.zu1")?;
//! let mut conn = db.connect()?;
//! let rows = conn.query("MATCH (p:Person) RETURN p.name")?;
//! # Ok::<(), zu::ZuError>(())
//! ```
//!
//! [`Database::open`] takes a path and nothing else, because a
//! configuration argument every caller has to write is a tax on every
//! caller to serve the few who set anything; [`Database::open_with`] is
//! for those few. Opening reads 12 KiB and pages the rest lazily, so
//! connecting is an open and a catalog load rather than a copy of the
//! file, and a connection is genuinely cheap to take.

use std::path::{Path, PathBuf};

use zu_common::{Interrupt, Result, ZuError};
use zu_query::exec::{self, Profile, Streamed};
use zu_query::frame::Frame;
use zu_query::plan::QueryPlan;
use zu_query::row::{Batch, Flow};

use crate::append::Appender;
use crate::query::{QueryResult, Value};
use crate::session::Session;
use crate::zu1::file::Zu1File;

/// How a database is opened and what its statements are allowed to do.
///
/// Every field has a default that works, so a caller sets the one thing
/// it cares about and nothing else. The builder takes `self` and gives
/// it back, so the whole configuration is one expression and a
/// half-built `Config` is never a thing anybody holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    memory_limit: usize,
    threads: usize,
    read_only: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            memory_limit: crate::zu1::file::DEFAULT_MEMORY_LIMIT,
            threads: 0,
            read_only: false,
        }
    }
}

impl Config {
    /// The defaults: the 128 MiB cache budget the perf gates run under,
    /// a thread count the executor picks from the machine, and writes
    /// allowed.
    pub fn new() -> Config {
        Config::default()
    }

    /// Bytes the caches may hold, split between block frames and the
    /// decoded-object pools by the ratios in perf/04 §2. This is a
    /// budget for what the engine keeps warm, not a ceiling on the
    /// process: a query still allocates its own intermediate state.
    pub fn memory_limit(mut self, bytes: usize) -> Config {
        self.memory_limit = bytes;
        self
    }

    /// Worker threads for the parallel stages of a query. Zero, the
    /// default, lets the executor pick `min(cores, 8)`; one forces
    /// sequential execution, which is what a caller running many
    /// connections at once wants, since the parallelism is then across
    /// connections and a thread pool per connection would oversubscribe
    /// the machine.
    pub fn threads(mut self, threads: usize) -> Config {
        self.threads = threads;
        self
    }

    /// Opens the file on a descriptor this process cannot write
    /// through, so nothing here or under here can modify the database,
    /// and a database on a read-only mount opens rather than failing at
    /// the open. Statements that would write are refused with an error
    /// naming the database, before the operating system gets a chance
    /// to refuse them less helpfully.
    pub fn read_only(mut self, read_only: bool) -> Config {
        self.read_only = read_only;
        self
    }

    /// The execution switches this configuration implies, over the ones
    /// the session read from the environment.
    fn over(&self, mut options: exec::Options) -> exec::Options {
        if self.threads != 0 {
            options.threads = self.threads;
        }
        options
    }
}

/// An open database: a path, a configuration, and the fact that both
/// have been checked against a real file.
///
/// It is `Send + Sync` and holds no file descriptor and no cache, which
/// is what makes it shareable without a lock. The state that cannot be
/// shared, the seek position of a handle and the plans compiled against
/// a catalog, lives on the connections instead, one set per connection,
/// which is the same division the C ABI and every binding above it use.
#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
    config: Config,
}

impl Database {
    /// Opens the database at `path` with the default configuration.
    ///
    /// The file is opened once here and closed again, so a path that is
    /// not a zu1 file, or is one this build cannot read, fails now
    /// rather than on the first connection. That open is the O(1) one
    /// of docs/04 §1: 12 KiB and two header slots.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Database::open_with(path, Config::default())
    }

    /// [`Database::open`] with a configuration.
    pub fn open_with(path: impl AsRef<Path>, config: Config) -> Result<Database> {
        let path = path.as_ref().to_path_buf();
        let db = Database { path, config };
        // Proving the file opens is the whole point of doing it here,
        // and a handle that proved it has nothing else to offer: a
        // connection opens its own, with its own caches.
        drop(db.handle()?);
        Ok(db)
    }

    /// Creates a database at `path` and opens it, with the default
    /// configuration.
    ///
    /// The path must not exist. A create that found a database there
    /// and opened it instead would be the call that quietly writes into
    /// data somebody else put there, and the caller who wanted either
    /// one has [`Database::open`] to fall back to and a decision to make
    /// about which.
    ///
    /// What it makes is a valid database with nothing in it: a file
    /// header, a database header, and no tables. Statements make the
    /// tables, and until v0 grows one that does, a bulk load is what
    /// puts rows in.
    pub fn create(path: impl AsRef<Path>) -> Result<Database> {
        Database::create_with(path, Config::default())
    }

    /// [`Database::create`] with a configuration.
    pub fn create_with(path: impl AsRef<Path>, config: Config) -> Result<Database> {
        if config.read_only {
            return Err(ZuError::InvalidArgument(
                "a database created read-only is one nothing could ever put a row in".to_string(),
            ));
        }
        // Created and closed again, so that what comes back is a
        // database opened the same way `open` opens one, rather than a
        // second path through this module that a caller could tell from
        // the first.
        drop(Zu1File::create(path.as_ref())?);
        Database::open_with(path, config)
    }

    /// Where this database lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configuration it was opened with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// A new connection: its own file handle, its own caches, its own
    /// plan cache. Costs an open and a catalog load, which on the
    /// 11 GB file of the B7 gate is tens of microseconds.
    pub fn connect(&self) -> Result<Connection> {
        let mut session = Session::on(self.handle()?)?;
        session.set_options(self.config.over(session.options().clone()));
        Ok(Connection {
            session,
            read_only: self.config.read_only,
        })
    }

    fn handle(&self) -> Result<Zu1File> {
        let mut file = if self.config.read_only {
            Zu1File::open_read_only(&self.path)?
        } else {
            Zu1File::open(&self.path)?
        };
        file.set_memory_limit(self.config.memory_limit);
        Ok(file)
    }
}

/// One connection: statements run on it, in order, one at a time.
///
/// It is `Send` and not `Sync`, and every method takes `&mut self`, so
/// the compiler is what stops two threads from using one connection.
/// That is the same rule the C ABI states and has to check at runtime
/// (`dx/02` §5), enforced here at no cost.
///
/// A connection reads the database as of when it connected. It holds
/// the header it opened at, so a write another connection published
/// since is not visible to it, and a reader that wants the latest
/// catalog takes a new connection. Cross-connection visibility without
/// reconnecting is the snapshot machinery of docs/08, not a header read
/// on the statement path.
pub struct Connection {
    session: Session,
    read_only: bool,
}

impl Connection {
    /// Runs one statement and returns its rows.
    pub fn query(&mut self, source: &str) -> Result<QueryResult> {
        self.query_with(source, &[])
    }

    /// Runs one statement with parameters bound by name, without the
    /// leading `$`.
    pub fn query_with(&mut self, source: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.refuse_if_read_only(source)?;
        self.session.run(source, params)
    }

    /// Runs one statement and hands its rows to `sink` in batches as
    /// they are made, instead of returning them all.
    ///
    /// This is the shape for a result that is too big to want in
    /// memory, and for a caller that will not read all of it: the sink
    /// answers [`Flow::Stop`] and the scan under it stops at the next
    /// boundary, the same boundary an interrupt is answered at. A batch
    /// borrows the rows for the length of the call, so a caller keeping
    /// anything past it copies what it wants out.
    ///
    /// ```no_run
    /// use zu::{Database, Flow};
    ///
    /// let db = Database::open("social.zu1")?;
    /// let mut conn = db.connect()?;
    /// let mut total = 0i64;
    /// conn.query_stream("MATCH (p:person) RETURN p.id AS id", &[], |batch| {
    ///     for row in batch.iter() {
    ///         total += row.get_at::<i64>(0)?;
    ///     }
    ///     Ok(Flow::More)
    /// })?;
    /// # Ok::<(), zu::ZuError>(())
    /// ```
    pub fn query_stream(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        sink: impl FnMut(Batch<'_>) -> Result<Flow>,
    ) -> Result<Streamed> {
        self.query_stream_batched(source, params, exec::STREAM_BATCH, sink)
    }

    /// The same, with the batch size named. One vector of rows is the
    /// default because it is the unit the executor already works in;
    /// a caller writing batches somewhere with a size of its own, an
    /// Arrow record batch or an HTTP chunk, says so here.
    pub fn query_stream_batched(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        batch_rows: usize,
        mut sink: impl FnMut(Batch<'_>) -> Result<Flow>,
    ) -> Result<Streamed> {
        self.refuse_if_read_only(source)?;
        self.session
            .run_streaming(source, params, batch_rows, &mut sink)
    }

    /// Runs a statement for its effect and returns the number of rows
    /// it produced, which is zero for the catalog statements that are
    /// the writes this milestone has.
    pub fn execute(&mut self, source: &str) -> Result<u64> {
        Ok(self.query(source)?.rows.len() as u64)
    }

    /// Compiles a statement and pins it under an id, so a loop binds
    /// and executes without recompiling. The names it wants come back
    /// with it, in the order the binder assigned them.
    pub fn prepare(&mut self, source: &str) -> Result<(u64, Vec<String>)> {
        self.refuse_if_read_only(source)?;
        self.session.prepare(source)
    }

    /// Runs a prepared statement with a set of bindings.
    pub fn execute_prepared(&mut self, stmt: u64, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.session.execute(stmt, params)
    }

    /// Releases a prepared statement, reporting whether the id was one.
    pub fn close_prepared(&mut self, stmt: u64) -> bool {
        self.session.close_stmt(stmt)
    }

    /// The plan this connection would run for `source`, rendered,
    /// without running it.
    pub fn explain(&mut self, source: &str) -> Result<String> {
        self.session.explain(source)
    }

    /// The same plan as operators rather than as text: the tree, the
    /// columns the statement answers with, the parameters it wants, and
    /// the notes compiling it raised.
    ///
    /// A caller that reads a plan is asking a question about it, and
    /// every one of those questions is easier to ask of a tree than of
    /// a listing: whether the scan reaches an index, how deep the
    /// expands go, which tables are touched. [`QueryPlan::render`] is
    /// what [`Self::explain`] returns, so the two are one thing printed
    /// two ways rather than two renderings that can drift.
    ///
    /// ```no_run
    /// use zu::Database;
    ///
    /// let db = Database::open("social.zu1")?;
    /// let mut conn = db.connect()?;
    /// let plan = conn.explain_plan("MATCH (p:person) RETURN p.id AS id")?;
    /// let root = plan.root.as_ref().expect("a statement with operators");
    /// assert_eq!(root.op, "Project");
    /// assert_eq!(plan.columns, ["id"]);
    /// # Ok::<(), zu::ZuError>(())
    /// ```
    pub fn explain_plan(&mut self, source: &str) -> Result<QueryPlan> {
        self.session.explain_plan(source)
    }

    /// Runs `source` with the counters on and hands back what it
    /// observed, one entry per operator per stage.
    ///
    /// This costs the execution, so it is the tool a caller reaches for
    /// when a statement is slower than the plan says it should be: the
    /// rows an operator really produced sit next to what the optimizer
    /// expected, and the self time says which one of them the wall
    /// clock went into. [`Profile::render`] prints it the way
    /// `EXPLAIN ANALYZE` does in the shell.
    ///
    /// A statement that writes is refused, because it runs as the parts
    /// it was split at its write into rather than as the one plan a
    /// profile describes, and profiling it would apply the write.
    pub fn profile(&mut self, source: &str, params: &[(&str, Value)]) -> Result<Profile> {
        self.refuse_if_read_only(source)?;
        self.session.profile(source, params)
    }

    /// Opens an appender on `table`, the bulk-load path of dx/04 §6.
    ///
    /// A load through statements pays a commit per row; an appender
    /// buffers rows and pays one per flush. Only one appender exists at
    /// a time on a connection, which the borrow says rather than a
    /// check: it holds the file this connection reads through, so the
    /// connection is unusable until the appender is closed or dropped,
    /// and a row appended is a row the next statement on it sees.
    pub fn appender(&mut self, table: &str) -> Result<Appender<'_>> {
        if self.read_only {
            return Err(ZuError::InvalidArgument(
                "an appender writes and the connection is read-only".to_string(),
            ));
        }
        Appender::open(self.session.file_mut()?, table)
    }

    /// Registers a frame as a table of this connection, under the name
    /// it carries.
    ///
    /// This is the replacement scan: a caller holding columns in memory
    /// gets to name them in a statement without loading them into the
    /// database first. Nothing is copied, at registration or at read.
    /// The frame is this connection's alone and goes when it does, and
    /// a name a stored table already holds is refused.
    pub fn register(&mut self, frame: Frame) -> Result<()> {
        self.session.register_frame(frame)
    }

    /// Drops a registered frame, answering whether there was one under
    /// that name.
    pub fn unregister(&mut self, name: &str) -> Result<bool> {
        self.session.unregister_frame(name)
    }

    /// The names registered on this connection, sorted.
    pub fn registered(&self) -> Vec<String> {
        self.session.registered_frames()
    }

    /// Whether this connection refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The handle a statement on this connection can be stopped
    /// through, and the count of rows it has read.
    ///
    /// A statement runs on the thread that asked for it, so the handle
    /// is taken before the call and raised from another thread: it is
    /// how a shell answers `Ctrl-C` and how a server answers a client
    /// that hung up. The statement returns
    /// [`zu_common::ZuError::Interrupted`] and the connection is
    /// unchanged, which is what makes this different from closing it.
    pub fn interrupt(&self) -> Interrupt {
        self.session.interrupt()
    }

    /// The session under this connection, for the paths that have not
    /// grown an API here yet: profiling, EXPLAIN ANALYZE, and the
    /// engine's own tooling.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// A read-only connection refuses a statement that would write
    /// before it compiles it, so the error names the statement rather
    /// than whatever block the storage layer got to first. Parsing is
    /// what tells the two apart, and a statement that does not parse is
    /// left to the compiler below to reject, with its position in it.
    fn refuse_if_read_only(&self, source: &str) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        if matches!(
            crate::query::not_a_query(source),
            Ok(Some(crate::query::NotAQuery::Catalog(_)))
        ) {
            return Err(ZuError::InvalidArgument(
                "this statement writes and the connection is read-only".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::graph;

    /// A database with one node table and one rel table in it,
    /// which is enough for a statement to have something to read.
    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        let edges: Vec<(u32, u32)> = (0..64u32).map(|i| (i % 8, (i * 5 + 1) % 8)).collect();
        let mut edges = edges;
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 8, &edges).expect("load");
    }

    fn scratch(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        seeded(&path);
        (dir, path)
    }

    #[test]
    fn a_database_opens_from_a_path_and_nothing_else() {
        let (_dir, path) = scratch("open.zu1");
        let db = Database::open(&path).expect("open");
        assert_eq!(db.path(), path);
        assert_eq!(db.config(), &Config::default());
    }

    #[test]
    fn a_database_is_created_empty_and_statements_run_against_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fresh.zu1");
        let db = Database::create(&path).expect("create");
        assert!(path.exists(), "the file is on disk when create returns");
        let mut conn = db.connect().expect("connect");
        let rows = conn.query("RETURN 1 AS n").expect("query");
        assert_eq!(rows.rows.len(), 1);
        // Empty is empty: a table nobody made is a table nothing finds,
        // and finding nothing is an empty answer rather than a refusal.
        let empty = conn.query("MATCH (p:person) RETURN p").expect("query");
        assert!(
            empty.rows.is_empty(),
            "a label nothing carries matches nothing"
        );
        // And what create wrote is what open reads, rather than a file
        // only the handle that made it can use.
        Database::open(&path).expect("open");
    }

    #[test]
    fn creating_a_database_where_one_is_already_refuses_rather_than_adding_to_it() {
        let (_dir, path) = scratch("taken.zu1");
        let err = Database::create(&path).expect_err("refused");
        assert!(matches!(err, ZuError::Io(_)), "{err}");
        // And the database that was there is untouched.
        let mut conn = Database::open(&path).expect("open").connect().expect("c");
        assert_eq!(
            conn.query("MATCH (p:person) RETURN p")
                .expect("query")
                .rows
                .len(),
            8
        );
    }

    #[test]
    fn a_read_only_create_is_a_contradiction_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("frozen.zu1");
        let err =
            Database::create_with(&path, Config::default().read_only(true)).expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
        assert!(!path.exists(), "and nothing was written");
    }

    #[test]
    fn a_path_that_is_not_a_database_fails_at_open_and_not_at_connect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"this is not a zu1 file").expect("write");
        let err = Database::open(&path).expect_err("refused");
        assert!(
            matches!(err, ZuError::Corrupt { .. } | ZuError::Io(_)),
            "{err}"
        );
    }

    #[test]
    fn a_missing_file_is_an_io_error_from_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Database::open(dir.path().join("absent.zu1")).expect_err("refused");
        assert!(matches!(err, ZuError::Io(_)), "{err}");
    }

    #[test]
    fn two_connections_read_the_same_graph_at_once() {
        let (_dir, path) = scratch("two.zu1");
        let db = Database::open(&path).expect("open");
        let mut a = db.connect().expect("connect");
        let mut b = db.connect().expect("connect");
        let one = a.query("MATCH (p:person) RETURN p").expect("query");
        let two = b.query("MATCH (p:person) RETURN p").expect("query");
        assert_eq!(one.rows.len(), two.rows.len());
        assert_eq!(one.rows.len(), 8);
    }

    /// The point of the split: a database is shared between threads and
    /// each of them takes its own connection. If `Database` were not
    /// `Send + Sync` this would not compile, which is the assertion.
    #[test]
    fn a_database_is_shared_across_threads() {
        let (_dir, path) = scratch("shared.zu1");
        let db = Database::open(&path).expect("open");
        let counted = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let db = &db;
                    scope.spawn(move || {
                        let mut conn = db.connect().expect("connect");
                        conn.query("MATCH (p:person) RETURN p")
                            .expect("query")
                            .rows
                            .len()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("joined"))
                .sum::<usize>()
        });
        assert_eq!(counted, 32);
    }

    #[test]
    fn a_prepared_statement_runs_more_than_once() {
        let (_dir, path) = scratch("prepared.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let (id, params) = conn.prepare("MATCH (p:person) RETURN p").expect("prepare");
        assert!(params.is_empty());
        for _ in 0..3 {
            assert_eq!(
                conn.execute_prepared(id, &[]).expect("execute").rows.len(),
                8
            );
        }
        assert!(conn.close_prepared(id));
        assert!(!conn.close_prepared(id));
    }

    #[test]
    fn execute_counts_the_rows_a_statement_produced() {
        let (_dir, path) = scratch("execute.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(conn.execute("MATCH (p:person) RETURN p").expect("run"), 8);
    }

    #[test]
    fn a_configured_thread_count_reaches_the_executor() {
        let (_dir, path) = scratch("threads.zu1");
        let db = Database::open_with(&path, Config::new().threads(3)).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(conn.session_mut().options().threads, 3);
    }

    /// Zero is the default and means the executor picks, so it must not
    /// overwrite what the session already decided.
    #[test]
    fn the_default_thread_count_leaves_the_executor_alone() {
        let (_dir, path) = scratch("auto.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(conn.session_mut().options().threads, 0);
    }

    #[test]
    fn a_memory_limit_is_carried_to_the_connections() {
        let (_dir, path) = scratch("budget.zu1");
        let db = Database::open_with(&path, Config::new().memory_limit(4 << 20)).expect("open");
        let mut conn = db.connect().expect("connect");
        assert_eq!(
            conn.query("MATCH (p:person) RETURN p")
                .expect("query")
                .rows
                .len(),
            8
        );
    }

    #[test]
    fn a_read_only_connection_reads() {
        let (_dir, path) = scratch("ro.zu1");
        let db = Database::open_with(&path, Config::new().read_only(true)).expect("open");
        let mut conn = db.connect().expect("connect");
        assert!(conn.is_read_only());
        assert_eq!(
            conn.query("MATCH (p:person) RETURN p")
                .expect("query")
                .rows
                .len(),
            8
        );
    }

    #[test]
    fn a_read_only_connection_refuses_a_statement_that_writes() {
        let (_dir, path) = scratch("refuse.zu1");
        let db = Database::open_with(&path, Config::new().read_only(true)).expect("open");
        let mut conn = db.connect().expect("connect");
        let err = conn
            .query("CREATE PROPERTY GRAPH second ANY")
            .expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
        let err = conn
            .query("INSERT (p:person {name: 'zoe'})")
            .expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    /// The API refusal above is a courtesy; this is the guarantee. The
    /// handle underneath cannot write even if something reached past
    /// the connection to it.
    #[test]
    fn a_read_only_handle_refuses_a_write_from_underneath() {
        let (_dir, path) = scratch("guard.zu1");
        let mut file = Zu1File::open_read_only(&path).expect("open");
        let err = file
            .write_block(1, &vec![0u8; crate::zu1::BLOCK_SIZE as usize])
            .expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
        let err = file.checkpoint().expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    /// A writable connection is the default, and it has to keep working
    /// after all of the above.
    #[test]
    fn a_writable_connection_still_writes() {
        let (_dir, path) = scratch("write.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        conn.execute("CREATE PROPERTY GRAPH second ANY")
            .expect("create");
        // The new graph is empty, so a statement against it gets past
        // the name and finds nothing, which is what says the catalog
        // took the write: an unknown graph is the error, and a graph
        // with nothing in it is an empty answer.
        let seen = conn.query("USE second MATCH (p) RETURN p").expect("query");
        assert!(seen.rows.is_empty(), "the new graph holds no elements");
    }

    /// What a connection sees is the database as of when it connected.
    ///
    /// A handle reads the header at open and keeps it, so a write
    /// another connection published afterwards is not visible to it:
    /// the epoch check that drops stale plans compares the header this
    /// handle holds, and that one moves when this handle writes. Making
    /// it move otherwise means re-reading two header slots per
    /// statement, which is the whole cost of a warm query, and the
    /// right answer to it is the snapshot machinery of docs/08 rather
    /// than a read on the hot path. Until that lands, a reader that
    /// wants the latest catalog takes a new connection, which is cheap
    /// and is what this test says.
    #[test]
    fn a_connection_reads_the_database_as_of_when_it_connected() {
        let (_dir, path) = scratch("epoch.zu1");
        let db = Database::open(&path).expect("open");
        let mut writer = db.connect().expect("connect");
        let mut reader = db.connect().expect("connect");
        writer
            .execute("CREATE PROPERTY GRAPH second ANY")
            .expect("create");

        let stale = reader
            .query("USE second MATCH (p) RETURN p")
            .expect_err("connected before the write");
        assert!(stale.to_string().contains("is no graph"), "{stale}");

        // The new graph is empty, so a fresh connection gets past the
        // name and finds nothing in it, which is what says the write is
        // on disk and being read: the stale connection above could not
        // resolve the name at all.
        let mut fresh = db.connect().expect("connect");
        let seen = fresh.query("USE second MATCH (p) RETURN p").expect("query");
        assert!(seen.rows.is_empty(), "the new graph holds no elements");
    }

    /// A write through the API a caller actually holds: the connection
    /// runs the statement, and the element it made answers the next
    /// question asked of the same connection.
    #[test]
    fn a_connection_writes_and_reads_back_what_it_wrote() {
        let (_dir, path) = scratch("write.zu1");
        // The seeded table stores no properties and a new row needs a
        // column to grow, so give it one.
        let mut file = Zu1File::open(&path).expect("open");
        let names: Vec<&[u8]> = vec![b"ada", b"bo", b"cy", b"di", b"ed", b"fi", b"gil", b"hal"];
        crate::zu1::props::store_props(
            &mut file,
            "person",
            &[("name", crate::zu1::props::PropValues::Str(&names))],
        )
        .expect("props");
        drop(file);

        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        conn.query("INSERT (p:person {name: 'zoe'})")
            .expect("insert");
        let out = conn
            .query("MATCH (p:person) WHERE p.name = 'zoe' RETURN p.name AS name")
            .expect("read");
        assert_eq!(out.rows.len(), 1, "the row that was written is there");
    }

    #[test]
    fn explain_renders_a_plan_without_running_it() {
        let (_dir, path) = scratch("explain.zu1");
        let db = Database::open(&path).expect("open");
        let mut conn = db.connect().expect("connect");
        let listing = conn.explain("MATCH (p:person) RETURN p").expect("explain");
        assert!(listing.contains("Scan"), "{listing}");
    }

    #[test]
    fn a_config_is_the_defaults_plus_what_was_set() {
        let config = Config::new()
            .memory_limit(1 << 20)
            .threads(2)
            .read_only(true);
        assert_eq!(
            config,
            Config {
                memory_limit: 1 << 20,
                threads: 2,
                read_only: true,
            }
        );
    }
}
