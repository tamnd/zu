//! A persistent query session over one zu1 file: the catalog, stats,
//! and schema stay resident, compiled plans are cached by source text,
//! and the graph readers keep their decoded groups warm across
//! queries. The one-shot entry points in [`crate::query`] pay the full
//! open-load-parse-plan cost on every call, which is fine for a CLI
//! invocation and hopeless for a server loop; a session pays it once
//! and then a warm query is a hash lookup, parameter binding, and
//! execution.
//!
//! Staleness is a single u64 compare: every entry point checks the
//! handle's header epoch and reloads the catalog, stats, and plan
//! cache when it moved. Reads through one session are serial (the
//! underlying handle seeks), so a session is Send but queries on it
//! do not overlap; open one session per thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use zu_common::gqlstatus::codes;
use zu_common::{Interrupt, Result, ZuError};
use zu_query::ast::TxnStmt;
use zu_query::binder::BoundQuery;
use zu_query::exec::{self, Streamed};
use zu_query::plan::{LogicalPlan, QueryPlan};
use zu_query::row::{Batch, Flow};

use crate::query::{self, NotAQuery, QueryResult, Value, Zu1Graph};
use crate::write::Writer;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;

/// Replays a sidecar WAL that a previous writer left behind.
///
/// A bulk load commits by writing its segments and appending one frame
/// naming them, and folds those segments into the base afterwards. The
/// commit is durable at the frame and the base is what a query reads,
/// so a crash between the two leaves rows that are on disk and that no
/// statement can see. Folding them here is what closes that window:
/// every writable open pays one existence check, and one that finds a
/// log with something in it puts the rows where a reader looks before
/// the first statement runs.
///
/// A read-only open cannot fold, and does not pretend to. It reads the
/// base as it stands, which is the state the last fold left, and the
/// next writable open recovers the rest.
fn replay_sidecar(db: &mut Zu1File) -> Result<()> {
    if !db.is_writable() {
        return Ok(());
    }
    let path = crate::append::sidecar(db.path());
    if !path.try_exists().unwrap_or(false) {
        return Ok(());
    }
    let mut wal = crate::zu1::wal::Wal::open(&path)?;
    if wal.is_empty() {
        return Ok(());
    }
    let mut mvcc = crate::zu1::fold::recover(db, &mut wal)?;
    crate::zu1::fold::checkpoint_fold(db, &mut mvcc, &mut wal)
}

/// Distinct query texts held before the cache starts over. Workloads
/// cycle a handful of statements; overflow means every text is unique
/// and caching buys nothing, so wholesale clearing loses nothing.
const PLAN_CAP: usize = 1024;

/// One compiled query: everything that depends on the text and the
/// schema alone, shared between the cache and prepared statements.
struct CachedPlan {
    /// The graph this text is against and the tables of it, which is
    /// the working graph unless a `USE` named another one. It rides
    /// with the plan because the executor needs the same schema the
    /// plan was built against and the next query may name a different
    /// graph.
    schema: Arc<zu_query::binder::Schema>,
    query: BoundQuery,
    plan: LogicalPlan,
    /// The parts a statement that writes runs as, `None` for one that
    /// only reads. The plan above is the whole statement, which is
    /// what EXPLAIN prints; these are what runs.
    parts: Option<Vec<crate::split::Part>>,
    /// What the optimizer wants EXPLAIN to say that the tree does not.
    notes: Vec<String>,
}

/// The explicit transaction a session is inside, from the
/// `START TRANSACTION` that opened it to the `COMMIT` or `ROLLBACK`
/// that ends it.
///
/// A statement written outside one already runs in a transaction of its
/// own, so this is not what makes writes atomic. What it holds is the
/// span: several statements are one unit, and the file is holding the
/// state they started from until one of the two words that end them.
#[derive(Debug)]
struct Explicit {
    /// `START TRANSACTION READ ONLY`, which is refused at the statement
    /// that would write rather than at the block it would have written.
    read_only: bool,
}

pub struct Session {
    graph: Zu1Graph<'static>,
    /// The graph a statement is against when it does not say, which is
    /// the home graph for the life of the session so far: there is no
    /// statement yet that moves it.
    working: u32,
    /// One schema per graph a statement has named, built on the first
    /// naming and dropped when the epoch moves.
    schemas: HashMap<u32, Arc<zu_query::binder::Schema>>,
    epoch: u64,
    /// What the pipeline executor's snapshot read last time. A
    /// snapshot lives for one execution, so without this every query
    /// reopens the table readers it needs, which on a small graph is
    /// most of what the query costs.
    snap: crate::snapshot::SnapshotCache,
    plans: HashMap<String, Arc<CachedPlan>>,
    stmts: HashMap<u64, String>,
    next_stmt: u64,
    /// The execution switches every statement on this session runs
    /// under, read from the environment once at open. Reading them per
    /// statement would make a query's thread count depend on what some
    /// other part of the process last put in the environment, and
    /// [`crate::db::Config`] is the way a caller sets them on purpose.
    options: exec::Options,
    /// The write side, opened on the first write and dropped whenever
    /// the file goes out on loan. Opening it costs a log open and a
    /// recovery pass, which a session that only reads should not pay.
    writer: Option<Writer>,
    /// The explicit transaction running here, if a statement opened one.
    txn: Option<Explicit>,
}

impl Session {
    pub fn open(path: &Path) -> Result<Session> {
        Session::on(Zu1File::open(path)?)
    }

    /// A session over a file handle the caller opened, which is how
    /// [`crate::db::Database`] applies a read-only or memory-limited
    /// open without this module growing a constructor per option.
    pub fn on(mut db: Zu1File) -> Result<Session> {
        replay_sidecar(&mut db)?;
        let (catalog, schema) = query::load_schema(&mut db)?;
        let epoch = db.db_header().epoch;
        let working = catalog.home_graph_id();
        Ok(Session {
            graph: Zu1Graph::owned(db, catalog),
            working,
            schemas: HashMap::from([(working, Arc::new(schema))]),
            epoch,
            snap: crate::snapshot::SnapshotCache::default(),
            plans: HashMap::new(),
            stmts: HashMap::new(),
            next_stmt: 1,
            options: exec::Options {
                interrupt: Interrupt::armed(),
                ..query::env_options()
            },
            writer: None,
            txn: None,
        })
    }

    /// The file this session reads through.
    ///
    /// The write paths that are not statements need it: the appender
    /// of [`crate::append`] seals its buffer straight into this handle,
    /// and it has to be this one rather than a second open of the same
    /// path, so that the epoch the write publishes is the epoch
    /// [`Self::refresh`] reads on the next statement.
    ///
    /// Handing out the file hands out the log with it. An appender
    /// opens the same sidecar this session's writer commits to and can
    /// truncate it, so the writer goes first and the next write opens
    /// a fresh one. That costs a recovery pass and loses nothing: a
    /// writer that has folded holds an empty overlay store, so what it
    /// was holding is what recovery reconstructs.
    pub fn file_mut(&mut self) -> &mut Zu1File {
        self.writer = None;
        self.graph.file_mut()
    }

    /// The catalog this session last loaded, which is how a caller
    /// staging a write turns a label into the table id the transaction
    /// wants.
    pub fn catalog(&self) -> &Catalog {
        self.graph.catalog()
    }

