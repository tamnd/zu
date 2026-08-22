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

use zu_common::gqlstatus::{DiagnosticRecord, Subject, codes};
use zu_common::{IdMap, Interrupt, Result, ZuError};
use zu_query::ast::{
    BindingDef, BindingInit, BindingKind, GraphRef, SchemaRef, SessionReset, SessionStmt, TxnStmt,
};
use zu_query::binder::BoundQuery;
use zu_query::exec::{self, Streamed};
use zu_query::frame::{Frame, FrameSet};
use zu_query::plan::{LogicalPlan, QueryPlan};
use zu_query::refs::{BindingTable, GraphHandle};
use zu_query::row::{Batch, Flow};

use crate::query::{self, NotAQuery, QueryResult, Value, Zu1Graph};
use crate::shared::{FileHandle, Lease, Published, WriteSide};
use crate::write::Patches;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::wal::Commits;

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

/// What a statement text came to when this session last compiled it,
/// which is either a plan or the condition it was refused with.
///
/// The refusal is kept for the same reason the plan is. Compiling
/// depends on the text and the schema and on nothing else, so a
/// statement that does not parse, or that names a variable nothing
/// bound, is refused the same way every time until the epoch moves,
/// and the epoch is already what empties this cache. A client sending
/// a bad statement in a loop is the case worth having: it pays for the
/// parse once instead of on every send, and the refusal it reads is
/// the one the parse produced.
/// The cache key of a text whose `USE` named a parameter, which is the
/// text and the graph it was compiled against. A nul byte joins them
/// because a statement cannot hold one, so no text can be written that
/// collides with the key of another.
fn focused_key(source: &str, graph: u32) -> String {
    format!("{graph}\0{source}")
}

enum Compiled {
    Plan(Arc<CachedPlan>),
    /// Only a GQL condition is kept. An io failure reading the stats a
    /// schema is built from says something about the moment rather
    /// than about the text, and remembering one would go on refusing a
    /// statement that has nothing wrong with it.
    Refused(Box<DiagnosticRecord>),
}

impl Compiled {
    fn result(&self) -> Result<Arc<CachedPlan>> {
        match self {
            Compiled::Plan(plan) => Ok(plan.clone()),
            Compiled::Refused(record) => Err(ZuError::Gql(record.clone())),
        }
    }
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

/// What a statement that writes took on the way in and owes on the way
/// out: the file's savepoint, the write side, or both.
///
/// They come apart because a statement inside an explicit transaction
/// takes neither, the transaction is holding both already, and one
/// that raises inside a transaction must not give back what the
/// statements after it still need.
#[derive(Clone, Copy, Debug)]
struct Held {
    /// Whether this statement is the one that will let go of the state
    /// the file is keeping.
    savepoint: bool,
    /// Whether this statement is the one that took the write side.
    entered: bool,
}

/// A value this session holds under a name (ISO 7.1, GS01 through
/// GS03).
///
/// It is a binding variable that outlived the statement that made it,
/// which is what a session parameter is: the same three kinds, worked
/// out the same way, kept until something resets it rather than until
/// the statement ends.
#[derive(Clone, Debug)]
struct SessionParam {
    /// Which of the three kinds it was set as. Kept so that what a
    /// session is holding can be listed and said back in the words it
    /// was written in.
    kind: BindingKind,
    value: Value,
}

/// How many epochs a held binding table may fall behind before the
/// session mentions it.
///
/// Any number here is a judgement rather than a fact, and this one is
/// the judgement that a session which has fallen sixteen epochs behind
/// is holding rows from before a fair amount of churn and would like to
/// hear about it, where one or two epochs behind is what an ordinary
/// busy store does under a session that is doing nothing wrong.
pub const DEFAULT_STALE_BOUND: u64 = 16;

/// What has happened to what a held reference parameter names, which is
/// what decides whether the session says anything about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    /// The reference still means what it meant.
    Fresh,
    /// Behind the bound but still usable: the rows hold no element
    /// references, so what they say is as true now as it was.
    Old,
    /// A statement reading it will be refused: the graph is dropped, or
    /// the rows name elements of a snapshot that has moved.
    Gone,
}

/// What one held reference parameter is pinned to (plan/06 §2).
///
/// Worth being clear about what a pin is here and what it is not. It is
/// an epoch number written on a handle, and it holds no blocks: a lease
/// lives for a statement, and a binding table parameter holds rows that
/// were copied out of the snapshot rather than a reader into it. So a
/// session sitting on an old parameter does not stop a checkpoint from
/// reusing anything and does not grow the store. What goes stale is the
/// meaning of the reference, not the space behind it, and that is what
/// this reports.
#[derive(Clone, Debug)]
pub struct Pin {
    /// The name the session holds it under, without the dollar.
    pub name: String,
    /// `GRAPH` or `BINDING TABLE`, the word it was set with.
    pub kind: &'static str,
    /// The epoch the reference was taken at.
    pub epoch: u64,
    /// What it names, as a line a person reads.
    pub what: String,
    /// Whether it still means what it meant.
    pub stale: Stale,
}