    /// The epoch this session last reloaded at.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Runs one write transaction: `stage` describes the change
    /// against an open transaction, and what it returns comes back
    /// once the change is committed and visible.
    ///
    /// Visible is the part that costs something. The write commits,
    /// folds, and the session reloads, so by the time this returns the
    /// next statement reads a catalog and a snapshot that both know
    /// about the change. A closure that raises commits nothing and
    /// publishes no epoch, which is the rollback an implicit
    /// transaction owes.
    pub fn write<T>(
        &mut self,
        stage: impl FnOnce(&mut crate::zu1::txn::WriteTxn<'_>) -> Result<T>,
    ) -> Result<T> {
        self.refresh()?;
        self.open_writer()?;
        let mut writer = self.writer.take().expect("opened just above");
        let written = writer.write(self.graph.file_mut(), stage);
        self.writer = Some(writer);
        let written = written?;
        self.refresh()?;
        Ok(written.value)
    }

    /// Opens the writer if it is not open already.
    ///
    /// Opening it recovers and folds whatever the log holds, which can
    /// move the epoch, so the reload comes with it rather than only
    /// after the next write.
    fn open_writer(&mut self) -> Result<()> {
        if self.writer.is_none() {
            self.writer = Some(Writer::open(self.graph.file_mut())?);
            self.refresh()?;
        }
        Ok(())
    }

    /// Whether an explicit transaction is running on this session.
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Runs one of the three statements that say where a transaction
    /// begins and ends (GT01).
    ///
    /// The work is in the two words that end one. A statement commits by
    /// folding what it staged into new segments and flipping the header,
    /// so by the time the next statement of the same transaction runs,
    /// the previous one is published and there is nothing in memory left
    /// to drop. Undoing it is therefore a file matter: the first
    /// statement that writes has the file keep the state it is about to
    /// publish over, `COMMIT` lets go of it, and `ROLLBACK` publishes it
    /// again.
    fn transaction(&mut self, stmt: TxnStmt) -> Result<QueryResult> {
        match stmt {
            TxnStmt::Start { read_only } => {
                if self.txn.is_some() {
                    return Err(ZuError::gql(
                        codes::C25G01,
                        "a transaction is already running on this session, and one does not start inside another".to_string(),
                    ));
                }
                self.txn = Some(Explicit { read_only });
            }
            TxnStmt::Commit | TxnStmt::Rollback => {
                let ending = if matches!(stmt, TxnStmt::Commit) {
                    "commit"
                } else {
                    "roll back"
                };
                if self.txn.take().is_none() {
                    return Err(ZuError::gql(
                        codes::C2D000,
                        format!("there is no transaction running on this session to {ending}"),
                    ));
                }
                // A transaction that only read, or that wrote nothing
                // yet, never had the file keep anything, so both words
                // end it the same way and neither costs an epoch.
                if self.graph.file().in_savepoint() {
                    if matches!(stmt, TxnStmt::Commit) {
                        self.graph.file_mut().release_savepoint()?;
                    } else {
                        self.undo()?;
                    }
                }
            }
        }
        Ok(QueryResult::new(Vec::new(), Vec::new()))
    }

    /// Refuses a statement that writes inside a transaction that was
    /// started `READ ONLY` (GT02), before the statement is compiled and
    /// before anything is staged.
    fn refuse_a_write(&self) -> Result<()> {
        if self.txn.as_ref().is_some_and(|txn| txn.read_only) {
            return Err(ZuError::gql(
                codes::C25G03,
                "this transaction was started READ ONLY and this statement writes".to_string(),
            ));
        }
        Ok(())
    }

    /// Has the file keep the state a statement is about to write over,
    /// and says whether this statement is the one that will let go of
    /// it.
    ///
    /// Outside an explicit transaction the statement owns what it takes,
    /// which is what makes an implicit transaction whole: a statement
    /// that raises halfway through leaves the file as it found it even
    /// when its earlier parts had already committed. Inside one, the
    /// first statement to write takes it and the transaction owns it,
    /// because the unit that can be taken back is then the transaction.
    fn hold(&mut self) -> Result<bool> {
        if self.graph.file().in_savepoint() {
            return Ok(false);
        }
        crate::write::writable(self.graph.file())?;
        // A transaction starts from a folded file. Opening the writer
        // is what folds it, and the savepoint keeps where the log
        // stands once it has, so a rollback takes back the frames the
        // transaction wrote and leaves alone the ones that were in the
        // log before it, which somebody else committed and this
        // transaction has no say over.
        self.open_writer()?;
        let floor = self.writer.as_ref().expect("opened just above").epoch();
        self.graph
            .file_mut()
            .begin_savepoint(self.txn.is_some(), floor)?;
        Ok(self.txn.is_none())
    }

    /// Ends what [`Self::hold`] took: a statement that answered keeps
    /// what it wrote, one that raised has it undone.
    fn settle<T>(&mut self, out: Result<T>, held: bool) -> Result<T> {
        if !held {
            return out;
        }
        match out {
            Ok(value) => {
                self.graph.file_mut().release_savepoint()?;
                Ok(value)
            }
            // The rollback is reported over the error that caused it
            // when it fails, because a statement that raised and was
            // undone leaves a database a caller can carry on with, and
            // one that raised and could not be undone does not.
            Err(err) => {
                self.undo()?;
                Err(err)
            }
        }
    }

    /// Publishes the state the file was keeping, and drops everything
    /// that describes the epochs going away with it.
    ///
    /// The writer goes first. It holds the log and the overlay store for
    /// epochs that are about to stop existing, and its next commit would
    /// number itself off them; opening a fresh one costs a log open and
    /// a recovery pass over a log the last fold truncated.
    fn undo(&mut self) -> Result<()> {
        self.writer = None;
        self.graph.file_mut().rollback_savepoint()?;
        self.refresh()
    }

    /// The execution switches this session runs statements under.
    pub fn options(&self) -> &exec::Options {
        &self.options
    }

    /// Replaces them. [`crate::db::Config`] is the supported way to do
    /// this; the setter is here because the switches belong to the
    /// session and a caller holding one directly should not have to
    /// reach through the environment to change them.
    ///
    /// The interrupt handle survives, whatever the new switches carry.
    /// It belongs to whoever opened the session rather than to the
    /// switches it rides in, and a caller changing the thread count
    /// would otherwise silently take away the only way to stop a
    /// statement.
    pub fn set_options(&mut self, options: exec::Options) {
        let interrupt = self.options.interrupt.clone();
        self.options = exec::Options {
            interrupt,
            ..options
        };
    }

    /// The handle a statement on this session can be stopped through.
    ///
    /// One handle for the life of the session, so a caller can take it
    /// once and keep it. It is raised from another thread while a
    /// statement runs, which is the only time it means anything: the
    /// statement stops at the next boundary the executor checks and
    /// answers [`ZuError::Interrupted`], and this session is exactly as
    /// it was, plans and readers included. Clear it before a statement
    /// rather than after, so a stop that arrived while nothing was
    /// running cannot end the next one.
    pub fn interrupt(&self) -> Interrupt {
        self.options.interrupt.clone()
    }

    /// Runs one query and hands its rows to `sink` in batches as they
    /// are made, instead of returning them.
    ///
    /// A statement that has to see every row before it can give one,
    /// which is ORDER BY, DISTINCT and the aggregates, runs whole and is
    /// handed over in batches afterwards, and so does a statement that
    /// writes, because a write runs as the parts it was split into and
    /// the rows come out of the last of them. Which happened is in
    /// [`Streamed::streamed`], and the caller's loop is the same either
    /// way.
    pub fn run_streaming(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        batch_rows: usize,
        sink: &mut dyn FnMut(Batch<'_>) -> Result<Flow>,
    ) -> Result<Streamed> {
        self.refresh()?;
        if query::not_a_query(source)?.is_some() {
            let result = self.run(source, params)?;
            return exec::stream_result(result, batch_rows, sink);
        }
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        if cached.parts.is_some() {
            let result = self.run(source, params)?;
            return exec::stream_result(result, batch_rows, sink);
        }
        let options = self.options.clone();
        let mut st = exec::Streaming::new(sink, &cached.query.columns, batch_rows);
        // The pipeline executor first, the same way `run` takes it
        // first, because a streamed statement that fell back to the old
        // engine would be several times slower than the same statement
        // read the ordinary way, and a caller would learn to avoid the
        // streaming API for the wrong reason. It has handed nothing over
        // when it answers false, so the fallback below starts on a
        // handoff nothing has been fed through.
        if query::exec2_enabled() {
            let catalog = self.graph.catalog().clone();
            let warm = std::mem::take(&mut self.snap);
            let mut snap =
                crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
            let out = zu_exec::try_execute_streaming(
                &cached.plan,
                &cached.query,
                &cached.schema,
                &mut snap,
                &args,
                &options,
                &mut st,
            );
            self.snap = snap.into_cache();
            if out? {
                return Ok(st.done(cached.query.columns.clone(), Vec::new()));
            }
        }
        exec::execute_streaming(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut self.graph,
            &args,
            &options,
            &mut st,
        )
    }

    /// Runs one query, compiling it on the first sighting of this text
    /// and reusing the cached plan afterwards.
    pub fn run(&mut self, source: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.refresh()?;
        match query::not_a_query(source)? {
            Some(NotAQuery::Transaction(stmt)) => return self.transaction(stmt),
            // A catalog statement publishes a new epoch, and the plans
            // and readers this session holds describe the old one.
            // Refreshing after it is what drops them, so the next query
            // compiles against the catalog the statement just wrote.
            Some(NotAQuery::Catalog(stmt)) => {
                self.refuse_a_write()?;
                let held = self.hold()?;
                let out = crate::catalog_stmt::apply(self.graph.file_mut(), &stmt);
                self.settle(out, held)?;
                self.refresh()?;
                return Ok(QueryResult::new(Vec::new(), Vec::new()));
            }
            None => {}
        }
        let cached = match self.plan_for(source) {
            Ok(cached) => cached,
            // A label under `INSERT` that names no node table is a table
            // the statement means the graph to have, and there is no
            // statement in GQL that makes one, so this makes it. It is a
            // catalog change this statement makes, so it happens under
            // the savepoint the statement holds and goes back with it.
            Err(err) => return self.declaring(source, params, err),
        };
        let args = query::bind_args(&cached.query.params, params)?;
        // A statement that writes runs as the parts it was split into,
        // because the clauses after the write read what it made rather
        // than reading the store again.
        if let Some(parts) = &cached.parts {
            self.refuse_a_write()?;
            let held = self.hold()?;
            let out = self.run_parts(&cached, parts, args);
            return self.settle(out, held);
        }
        let options = self.options.clone();
        if query::exec2_enabled() {
            let catalog = self.graph.catalog().clone();
            let warm = std::mem::take(&mut self.snap);
            let mut snap =
                crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
            let out = zu_exec::try_execute(
                &cached.plan,
                &cached.query,
                &cached.schema,
                &mut snap,
                &args,
                &options,
            );
            self.snap = snap.into_cache();
            if let Some(r) = out? {
                return Ok(r);
            }
        }
        exec::execute(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut self.graph,
            &args,
            &options,
        )
    }

    /// Runs a statement that writes, one part at a time: the clauses
    /// before the write answer the rows it runs for, the write runs
    /// once for each of them in one transaction, and the clauses after
    /// it run over those rows with the created elements on the end.
    ///
    /// Writing here rather than inside the executor is the seam
    /// [`zu_query::plan::LogicalPlan::Insert`] describes: the executor
    /// reads through a graph, and a graph reads.
    fn run_parts(
        &mut self,
        cached: &CachedPlan,
        parts: &[crate::split::Part],
        args: Vec<Value>,
    ) -> Result<QueryResult> {
        // Asked before anything is worked out, so that a read-only
        // connection is told that it is read-only rather than told
        // something about the statement.
        crate::write::writable(self.graph.file())?;
        let options = self.options.clone();
        let mut carried: Option<Value> = None;
        for part in parts {
            let Some(write) = &part.write else {
                // A write with nothing after it answers no rows, and a
                // plan of no clauses would answer one row of no
                // columns, which is a different answer.
                if part.query.clauses.is_empty() {
                    return Ok(QueryResult::new(Vec::new(), Vec::new()));
                }
                let mut args = args.clone();
                args.extend(carried);
                return exec::execute(
                    &part.plan,
                    &part.query,
                    &cached.schema,
                    &mut self.graph,
                    &args,
                    &options,
                );
            };
            let mut args = args.clone();
            args.extend(carried.take());
            let rows = exec::execute(
                &part.plan,
                &part.query,
                &cached.schema,
                &mut self.graph,
                &args,
                &options,
            )?
            .rows;
            // The row holds the slots the write carries across it and
            // then the values it wrote, which is the order the
            // projection at the end of this part wrote them in.
            let carry = write.carry().len();
            let next = match write {
                crate::split::Write::Insert(insert) => {
                    let catalog = self.graph.catalog().clone();
                    let mut batch =
                        crate::insert::Batch::open(self.graph.file_mut(), insert, catalog)?;
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, props) = row.split_at(carry);
                        let made = batch.row(carried, props)?;
                        next.push(Value::List(carried.iter().cloned().chain(made).collect()));
                    }
                    let (new, edges) = batch.staged();
                    self.write(|txn| crate::insert::stage(txn, &new, &edges))?;
                    next
                }
                crate::split::Write::Delete(delete) => {
                    let catalog = self.graph.catalog().clone();
                    let mut removals = crate::delete::Removals::open(delete, catalog);
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, _) = row.split_at(carry);
                        removals.row(self.graph.file_mut(), carried)?;
                        next.push(Value::List(carried.to_vec()));
                    }
                    let (rows, edges) = removals.staged();
                    self.write(|txn| crate::delete::stage(txn, &rows, &edges))?;
                    next
                }
                crate::split::Write::Set(set) => {
                    let catalog = self.graph.catalog().clone();
                    let mut changes = crate::set::Changes::open(set, catalog);
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, values) = row.split_at(carry);
                        changes.row(self.graph.file_mut(), carried, values)?;
                        next.push(Value::List(carried.to_vec()));
                    }
                    let (updates, widened) = changes.staged();
                    // A label the table had not declared is a catalog
                    // change this statement makes, and it goes in first:
                    // the fold turns away a label change onto a table
                    // that has not declared the bit, and what it reads
                    // to decide that is the catalog in the file. The
                    // statement holds a savepoint, so a change that
                    // fails after this takes the declaration with it.
                    if let Some(catalog) = widened {
                        catalog.store(self.graph.file_mut())?;
                    }
                    self.write(|txn| crate::set::stage(txn, &updates))?;
                    next
                }
            };
            carried = Some(Value::List(next));
        }
        unreachable!("the last part of a split statement writes nothing")
    }

    /// Compiles a statement and pins it under an id. The id maps back
    /// to the source text, so a catalog change between prepare and
    /// execute recompiles instead of running a stale plan. Returns the
    /// id and the parameter names the statement wants, in binder
    /// order.
    pub fn prepare(&mut self, source: &str) -> Result<(u64, Vec<String>)> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let params = cached.query.params.clone();
        let id = self.next_stmt;
        self.next_stmt += 1;
        self.stmts.insert(id, source.to_string());
        Ok((id, params))
    }

    /// Makes sure a statement is compiled and cached, and reports
    /// whether it already was. A server preloads its statement set
    /// with this at startup; the P0 plan-hit gate times the `true`
    /// path, which is exactly the work between receiving a known
    /// query text and starting execution: the epoch check and the
    /// cache lookup.
    pub fn warm(&mut self, source: &str) -> Result<bool> {
        self.refresh()?;
        if self.plans.contains_key(source) {
            return Ok(true);
        }
        self.plan_for(source)?;
        Ok(false)
    }

    pub fn execute(&mut self, stmt: u64, params: &[(&str, Value)]) -> Result<QueryResult> {
        let source = self
            .stmts
            .get(&stmt)
            .cloned()
            .ok_or_else(|| ZuError::InvalidArgument(format!("no prepared statement {stmt}")))?;
        self.run(&source, params)
    }

    pub fn close_stmt(&mut self, stmt: u64) -> bool {
        self.stmts.remove(&stmt).is_some()
    }

    /// The plan this session would run for `source`, rendered, without
    /// running it.
    ///
    /// [`Self::explain_analyze`] is the same listing with the observed
    /// counters in it, and it costs a full execution to produce. A
    /// caller that wants to record how a statement ran beside a latency
    /// it measured somewhere else cannot afford that execution: for a
    /// read it would double the work being timed, and for a write it
    /// would apply the write a second time. This one compiles and
    /// renders, which on a warm session is a hash lookup and a string.
    ///
    /// It takes no parameters because zu plans on the statement text
    /// and the schema alone. Accepting them would say the plan depends
    /// on their values, and then a caller would have some reason to
    /// pass the ones it is about to bind.
    pub fn explain(&mut self, source: &str) -> Result<String> {
        Ok(self.explain_plan(source)?.render())
    }

    /// The same plan as the operators it is made of, for a caller that
    /// wants to read it rather than print it.
    ///
    /// A plan viewer, a test asserting that a statement reaches an
    /// index, a tool colouring the expands red: each of those wants the
    /// tree, and each of them parsing the listing back into one would
    /// be a parser of a format nothing promised to keep. The listing is
    /// [`zu_query::plan::QueryPlan::render`] of this, so what a caller
    /// reads and what a caller prints cannot disagree.
    pub fn explain_plan(&mut self, source: &str) -> Result<QueryPlan> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let mut described = zu_query::plan::describe(&cached.plan, &cached.query, &cached.schema);
        described.notes = cached.notes.clone();
        Ok(described)
    }

    /// EXPLAIN ANALYZE through the session: same cache, same options,
    /// profiled execution.
    pub fn explain_analyze(&mut self, source: &str, params: &[(&str, Value)]) -> Result<String> {
        let notes = self.plan_for(source)?.notes.clone();
        let listing = self.profile(source, params)?.render();
        let listing = match self.decisions(source, params)? {
            Some(d) => format!("{listing}decisions:\n{}", d.render()),
            None => listing,
        };
        Ok(query::noted(notes, listing))
    }

    /// The record the pipeline executor keeps of what it decided while
    /// it ran, `None` when this query is not one it covers. The
    /// counters above come from the old executor, so reading the record
    /// costs a second run of the query on the other engine; EXPLAIN
    /// ANALYZE already costs one execution and is not on any path that
    /// answers a caller.
    fn decisions(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
    ) -> Result<Option<zu_exec::decide::Decisions>> {
        if !query::exec2_enabled() {
            return Ok(None);
        }
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        let options = self.options.clone();
        let catalog = self.graph.catalog().clone();
        let warm = std::mem::take(&mut self.snap);
        let mut snap =
            crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
        let out = zu_exec::try_execute_profiled(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut snap,
            &args,
            &options,
        );
        self.snap = snap.into_cache();
        Ok(out?.map(|(_, d)| d))
    }

    /// The same run, handing back the counters instead of the
    /// rendering, for callers that want the numbers. `zu bench
    /// cardinality` reads q-error off this.
    pub fn profile(&mut self, source: &str, params: &[(&str, Value)]) -> Result<exec::Profile> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        if cached.parts.is_some() {
            return Err(ZuError::Unsupported {
                what: "profiling a statement that writes, which runs as the parts it was split at its write into rather than as the one plan a profile describes",
                id: 0,
            });
        }
        let args = query::bind_args(&cached.query.params, params)?;
        let options = self.options.clone();
        let (_, profile) = exec::execute_profiled(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut self.graph,
            &args,
            &options,
        )?;
        Ok(profile)
    }

    /// Runs a statement that did not compile, in case what it wanted was
    /// a table named by a label nothing has declared.
    ///
    /// `failed` is what compiling said and it is handed back untouched
    /// when there is no such table to make, which is nearly every time
    /// this is reached: the second parse is paid for by statements that
    /// were going to raise anyway, and the statements that compile pay
    /// nothing.
    fn declaring(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        failed: ZuError,
    ) -> Result<QueryResult> {
        // Text that will not parse and a `USE` of a graph that is not
        // there are both what `failed` already says, better than
        // anything this could add.
        let Ok(parsed) = zu_query::parser::parse(source) else {
            return Err(failed);
        };
        let Ok(graph) = query::graph_of(self.graph.catalog(), self.working, &parsed) else {
            return Err(failed);
        };
        let wanted = crate::declare::wanted(self.graph.catalog(), graph, &parsed)?;
        if wanted.is_empty() {
            return Err(failed);
        }
        self.refuse_a_write()?;
        let held = self.hold()?;
        let out = self.declared(source, params, graph, &wanted);
        self.settle(out, held)
    }

    /// Makes the tables and runs the statement that wanted them, both
    /// under the savepoint [`Self::declaring`] took.
    fn declared(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        graph: u32,
        wanted: &[crate::declare::NewTable],
    ) -> Result<QueryResult> {
        crate::declare::create(self.graph.file_mut(), graph, wanted)?;
        // The tables are published now, and the schemas this session
        // holds describe the catalog from before them.
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        let parts = cached.parts.as_ref().ok_or_else(|| {
            ZuError::InvalidArgument(
                "a statement that makes a table writes, and this one compiled as a read"
                    .to_string(),
            )
        })?;
        self.run_parts(&cached, parts, args)
    }

    fn plan_for(&mut self, source: &str) -> Result<Arc<CachedPlan>> {
        if let Some(cached) = self.plans.get(source) {
            return Ok(cached.clone());
        }
        // The text is parsed before anything is compiled because the
        // `USE` clause in front of it says which graph's tables the
        // names below it are names of.
        let parsed = zu_query::parser::parse(source)?;
        let graph = query::graph_of(self.graph.catalog(), self.working, &parsed)?;
        let schema = self.schema_for(graph)?;
        let (query, plan, notes) = query::compile_parsed(&parsed, &schema)?;
        let parts = crate::split::split(&query, &schema)?;
        let cached = Arc::new(CachedPlan {
            schema,
            query,
            plan,
            parts,
            notes,
        });
        if self.plans.len() >= PLAN_CAP {
            self.plans.clear();
        }
        self.plans.insert(source.to_string(), cached.clone());
        Ok(cached)
    }

    /// The schema of one graph, built on the first statement that names
    /// it. A session that never leaves its working graph builds one.
    fn schema_for(&mut self, graph: u32) -> Result<Arc<zu_query::binder::Schema>> {
        if let Some(schema) = self.schemas.get(&graph) {
            return Ok(schema.clone());
        }
        let catalog = self.graph.catalog().clone();
        let schema = Arc::new(query::schema_with_stats(
            self.graph.file_mut(),
            &catalog,
            graph,
        )?);
        self.schemas.insert(graph, schema.clone());
        Ok(schema)
    }

    fn refresh(&mut self) -> Result<()> {
        let epoch = self.graph.file().db_header().epoch;
        if epoch == self.epoch {
            return Ok(());
        }
        let (catalog, schema) = query::load_schema(self.graph.file_mut())?;
        self.working = catalog.home_graph_id();
        self.graph.set_catalog(catalog);
        self.schemas.clear();
        self.schemas.insert(self.working, Arc::new(schema));
        self.plans.clear();
        // The readers the last epoch's snapshots loaded describe a
        // layout that has moved, so they go with the plans.
        self.snap = crate::snapshot::SnapshotCache::default();
        self.epoch = epoch;
        Ok(())
    }
}