/// The parameters one statement runs with, which are what the caller
/// passed on top of what the session holds (GS01 through GS03).
///
/// The caller's win, name for name. A parameter passed with the
/// statement is the nearer of the two, the way a binding variable
/// defined at the head of a statement stands in front of a graph the
/// catalog happens to hold under that name.
fn merged_params<'a>(
    held: &'a HashMap<String, SessionParam>,
    passed: &[(&'a str, Value)],
) -> Vec<(&'a str, Value)> {
    let mut out = Vec::with_capacity(held.len() + passed.len());
    out.extend_from_slice(passed);
    for (name, param) in held {
        if !passed.iter().any(|(n, _)| *n == name) {
            out.push((name.as_str(), param.value.clone()));
        }
    }
    out
}

pub struct Session {
    graph: Zu1Graph<'static>,
    /// The graph a statement is against when it does not say, which is
    /// the home graph until a `SESSION SET PROPERTY GRAPH` moves it
    /// (GS06).
    working: u32,
    /// GS05. The catalog schema this session works in, which is the
    /// root until a `SESSION SET SCHEMA` moves it. A name written with
    /// no path in front of it is looked up here: the graph a `USE`
    /// names and the procedure a `CALL` names.
    schema: String,
    /// GS01 through GS03. The parameters this session holds, read by a
    /// statement under the same `$name` a caller's parameter is read
    /// under.
    ///
    /// Behind an `Arc` because a statement reads the set while the
    /// session it belongs to is being run through. Setting one clones
    /// the map, which happens once per `SESSION SET`; running a
    /// statement clones the pointer, and a session holding none does
    /// not even do that, which is what the plan-hit path can afford.
    params: Arc<HashMap<String, SessionParam>>,
    /// How many epochs a held binding table may fall behind before the
    /// statement after it carries a warning saying so. See
    /// [`DEFAULT_STALE_BOUND`].
    stale_bound: u64,
    /// One schema per graph a statement has named, built on the first
    /// naming and dropped when the epoch moves.
    schemas: IdMap<u32, Arc<zu_query::binder::Schema>>,
    epoch: u64,
    /// What the pipeline executor's snapshot read last time. A
    /// snapshot lives for one execution, so without this every query
    /// reopens the table readers it needs, which on a small graph is
    /// most of what the query costs.
    snap: crate::snapshot::SnapshotCache,
    plans: HashMap<String, Compiled>,
    /// The texts whose `USE` named a parameter, and the parameter each
    /// one named.
    ///
    /// A plan is against one graph's tables, and one of these texts
    /// says which graph only once the parameter is there, so its plans
    /// are held one per graph under [`focused_key`] and never under the
    /// text alone. This is what the lookup reads to know which of them
    /// this call wants. It is a map of its own so that a text naming no
    /// parameter, which is nearly every text, is still one lookup.
    focused: HashMap<String, String>,
    stmts: IdMap<u64, String>,
    next_stmt: u64,
    /// The execution switches every statement on this session runs
    /// under, read from the environment once at open. Reading them per
    /// statement would make a query's thread count depend on what some
    /// other part of the process last put in the environment, and
    /// [`crate::db::Config`] is the way a caller sets them on purpose.
    options: exec::Options,
    /// The one write side of this file in this process, shared with
    /// every other connection that has it open.
    handle: Arc<FileHandle>,
    /// The write side itself, while this session is the one writing.
    /// Taken by the statement that writes and given back when it ends,
    /// or held across an explicit transaction, which is the writer
    /// lock of docs/08 §1.
    side: Option<WriteSide>,
    /// The version of the published state this session has taken. A
    /// statement compares it and picks up what a commit on another
    /// connection left, which costs one read lock and one word when
    /// nothing has moved.
    seen: u64,
    /// The claim this session has on the epoch it is reading, taken at
    /// the top of a statement and let go of at the end of it. While it
    /// is held, a writer on another connection lists the blocks this
    /// epoch reads as free but does not allocate into them.
    lease: Option<Lease>,
    /// The explicit transaction running here, if a statement opened one.
    txn: Option<Explicit>,
    /// The props directories of the tables writes have touched, held
    /// across statements because reading one back is a block chain walk
    /// and what it says only changes when the epoch does.
    dirs: crate::set::Dirs,
    /// The cells the writer's committed but unfolded statements wrote,
    /// as the readers were last given them. Held so that handing them
    /// over again costs a pointer comparison on a session that only
    /// reads.
    patches: Arc<Patches>,
    /// The frames registered on this session, as both read paths were
    /// last given them. A registration builds a new set rather than
    /// editing this one, so a statement already running keeps the set
    /// it started with and an unregistered frame stays readable until
    /// that statement ends.
    frames: Arc<FrameSet>,
    /// The state this session's statement made, held back until it is
    /// durable, with whether the log owed nothing when it was taken.
    /// See [`Self::durable`].
    pending: Option<(Published, bool)>,
    /// The byte the log has to reach the platter through for what this
    /// session staged to have committed, and what waits for it. Held
    /// here because the wait happens after the write side has gone
    /// back and the log with it.
    owed: Option<(Arc<Commits>, u64)>,
}

impl Session {
    pub fn open(path: &Path) -> Result<Session> {
        Session::attached(FileHandle::attach(path, false, || Zu1File::open(path))?)
    }

    /// A session on a database that never touches the filesystem: the
    /// blocks a file would hold, and the log beside it, held in memory
    /// and gone when this session is. It is what the shell opens when
    /// nobody named a file, and what a caller wanting a graph rather
    /// than a directory reaches for.
    pub fn memory() -> Result<Session> {
        Session::on(crate::db::memory_file()?)
    }

    /// A session over a file handle the caller opened, which is how
    /// [`crate::db::Database`] applies a read-only or memory-limited
    /// open without this module growing a constructor per option.
    ///
    /// The handle is what the file is registered under if this process
    /// has not opened it yet, and dropped if it has: one file is one
    /// write side, and the one already registered is the one every
    /// other connection is reading through.
    pub fn on(db: Zu1File) -> Result<Session> {
        Session::attached(FileHandle::attach_to(db)?)
    }

    /// A session on a file this process already holds the write side
    /// of: its own descriptor for reading, forked off that side so the
    /// block cache and the decoded pools are shared, and the roots the
    /// side has published.
    pub fn attached(handle: Arc<FileHandle>) -> Result<Session> {
        // Leased for the length of the open, because loading the schema
        // is a read like any other and a writer on another connection
        // is free to be folding underneath it.
        let (published, lease) = FileHandle::observe(&handle);
        let mut db = handle.reader()?;
        db.follow(published.header(), published.slot());
        let (catalog, schema) = query::load_schema(&mut db)?;
        let epoch = db.db_header().epoch;
        let working = catalog.home_graph_id();
        let patches = Arc::clone(published.patches());
        let mut graph = Zu1Graph::owned(db, catalog);
        graph.set_patches(Arc::clone(&patches));
        let mut snap = crate::snapshot::SnapshotCache::default();
        snap.set_patches(Arc::clone(&patches), graph.catalog());
        Ok(Session {
            graph,
            working,
            schema: zu_query::procedures::ROOT.to_string(),
            params: Arc::new(HashMap::new()),
            stale_bound: DEFAULT_STALE_BOUND,
            schemas: IdMap::from_iter([(working, Arc::new(schema))]),
            epoch,
            snap,
            plans: HashMap::new(),
            focused: HashMap::new(),
            stmts: IdMap::default(),
            next_stmt: 1,
            options: exec::Options {
                interrupt: Interrupt::armed(),
                ..query::env_options()
            },
            seen: published.version(),
            lease: Some(lease),
            handle,
            side: None,
            txn: None,
            dirs: crate::set::Dirs::default(),
            patches,
            frames: Arc::new(FrameSet::new()),
            pending: None,
            owed: None,
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
    ///
    /// The publish before that is what makes the truncation safe. A
    /// commit folds without checkpointing and leaves the frames in the
    /// log, so a log cut with the header still behind them would lose
    /// committed statements. Publishing first puts the folds on the
    /// file and empties the log itself, and the appender then opens a
    /// log that says nothing anyone still needs.
    ///
    /// What goes out is the shared write side, so this session holds
    /// the writer lock from here until its next statement or its drop.
    /// Nothing else could be true: the file itself is on loan and the
    /// caller says when it is done with it by stopping using it, which
    /// is not something another connection can wait on.
    pub fn file_mut(&mut self) -> Result<&mut Zu1File> {
        self.enter()?;
        let side = self.side.as_mut().expect("entered just above");
        side.fold_writer()?;
        // The fold dropped the writer, and a fold that checkpointed
        // synced the file before it cut the log, so there is nothing
        // outstanding here and the share always goes through.
        self.share();
        self.sync()?;
        Ok(self
            .side
            .as_mut()
            .expect("held from the enter above")
            .file_mut())
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

    /// Registers a frame as a table of the working graph, under the
    /// name it carries.
    ///
    /// Nothing is copied. The frame says where the caller's columns are
    /// and holds whatever keeps them alive, and a statement that reads
    /// it reads those bytes where they lie. Registering a name that is
    /// already registered replaces what it stands for and keeps the
    /// table id, so a prepared statement over it stays bound to the same
    /// table. A name a stored table already holds is refused.
    pub fn register_frame(&mut self, frame: Frame) -> Result<()> {
        if self.txn.is_some() {
            return Err(ZuError::gql(
                codes::C25G01,
                "a frame is registered on the session and not inside a transaction, and this session has one running".to_string(),
            ));
        }
        let catalog = self.graph.catalog();
        let taken = |id: u32| {
            catalog.node_by_id(id).is_some() || catalog.rel_tables().iter().any(|r| r.id == id)
        };
        let set = self.frames.with(frame, &taken)?;
        self.publish_frames(set)
    }

    /// Drops a registered frame, answering whether there was one under
    /// that name. The bytes go when the caller's handle on them does,
    /// which is not necessarily now: a statement still reading the frame
    /// holds it until it ends.
    pub fn unregister_frame(&mut self, name: &str) -> Result<bool> {
        let Some(set) = self.frames.without(name) else {
            return Ok(false);
        };
        self.publish_frames(set)?;
        Ok(true)
    }

    /// The registered names, sorted.
    pub fn registered_frames(&self) -> Vec<String> {
        self.frames.names()
    }

    /// Hands a new frame set to the two read paths.
    ///
    /// The labels are settled here rather than at registration, because
    /// a label is a position in the schema's label list and the schema
    /// is built from the catalog of the epoch this session is on. Both
    /// caches go: a plan compiled before this bound its table names
    /// against a schema that did not hold these frames.
    fn publish_frames(&mut self, mut set: FrameSet) -> Result<()> {
        let catalog = self.graph.catalog().clone();
        let mut schema = query::schema_of_graph(&catalog, self.working, self.epoch)?;
        set.set_labels(&query::merge_frames(&mut schema, &set)?);
        let set = Arc::new(set);
        self.graph.set_frames(Arc::clone(&set));
        self.snap.set_frames(Arc::clone(&set));
        self.frames = set;
        self.schemas.clear();
        self.plans.clear();
        Ok(())
    }

    /// Refuses a write that names a frame, before it stages anything.
    ///
    /// A frame is the caller's memory and this engine only reads it, so
    /// the refusal names the frame rather than letting the write fail
    /// against a catalog that has never heard of the table.
    fn refuse_a_frame_write(&self, write: &crate::split::Write, rows: &[Vec<Value>]) -> Result<()> {
        if self.frames.is_empty() {
            return Ok(());
        }
        let named = match write {
            crate::split::Write::Insert(insert) => insert
                .nodes
                .iter()
                .find_map(|node| self.frames.get(node.table)),
            // A `SET` and a `DELETE` name their elements through the
            // row, so the row is where the table comes from.
            crate::split::Write::Set(_) | crate::split::Write::Delete(_) => {
                rows.iter().flatten().find_map(|value| match value {
                    Value::Node { table, .. } => self.frames.get(*table),
                    _ => None,
                })
            }
            // A `MERGE` names its tables both ways, so it is asked both
            // ways: the elements it writes are in the clause and the
            // ones it found are in the row.
            crate::split::Write::Merge(merge) => merge
                .insert
                .nodes
                .iter()
                .find_map(|node| self.frames.get(node.table))
                .or_else(|| {
                    rows.iter().flatten().find_map(|value| match value {
                        Value::Node { table, .. } => self.frames.get(*table),
                        _ => None,
                    })
                }),
        };
        match named {
            Some(frame) => Err(ZuError::gql(
                codes::C25G03,
                format!(
                    "'{}' is a registered frame, which is read where it lies and never written",
                    frame.name()
                ),
            )),
            None => Ok(()),
        }
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
    ///
    /// The frames go to the log here and the wait for the platter does
    /// not: it happens in [`Self::durable`], after the write side has
    /// gone back, so the writer queued behind this one stages into the
    /// same sync rather than after it. What that costs is that the
    /// state has to be held back too, because a reader shown a commit
    /// a crash could take away is the one thing the deferral must not
    /// buy.
    pub fn write<T>(
        &mut self,
        stage: impl FnOnce(&mut crate::zu1::txn::WriteTxn<'_>) -> Result<T>,
    ) -> Result<T> {
        let entered = self.enter()?;
        let staged = {
            let side = self.side.as_mut().expect("entered just above");
            let staged = side.write(stage);
            // One debt per session, and this is the later one: the log
            // only grows, so the wait for this byte is also the wait
            // for anything this session staged before it. A write that
            // owes nothing leaves the debt where it was, because what
            // it means is that this transaction added nothing to the
            // log rather than that the log has come clean.
            if let (Ok(written), Some(commits)) = (&staged, side.commits())
                && let Some(need) = written.owed
            {
                self.owed = Some((commits, need));
            }
            staged
        };
        // Staged even when the closure raised, because a transaction
        // that staged nothing still folded whatever opening the writer
        // recovered.
        self.stage_side();
        let synced = self.sync();
        // After the sync, so that this session's own lease is on the
        // epoch it has just written rather than the one before it and
        // the blocks it freed come back now rather than a statement
        // later, which is what a single connection wants.
        self.reclaim();
        self.leave(entered);
        // A statement holds a savepoint, so the leave above usually
        // keeps the side and this waits with it held. That is the
        // explicit transaction case, where there is nobody to share a
        // sync with anyway; `settle` is where the statement path waits.
        let durable = match self.side.is_some() {
            true => Ok(()),
            false => self.durable(),
        };
        let written = staged?;
        synced?;
        durable?;
        Ok(written.value)
    }

    /// Waits for the log to reach the platter through what this session
    /// owes, then installs the state its write made.
    ///
    /// Both halves happen with the write side let go of. That is what
    /// makes a burst of commits cost one sync between them: the writers
    /// behind this one stage their frames while this sync is in the
    /// air, and one sync covers all of them. The publish comes after
    /// the wait rather than before it, so visibility follows durability
    /// the way it did when the sync was inside the lock.
    fn durable(&mut self) -> Result<()> {
        let pending = self.pending.take();
        let mut waited = false;
        if let Some((commits, need)) = self.owed.take() {
            commits.sync_through(need)?;
            waited = true;
        }
        // A state taken while the log owed nothing is durable as it
        // stands, and one taken while it owed something is durable now
        // if the wait above covered it. Anything else belongs to a
        // writer that has not waited yet, and is theirs to show.
        if let Some((next, settled)) = pending
            && (waited || settled)
        {
            self.handle.publish_staged(next);
        }
        Ok(())
    }

    /// Captures what the write side holds, to be published once the
    /// log has made it durable.
    ///
    /// Whether the log owed anything is read here rather than at the
    /// publish, with the side still held. That is what makes it an
    /// answer about this state: a writer that stages after this one
    /// cannot make what was already on the platter not be.
    fn stage_side(&mut self) {
        if let Some(side) = self.side.as_ref() {
            self.pending = Some((self.handle.stage(side), side.settled()));
        }
    }

    /// Publishes what the write side holds, unless the log owes a sync.
    ///
    /// This is the share the paths that commit nothing do: a fold from
    /// recovery, a rollback, a writer being handed over. None of them
    /// has a byte of its own to wait for, and none of them may show
    /// somebody else's staged commit early, so they publish when the
    /// log is clean and leave it to the writer that owes the sync when
    /// it is not.
    fn share(&mut self) {
        if let Some(side) = self.side.as_ref()
            && side.settled()
        {
            self.handle.publish(side);
        }
    }

    /// Hands back the blocks the write just freed, unless a statement
    /// on another connection is still reading the epoch that held them.
    fn reclaim(&mut self) {
        if let Some(side) = self.side.as_mut() {
            self.handle.reclaim(side);
        }
    }

    /// Lets go of the epoch the statement that just ended was reading.
    ///
    /// A connection between statements holds no lease, so a writer is
    /// free to reuse everything it has freed. Not calling it is a
    /// correctness-free mistake: the next statement's lease replaces
    /// this one, and until then the file grows instead of reusing.
    pub fn idle(&mut self) {
        self.lease = None;
    }

    /// Takes the write side of this file, unless this session is
    /// already holding it, and answers whether this call is what took
    /// it. Waiting here is a connection waiting for another
    /// connection's write statement, in the order they asked.
    ///
    /// Taking it picks up whatever the last writer left, which is the
    /// roots it folded to and the cells it committed without folding,
    /// and opening the writer for the first time recovers and folds
    /// whatever the log holds on top of that.
    fn enter(&mut self) -> Result<bool> {
        if self.side.is_some() {
            return Ok(false);
        }
        let mut side = self.handle.take();
        let opened = side.open_writer();
        // The side goes back before the failure does, or a log this
        // process cannot open is a file nothing else can write either.
        if let Err(err) = opened {
            self.handle.put(side);
            return Err(err);
        }
        self.side = Some(side);
        self.share();
        self.sync()?;
        // Opening the writer folds, and an appender that had the file
        // between statements checkpointed on it, so there is usually
        // something waiting here even before this statement writes.
        self.reclaim();
        Ok(true)
    }

    /// The file a statement that writes writes through, which is the
    /// shared write side it took at the top of the statement rather
    /// than the handle it reads with.
    fn writing(&mut self) -> &mut Zu1File {
        self.side
            .as_mut()
            .expect("a statement that writes holds the write side")
            .file_mut()
    }

    /// Puts what the writer is deferring on the file, for a statement
    /// that reads it directly. See [`WriteSide::fold_patches`].
    fn fold_patches(&mut self) -> Result<()> {
        self.side
            .as_mut()
            .expect("a statement that writes holds the write side")
            .fold_patches()
    }

    /// Says where the write side has got to, for the readers of every
    /// connection on this file.
    fn publish_side(&mut self) {
        self.share();
    }

    /// Gives the write side back to whoever is waiting for it.
    ///
    /// A savepoint keeps it here. The state a rollback goes back to
    /// lives on that handle, so an explicit transaction holds the
    /// writer lock from its first write to the word that ends it,
    /// which is what `BEGIN WRITE` means in docs/08 §1.
    fn leave(&mut self, entered: bool) {
        if !entered {
            return;
        }
        if self
            .side
            .as_ref()
            .is_some_and(|side| side.file().in_savepoint())
        {
            return;
        }
        if let Some(side) = self.side.take() {
            self.handle.put(side);
        }
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
                // end it the same way, neither costs an epoch, and the
                // write side was never taken to give back.
                if self
                    .side
                    .as_ref()
                    .is_some_and(|side| side.file().in_savepoint())
                {
                    if matches!(stmt, TxnStmt::Commit) {
                        self.side
                            .as_mut()
                            .expect("held just above")
                            .file_mut()
                            .release_savepoint()?;
                    } else {
                        self.undo()?;
                    }
                    // The savepoint is what was keeping the write side
                    // here, so this is where the next writer gets it.
                    self.leave(true);
                }
            }
        }
        Ok(QueryResult::new(Vec::new(), Vec::new()))
    }

    /// Runs one session statement, which changes this session and
    /// answers no rows (ISO 7.1 and 7.2, GS01 through GS16).
    ///
    /// A session is a named mutable environment and this is the whole
    /// of what a statement may move in it: the parameters it holds, the
    /// schema and the graph it works in, and the time zone it reads a
    /// clock in. None of it touches the file, so none of it is a write,
    /// none of it takes the write side, and none of it costs an epoch:
    /// two sessions on the same file may sit in different schemas
    /// holding different parameters and neither can tell.
    fn session_stmt(&mut self, stmt: SessionStmt, params: &[(&str, Value)]) -> Result<QueryResult> {
        match stmt {
            SessionStmt::SetParameter { def, if_not_exists } => {
                self.set_parameter(&def, if_not_exists, params)?;
            }
            SessionStmt::SetSchema(reference) => self.set_schema(&reference)?,
            SessionStmt::SetGraph(reference) => self.set_graph(&reference, params)?,
            SessionStmt::SetTimeZone(minutes) => self.options.zone = minutes,
            SessionStmt::Reset(what) => self.reset(&what)?,
        }
        Ok(QueryResult::new(Vec::new(), Vec::new()))
    }

    /// Sets one session parameter (GS01 through GS03, GS10 through
    /// GS14).
    ///
    /// A session parameter is a binding variable definition whose name
    /// is written with a dollar, so it is worked out the way one is:
    /// the definition is run as the query it means, `RETURN` of the
    /// name it defines, and what comes back is what the session holds.
    /// That is what makes the two initializer forms cost nothing extra
    /// here. An expression is a definition initialized with one and a
    /// query in braces is a definition initialized with one, and both
    /// were already worked out for GP05 through GP13.
    ///
    /// It is worked out once, now, and not once per statement that
    /// reads it. A parameter set from a query holds the rows that query
    /// answered at the moment it was set, which is the point of the
    /// form: a session names a result and hands it to whatever runs
    /// next without reading the graph again.
    fn set_parameter(
        &mut self,
        def: &BindingDef,
        if_not_exists: bool,
        params: &[(&str, Value)],
    ) -> Result<()> {
        if if_not_exists && self.params.contains_key(&def.name) {
            return Ok(());
        }
        let value = self.definition_value(def, params)?;
        let value = self.held(def, value);
        Arc::make_mut(&mut self.params).insert(
            def.name.clone(),
            SessionParam {
                kind: def.kind,
                value,
            },
        );
        Ok(())
    }

    /// What the session holds, out of what the definition defined.
    ///
    /// The two differ in one case. A table collected by a query in
    /// braces is built inside the statement that collects it and is
    /// therefore stamped with epoch nought, since a table that dies
    /// with its statement has no later to be stale in. Held by a
    /// session it does have one, so it is stamped here with the epoch
    /// it was in fact read at. A definition initialized with a
    /// reference (GS13) is left alone: that table was read when it was
    /// read, and naming it a second time does not make it newer.
    fn held(&self, def: &BindingDef, value: Value) -> Value {
        match value {
            Value::BindingTable(t)
                if def.kind == BindingKind::Table && matches!(def.init, BindingInit::Query(_)) =>
            {
                Value::BindingTable(BindingTable::held(t, self.epoch))
            }
            other => other,
        }
    }

    /// What one binding variable definition defines, worked out here
    /// rather than inside a statement.
    fn definition_value(&mut self, def: &BindingDef, params: &[(&str, Value)]) -> Result<Value> {
        let mut wrapped =
            zu_query::binder::returning(zu_query::ast::Expr::Variable(def.name.clone()), &def.name);
        wrapped.bindings = vec![def.clone()];
        let result = self.run_parsed(&wrapped, params)?;
        // A definition stands for one value, which the binder has
        // already made sure of by refusing anything that answers more
        // than one column. What is left is the row count, and a query
        // that answered no row at all defines the name as null rather
        // than leaving it undefined: a parameter that exists and holds
        // nothing is something a statement can read and ask about, and
        // one that does not exist is a reference that resolves to
        // nothing.
        match result.rows.len() {
            0 => Ok(Value::Null),
            1 => Ok(result.rows.into_vec().swap_remove(0).swap_remove(0)),
            n => Err(ZuError::gql(
                codes::C42001,
                format!(
                    "{} $ {} stands for one value and what defines it answered {n} rows",
                    def.kind.word(),
                    def.name
                ),
            )),
        }
    }

    /// Runs one parsed query on this session, off the plan cache.
    ///
    /// A session parameter's initializer is a query with no text of its
    /// own: it was written inside a statement that is not a query, so
    /// there is nothing to key a cached plan on and nothing that would
    /// be asked for twice. It compiles against the graph this session
    /// is working in, which is the graph the initializer would read if
    /// it had been written inside a statement instead.
    fn run_parsed(
        &mut self,
        parsed: &zu_query::ast::Query,
        params: &[(&str, Value)],
    ) -> Result<QueryResult> {
        let graph = query::graph_of(
            self.graph.catalog(),
            &self.schema,
            self.working,
            parsed,
            params,
        )?;
        let cached = Arc::new(self.compile_ast(parsed, graph)?);
        let args = self.args_for(&cached.query.params, params)?;
        match &cached.parts {
            Some(parts) if crate::split::writes(parts) => {
                self.refuse_a_write()?;
                let held = self.hold()?;
                let out = self.run_parts(&cached, parts, args, params);
                self.settle(out, held)
            }
            Some(parts) => self.run_parts(&cached, parts, args, params),
            None => self.run_plan(&cached, args),
        }
    }

    /// Moves the schema this session works in (GS05).
    ///
    /// A schema the catalog does not hold is a reference that resolves
    /// to nothing, which is `42002`, and it is checked here rather than
    /// left to the first statement that fails in it: the statement that
    /// moved the session is the one that knows where it was going.
    ///
    /// The plans go with it. A plan was compiled against the schema the
    /// session was in, and a name in it that resolved there may resolve
    /// somewhere else or nowhere now, so keeping them would run a
    /// statement against a lookup nobody would make today.
    fn set_schema(&mut self, reference: &SchemaRef) -> Result<()> {
        let path = match reference {
            SchemaRef::Current => return Ok(()),
            SchemaRef::Home => zu_query::procedures::ROOT.to_string(),
            SchemaRef::Path(path) => path.clone(),
        };
        if !self.graph.catalog().has_schema(&path) {
            return Err(ZuError::gql(
                codes::C42002,
                format!("'{path}' is no schema in this catalog"),
            )
            .about(Subject::Schema(path)));
        }
        if path != self.schema {
            self.schema = path;
            self.plans.clear();
            self.focused.clear();
        }
        Ok(())
    }

    /// Moves the graph this session works in (GS06), which is the graph
    /// a statement runs against when it carries no `USE`.
    ///
    /// The plans stay. A plan is held under its text and the graph it
    /// was compiled against is in the schema riding with it, so the
    /// cache is not wrong after this; what it holds is a plan for the
    /// graph the text named, and a text that named no graph is keyed
    /// under the text alone and would be. That last case is what the
    /// clear is for.
    fn set_graph(&mut self, reference: &GraphRef, params: &[(&str, Value)]) -> Result<()> {
        let graph = query::graph_of_ref(
            self.graph.catalog(),
            &self.schema,
            self.working,
            reference,
            &[],
            params,
        )?;
        if graph != self.working {
            self.working = graph;
            self.plans.clear();
            self.focused.clear();
        }
        Ok(())
    }

    /// Puts back what a `SESSION RESET` names (GS04 through GS08, GS16).
    ///
    /// Reset is to what the session opened with and not to nothing: the
    /// schema goes back to the root, the graph to the home graph, the
    /// zone to UTC, and the parameters go away, since a session opened
    /// holding none. `ALL CHARACTERISTICS` is all four, which is the
    /// session as it was on its first statement.
    fn reset(&mut self, what: &SessionReset) -> Result<()> {
        match what {
            SessionReset::Characteristics => {
                self.set_schema(&SchemaRef::Home)?;
                self.set_graph(&GraphRef::Home, &[])?;
                self.options.zone = 0;
                self.clear_params();
            }
            SessionReset::Schema => self.set_schema(&SchemaRef::Home)?,
            SessionReset::Graph => self.set_graph(&GraphRef::Home, &[])?,
            SessionReset::TimeZone => self.options.zone = 0,
            SessionReset::Parameters => self.clear_params(),
            // GS16. A name the session is not holding is not a refusal:
            // the statement says what the session should be holding
            // afterwards, and afterwards it is holding nothing under
            // that name either way.
            SessionReset::Parameter(name) => {
                if self.params.contains_key(name) {
                    Arc::make_mut(&mut self.params).remove(name);
                }
            }
        }
        Ok(())
    }

    /// GS08. Lets go of every parameter, without touching the pointer
    /// when there is nothing behind it to let go of.
    fn clear_params(&mut self) {
        if !self.params.is_empty() {
            self.params = Arc::new(HashMap::new());
        }
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
    fn hold(&mut self) -> Result<Held> {
        // Asked of the reading handle, which was opened the way the
        // connection was, so a read-only connection is turned away
        // before it queues for a write side it may not use.
        crate::write::writable(self.graph.file())?;
        // A transaction starts from a folded file. Opening the writer
        // is what folds it, and the savepoint keeps where the log
        // stands once it has, so a rollback takes back the frames the
        // transaction wrote and leaves alone the ones that were in the
        // log before it, which somebody else committed and this
        // transaction has no say over.
        let entered = self.enter()?;
        let side = self.side.as_mut().expect("entered just above");
        if side.file().in_savepoint() {
            return Ok(Held {
                savepoint: false,
                entered,
            });
        }
        let floor = side.epoch();
        let inside = self.txn.is_some();
        side.file_mut().begin_savepoint(inside, floor)?;
        Ok(Held {
            savepoint: !inside,
            entered,
        })
    }

    /// Ends what [`Self::hold`] took: a statement that answered keeps
    /// what it wrote, one that raised has it undone.
    fn settle<T>(&mut self, out: Result<T>, held: Held) -> Result<T> {
        let out = match (held.savepoint, out) {
            (false, out) => out,
            (true, Ok(value)) => self
                .side
                .as_mut()
                .expect("a savepoint is held on the write side")
                .file_mut()
                .release_savepoint()
                .map(|()| value),
            // The rollback is reported over the error that caused it
            // when it fails, because a statement that raised and was
            // undone leaves a database a caller can carry on with, and
            // one that raised and could not be undone does not.
            (true, Err(err)) => match self.undo() {
                Ok(()) => Err(err),
                Err(failed) => Err(failed),
            },
        };
        // Staged rather than published: this is the state the statement
        // is about to be answered on, and the log may still owe the
        // sync that makes it durable. It goes in below, after the wait.
        self.stage_side();
        let synced = self.sync();
        self.reclaim();
        self.leave(held.entered);
        // Off the write side, so the statement queued behind this one
        // is staging its own frames while this sync is in the air.
        let durable = self.durable();
        let value = out?;
        synced?;
        durable?;
        Ok(value)
    }

    /// Publishes the state the file was keeping, and drops everything
    /// that describes the epochs going away with it.
    ///
    /// The writer goes first. It holds the log and the overlay store for
    /// epochs that are about to stop existing, and its next commit would
    /// number itself off them; opening a fresh one costs a log open and
    /// a recovery pass.
    ///
    /// The log is cut before it is let go of. A fold that did not
    /// checkpoint did not truncate either, so the frames of the
    /// statements this is taking back are still there, and a fresh
    /// writer would replay them straight back on top of the roots
    /// going in. The cut stops at the floor the savepoint kept, which
    /// is where the log stood when the transaction began, so frames
    /// somebody else committed before it are left alone.
    fn undo(&mut self) -> Result<()> {
        self.enter()?;
        let side = self.side.as_mut().expect("entered just above");
        let floor = side.file().savepoint_floor();
        let had_writer = side.has_writer();
        if let Some(floor) = floor {
            side.discard_above(floor)?;
        }
        side.file_mut().rollback_savepoint()?;
        // The writer went with the epochs the rollback took, and what it
        // was deferring went with it. Those commits are in the log below
        // the floor and not on the file, so with no writer nothing
        // answers for them. Opening one again replays them and folds
        // them down, which is where they would have been all along had
        // this transaction never run.
        if had_writer {
            side.open_writer()?;
        }
        // The rollback cut the log and synced the cut, and the writer
        // went with the epochs it held, so nothing here is waiting on
        // the platter and the share goes through.
        self.share();
        // The writer went with the epochs it was holding, and the cells
        // its unfolded commits wrote went with it, so what the readers
        // were shown has to go too.
        self.sync()
    }

    /// A graph reference value naming one graph in the catalog (GV60),
    /// or `42002` when the catalog has no such graph.
    ///
    /// This is where a graph reference comes from. GQL has no literal
    /// that writes one and there is no expression yet that returns one
    /// either, so the engine hands them out: a caller resolves a name
    /// once, holds the handle, and passes it to as many statements as
    /// it likes.
    pub fn graph_ref(&mut self, schema: &str, name: &str) -> Result<Value> {
        self.sync()?;
        let graph = self.graph.catalog().graph(schema, name).ok_or_else(|| {
            ZuError::gql(codes::C42002, format!("no graph '{name}' in '{schema}'"))
                .about(Subject::Graph(name.to_string()))
        })?;
        Ok(Value::Graph(GraphHandle::new(
            graph.id, schema, name, self.epoch,
        )))
    }

    /// A graph reference on the graph this session is working in,
    /// which is what `CURRENT_PROPERTY_GRAPH` will hand back once a
    /// graph expression can be written (GE01, G6).
    pub fn working_graph_ref(&mut self) -> Result<Value> {
        self.sync()?;
        let graph = self
            .graph
            .catalog()
            .graph_by_id(self.working)
            .ok_or_else(|| {
                ZuError::gql(
                    codes::C42002,
                    "the graph this session is working in is gone".to_string(),
                )
            })?;
        let (schema, name) = (graph.schema.clone(), graph.name.clone());
        Ok(Value::Graph(GraphHandle::new(
            self.working,
            schema,
            name,
            self.epoch,
        )))
    }

    /// A graph reference on the graph this session started in, which
    /// is what `HOME_PROPERTY_GRAPH` hands back. It is the working
    /// graph until something moves the working graph, and the two are
    /// separate calls for the same reason the two words are separate
    /// words: a caller that wants to go back has to be able to say so.
    pub fn home_graph_ref(&mut self) -> Result<Value> {
        self.sync()?;
        let id = self.graph.catalog().home_graph_id();
        let graph = self.graph.catalog().graph_by_id(id).ok_or_else(|| {
            ZuError::gql(
                codes::C42002,
                "the graph this session started in is gone".to_string(),
            )
        })?;
        let (schema, name) = (graph.schema.clone(), graph.name.clone());
        Ok(Value::Graph(GraphHandle::new(id, schema, name, self.epoch)))
    }

    /// A binding table reference over the rows of a result (GV61).
    ///
    /// The rows are taken as they are: a binding table value is a
    /// result that has already been read, held behind a handle so that
    /// handing it to the next statement costs a pointer. The epoch it
    /// was read at rides along, because that is what makes a table
    /// holding element references answerable later.
    pub fn binding_table(&self, result: QueryResult) -> Value {
        Value::BindingTable(BindingTable::new(
            result.columns,
            result.rows.into_vec(),
            self.epoch,
        ))
    }

    /// Checks the reference values among a statement's parameters
    /// against the epoch this session is at now.
    ///
    /// A handle can outlive what it names, and the two ways it can are
    /// different. A graph reference is a catalog id, so it survives any
    /// number of epochs and stops meaning something only when the graph
    /// is dropped, which is a lookup. A binding table reference holds
    /// rows that were read at one epoch, and the values in them are
    /// still the values, except for the element references: a node is a
    /// table and an offset in the snapshot it came from, and the row at
    /// that offset in a later snapshot may belong to something else. So
    /// a table of numbers and strings carries forward and a table
    /// holding elements does not.
    ///
    /// Both refusals are `42002 invalid reference`, which is what they
    /// are: the parameter is a reference to something the statement
    /// cannot be run against.
    ///
    /// A parameter the session holds is skipped here and checked where
    /// the statement reads it, in [`Self::args_for`]. The two are not
    /// the same promise. A caller passing a reference with a statement
    /// is saying it is good now, so it is checked whether the statement
    /// reads it or not; a session holding one is saying nothing about
    /// this statement, and refusing every statement on a session whose
    /// held graph has been dropped would take away the statement that
    /// resets it.
    fn check_refs(&self, params: &[(&str, Value)]) -> Result<()> {
        for (name, value) in params {
            if self.params.contains_key(*name) {
                continue;
            }
            self.check_ref(name, value)?;
        }
        Ok(())
    }

    /// The same over the parameters a caller passed, all of them, which
    /// is what a session with parameters of its own runs before folding
    /// the two sets together and losing the distinction.
    fn check_passed(&self, params: &[(&str, Value)]) -> Result<()> {
        for (name, value) in params {
            self.check_ref(name, value)?;
        }
        Ok(())
    }

    /// One parameter, which is where both of the two rules live.
    fn check_ref(&self, name: &str, value: &Value) -> Result<()> {
        match value {
            Value::Graph(g) if self.graph.catalog().graph_by_id(g.id).is_none() => {
                Err(ZuError::gql(
                    codes::C42002,
                    format!(
                        "${name} references {}, and that graph has been dropped",
                        g.label()
                    ),
                )
                .about(Subject::Graph(g.label())))
            }
            Value::BindingTable(t) if t.epoch() != self.epoch && t.holds_elements() => {
                Err(ZuError::gql(
                    codes::C42002,
                    format!(
                        "${name} references a binding table read at epoch {}, the session is at \
                         epoch {}, and the table holds element references that name rows of the \
                         older snapshot",
                        t.epoch(),
                        self.epoch
                    ),
                ))
            }
            _ => Ok(()),
        }
    }

    /// The values a statement's parameter positions are filled with,
    /// with the references among them checked.
    ///
    /// This is where a parameter the session holds is checked, because
    /// `names` is exactly what the statement reads: a held reference
    /// that has gone stale stops a statement that reads it and leaves
    /// every other statement alone, including the one that resets it.
    fn args_for(&self, names: &[String], params: &[(&str, Value)]) -> Result<Vec<Value>> {
        let args = query::bind_args(names, params)?;
        for (name, value) in names.iter().zip(&args) {
            if self.params.contains_key(name) {
                self.check_ref(name, value)?;
            }
        }
        Ok(args)
    }

    /// The catalog schema this session works in (GS05), which is the
    /// root until a `SESSION SET SCHEMA` moves it.
    pub fn session_schema(&self) -> &str {
        &self.schema
    }

    /// The session time zone as minutes east of UTC (GS07 and GS15),
    /// nought being the UTC a session opens in.
    pub fn time_zone(&self) -> i16 {
        self.options.zone
    }

    /// The id of the graph a statement on this session runs against
    /// when it carries no `USE` (GS06).
    pub fn working_graph(&self) -> u32 {
        self.working
    }

    /// What the reference parameters this session holds are pinned to,
    /// in name order, one entry per parameter holding a graph or a
    /// binding table (GS01, GS02, GS10 and GS13).
    ///
    /// A value parameter is not here because it pins nothing: a number
    /// is a number at every epoch. The two reference kinds each carry
    /// the epoch they were taken at, and that is the pin.
    pub fn pins(&self) -> Vec<Pin> {
        let mut out: Vec<Pin> = self
            .params
            .iter()
            .filter_map(|(name, param)| self.pin_of(name, param))
            .collect();
        out.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The oldest epoch any held parameter is pinned to, which is the
    /// one number a caller watching a long-lived session wants.
    pub fn pinned_epoch(&self) -> Option<u64> {
        self.pins().into_iter().map(|pin| pin.epoch).min()
    }

    /// The pin one held parameter is, or nothing when it holds a value.
    ///
    /// This is the reporting form and it builds strings, so it is for
    /// the caller that asked rather than for the statement path. What
    /// the statement path wants is [`Session::stale_of`], which answers
    /// the same question and allocates nothing.
    fn pin_of(&self, name: &str, param: &SessionParam) -> Option<Pin> {
        let (epoch, what) = match &param.value {
            Value::Graph(g) => (g.epoch, g.label()),
            Value::BindingTable(t) => (
                t.epoch(),
                format!("BINDING TABLE of {} rows", t.rows().len()),
            ),
            _ => return None,
        };
        Some(Pin {
            name: name.to_string(),
            kind: param.kind.word(),
            epoch,
            what,
            stale: self.stale_of(param)?,
        })
    }

    /// What has become of what one held parameter names, or nothing
    /// when it holds a value, a value being the same value at every
    /// epoch and so never pinned to one.
    ///
    /// Every statement on a session holding parameters asks this once
    /// per parameter, so it costs a catalog lookup for a graph and, for
    /// a table that is not at the session's epoch, the walk that says
    /// whether the rows name elements. Nothing here allocates: a
    /// session that is holding nothing stale pays the comparison and
    /// stops.
    fn stale_of(&self, param: &SessionParam) -> Option<Stale> {
        match &param.value {
            Value::Graph(g) => Some(match self.graph.catalog().graph_by_id(g.id) {
                Some(_) => Stale::Fresh,
                None => Stale::Gone,
            }),
            Value::BindingTable(t) => Some(if t.epoch() == self.epoch {
                Stale::Fresh
            } else if t.holds_elements() {
                Stale::Gone
            } else if self.epoch - t.epoch() > self.stale_bound {
                Stale::Old
            } else {
                Stale::Fresh
            }),
            _ => None,
        }
    }

    /// How many epochs a held binding table may fall behind before the
    /// session says so on the statement after it. The default is
    /// [`DEFAULT_STALE_BOUND`].
    pub fn set_stale_bound(&mut self, epochs: u64) {
        self.stale_bound = epochs;
    }

    /// Attaches a warning for every held reference parameter that has
    /// gone stale, on a statement that ran (ISO 7.1, and plan/06 §2).
    ///
    /// A statement is answered and then told about the state of the
    /// session it ran on, which is what a warning class status is for:
    /// the answer is an answer, and the parameter the caller has stopped
    /// being able to use is worth hearing about at the point it stopped
    /// rather than at the statement that finally reads it.
    ///
    /// It runs only on a session holding parameters, which is the same
    /// test the fold above it makes, so a session that never set one
    /// pays nothing.
    fn warn_stale(&self, held: &HashMap<String, SessionParam>, result: &mut QueryResult) {
        for (name, param) in held {
            // The cheap question first, and the strings only for a
            // parameter that has an answer worth hearing. A session
            // whose parameters are all fresh, which is nearly every
            // session nearly all of the time, allocates nothing here.
            match self.stale_of(param) {
                None | Some(Stale::Fresh) => continue,
                Some(_) => {}
            }
            let Some(pin) = self.pin_of(name, param) else {
                continue;
            };
            let record = match pin.stale {
                Stale::Fresh => continue,
                Stale::Gone if matches!(param.value, Value::Graph(_)) => DiagnosticRecord::new(
                    codes::C01G03,
                    format!(
                        "${name} holds {}, and that graph has been dropped, so a statement \
                         reading it will be refused",
                        pin.what
                    ),
                ),
                Stale::Gone => DiagnosticRecord::new(
                    codes::C01000,
                    format!(
                        "${name} holds a {} read at epoch {}, the session is at epoch {}, and \
                         the rows name elements of the older snapshot, so a statement reading \
                         it will be refused",
                        pin.what, pin.epoch, self.epoch
                    ),
                ),
                Stale::Old => DiagnosticRecord::new(
                    codes::C01000,
                    format!(
                        "${name} holds a {} read at epoch {}, and the session is at epoch {}, \
                         which is more than {} epochs on",
                        pin.what, pin.epoch, self.epoch, self.stale_bound
                    ),
                ),
            };
            result.notice(record);
        }
    }

    /// The parameters this session is holding, each as its name, the
    /// word it was set with, and the value it holds, in name order
    /// (GS01 through GS03).
    ///
    /// A statement cannot ask this: `$p` answers what `$p` holds, and
    /// there is no expression that answers what a session holds under
    /// every name. So a shell that prints them and a test that says
    /// what a reset took away both come here.
    pub fn session_params(&self) -> Vec<(&str, &'static str, &Value)> {
        let mut out: Vec<(&str, &'static str, &Value)> = self
            .params
            .iter()
            .map(|(name, param)| (name.as_str(), param.kind.word(), &param.value))
            .collect();
        out.sort_unstable_by_key(|(name, _, _)| *name);
        out
    }

    /// The file this session reads through, shared with every other
    /// session this process has on the same file.
    ///
    /// It is handed out so that a caller holding one session can open
    /// another beside it without going back to the path:
    /// [`Session::attached`] takes exactly this and forks a descriptor
    /// off it. What the two share is the write side and the caches
    /// under it; plans, readers and the interrupt are per session.
    pub fn handle(&self) -> &Arc<FileHandle> {
        &self.handle
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
    ///
    /// So does the time zone, for the same reason and a stronger one:
    /// it is a session characteristic a statement set (GS15), not a
    /// switch a caller configured, and a thread count arriving from
    /// [`crate::db::Config`] has nothing to say about what time it is.
    pub fn set_options(&mut self, options: exec::Options) {
        let interrupt = self.options.interrupt.clone();
        let zone = self.options.zone;
        self.options = exec::Options {
            interrupt,
            zone,
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
        if self.params.is_empty() {
            return self.stream_in(source, params, batch_rows, sink);
        }
        self.check_passed(params)?;
        let held = Arc::clone(&self.params);
        let merged = merged_params(&held, params);
        self.stream_in(source, &merged, batch_rows, sink)
    }

    /// The same stream, with the session's parameters folded in.
    fn stream_in(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
        batch_rows: usize,
        sink: &mut dyn FnMut(Batch<'_>) -> Result<Flow>,
    ) -> Result<Streamed> {
        self.sync()?;
        self.check_refs(params)?;
        if query::not_a_query(source)?.is_some() {
            let result = self.run_in(source, params)?;
            return exec::stream_result(result, batch_rows, sink);
        }
        let cached = self.plan_for(source, params)?;
        let args = self.args_for(&cached.query.params, params)?;
        if cached.parts.is_some() {
            let result = self.run_in(source, params)?;
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
        if options.engine == exec::Engine::Pipeline {
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
        if self.params.is_empty() {
            return self.run_in(source, params);
        }
        self.check_passed(params)?;
        let held = Arc::clone(&self.params);
        let merged = merged_params(&held, params);
        let mut result = self.run_in(source, &merged)?;
        // After the statement rather than before it, because the epoch
        // a pin is judged against is the one the statement ran at and
        // the session picks that up on its way in.
        self.warn_stale(&held, &mut result);
        Ok(result)
    }

    /// The same run, with the session's parameters already folded into
    /// the ones it was passed, and the place the statement ran written
    /// onto everything it has to say about itself.
    ///
    /// ISO 23.2 asks a diagnostic record to name the graph and the
    /// schema, and this is the one place that knows them. There are
    /// two hundred raise sites and none of them holds a session, so
    /// filling it here is not a shortcut: a raise site that named the
    /// working graph would be repeating what the session already
    /// knows, and it would be repeating it two hundred times. A record
    /// that named a graph of its own keeps the one it named, since a
    /// condition about some other graph is exactly the case where the
    /// session's answer would be wrong.
    fn run_in(&mut self, source: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        let mut answer = self.run_within(source, params);
        let (graph, schema) = self.where_it_ran();
        match &mut answer {
            Err(ZuError::Gql(record)) => record.within(&graph, &schema),
            Ok(result) => {
                for notice in &mut result.notices {
                    notice.within(&graph, &schema);
                }
            }
            Err(_) => {}
        }
        answer
    }

    /// The graph a statement just ran against and the schema it was
    /// reached through, for the record. The graph is named by the
    /// catalog and falls back to its id, because a record saying which
    /// graph is more use than a record saying nothing when the graph
    /// has been dropped out from under the statement.
    fn where_it_ran(&self) -> (String, String) {
        let graph = self
            .graph
            .catalog()
            .graph_by_id(self.working)
            .map_or_else(|| format!("#{}", self.working), |g| g.name.clone());
        (graph, self.schema.clone())
    }

    fn run_within(&mut self, source: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.sync()?;
        self.check_refs(params)?;
        match query::not_a_query(source)? {
            Some(NotAQuery::Transaction(stmt)) => return self.transaction(stmt),
            // GS01 through GS16. It changes this session and answers no
            // rows, and it is not held in the plan cache: there is
            // nothing compiled to hold.
            Some(NotAQuery::Session(stmt)) => return self.session_stmt(stmt, params),
            // A catalog statement publishes a new epoch, and the plans
            // and readers this session holds describe the old one.
            // Refreshing after it is what drops them, so the next query
            // compiles against the catalog the statement just wrote.
            Some(NotAQuery::Catalog(stmt)) => {
                self.refuse_a_write()?;
                let held = self.hold()?;
                // It reads the tables off the file rather than through
                // a reader, so what a commit deferred has to be on the
                // file before it looks.
                let out = self
                    .fold_patches()
                    .and_then(|()| crate::catalog_stmt::apply(self.writing(), &stmt, params));
                self.settle(out, held)?;
                return Ok(QueryResult::new(Vec::new(), Vec::new()));
            }
            // GP18. The parts run in order under one savepoint, so the
            // graph a `CREATE` in the middle made is there for the
            // statements behind it and a part that raises takes the
            // whole block back, catalog and rows together.
            Some(NotAQuery::Block(parts)) => {
                self.refuse_a_write()?;
                let held = self.hold()?;
                let out = self.run_block(&parts, params);
                return self.settle(out, held);
            }
            None => {}
        }
        let cached = match self.plan_for(source, params) {
            Ok(cached) => cached,
            // A label under `INSERT` that names no node table is a table
            // the statement means the graph to have, and there is no
            // statement in GQL that makes one, so this makes it. It is a
            // catalog change this statement makes, so it happens under
            // the savepoint the statement holds and goes back with it.
            Err(err) => return self.declaring(source, params, err),
        };
        let args = self.args_for(&cached.query.params, params)?;
        // A statement that writes runs as the parts it was split into,
        // because the clauses after the write read what it made rather
        // than reading the store again.
        if let Some(parts) = &cached.parts {
            // A statement split at a match written several ways and
            // nothing else changes nothing, so it runs without the
            // transaction a write is held in.
            if !crate::split::writes(parts) {
                return self.run_parts(&cached, parts, args, params);
            }
            self.refuse_a_write()?;
            let held = self.hold()?;
            let out = self.run_parts(&cached, parts, args, params);
            return self.settle(out, held);
        }
        self.run_plan(&cached, args)
    }

    /// Runs one compiled plan whole: on the pipeline executor when it
    /// covers the plan, and on the row executor when it hands the plan
    /// back.
    fn run_plan(&mut self, cached: &CachedPlan, args: Vec<Value>) -> Result<QueryResult> {
        let options = self.options.clone();
        if options.engine == exec::Engine::Pipeline {
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

    /// Runs a statement block, which is the parts of it in the order
    /// they were written (GP18, ISO 13.6).
    ///
    /// A part is run the way a statement sent on its own is run, plan
    /// cache and write splitting and all, because that is what it is:
    /// what a block adds is the order and the one savepoint around it,
    /// and the caller holds that. A catalog statement hands nothing to
    /// the part behind it, so the parts do not share rows, and the
    /// block answers what its last part answered.
    fn run_block(&mut self, parts: &[String], params: &[(&str, Value)]) -> Result<QueryResult> {
        let mut last = QueryResult::new(Vec::new(), Vec::new());
        for part in parts {
            last = self.run_in(part, params)?;
        }
        Ok(last)
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
        params: &[(&str, Value)],
    ) -> Result<QueryResult> {
        // Asked before anything is worked out, so that a read-only
        // connection is told that it is read-only rather than told
        // something about the statement.
        if crate::split::writes(parts) {
            crate::write::writable(self.graph.file())?;
        }
        let options = self.options.clone();
        let mut carried: Option<Value> = None;
        for part in parts {
            let write = match &part.seam {
                Some(crate::split::Seam::Write(write)) => write,
                Some(crate::split::Seam::Fork(fork)) => {
                    let mut held = args.clone();
                    held.extend(carried.take());
                    let rows = self
                        .read_part(&part.plan, &part.query, &cached.schema, &held, &options)?
                        .rows;
                    let seed = Value::List(rows.into_iter().map(Value::List).collect());
                    carried = Some(self.run_fork(cached, fork, seed, &args, &options)?);
                    continue;
                }
                None => {
                    // A write with nothing after it answers no rows,
                    // and a plan of no clauses would answer one row of
                    // no columns, which is a different answer.
                    if part.query.clauses.is_empty() {
                        return Ok(QueryResult::new(Vec::new(), Vec::new()));
                    }
                    let mut args = args.clone();
                    args.extend(carried);
                    return self.read_part(
                        &part.plan,
                        &part.query,
                        &cached.schema,
                        &args,
                        &options,
                    );
                }
            };
            let mut args = args.clone();
            args.extend(carried.take());
            let rows = self
                .read_part(&part.plan, &part.query, &cached.schema, &args, &options)?
                .rows;
            self.refuse_a_frame_write(write, &rows)?;
            // The row holds the slots the write carries across it and
            // then the values it wrote, which is the order the
            // projection at the end of this part wrote them in.
            let carry = write.carry().len();
            let next = match write {
                crate::split::Write::Insert(insert) => {
                    let catalog = self.graph.catalog().clone();
                    let patches = Arc::clone(&self.patches);
                    let mut batch = crate::insert::Batch::open(
                        self.graph.file_mut(),
                        insert,
                        catalog,
                        &patches,
                        &mut self.dirs,
                    )?;
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, props) = row.split_at(carry);
                        let made = batch.row(carried, props)?;
                        next.push(Value::List(carried.iter().cloned().chain(made).collect()));
                    }
                    let propful = batch.propful();
                    let created = batch.created_rows();
                    let (new, edges) = batch.staged();
                    let catalog = self.graph.catalog().clone();
                    crate::insert::refuse_duplicate_pairs(
                        &mut self.graph,
                        &catalog,
                        &edges,
                        &propful,
                        &created,
                    )?;
                    self.write(|txn| crate::insert::stage(txn, &new, &edges))?;
                    // The edges have rows of their own now, which they
                    // had not when the row that carries them was built.
                    crate::insert::settle(&mut self.graph, &mut next)?;
                    next
                }
                crate::split::Write::Delete(delete) => {
                    // The queries the `VALUE { ... }` items hold read
                    // nothing from the row, so they run once rather
                    // than once per row. They run before the rows are
                    // walked because they read the store, and the store
                    // still holds everything this statement is about to
                    // take away. A delete runs once per row the clauses
                    // before it answered, so a statement they answered
                    // nothing for runs none of this.
                    let mut named = Vec::with_capacity(delete.queries.len());
                    if !rows.is_empty() {
                        for nested in &delete.queries {
                            named.push(self.one_element(
                                nested,
                                &cached.schema,
                                params,
                                &options,
                            )?);
                        }
                    }
                    let mut removals = crate::delete::Removals::open(delete);
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, _) = row.split_at(carry);
                        removals.row(&mut self.graph, carried)?;
                        next.push(Value::List(carried.to_vec()));
                    }
                    for value in &named {
                        removals.element(&mut self.graph, value)?;
                    }
                    let (rows, edges) = removals.staged();
                    self.write(|txn| crate::delete::stage(txn, &rows, &edges))?;
                    next
                }
                crate::split::Write::Merge(merge) => {
                    // The walk ran with the read half of this part, so
                    // the row already says which of the two halves it
                    // is for: null where the pattern is means the walk
                    // found nothing, and that is what the insert runs
                    // for.
                    let catalog = self.graph.catalog().clone();
                    let patches = Arc::clone(&self.patches);
                    let mut batch = crate::insert::Batch::open(
                        self.graph.file_mut(),
                        &merge.insert,
                        catalog,
                        &patches,
                        &mut self.dirs,
                    )?;
                    let mut merged = crate::merge::Merged::default();
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, values) = row.split_at(carry);
                        if carried[merge.at] != Value::Null {
                            next.push(Value::List(carried.to_vec()));
                            continue;
                        }
                        let props = &values[..merge.props];
                        let given: Vec<Value> =
                            merge.ends.iter().map(|at| carried[*at].clone()).collect();
                        let made = merged.made(&given, props, || batch.row(carried, props))?;
                        next.push(Value::List(
                            carried[..merge.at].iter().cloned().chain(made).collect(),
                        ));
                    }
                    let propful = batch.propful();
                    let created = batch.created_rows();
                    let (new, edges) = batch.staged();
                    let inserting = !new.is_empty() || !edges.is_empty();
                    if inserting {
                        let catalog = self.graph.catalog().clone();
                        crate::insert::refuse_duplicate_pairs(
                            &mut self.graph,
                            &catalog,
                            &edges,
                            &propful,
                            &created,
                        )?;
                    }
                    // What the walk did find, changed. The elements are
                    // in the store already, so this is an ordinary
                    // `SET`, and it is worked out here rather than
                    // after the insert because the rows it reads are
                    // the ones the walk matched and the insert does not
                    // touch them.
                    let mut updates = Vec::new();
                    if !merge.matched.items.is_empty() {
                        let catalog = self.graph.catalog().clone();
                        let mut changes = crate::set::Changes::open(&merge.matched, catalog);
                        for row in &rows {
                            let (carried, values) = row.split_at(carry);
                            if carried[merge.at] == Value::Null {
                                continue;
                            }
                            changes.row(
                                self.graph.file_mut(),
                                &mut self.dirs,
                                carried,
                                &values[merge.props..],
                            )?;
                        }
                        let (staged, widened) = changes.staged();
                        // A label the matched half declares goes in
                        // ahead of the frames, the same way it does for
                        // a plain `SET`, because the fold reads the
                        // catalog in the file to decide whether a label
                        // change is allowed on the table.
                        if let Some(catalog) = widened {
                            catalog.store(self.writing())?;
                            self.publish_side();
                            self.sync()?;
                        }
                        updates = staged;
                    }
                    // Both halves of the merge in one transaction. A
                    // row the walk missed and a row it found are
                    // different rows, so nothing in the insert is what
                    // the update reads, and one commit is the whole
                    // statement rather than one commit per half.
                    if inserting || !updates.is_empty() {
                        self.write(|txn| {
                            crate::insert::stage(txn, &new, &edges)?;
                            crate::set::stage(txn, &updates)
                        })?;
                    }
                    if inserting {
                        crate::insert::settle(&mut self.graph, &mut next)?;
                    }
                    next
                }
                crate::split::Write::Set(set) => {
                    let catalog = self.graph.catalog().clone();
                    let mut changes = crate::set::Changes::open(set, catalog);
                    let mut next = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let (carried, values) = row.split_at(carry);
                        changes.row(self.graph.file_mut(), &mut self.dirs, carried, values)?;
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
                        catalog.store(self.writing())?;
                        self.publish_side();
                        self.sync()?;
                    }
                    self.write(|txn| crate::set::stage(txn, &updates))?;
                    next
                }
            };
            carried = Some(Value::List(next));
        }
        unreachable!("the last part of a split statement writes nothing")
    }

    /// Runs a match written several ways, which is
    /// [`crate::split::fork_rows`] reading its parts through the
    /// session: the same walk a one-shot read does, over a store this
    /// one may have written to.
    fn run_fork(
        &mut self,
        cached: &CachedPlan,
        fork: &crate::split::Fork,
        seed: Value,
        args: &[Value],
        options: &exec::Options,
    ) -> Result<Value> {
        crate::split::fork_rows(fork, seed, args, &mut |plan, query, args| {
            self.read_part(plan, query, &cached.schema, args, options)
        })
    }

    /// Runs the read half of one part of a write statement.
    ///
    /// A write is a read and then a write, and the read is an ordinary
    /// `MATCH` that the pipeline executor covers here as well as it
    /// covers the same clauses in a statement of their own. This used to
    /// go straight to the old executor whatever the plan was, which
    /// walks a row at a time and builds a [`Value`] per property, and on
    /// a `SET` of one row out of a hundred thousand that scan was most
    /// of what the statement cost: the write path itself is a log frame
    /// and a fold of the chunks that moved. So the pipeline goes first
    /// here, the same way it does in [`Self::run`], and the fallback is
    /// the same fallback.
    ///
    /// The warm snapshot is the session's, and it stays right across a
    /// write because [`Self::write`] refreshes when the epoch moves,
    /// which drops the readers the fold made stale.
    fn read_part(
        &mut self,
        plan: &LogicalPlan,
        query: &BoundQuery,
        schema: &zu_query::binder::Schema,
        args: &[Value],
        options: &exec::Options,
    ) -> Result<QueryResult> {
        if options.engine == exec::Engine::Pipeline {
            let catalog = self.graph.catalog().clone();
            let warm = std::mem::take(&mut self.snap);
            let mut snap =
                crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
            let out = zu_exec::try_execute(plan, query, schema, &mut snap, args, options);
            self.snap = snap.into_cache();
            if let Some(rows) = out? {
                return Ok(rows);
            }
        }
        exec::execute(plan, query, schema, &mut self.graph, args, options)
    }

    /// Runs the query inside a `DELETE VALUE { ... }` and answers the
    /// one element it named.
    ///
    /// A value query expression is a value, so the query has to answer
    /// one row of one column: two rows have not said which element the
    /// item is about and no rows have not named one at all, and both
    /// are 22G03 rather than a delete of nothing. The parameters are
    /// the caller's, bound again for this query, because the nested
    /// query names what it names and the statement around it need not
    /// name the same things.
    fn one_element(
        &mut self,
        nested: &crate::split::Subquery,
        schema: &zu_query::binder::Schema,
        params: &[(&str, Value)],
        options: &zu_query::exec::Options,
    ) -> Result<Value> {
        let args = self.args_for(&nested.query.params, params)?;
        let out = exec::execute(
            &nested.plan,
            &nested.query,
            schema,
            &mut self.graph,
            &args,
            options,
        )?;
        let mut rows = out.rows.into_iter();
        let (Some(row), None) = (rows.next(), rows.next()) else {
            return Err(ZuError::gql(
                codes::C22G03,
                "the query inside DELETE VALUE names the one element to delete, and this one answered a different number of rows",
            ));
        };
        match <[Value; 1]>::try_from(row) {
            Ok([value]) => Ok(value),
            Err(_) => Err(ZuError::gql(
                codes::C22G03,
                "the query inside DELETE VALUE names the one element to delete, and this one answered more than one column",
            )),
        }
    }

    /// Compiles a statement and pins it under an id. The id maps back
    /// to the source text, so a catalog change between prepare and
    /// execute recompiles instead of running a stale plan. Returns the
    /// id and the parameter names the statement wants, in binder
    /// order.
    pub fn prepare(&mut self, source: &str) -> Result<(u64, Vec<String>)> {
        self.sync()?;
        let cached = self.plan_for(source, &[])?;
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
        self.sync()?;
        // A text that is held as a refusal is not warm: it has nothing
        // to run, and the call below hands the caller the condition it
        // was refused with rather than reporting a cache hit.
        if matches!(self.plans.get(source), Some(Compiled::Plan(_))) {
            return Ok(true);
        }
        self.plan_for(source, &[])?;
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
        self.sync()?;
        let cached = self.plan_for(source, &[])?;
        let mut described = zu_query::plan::describe(&cached.plan, &cached.query, &cached.schema);
        described.notes = cached.notes.clone();
        Ok(described)
    }

    /// EXPLAIN ANALYZE through the session: same cache, same options,
    /// profiled execution.
    pub fn explain_analyze(&mut self, source: &str, params: &[(&str, Value)]) -> Result<String> {
        if !self.params.is_empty() {
            self.check_passed(params)?;
            let held = Arc::clone(&self.params);
            let merged = merged_params(&held, params);
            return self.analyze_in(source, &merged);
        }
        self.analyze_in(source, params)
    }

    /// The same listing, with the session's parameters folded in.
    fn analyze_in(&mut self, source: &str, params: &[(&str, Value)]) -> Result<String> {
        let notes = self.plan_for(source, params)?.notes.clone();
        let listing = self.profile_in(source, params)?.render();
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
        if self.options.engine != exec::Engine::Pipeline {
            return Ok(None);
        }
        let cached = self.plan_for(source, params)?;
        let args = self.args_for(&cached.query.params, params)?;
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
        if !self.params.is_empty() {
            self.check_passed(params)?;
            let held = Arc::clone(&self.params);
            let merged = merged_params(&held, params);
            return self.profile_in(source, &merged);
        }
        self.profile_in(source, params)
    }

    /// The same profile, with the session's parameters folded in.
    fn profile_in(&mut self, source: &str, params: &[(&str, Value)]) -> Result<exec::Profile> {
        self.sync()?;
        let cached = self.plan_for(source, params)?;
        if cached.parts.is_some() {
            return Err(ZuError::Unsupported {
                what: "profiling a statement that writes, which runs as the parts it was split at its write into rather than as the one plan a profile describes",
                id: 0,
            });
        }
        let args = self.args_for(&cached.query.params, params)?;
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
        let Ok(graph) = query::graph_of(
            self.graph.catalog(),
            &self.schema,
            self.working,
            &parsed,
            params,
        ) else {
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
        wanted: &crate::declare::Wanted,
    ) -> Result<QueryResult> {
        crate::declare::create(self.writing(), graph, wanted)?;
        // The tables are published now, and the schemas this session
        // holds describe the catalog from before them.
        self.publish_side();
        self.sync()?;
        let cached = self.plan_for(source, params)?;
        let args = self.args_for(&cached.query.params, params)?;
        let parts = cached.parts.as_ref().ok_or_else(|| {
            ZuError::InvalidArgument(
                "a statement that makes a table writes, and this one compiled as a read"
                    .to_string(),
            )
        })?;
        self.run_parts(&cached, parts, args, params)
    }

    /// The plan for a text, compiled on the first statement that writes
    /// it and held for the ones after.
    ///
    /// The parameters are read only by a text whose `USE` named one,
    /// because that is the only text whose graph is not in it. Every
    /// other text is one lookup and nothing else, which is what the P0
    /// plan-hit gate times.
    fn plan_for(&mut self, source: &str, params: &[(&str, Value)]) -> Result<Arc<CachedPlan>> {
        if let Some(compiled) = self.plans.get(source) {
            return compiled.result();
        }
        if let Some(name) = self.focused.get(source) {
            let name = name.clone();
            let graph = query::graph_of_param(self.graph.catalog(), &name, params)?;
            let key = focused_key(source, graph);
            if let Some(compiled) = self.plans.get(&key) {
                return compiled.result();
            }
            return self.keep(key, source, graph);
        }
        // The text is parsed before anything is compiled because the
        // `USE` clause in front of it says which graph's tables the
        // names below it are names of. A text that will not parse is
        // held as the refusal it is: it will not parse next time
        // either.
        let parsed = match zu_query::parser::parse(source) {
            Ok(parsed) => parsed,
            Err(ZuError::Gql(record)) => {
                self.remember(source.to_string(), Compiled::Refused(record.clone()));
                return Err(ZuError::Gql(record));
            }
            Err(other) => return Err(other),
        };
        let graph = query::graph_of(
            self.graph.catalog(),
            &self.schema,
            self.working,
            &parsed,
            params,
        )?;
        // A `USE` that named a parameter is remembered as the parameter
        // it named, and its plan is held under the graph as well as the
        // text. Holding it under the text alone would hand the next
        // call a plan against the graph the call before it passed.
        if let Some(zu_query::ast::GraphRef::Param(name)) = &parsed.use_graph {
            self.focused.insert(source.to_string(), name.clone());
            return self.keep(focused_key(source, graph), source, graph);
        }
        self.keep(source.to_string(), source, graph)
    }

    /// Compiles a text against one graph and holds what came of it
    /// under `key`, a GQL refusal included: a statement the standard
    /// refuses is refused the same way however often it is written.
    fn keep(&mut self, key: String, source: &str, graph: u32) -> Result<Arc<CachedPlan>> {
        let compiled = match self.compile(source, graph) {
            Ok(plan) => Compiled::Plan(plan),
            Err(ZuError::Gql(record)) => Compiled::Refused(record),
            Err(other) => return Err(other),
        };
        let result = compiled.result();
        self.remember(key, compiled);
        result
    }

    /// Puts one compiled text in the cache, emptying it first when it
    /// is full. The two maps are emptied together because one of them
    /// says where the other one holds a plan.
    fn remember(&mut self, key: String, compiled: Compiled) {
        if self.plans.len() >= PLAN_CAP {
            self.plans.clear();
            self.focused.clear();
        }
        self.plans.insert(key, compiled);
    }

    fn compile(&mut self, source: &str, graph: u32) -> Result<Arc<CachedPlan>> {
        let parsed = zu_query::parser::parse(source)?;
        Ok(Arc::new(self.compile_ast(&parsed, graph)?))
    }

    /// The same compile from the parse rather than the text, which is
    /// what a session parameter's initializer has: it was written
    /// inside a statement that is not a query, so it never had a text
    /// of its own.
    fn compile_ast(&mut self, parsed: &zu_query::ast::Query, graph: u32) -> Result<CachedPlan> {
        let schema = self.schema_for(graph)?;
        let (query, plan, notes) = query::compile_parsed(parsed, &schema, &self.schema)?;
        let parts = crate::split::split(&query, &schema, &self.schema)?;
        Ok(CachedPlan {
            schema,
            query,
            plan,
            parts,
            notes,
        })
    }

    /// The schema of one graph, built on the first statement that names
    /// it. A session that never leaves its working graph builds one.
    fn schema_for(&mut self, graph: u32) -> Result<Arc<zu_query::binder::Schema>> {
        if let Some(schema) = self.schemas.get(&graph) {
            return Ok(schema.clone());
        }
        let catalog = self.graph.catalog().clone();
        let mut built = query::schema_with_stats(self.graph.file_mut(), &catalog, graph)?;
        // The frames go in after the statistics, so a schema loaded
        // with them keeps them, and they go into the working graph
        // because that is the graph they were registered on.
        if graph == self.working {
            query::merge_frames(&mut built, &self.frames)?;
        }
        let schema = Arc::new(built);
        self.schemas.insert(graph, schema.clone());
        Ok(schema)
    }

    /// Passes the writer's unfolded cells to the two read paths, or
    /// takes back what they were given when a fold has sealed them.
    ///
    /// This is what makes a write visible without a fold. The readers
    /// keep everything they had loaded, which is the saving: the
    /// columns have not moved, so the plan cache, the catalog and the
    /// decoded chunks all stay, and the statement pays the log sync and
    /// nothing else.
    fn hand_patches(&mut self, patches: Arc<Patches>) {
        if Arc::ptr_eq(&patches, &self.patches) {
            return;
        }
        if patches.is_empty() && self.patches.is_empty() {
            return;
        }
        self.graph.set_patches(Arc::clone(&patches));
        self.snap
            .set_patches(Arc::clone(&patches), self.graph.catalog());
        self.patches = patches;
    }

    /// Puts this session on the state the write side has reached.
    ///
    /// This is where a snapshot begins. A session holding the side
    /// reads it straight, because it is the one moving it; a session
    /// that is not compares the published version with what it last
    /// took, which is one read lock and one word on a statement that
    /// only reads and nothing has happened under. Either way what
    /// comes over is the roots and the unfolded cells, which together
    /// are the database as the last commit left it.
    fn sync(&mut self) -> Result<()> {
        let (published, lease) = FileHandle::observe(&self.handle);
        // Taking the new lease before letting go of the old one is what
        // keeps a statement from being briefly on no epoch at all,
        // which a writer between the two would read as nobody looking.
        self.lease = Some(lease);
        let ahead = match &self.side {
            Some(side) => Some((
                side.file().db_header().clone(),
                side.file().active_slot(),
                side.patches(),
            )),
            None if published.newer_than(self.seen) => Some((
                published.header().clone(),
                published.slot(),
                Arc::clone(published.patches()),
            )),
            None => None,
        };
        let Some((header, slot, patches)) = ahead else {
            // Nothing has been published since the last statement, so
            // the roots are the ones already in hand. The epoch check
            // still runs, because it is a word and because it is what
            // catches a handle moved by anything that went round the
            // published state.
            return self.refresh();
        };
        self.seen = published.version();
        self.graph.file_mut().follow(&header, slot);
        self.refresh()?;
        self.hand_patches(patches);
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let epoch = self.graph.file().db_header().epoch;
        if epoch == self.epoch {
            return Ok(());
        }
        let (catalog, schema) = query::load_schema(self.graph.file_mut())?;
        // The graph this session is working in survives the epoch,
        // because it is a reference and not a name (GS06). Resolving it
        // by name again here is the thing that must not happen: a
        // session working in a graph somebody dropped and made again
        // would carry on against the new one without being told, and a
        // session that had moved would find itself back home for no
        // reason it could see. What does not survive is the graph being
        // dropped, which the statement that needs it raises on, since a
        // session is free to move somewhere else or reset before then.
        //
        // The schema loaded here is the home graph's, so it goes in
        // under the home graph and not under wherever this session has
        // moved to.
        let home = catalog.home_graph_id();
        self.graph.set_catalog(catalog);
        self.schemas.clear();
        self.schemas.insert(home, Arc::new(schema));
        self.plans.clear();
        self.focused.clear();
        // The readers the last epoch's snapshots loaded describe a
        // layout that has moved, so they go with the plans.
        self.snap = crate::snapshot::SnapshotCache::default();
        // The readers that had the unfolded cells are gone with them,
        // so the next hand-over starts from nothing rather than being
        // skipped as already done.
        self.patches = Arc::new(Patches::new());
        // A moved epoch is a moved layout, and the directories say
        // where the columns of a table are.
        self.dirs = crate::set::Dirs::default();
        self.epoch = epoch;
        // The frames survive the epoch, because they were never in the
        // file. What does not survive is where they sit in a schema:
        // the caches above were built from the old catalog, so the set
        // goes back through the registration path and comes out with
        // the labels the new one gives it.
        if !self.frames.is_empty() {
            let set = self.frames.as_ref().clone();
            self.publish_frames(set)?;
        }
        Ok(())
    }
}

/// A session that goes away with a transaction still running on it has
/// not committed one, so the transaction is rolled back rather than
/// left published. Nothing here can report a failure to do that, which
/// is the reason to end a transaction with a statement and not with a
/// drop: the statement says what went wrong.
///
/// A session that goes away with nothing running publishes what its
/// folds left staged. Skipping it would lose nothing, because the log
/// beside the file holds every one of those commits and the next open
/// replays them, but it would make an ordinary close leave work for an
/// ordinary open, and a process that only ever writes would leave a
/// log as long as its life.
impl Drop for Session {
    fn drop(&mut self) {
        let holding = self
            .side
            .as_ref()
            .is_some_and(|side| side.file().in_savepoint());
        if self.txn.is_some() && holding {
            let _ = self.undo();
        } else if self.side.is_some() {
            if let Some(side) = self.side.as_mut() {
                let _ = side.fold_writer();
            }
            self.share();
        }
        // Whatever happened, the write side goes back: a session that
        // kept it would be a file no other connection could ever write.
        if let Some(side) = self.side.take() {
            self.handle.put(side);
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

    /// The epoch the writer has committed through, which counts up by
    /// one per commit and is therefore how many commits a statement
    /// costs. Readable while a transaction is open, because that is
    /// what keeps the write side on the session between statements.
    fn commits(session: &Session) -> u64 {
        session
            .side
            .as_ref()
            .expect("a transaction holds the write side")
            .epoch()
    }

    /// A merge over a mix of rows is one commit, not one per half.
    ///
    /// The rows the walk missed are an insert and the rows it found are
    /// a `SET`, and they used to go in as two transactions. They are
    /// different rows by definition, so neither half reads what the
    /// other wrote and there is nothing to order them by, which is what
    /// lets one transaction carry both. What it saves is a commit
    /// frame, an epoch and a fold on every merge that does both.
    #[test]
    fn a_merge_that_writes_both_halves_commits_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("merge-commit.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        // Opens the writer, so the epoch below is the writer's own
        // rather than the one the file header still holds.
        session
            .run("INSERT (p:person {name: 'zoe'})", &[])
            .expect("seed");
        let before = commits(&session);
        session
            .run(
                "UNWIND ['ada', 'eve'] AS n MERGE (p:person {name: n}) \
                 ON MATCH SET p.name = 'ada'",
                &[],
            )
            .expect("merge");
        assert_eq!(commits(&session) - before, 1, "one statement, one commit");
        // Both halves are in the store, so the one commit carried them
        // rather than one of them having been dropped on the way.
        assert_eq!(count(&mut session, PEOPLE), 4);
        session.run("COMMIT", &[]).expect("commit");
        assert_eq!(count(&mut session, PEOPLE), 4);
    }

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
            .expect("later");
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
            .expect("later");
        assert_eq!(count(&mut session, PEOPLE), 4);
        std::mem::forget(session);
        crate::shared::forget(&path);

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
            .expect("later");
        session.run("COMMIT", &[]).expect("commit");
        std::mem::forget(session);
        crate::shared::forget(&path);

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

    /// GP18, the same mixing written as one statement. A block chained
    /// by `NEXT` needs no words around it: the graph a `CREATE` in the
    /// middle made is there for the parts behind it, and the block
    /// answers what its last part answered.
    #[test]
    fn a_statement_block_mixes_catalog_and_data_and_the_parts_see_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("block.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        assert_eq!(
            count(
                &mut session,
                "CREATE GRAPH TYPE social { (:person) } \
                 NEXT INSERT (p:person {name: 'zoe'}) \
                 NEXT MATCH (q:person) RETURN count(q) AS n",
            ),
            3,
            "the write in the middle is there for the read behind it",
        );
        assert!(session.graph.catalog().graph_type("social").is_some());
        assert_eq!(count(&mut session, PEOPLE), 3);

        // The graph the first part made is the graph the third part
        // reads, which is what the transaction local catalog is for.
        assert_eq!(
            count(
                &mut session,
                "CREATE GRAPH twin ANY AS COPY OF CURRENT_PROPERTY_GRAPH \
                 NEXT USE twin MATCH (p:person) RETURN count(p) AS n",
            ),
            3,
        );
    }

    /// A part that raises takes the whole block back, the catalog it
    /// changed and the rows it wrote together, because the block is one
    /// transaction and not several.
    #[test]
    fn a_block_that_raises_halfway_leaves_the_file_as_it_found_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("block-undone.zu1");
        people(&path);
        let mut session = Session::open(&path).expect("open");

        let err = session
            .run(
                "CREATE GRAPH TYPE social { (:person) } \
                 NEXT INSERT (p:person {name: 'zoe'}) \
                 NEXT RETURN 1 / 0 AS boom",
                &[],
            )
            .expect_err("the last part raises");
        assert_eq!(err.gqlstatus(), Some(codes::C22012));
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
        assert!(
            err.to_string().contains("is no graph in the schema '/'"),
            "{err}"
        );
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
            .expect("later");
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
    fn a_refused_statement_is_held_and_goes_with_the_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("refused.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let bad = "MATCH (a:person) RETURN c.id AS n";
        let first = session.run(bad, &[]).expect_err("undefined reference");
        assert_eq!(first.gqlstatus(), Some(codes::C42002));
        assert_eq!(session.plans.len(), 1, "the refusal is held");
        // The second send reads the held condition rather than parsing
        // and binding again, and reads the same one.
        let second = session.run(bad, &[]).expect_err("still refused");
        assert_eq!(second.to_string(), first.to_string());
        // A statement that is refused is not warm, because there is
        // nothing to run.
        assert!(session.warm(bad).is_err(), "warm reports the refusal");

        // A moved epoch describes a catalog the refusal was decided
        // against, so it goes with the plans.
        session.graph.file_mut().db_header_mut().epoch += 1;
        session.refresh().expect("refresh");
        assert!(session.plans.is_empty(), "the refusal went with the epoch");
    }

    #[test]
    fn a_refusal_the_schema_decided_is_reconsidered_once_it_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reconsider.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let source = "USE other MATCH (a:person) RETURN count(a) AS n";
        assert!(session.run(source, &[]).is_err(), "no such graph yet");
        session
            .run("CREATE GRAPH other ANY AS COPY OF home", &[])
            .expect("create");
        let answered = session.run(source, &[]).expect("after the table exists");
        assert_eq!(answered.rows.len(), 1);
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
                "CREATE PROPERTY GRAPH shaped { (:Person {name :: STRING}) }",
                &[],
            )
            .expect("inline type");
        let typed = session
            .graph
            .catalog()
            .graph("/", "shaped")
            .expect("shaped")
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
            .run("CREATE PROPERTY GRAPH mirror LIKE shaped", &[])
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
        assert!(err.contains("is no graph in the schema"), "{err}");
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