/// A session that goes away with a transaction still running on it has
/// not committed one, so the transaction is rolled back rather than
/// left published. Nothing here can report a failure to do that, which
/// is the reason to end a transaction with a statement and not with a
/// drop: the statement says what went wrong.
impl Drop for Session {
    fn drop(&mut self) {
        if self.txn.is_some() && self.graph.file().in_savepoint() {
            self.writer = None;
            let _ = self.graph.file_mut().rollback_savepoint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::catalog::GraphTypeOf;
    use crate::zu1::graph;

    fn seeded(path: &Path) -> Vec<(u32, u32)> {
        let mut db = Zu1File::create(path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        edges
    }

    /// Two people with a name, which is what a statement in a
    /// transaction writes and what the statement after it counts.
    fn people(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        graph::bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay"];
        crate::zu1::props::store_props(
            &mut db,
            "person",
            &[("name", crate::zu1::props::PropValues::Str(&names))],
        )
        .expect("props");
    }

    fn count(session: &mut Session, source: &str) -> i64 {
        match &session.run(source, &[]).expect("count").rows[0][0] {
            Value::Int(n) => *n,
            other => panic!("expected a count, got {other:?}"),
        }
    }

    const PEOPLE: &str = "MATCH (p:person) RETURN count(p) AS n";

    /// The statement the milestone is about: two statements are one
    /// transaction, and the word at the end unmakes both of them even
    /// though each of them published an epoch of its own on the way.
    #[test]
    fn a_rollback_unmakes_every_statement_of_its_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rollback.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");
        assert_eq!(count(&mut session, PEOPLE), 2);

        session.run("START TRANSACTION", &[]).expect("start");
        assert!(session.in_transaction());
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("first");
        session
            .run("INSERT (p:person {name: 'raj'})", &[])
            .expect("second");
        // Read your own writes: each statement committed, so the next
        // one reads what it wrote.
        assert_eq!(count(&mut session, PEOPLE), 4);

        session.run("ROLLBACK", &[]).expect("rollback");
        assert!(!session.in_transaction());
        assert_eq!(count(&mut session, PEOPLE), 2);
        drop(session);

        // And it is the file that was put back, not a view of it.
        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(count(&mut reopened, PEOPLE), 2);
        let names = reopened
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("names");
        assert_eq!(names.rows.len(), 2);
    }

    /// The rollback the process never got to run. A session that goes
    /// away without a word at the end has its transaction taken back by
    /// the drop, so the way to be the crash is to leave the handle
    /// where the drop cannot reach it, which is what a process that
    /// stops does to every handle it holds.
    #[test]
    fn a_transaction_the_process_died_inside_is_gone_when_the_file_opens_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("first");
        session
            .run("INSERT (p:person {name: 'raj'})", &[])
            .expect("second");
        assert_eq!(count(&mut session, PEOPLE), 4);
        std::mem::forget(session);

        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(
            count(&mut reopened, PEOPLE),
            2,
            "both statements of the open transaction went with it"
        );
        // And the file is a file again: what comes after the rollback
        // is written on top of the state it went back to and stays.
        reopened
            .run("INSERT (p:person {name: 'ann'})", &[])
            .expect("after");
        drop(reopened);
        let mut again = Session::open(&path).expect("open a third time");
        assert_eq!(count(&mut again, PEOPLE), 3);
    }

    /// The other half of it: a transaction whose `COMMIT` returned is a
    /// transaction a crash cannot take back.
    #[test]
    fn a_committed_transaction_survives_the_process_going_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crash-after-commit.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("first");
        session
            .run("INSERT (p:person {name: 'raj'})", &[])
            .expect("second");
        session.run("COMMIT", &[]).expect("commit");
        std::mem::forget(session);

        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(count(&mut reopened, PEOPLE), 4);
    }

    #[test]
    fn a_commit_keeps_every_statement_of_its_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("commit.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("insert");
        session
            .run("MATCH (p:person {name: 'ada'}) SET p.name = 'ada2'", &[])
            .expect("set");
        session.run("COMMIT", &[]).expect("commit");
        assert!(!session.in_transaction());
        assert_eq!(count(&mut session, PEOPLE), 3);
        drop(session);

        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(count(&mut reopened, PEOPLE), 3);
        assert_eq!(
            count(
                &mut reopened,
                "MATCH (p:person {name: 'ada2'}) RETURN count(p) AS n"
            ),
            1,
            "the committed transaction kept what its second statement wrote"
        );
    }

    /// A transaction that only reads holds nothing, so neither word
    /// that ends one costs an epoch.
    #[test]
    fn a_transaction_that_reads_costs_nothing_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");
        let epoch = session.epoch();

        for ending in ["COMMIT", "ROLLBACK"] {
            session
                .run("START TRANSACTION READ ONLY", &[])
                .expect("start");
            assert_eq!(count(&mut session, PEOPLE), 2);
            session.run(ending, &[]).expect(ending);
            assert_eq!(session.epoch(), epoch, "{ending} published an epoch");
        }
    }

    /// READ ONLY is enforced rather than advisory, and it is enforced
    /// at the statement rather than at the block it would have written.
    #[test]
    fn a_read_only_transaction_turns_a_write_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mode.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session
            .run("START TRANSACTION READ ONLY", &[])
            .expect("start");
        let err = session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect_err("a write in a read only transaction");
        assert_eq!(err.gqlstatus(), Some(codes::C25G03));
        assert!(err.to_string().contains("started READ ONLY"), "{err}");
        let err = session
            .run("CREATE GRAPH TYPE t { (:person) }", &[])
            .expect_err("a catalog statement writes too");
        assert_eq!(err.gqlstatus(), Some(codes::C25G03));
        session.run("ROLLBACK", &[]).expect("rollback");

        // The same statement in the mode that is implied when none is
        // written is a statement that runs.
        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("read write");
        session.run("COMMIT", &[]).expect("commit");
        assert_eq!(count(&mut session, PEOPLE), 3);
    }

    #[test]
    fn a_transaction_does_not_nest_and_neither_word_ends_one_that_is_not_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nesting.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        for ending in ["COMMIT", "ROLLBACK"] {
            let err = session.run(ending, &[]).expect_err("nothing to end");
            assert_eq!(err.gqlstatus(), Some(codes::C2D000));
            assert!(err.to_string().contains("no transaction running"), "{err}");
        }
        session.run("START TRANSACTION", &[]).expect("start");
        let err = session
            .run("START TRANSACTION", &[])
            .expect_err("already running");
        assert_eq!(err.gqlstatus(), Some(codes::C25G01));
        session.run("COMMIT", &[]).expect("commit");
    }

    /// GP18, catalog and data statements in one transaction: the graph
    /// a `CREATE` made is there for the statements after it, and a
    /// rollback unmakes the graph and the rows written into it
    /// together.
    #[test]
    fn a_catalog_statement_and_a_write_are_one_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mixing.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("CREATE GRAPH TYPE social { (:person) }", &[])
            .expect("create");
        assert!(
            session.graph.catalog().graph_type("social").is_some(),
            "the statements after it see what it declared"
        );
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("insert");
        session.run("ROLLBACK", &[]).expect("rollback");

        assert!(session.graph.catalog().graph_type("social").is_none());
        assert_eq!(count(&mut session, PEOPLE), 2);
        drop(session);
        let mut reopened = Session::open(&path).expect("reopen");
        assert!(reopened.graph.catalog().graph_type("social").is_none());
        assert_eq!(count(&mut reopened, PEOPLE), 2);
    }

    /// GT03, several graphs in one transaction. A session is one file
    /// and a file is one catalog, so the graphs of a transaction are
    /// graphs of one catalog and the single writer commits them
    /// together; there is no second catalog here for a transaction to
    /// span.
    #[test]
    fn two_graphs_change_and_are_undone_in_one_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graphs.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");
        session
            .run("CREATE GRAPH other ANY AS COPY OF home", &[])
            .expect("a second graph to write into");
        assert_eq!(count(&mut session, PEOPLE), 2);

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("into the working graph");
        session
            .run("USE other INSERT (p:person {name: 'raj'})", &[])
            .expect("into the other one");
        assert_eq!(count(&mut session, PEOPLE), 3);
        assert_eq!(count(&mut session, &format!("USE other {PEOPLE}")), 3);

        session.run("ROLLBACK", &[]).expect("rollback");
        assert_eq!(count(&mut session, PEOPLE), 2);
        assert_eq!(count(&mut session, &format!("USE other {PEOPLE}")), 2);
    }

    /// A statement outside a transaction is one anyway, and this is
    /// the part of that which needed the file to keep something: a
    /// statement that writes and then raises had already committed the
    /// write, because the clauses after a write read what it wrote.
    #[test]
    fn a_statement_that_raises_after_it_wrote_leaves_the_file_as_it_found_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("implicit.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        let err = session
            .run(
                "INSERT (p:person {name: 'zoe'}) WITH p RETURN p.name / 0 AS bad",
                &[],
            )
            .expect_err("the clause after the write raises");
        assert!(!err.to_string().is_empty());
        assert_eq!(count(&mut session, PEOPLE), 2, "the row went with it");
        drop(session);
        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(count(&mut reopened, PEOPLE), 2);
    }

    /// A session that goes away with a transaction open has not
    /// committed it.
    #[test]
    fn a_session_dropped_mid_transaction_rolls_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dropped.zu1");
        people(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session.run("START TRANSACTION", &[]).expect("start");
            session
                .run("INSERT (p:person {name: 'zoe'})", &[])
                .expect("insert");
            assert_eq!(count(&mut session, PEOPLE), 3);
        }
        let mut reopened = Session::open(&path).expect("reopen");
        assert_eq!(count(&mut reopened, PEOPLE), 2);
    }

    #[test]
    fn cached_runs_match_the_one_shot_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.zu1");
        let edges = seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let source = "MATCH (a:person {id: $src})-[:follows]->(b) \
                      RETURN b.id AS friend ORDER BY friend";
        for src in [3i64, 10, 42, 3, 3] {
            let got = session
                .run(source, &[("src", Value::Int(src))])
                .expect("session run");
            let mut want: Vec<i64> = edges
                .iter()
                .filter(|(s, _)| i64::from(*s) == src)
                .map(|(_, d)| i64::from(*d))
                .collect();
            want.sort_unstable();
            let rows: Vec<i64> = got
                .rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Int(i) => *i,
                    other => panic!("expected int, got {other:?}"),
                })
                .collect();
            assert_eq!(rows, want, "src {src}");
        }
        // The second and later runs hit the plan cache; same text, one
        // compiled entry.
        assert_eq!(session.plans.len(), 1);
    }

    #[test]
    fn a_use_clause_picks_the_graph_the_query_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("use.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");

        let source = "MATCH (a:person) RETURN count(a) AS n";
        let plain = session.run(source, &[]).expect("plain");
        let used = session
            .run(&format!("USE CURRENT_PROPERTY_GRAPH {source}"), &[])
            .expect("use current");
        assert_eq!(plain.rows, used.rows);

        // The home graph by its name is the same graph again, and a
        // name the catalog does not hold is a reference to nothing.
        let named = session
            .run(&format!("USE home {source}"), &[])
            .expect("use home");
        assert_eq!(plain.rows, named.rows);
        let err = session
            .run(&format!("USE nowhere {source}"), &[])
            .expect_err("no such graph");
        assert!(err.to_string().contains("is no graph in '/'"), "{err}");
    }

    #[test]
    fn a_second_graph_holds_no_tables_of_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("two.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        session
            .run("CREATE PROPERTY GRAPH empty ANY", &[])
            .expect("create");

        // The tables are the home graph's, so a query against the new
        // graph does not find them: a name is a name in a graph, and
        // the same statement counts rows in the one and none in the
        // other.
        let elsewhere = session
            .run("USE empty MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("the statement runs against the new graph");
        assert_eq!(elsewhere.rows, vec![vec![Value::Int(0)]]);
        let home = session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("the statement runs against the home graph");
        assert_ne!(home.rows, vec![vec![Value::Int(0)]]);
    }

    #[test]
    fn prepared_statements_bind_and_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stmt.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let (id, params) = session
            .prepare("MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n")
            .expect("prepare");
        assert_eq!(params, ["src"]);
        let r = session
            .execute(id, &[("src", Value::Int(3))])
            .expect("execute");
        assert_eq!(r.columns, ["n"]);
        assert!(session.close_stmt(id));
        assert!(!session.close_stmt(id));
        let err = session
            .execute(id, &[("src", Value::Int(3))])
            .expect_err("closed");
        assert!(err.to_string().contains("no prepared statement"));
    }

    #[test]
    fn missing_parameter_is_an_error_not_a_stale_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("param.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        let err = session
            .run(
                "MATCH (a:person {id: $src}) RETURN a.id AS id",
                &[("wrong", Value::Int(1))],
            )
            .expect_err("missing param");
        assert!(err.to_string().contains("missing parameter $src"));
    }

    #[test]
    fn table_readers_outlive_a_query_but_not_an_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readers.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        assert!(session.snap.readers.is_empty(), "nothing read yet");
        let source = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";
        session
            .run(source, &[("src", Value::Int(3))])
            .expect("warm");
        // The follows reader the seek and the hop needed is still
        // here, so the next query starts on it instead of reading its
        // directory back out of the file.
        assert_eq!(session.snap.readers.len(), 1);
        session
            .run(source, &[("src", Value::Int(10))])
            .expect("second");
        assert_eq!(session.snap.readers.len(), 1);

        // A moved epoch describes a layout those readers were built
        // for and no longer describes, so they go with the plans.
        session.graph.file_mut().db_header_mut().epoch += 1;
        session.refresh().expect("refresh");
        assert!(session.snap.readers.is_empty(), "stale readers dropped");
        assert!(session.snap.props.is_empty(), "stale props dropped");
        session
            .run(source, &[("src", Value::Int(3))])
            .expect("after epoch move");
        assert_eq!(session.snap.readers.len(), 1);
    }

    #[test]
    fn epoch_move_reloads_catalog_and_drops_plans() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("epoch.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("warm");
        assert_eq!(session.plans.len(), 1);
        // Push the header epoch forward behind the session's back, the
        // shape a writer commit through the same handle leaves.
        session.graph.file_mut().db_header_mut().epoch += 1;
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("after epoch move");
        assert_eq!(session.plans.len(), 1);
        assert_eq!(session.epoch, session.graph.file().db_header().epoch);
    }

    #[test]
    fn a_catalog_statement_publishes_and_the_session_sees_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("types.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("warm");
        assert_eq!(session.plans.len(), 1);
        let before = session.epoch;

        let result = session
            .run(
                "CREATE PROPERTY GRAPH TYPE social {
                   NODE TYPE PersonType (:person {name :: STRING}),
                   (PersonType)-[:follows]->(PersonType)
                 }",
                &[],
            )
            .expect("create graph type");
        assert!(
            result.columns.is_empty(),
            "a statement that answers no rows"
        );
        assert_eq!(result.status(), zu_common::gqlstatus::codes::C00001);
        assert!(session.epoch > before, "the statement published an epoch");
        assert!(session.plans.is_empty(), "the plans went with the epoch");
        let ty = session
            .graph
            .catalog()
            .graph_type("social")
            .expect("the session reloaded the catalog it just wrote");
        assert!(ty.closed);
        assert_eq!(ty.elements.len(), 2);

        // The graph still answers the query it answered before, now
        // against a catalog that declares a type for it.
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("after the statement");

        // GC03: the modifier is what turns a taken name into a
        // statement that did nothing.
        let err = session
            .run("CREATE GRAPH TYPE social { (:person) }", &[])
            .expect_err("the name is taken")
            .to_string();
        assert!(err.contains("already a graph type"), "{err}");
        session
            .run("CREATE GRAPH TYPE IF NOT EXISTS social { (:person) }", &[])
            .expect("if not exists");
        assert_eq!(session.graph.catalog().graph_types().len(), 1);
        // The other answer to a taken name: take it over.
        session
            .run("CREATE OR REPLACE GRAPH TYPE social { (:person) }", &[])
            .expect("or replace");
        assert_eq!(session.graph.catalog().graph_types().len(), 1);
        assert_eq!(
            session
                .graph
                .catalog()
                .graph_type("social")
                .expect("social")
                .elements
                .len(),
            1,
            "the replacement is the type that is there now"
        );

        // GG04, off the tables of the graph named and not off its data.
        session
            .run("CREATE GRAPH TYPE mirror LIKE home", &[])
            .expect("like the graph this file holds");
        let mirror = session
            .graph
            .catalog()
            .graph_type("mirror")
            .expect("mirror");
        assert_eq!(
            mirror
                .elements
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["person", "follows"]
        );

        session.run("DROP GRAPH TYPE mirror", &[]).expect("drop");
        assert!(session.graph.catalog().graph_type("mirror").is_none());
        let err = session
            .run("DROP GRAPH TYPE mirror", &[])
            .expect_err("gone already")
            .to_string();
        assert!(err.contains("is no graph type here"), "{err}");
        session
            .run("DROP GRAPH TYPE IF EXISTS mirror", &[])
            .expect("if exists");
    }

    #[test]
    fn a_schema_is_a_directory_the_file_holds_and_a_graph_lives_in_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schemas.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // GC01, GC02.
        session.run("CREATE SCHEMA /app", &[]).expect("create");
        let err = session
            .run("CREATE SCHEMA /app", &[])
            .expect_err("the path is taken")
            .to_string();
        assert!(err.contains("already a schema"), "{err}");
        session
            .run("CREATE SCHEMA IF NOT EXISTS /app", &[])
            .expect("if not exists");
        assert!(session.graph.catalog().has_schema("/app"));

        // GC04: a graph in that schema, named by the path it is at.
        session
            .run("CREATE GRAPH /app/social ANY", &[])
            .expect("create graph");
        assert!(session.graph.catalog().graph("/app", "social").is_some());

        // ISO's default is RESTRICT, so the schema holding it stays.
        let err = session
            .run("DROP SCHEMA /app", &[])
            .expect_err("the schema still holds a graph")
            .to_string();
        assert!(err.contains("still holds the graph 'social'"), "{err}");
        session
            .run("DROP GRAPH /app/social", &[])
            .expect("drop graph");
        session.run("DROP SCHEMA /app", &[]).expect("drop schema");
        assert!(!session.graph.catalog().has_schema("/app"));
        let err = session
            .run("DROP SCHEMA /", &[])
            .expect_err("the root schema is not one to drop")
            .to_string();
        assert!(err.contains("every file has"), "{err}");
    }

    #[test]
    fn a_graph_is_created_with_a_type_or_with_none_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graphs.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // GG01: any graph, which is what a zu1 file has always been.
        session.run("CREATE GRAPH open_one ANY", &[]).expect("any");
        assert_eq!(
            session.graph.catalog().graph("/", "open_one"),
            session
                .graph
                .catalog()
                .graphs()
                .iter()
                .find(|g| g.name == "open_one")
        );
        // GC05.
        let err = session
            .run("CREATE GRAPH open_one ANY", &[])
            .expect_err("the name is taken")
            .to_string();
        assert!(err.contains("already a graph"), "{err}");
        session
            .run("CREATE GRAPH IF NOT EXISTS open_one ANY", &[])
            .expect("if not exists");

        // GG03: a type written where the graph is created has no name
        // of its own, so it is no graph type the file holds.
        session
            .run(
                "CREATE PROPERTY GRAPH typed { (:Person {name :: STRING}) }",
                &[],
            )
            .expect("inline type");
        let typed = session
            .graph
            .catalog()
            .graph("/", "typed")
            .expect("typed")
            .clone();
        let GraphTypeOf::Inline(ty) = &typed.graph_type else {
            panic!("a type written inline");
        };
        assert!(ty.closed);
        assert_eq!(ty.elements.len(), 1);
        assert!(session.graph.catalog().graph_types().is_empty());

        // GG02, GG04: the name of a graph type the file holds, and the
        // type of a graph that already has one.
        session
            .run("CREATE GRAPH TYPE social { (:person) }", &[])
            .expect("graph type");
        session
            .run("CREATE GRAPH of_named :: social", &[])
            .expect("of a named type");
        assert_eq!(
            session
                .graph
                .catalog()
                .graph("/", "of_named")
                .map(|g| &g.graph_type),
            Some(&GraphTypeOf::Named("social".to_string()))
        );
        session
            .run("CREATE PROPERTY GRAPH mirror LIKE typed", &[])
            .expect("like a graph");
        let GraphTypeOf::Inline(ty) = &session
            .graph
            .catalog()
            .graph("/", "mirror")
            .expect("mirror")
            .graph_type
        else {
            panic!("the type of the graph it is like");
        };
        assert_eq!(ty.elements.len(), 1);

        // GG05: an empty graph copies as the empty graph it is.
        session
            .run("CREATE GRAPH copy_of_it ANY AS COPY OF open_one", &[])
            .expect("copy of an empty graph");
        assert!(
            session
                .graph
                .catalog()
                .graph_tables(
                    session
                        .graph
                        .catalog()
                        .graph("/", "copy_of_it")
                        .expect("the copy")
                        .id
                )
                .is_empty()
        );

        let err = session
            .run("CREATE GRAPH lost ANY AS COPY OF nowhere", &[])
            .expect_err("no such graph")
            .to_string();
        assert!(err.contains("is no graph in"), "{err}");
        assert!(session.graph.catalog().graph("/", "lost").is_none());

        // A replacement frees what the old graph held, so one that
        // cannot be kept is refused before anything is freed and the
        // graph that was there is still there, tables included.
        let tables = session.graph.catalog().node_tables().len();
        let err = session
            .run("CREATE OR REPLACE GRAPH home :: nowhere", &[])
            .expect_err("no such graph type")
            .to_string();
        assert!(err.contains("is no graph type here"), "{err}");
        assert_eq!(session.graph.catalog().node_tables().len(), tables);
        assert!(session.graph.catalog().graph("/", "home").is_some());
    }

    #[test]
    fn a_copy_of_a_graph_holds_the_rows_of_the_one_it_copied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("copy.zu1");
        let edges = seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("CREATE GRAPH twin ANY AS COPY OF home", &[])
            .expect("copy of a graph that holds tables");

        let pattern = "MATCH (a:person {id: $src})-[:follows]->(b) \
                       RETURN b.id AS friend ORDER BY friend";
        let mut want: Vec<i64> = edges
            .iter()
            .filter(|(s, _)| *s == 3)
            .map(|(_, d)| i64::from(*d))
            .collect();
        want.sort_unstable();
        let friends = |session: &mut Session, source: &str| -> Vec<i64> {
            session
                .run(source, &[("src", Value::Int(3))])
                .expect("query")
                .rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Int(i) => *i,
                    other => panic!("expected int, got {other:?}"),
                })
                .collect()
        };
        assert_eq!(friends(&mut session, pattern), want);
        assert_eq!(friends(&mut session, &format!("USE twin {pattern}")), want);

        // The graph the statement is against is a graph to copy like
        // any other, and the one a file loaded from outside has its
        // tables in.
        session
            .run(
                "CREATE GRAPH twin_of_here ANY AS COPY OF CURRENT_PROPERTY_GRAPH",
                &[],
            )
            .expect("copy of the graph the statement is against");
        assert_eq!(
            friends(&mut session, &format!("USE twin_of_here {pattern}")),
            want
        );

        // The copy holds blocks of its own, which dropping it is what
        // proves: a copy that had merely pointed at the source's
        // segments would have handed the source's blocks back here and
        // the query below would read a graph that is no longer there.
        session.run("DROP GRAPH twin", &[]).expect("drop the copy");
        assert_eq!(friends(&mut session, pattern), want);
        let err = session
            .run(&format!("USE twin {pattern}"), &[])
            .expect_err("the copy is gone")
            .to_string();
        assert!(err.contains("which is no graph in"), "{err}");
    }

    /// How many blocks the committed free list names, which is what a
    /// drop that reclaims has to grow.
    fn free_blocks(db: &mut Zu1File) -> u64 {
        let root = db.db_header().free_list_root;
        if root == crate::zu1::file::NULL_BLOCK {
            return 0;
        }
        let count = db.db_header().block_count;
        let bytes = crate::zu1::meta::read_chain(db, root).expect("the free list chain");
        crate::zu1::file::decode_free_list(&bytes, count)
            .expect("a free list")
            .len() as u64
    }

    #[test]
    fn dropping_a_graph_hands_the_blocks_its_tables_held_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reclaim.zu1");
        let edges = seeded(&path);

        let first_load = {
            let mut db = Zu1File::open(&path).expect("open");
            assert_eq!(free_blocks(&mut db), 0, "a fresh load frees nothing");
            db.db_header().block_count
        };
        let mut session = Session::open(&path).expect("open");
        session
            .run("DROP GRAPH home", &[])
            .expect("drop the graph this file holds");
        assert!(session.graph.catalog().node_tables().is_empty());
        assert!(session.graph.catalog().rel_tables().is_empty());
        drop(session);

        let mut db = Zu1File::open(&path).expect("reopen");
        let freed = free_blocks(&mut db);
        assert!(
            freed >= first_load / 2,
            "{freed} blocks back of the {first_load} the load took"
        );
        // The blocks are back rather than merely unnamed, so loading
        // the same graph again writes into them: it costs the file a
        // block or two where the first load cost it all of them.
        let before = db.db_header().block_count;
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load again");
        let grew = db.db_header().block_count - before;
        assert!(
            grew < first_load / 2,
            "the reload grew the file by {grew} blocks, the first load by {first_load}"
        );
        let mut session = Session::open(&path).expect("open again");
        let rows = session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("the reloaded graph answers");
        assert_eq!(rows.rows.len(), 1);
    }

    #[test]
    fn a_graph_type_that_cannot_be_kept_leaves_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("refused.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let epoch = session.epoch;
        let labels = session.graph.catalog().labels().len();
        // The catalog writes a property type with the codes a column
        // stores, and a list of lists is not one of them. The statement
        // interned `Ghost` on the way to finding that out, and the file
        // still has to come out unchanged.
        let err = session
            .run(
                "CREATE GRAPH TYPE strict { (:Ghost {seen :: LIST<LIST<STRING>>}) }",
                &[],
            )
            .expect_err("a property type no column can hold")
            .to_string();
        assert!(err.contains("a type this file cannot write"), "{err}");
        session.refresh().expect("refresh");
        assert_eq!(session.epoch, epoch, "nothing was published");
        assert_eq!(session.graph.catalog().labels().len(), labels);
        assert!(session.graph.catalog().graph_type("strict").is_none());
    }
}
