//! The morsel scheduler and the push driver.
//!
//! One query runs as one scan split into group-aligned morsels. A
//! shared atomic hands morsels to workers in scan order, which doubles
//! as the determinism story: batches stitch back by morsel index, so
//! the parallel run emits exactly the sequential row order. Worker 0
//! is the calling thread on the caller's snapshot; the others run on
//! forked handles that share the block cache and decoded pools, so a
//! fork is warm from the first read.
//!
//! LIMIT stops early through a shared quota: a worker finishing (or
//! mid-morsel, heading) the contiguous completed prefix checks whether
//! the prefix already covers skip plus limit rows, and the stop flag
//! then drains every worker at the next chunk boundary. Rows past the
//! quota cut can only come from morsels past the prefix, which the
//! final truncate drops, so early stop never changes the answer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use zu_common::{GROUP_ROWS, Result, ZuError};
use zu_query::exec::{Options, QueryResult, Value};
use zu_query::snapshot::{ColId, CsrPin, Dir, FuncCol, RelId, SCAN_ROWS, Snapshot};
use zu_vector::{
    Bitmap, ChunkSet, DataChunk, MorselArena, PhysType, SelVector, StrView, ValueVector,
    VecEncoding,
};

use crate::compile::{
    AggSpec, Close, ColSpec, Dirs, ExecPlan, Op, PostSpec, ScalarRef, SinkSpec, Source,
};
use crate::decide::{Decisions, Split};
use crate::group::{GroupTable, KeyBatch, PartKind};
use crate::join::JoinTable;
use crate::pool;
use crate::sink::{self, Acc, SinkState};
use crate::sip::SipFilter;

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// Runs a compiled pipeline to completion, handing back the answer and
/// every decision the run made on the way (`decide`).
pub(crate) fn run(
    plan: &ExecPlan,
    snap: &mut dyn Snapshot,
    options: &Options,
) -> Result<(QueryResult, Decisions)> {
    let sched = match &plan.source {
        Source::Seek(key) => seek_work(plan, snap, options, *key)?,
        Source::Seeks(keys) => seeks_work(keys, options),
        Source::Scan(_) => scan_work(plan, snap, options)?,
    };
    let (partials, mut decisions) = drive(plan, snap, &sched)?;
    decisions.split = sched.split.clone();

    let rows = match &plan.sink {
        SinkSpec::Count => {
            let total: i64 = partials.iter().map(|p| p.count).sum();
            Ok(QueryResult::new(
                plan.columns.clone(),
                vec![vec![Value::Int(total)]],
            ))
        }
        SinkSpec::CountDistinct { .. } => Ok(QueryResult::new(
            plan.columns.clone(),
            vec![vec![Value::Int(sink::finish_distinct(partials)?)]],
        )),
        SinkSpec::Agg {
            item_agg,
            keys,
            aggs,
            post,
        } => sink::finish_agg(
            plan.columns.clone(),
            item_agg,
            aggs,
            post,
            partials,
            keys.is_empty(),
        ),
        SinkSpec::Rows { post, .. } => Ok(sink::finish_rows(plan.columns.clone(), post, partials)),
    }?;
    Ok((rows, decisions))
}

/// What a worker does with a morsel. Both kinds split the same way,
/// contiguous ranges claimed in order and stitched back by index, so
/// the parallel answer is the sequential one either way.
#[derive(Clone, Copy)]
enum Work {
    /// Rows of the driving scan.
    Scan,
    /// The whole seeded plan on one morsel; `None` is a key that hit
    /// no row, which still owes the sink its empty batch.
    Seek(Option<u64>),
    /// A range of a batch of seeks: the morsel owns those keys, in
    /// order, and builds its own level 0 out of the rows they find.
    Seeks,
    /// One slice of a seeded plan's first frontier: the seed row is
    /// fixed and the morsel owns a range of its neighbor list.
    Frontier {
        seed: u64,
        rel: RelId,
        dir: Dir,
        to: usize,
    },
}

/// The parts of an `Op::Expand` the walk itself reads, lifted out of
/// the opcode so the call does not hand over five loose scalars.
#[derive(Clone, Copy)]
struct Hop {
    rel: RelId,
    dirs: Dirs,
    to: usize,
    batch: bool,
    close: Option<Close>,
}

/// One node's neighbor list, however storage handed it over: borrowed
/// out of a pinned group where pinning was the cheaper read, copied
/// into a scratch buffer where reading the range the list occupies
/// was. The operators that hold a pinned end's list for a whole vector
/// take it this way, since one list is never enough to pay for
/// decoding the group around it on a graph of any size.
enum RowList {
    Pinned(CsrPin),
    Read(Vec<u64>),
}

/// A node's lists, one per side of the direction it was read in, in
/// the order the sides are walked.
struct RowLists {
    sides: Vec<RowList>,
    /// The node's position inside its group, where the pinned sides
    /// find it.
    at: usize,
}

impl RowLists {
    fn slices(&self) -> Vec<&[u64]> {
        self.sides
            .iter()
            .map(|side| match side {
                RowList::Pinned(pin) => pin.list(self.at),
                RowList::Read(list) => &list[..],
            })
            .collect()
    }
}

/// One query's morsels: what a morsel means, the ranges themselves,
/// and how many workers to put on them.
struct Schedule {
    work: Work,
    morsels: Vec<(u64, u64)>,
    threads: usize,
    /// The same three numbers, in the shape EXPLAIN ANALYZE prints.
    split: Split,
}

/// The driving scan, split into morsels, with the worker count the
/// table size justifies.
fn scan_work(plan: &ExecPlan, snap: &mut dyn Snapshot, options: &Options) -> Result<Schedule> {
    let total_rows = snap.table_rows(plan.table)?;
    let threads = match options.threads {
        // A scan under one storage group is a handful of morsels;
        // forking snapshots and spawning workers costs more than the
        // scan, so auto stays sequential and only an explicit thread
        // count forces the parallel path. An intersection is the
        // exception: it walks a neighbor list per edge leaving every
        // scanned row, so the row count says nothing about what the
        // query costs and a small table can still be minutes of work.
        0 if total_rows <= u64::from(GROUP_ROWS) && !intersects(plan) => 1,
        0 => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
        n => n,
    };
    let morsels = make_morsels(total_rows, threads.max(1));
    Ok(Schedule {
        work: Work::Scan,
        split: Split {
            of: "scan",
            morsels: morsels.len(),
            threads,
            weighted: false,
        },
        morsels,
        threads,
    })
}

/// Probe rows a sideways filter is judged over before the runner will
/// switch it off. Four vectors: enough that the rejection rate is the
/// key distribution rather than one chunk's, and few enough that a
/// filter which rejects nothing has cost almost nothing by the time it
/// goes quiet.
const SIP_TRIAL: u64 = 4 * zu_vector::VECTOR_SIZE as u64;

/// What one `Op::Sip` has done so far in this worker. Every worker
/// decides for itself: they see different morsels, and a filter that
/// pays on one part of a table and not another is a filter each of
/// them should be free to drop.
#[derive(Clone, Copy)]
struct SipState {
    probes: u64,
    kept: u64,
    on: bool,
}

impl Default for SipState {
    fn default() -> Self {
        Self {
            probes: 0,
            kept: 0,
            on: true,
        }
    }
}

/// A morsel is worth handing to another core once it is this many rows
/// of work: below it the fork and the handoff cost more than the split
/// saves. Measured on the LDBC friends-of-friends read, where the split
/// starts paying at a few tens of thousands of paths.
const SPLIT_ROWS: u64 = 2 * 1024;

/// The seeded plan's morsels. A seek touches one row, so there is
/// normally nothing to split and the whole pipeline runs on the calling
/// thread. The exception is the celebrity seed (perf/13 section 2): a
/// person with a huge two-hop frontier is one query's worth of work
/// behind one row, and left alone it sets the p99 of the whole class.
/// The frontier splits into morsels weighted by each neighbor's own
/// degree, so the workers get equal work rather than equal neighbors.
fn seek_work(
    plan: &ExecPlan,
    snap: &mut dyn Snapshot,
    options: &Options,
    key: u64,
) -> Result<Schedule> {
    let one = |work| {
        Ok(Schedule {
            work,
            morsels: vec![(0, 0)],
            threads: 1,
            split: Split {
                of: "seed",
                morsels: 1,
                threads: 1,
                weighted: false,
            },
        })
    };
    let Some(seed) = snap.seek_key(plan.table, key)? else {
        return one(Work::Seek(None));
    };
    if options.threads == 1 {
        return one(Work::Seek(Some(seed)));
    }
    let Some(&Op::Expand {
        rel,
        dirs: Dirs::One(dir),
        from: 0,
        to,
        ..
    }) = plan.ops.first()
    else {
        return one(Work::Seek(Some(seed)));
    };
    // Weigh the frontier by the next hop when there is one, so a
    // neighbor with a thousand edges counts for a thousand.
    // One list, read as a range: the seed's group holds every edge of
    // a hundred and thirty thousand nodes and this wants one node's.
    let mut list = Vec::new();
    snap.list_into(rel, seed, dir, &mut list)?;
    let mut weight = vec![1u64; list.len()];
    if let Some(&Op::Expand {
        rel: next,
        dirs: Dirs::One(next_dir),
        from: 1,
        ..
    }) = plan.ops.get(1)
    {
        snap.degrees(next, &list, next_dir, &mut weight)?;
        for w in &mut weight {
            // `degrees` adds onto the slot, so the seed of 1 above is
            // still in there; that floor is what keeps a zero-degree
            // neighbor from disappearing out of the weighting.
            *w = (*w).max(1);
        }
    }
    let total: u64 = weight.iter().sum();
    if total < SPLIT_ROWS {
        return one(Work::Seek(Some(seed)));
    }
    let threads = match options.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
        n => n,
    };
    if threads < 2 {
        return one(Work::Seek(Some(seed)));
    }
    // Four morsels a worker, the same oversubscription the scan uses:
    // the weights are estimates, and a short tail morsel is what lets
    // a worker that drew a heavy one be overtaken.
    let target = (total / (threads as u64 * 4)).max(1);
    let mut morsels = Vec::new();
    let (mut lo, mut acc) = (0u64, 0u64);
    for (i, w) in weight.iter().enumerate() {
        acc += w;
        if acc >= target && i as u64 + 1 < list.len() as u64 {
            morsels.push((lo, i as u64 + 1));
            lo = i as u64 + 1;
            acc = 0;
        }
    }
    morsels.push((lo, list.len() as u64));
    Ok(Schedule {
        work: Work::Frontier { seed, rel, dir, to },
        split: Split {
            of: "seed frontier",
            morsels: morsels.len(),
            threads,
            weighted: true,
        },
        morsels,
        threads,
    })
}

/// A batch of seeks, split into morsels of the key list. The keys are
/// the work here, not the rows behind them, so the split is by position
/// in the list and a morsel keeps its keys in the order they were
/// written: batches stitch back into the order one worker walking the
/// list would have emitted.
fn seeks_work(keys: &[u64], options: &Options) -> Schedule {
    let threads = match options.threads {
        // A page of ids is a handful of lookups, and forking snapshots
        // costs more than the lookups do.
        0 if keys.len() as u64 <= SPLIT_ROWS => 1,
        0 => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
        n => n,
    };
    let morsels = make_morsels(keys.len() as u64, threads.max(1));
    Schedule {
        work: Work::Seeks,
        split: Split {
            of: "seek keys",
            morsels: morsels.len(),
            threads,
            weighted: false,
        },
        morsels,
        threads,
    }
}

/// Where one worker leaves what it finished with, empty until it does.
type Slot = Mutex<Option<Result<(SinkState, Decisions)>>>;

/// Runs the morsels across workers and returns each worker's partial
/// sink, with every worker's decisions folded into one record.
fn drive(
    plan: &ExecPlan,
    snap: &mut dyn Snapshot,
    sched: &Schedule,
) -> Result<(Vec<SinkState>, Decisions)> {
    let Schedule {
        work,
        morsels,
        threads,
        split: _,
    } = sched;
    let (work, threads) = (*work, *threads);
    let quota = match &plan.sink {
        SinkSpec::Rows { post, .. } => quota_of(post),
        _ => None,
    };
    let stop = StopState::new(quota, morsels.len());
    let claim = AtomicUsize::new(0);

    // A single worker needs none of the handoff machinery, and a point
    // read is short enough that setting it up shows in the latency.
    if threads <= 1 || morsels.len() <= 1 {
        let mut w = Worker::new(plan, SnapHandle::Main(snap), &stop, work);
        w.work(morsels, &claim)?;
        let mut all = Decisions::with_sips(w.decisions.sip.len());
        all.merge(&w.decisions);
        return Ok((vec![w.sink], all));
    }

    // Fork one handle per extra worker; a backend that cannot fork
    // runs the query on this thread alone.
    let mut forks = Vec::new();
    if threads > 1 && morsels.len() > 1 {
        for _ in 1..threads.min(morsels.len()) {
            match snap.fork() {
                Some(f) => forks.push(f),
                None => {
                    forks.clear();
                    break;
                }
            }
        }
    }

    // Extra workers run on the persistent pool; worker 0 is this
    // thread. Result slots start empty, and a slot still empty after
    // the latch means that worker panicked.
    let slots: Vec<Slot> = forks.iter().map(|_| Mutex::new(None)).collect();
    let main = {
        let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = forks
            .into_iter()
            .zip(&slots)
            .map(|(f, slot)| {
                let (stop, claim) = (&stop, &claim);
                Box::new(move || {
                    let mut w = Worker::new(plan, SnapHandle::Fork(f), stop, work);
                    let res = w.work(morsels, claim).map(|()| (w.sink, w.decisions));
                    *slot.lock().unwrap() = Some(res);
                }) as Box<dyn FnOnce() + Send + '_>
            })
            .collect();
        let pending = pool::submit(jobs);
        let mut w = Worker::new(plan, SnapHandle::Main(snap), &stop, work);
        let main = w.work(morsels, &claim).map(|()| (w.sink, w.decisions));
        pending.wait();
        main
    };
    let mut out = Vec::with_capacity(slots.len() + 1);
    let mut all = Decisions::with_sips(sip_count(plan));
    let mut first_err = None;
    for res in std::iter::once(main).chain(slots.into_iter().map(|slot| {
        slot.into_inner()
            .unwrap()
            .unwrap_or_else(|| Err(invalid("executor worker panicked".into())))
    })) {
        match res {
            Ok((p, d)) => {
                all.merge(&d);
                out.push(p);
            }
            Err(e) => first_err = first_err.or(Some(e)),
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok((out, all)),
    }
}

/// Sideways filters in a plan, which is how many slots a worker's
/// decisions need before it starts recording against them by position.
fn sip_count(plan: &ExecPlan) -> usize {
    plan.ops
        .iter()
        .filter(|op| matches!(op, Op::Sip { .. }))
        .count()
}

/// Whether the pipeline closes a cycle, the one shape whose cost is
/// set by the edges under the scan rather than by the rows in it.
fn intersects(plan: &ExecPlan) -> bool {
    plan.ops.iter().any(|op| matches!(op, Op::Intersect { .. }))
}

/// Splits the scan into morsels: chunk-multiple sizes targeting eight
/// morsels per worker, never crossing a storage group boundary so a
/// morsel's CSR pins and zone reads stay within one group.
///
/// Eight rather than four because the tail decides the query: on a
/// machine with slow and fast cores the slow worker picks up a last
/// morsel nobody else can finish for it, so the whole query waits on
/// one morsel's worth of rows. Halving the morsel halves that tail.
/// Sixteen per worker gave nothing back over eight, so the claim
/// traffic starts costing what the shorter tail saves somewhere in
/// between.
fn make_morsels(rows: u64, workers: usize) -> Vec<(u64, u64)> {
    const CHUNK: u64 = SCAN_ROWS as u64;
    const GROUP: u64 = GROUP_ROWS as u64;
    let mut out = Vec::new();
    if rows == 0 {
        return out;
    }
    let target = (rows / (workers as u64 * 8)).clamp(CHUNK, GROUP) / CHUNK * CHUNK;
    let mut group_lo = 0;
    while group_lo < rows {
        let group_hi = (group_lo + GROUP).min(rows);
        let mut lo = group_lo;
        while lo < group_hi {
            let hi = (lo + target).min(group_hi);
            out.push((lo, hi));
            lo = hi;
        }
        group_lo = group_hi;
    }
    out
}

/// The bounded buffer a worker runs its row sink with, on the plans
/// whose ORDER BY sits under a small enough LIMIT. Every worker builds
/// it off the same plan, so they all agree on whether the run is
/// bounded before any of them emits a row.
fn bounded_sink(plan: &ExecPlan) -> Option<sink::TopN> {
    let SinkSpec::Rows { post, .. } = &plan.sink else {
        return None;
    };
    let (keys, need) = sink::topn_of(post)?;
    Some(sink::TopN::new(keys, need))
}

/// Rows a LIMIT query needs before every later row is dead weight,
/// `None` when early stop is not sound for this post chain.
fn quota_of(post: &[PostSpec]) -> Option<u64> {
    let mut skip = 0u64;
    let mut limit = None;
    for p in post {
        match p {
            // Ordered output has no early stop: the last row scanned
            // can still sort into the answer.
            PostSpec::Distinct | PostSpec::Sort(_) => return None,
            PostSpec::Skip(n) => skip = *n,
            PostSpec::Limit(n) => limit = Some(*n),
        }
    }
    limit.map(|l| skip.saturating_add(l))
}

/// Shared early-stop state. `prefix` walks the contiguous run of
/// completed morsels; only rows inside that run can be part of the
/// answer, so the quota check never races ahead of correctness.
struct StopState {
    needed: Option<u64>,
    stop: AtomicBool,
    progress: Mutex<Progress>,
}

struct Progress {
    counts: Vec<Option<u64>>,
    prefix: usize,
    prefix_rows: u64,
}

impl StopState {
    fn new(needed: Option<u64>, morsels: usize) -> Self {
        StopState {
            needed,
            stop: AtomicBool::new(false),
            progress: Mutex::new(Progress {
                counts: vec![None; morsels],
                prefix: 0,
                prefix_rows: 0,
            }),
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn abort(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Records a finished morsel and advances the prefix.
    fn morsel_done(&self, idx: usize, rows: u64) {
        let Some(needed) = self.needed else { return };
        let mut p = self.progress.lock().expect("no poisoned quota");
        p.counts[idx] = Some(rows);
        while p.prefix < p.counts.len()
            && let Some(n) = p.counts[p.prefix]
        {
            p.prefix_rows += n;
            p.prefix += 1;
        }
        if p.prefix_rows >= needed {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// True when this worker heads the completed prefix and its local
    /// rows already close the quota, so the rest of its morsel cannot
    /// reach the answer.
    fn quota_met(&self, idx: usize, local_rows: u64) -> bool {
        let Some(needed) = self.needed else {
            return false;
        };
        let p = self.progress.lock().expect("no poisoned quota");
        p.prefix == idx && p.prefix_rows + local_rows >= needed
    }
}

/// The snapshot a worker reads through: the caller's own handle on
/// worker 0, a fork elsewhere.
enum SnapHandle<'a> {
    Main(&'a mut (dyn Snapshot + 'a)),
    Fork(Box<dyn Snapshot + Send>),
}

impl SnapHandle<'_> {
    fn get(&mut self) -> &mut dyn Snapshot {
        match self {
            SnapHandle::Main(s) => *s,
            SnapHandle::Fork(s) => s.as_mut(),
        }
    }
}

struct Worker<'a> {
    plan: &'a ExecPlan,
    snap: SnapHandle<'a>,
    arena: MorselArena,
    /// Pinned CSR groups, keyed (rel, backward, group). Pins are Arc
    /// pairs; the cache lives for the query, across morsels.
    pins: HashMap<(RelId, bool, u32), CsrPin>,
    /// Level 0 column ids, resolved once.
    scan_cols: Vec<ColId>,
    /// Row id scratch for degree sums.
    scratch: Vec<u64>,
    /// Neighbor scratch for the fused expand-then-count path.
    neigh: Vec<u64>,
    /// Intersection scratch for the WCOJ close.
    hits: Vec<u64>,
    /// Survivor scratch for the binary close.
    keep: Vec<u16>,
    /// Per-row degree and running product scratch for hub counts.
    deg: Vec<u64>,
    prod: Vec<u64>,
    /// What a fused degree product hands the sink: the weight of every
    /// row that kept one, and the positions those rows sit at, which is
    /// the selection the keys and the arguments are read through.
    wts: Vec<i64>,
    wsel: Vec<u16>,
    /// Where each source row's neighbors end in the concatenated list,
    /// for a weighted hop that still has to read the lists.
    ends: Vec<u32>,
    /// An ascending order over a weighted hop's rows and those rows in
    /// it, so the degree read stays inside a storage group at a time
    /// while the weights stay where their rows are.
    order: Vec<u32>,
    sorted: Vec<u64>,
    /// The hub weight of every row of a batched descent, in the
    /// positions of the vector it hands down. Empty unless a batched
    /// expand is carrying one, which is the flag the degree product
    /// under it reads: one weight per row there, one for the whole
    /// vector off the pin otherwise.
    bwts: Vec<i64>,
    /// Index and row scratch pools for expand iteration, one pair per
    /// recursion depth in steady state.
    idx_pool: Vec<Vec<u32>>,
    row_pool: Vec<Vec<u64>>,
    /// Grouping scratch, all reused down the whole run so a GROUP BY
    /// vector never allocates: the packed keys, the group index the
    /// table gave each row, and one aggregate's arguments.
    batch: KeyBatch,
    gids: Vec<u32>,
    args: Vec<i64>,
    sink: SinkState,
    stop: &'a StopState,
    /// What this worker's morsels mean.
    work: Work,
    /// Rows emitted in the morsel in flight, for the quota check.
    local_rows: u64,
    /// The morsel in flight, which with `local_rows` says where a row
    /// would have sat had the batches been stitched.
    morsel: usize,
    /// Sort key scratch for the bounded sink, refilled per row so a
    /// row the buffer rejects costs no allocation at all.
    keybuf: Vec<Value>,
    /// One entry per `Op::Sip` in the plan: what that filter has been
    /// asked and what it let through, which is how the operator knows
    /// whether it is earning the pass it costs.
    sips: Vec<SipState>,
    /// What this worker decided while it ran, folded into the run's
    /// record once it is done.
    decisions: Decisions,
    /// One vector's keys and the positions they came off, and the
    /// survivors the filter picked out of them. Reused down the run,
    /// so a filter that runs on every chunk allocates once.
    sip_keys: Vec<u64>,
    sip_rows: Vec<u16>,
    sip_out: Vec<u32>,
    /// Whether the OPTIONAL MATCH group in flight produced a row for
    /// the outer row it is running on. Cleared before the group and
    /// set by `Op::OptionalHit`, which is the only thing under the
    /// group that can say so.
    opt_hit: bool,
    /// The null level a missed OPTIONAL MATCH binds, kept across the
    /// misses of one morsel. It is the same one row every time, so
    /// building it per miss would be a handful of arena bytes per
    /// outer row that matched nothing, which on a selective group is
    /// most of them. Dropped whenever the arena resets, since that is
    /// where its buffers live.
    null: Option<DataChunk>,
}

impl<'a> Worker<'a> {
    fn new(plan: &'a ExecPlan, snap: SnapHandle<'a>, stop: &'a StopState, work: Work) -> Self {
        Worker {
            plan,
            snap,
            arena: MorselArena::new(),
            pins: HashMap::new(),
            scan_cols: plan.levels[0]
                .cols
                .iter()
                .filter_map(|c| match *c {
                    ColSpec::Stored(id, _) => Some(id),
                    ColSpec::Computed(_) | ColSpec::Outer { .. } | ColSpec::Key | ColSpec::Func => {
                        None
                    }
                })
                .collect(),
            scratch: Vec::new(),
            neigh: Vec::new(),
            hits: Vec::new(),
            keep: Vec::new(),
            deg: Vec::new(),
            prod: Vec::new(),
            wts: Vec::new(),
            wsel: Vec::new(),
            ends: Vec::new(),
            order: Vec::new(),
            sorted: Vec::new(),
            bwts: Vec::new(),
            idx_pool: Vec::new(),
            row_pool: Vec::new(),
            batch: KeyBatch::default(),
            gids: Vec::new(),
            args: Vec::new(),
            sink: SinkState {
                top: bounded_sink(plan),
                ..SinkState::default()
            },
            stop,
            work,
            local_rows: 0,
            morsel: 0,
            keybuf: Vec::new(),
            sips: vec![
                SipState::default();
                plan.ops
                    .iter()
                    .filter(|op| matches!(op, Op::Sip { .. }))
                    .count()
            ],
            sip_keys: Vec::new(),
            sip_rows: Vec::new(),
            sip_out: Vec::new(),
            decisions: Decisions::with_sips(sip_count(plan)),
            opt_hit: false,
            null: None,
        }
    }

    fn work(&mut self, morsels: &[(u64, u64)], claim: &AtomicUsize) -> Result<()> {
        let mut claimed = 0;
        loop {
            let m = claim.fetch_add(1, Ordering::Relaxed);
            if m >= morsels.len() || self.stop.stopped() {
                self.decisions.claims.push(claimed);
                return Ok(());
            }
            claimed += 1;
            if let Err(e) = self.run_morsel(m, morsels[m]) {
                self.stop.abort();
                return Err(e);
            }
        }
    }

    fn run_morsel(&mut self, idx: usize, range: (u64, u64)) -> Result<()> {
        self.arena.reset();
        // Its buffers came out of the arena that just went away.
        self.null = None;
        self.local_rows = 0;
        self.morsel = idx;
        let rows_sink = matches!(self.plan.sink, SinkSpec::Rows { .. }) && self.sink.top.is_none();
        match self.work {
            Work::Scan => self.scan_morsel(idx, range)?,
            Work::Seek(seed) => self.seek_morsel(seed)?,
            Work::Seeks => self.seeks_morsel(idx, range)?,
            Work::Frontier { seed, rel, dir, to } => {
                self.frontier_morsel(seed, rel, dir, to, range)?
            }
        }
        if rows_sink {
            let batch = std::mem::take(&mut self.sink.rows);
            self.sink.batches.push((idx, batch));
        }
        self.stop.morsel_done(idx, self.local_rows);
        Ok(())
    }

    /// The seeded plan on one worker: the key index already found the
    /// row, so level 0 is that row and everything above it is the
    /// pipeline a scan runs. A key that hit nothing runs no ops and
    /// still leaves the sink an empty batch.
    fn seek_morsel(&mut self, seed: Option<u64>) -> Result<()> {
        let Some(seed) = seed else { return Ok(()) };
        let plan = self.plan;
        let level0 = self.make_level(0, &[seed], &[], &[])?;
        let mut set = ChunkSet::new(vec![level0]);
        self.run_ops(&plan.ops, &mut set)
    }

    /// One morsel's slice of a batch of seeks. The keys resolve a
    /// vector at a time and level 0 is built out of the rows they hit,
    /// so the gather and everything above it run over a full vector
    /// rather than once per key, which is what a batch of point reads
    /// used to cost. A key that finds nothing takes its place out of
    /// the vector and no row comes of it.
    fn seeks_morsel(&mut self, idx: usize, (lo, hi): (u64, u64)) -> Result<()> {
        let plan = self.plan;
        let Source::Seeks(keys) = &plan.source else {
            unreachable!("the seeks morsel runs under a batch of seeks");
        };
        let rows_sink = matches!(plan.sink, SinkSpec::Rows { .. });
        let (mut found, mut hit) = (Vec::new(), Vec::new());
        for part in keys[lo as usize..hi as usize].chunks(zu_vector::VECTOR_SIZE) {
            if self.stop.stopped() {
                break;
            }
            found.clear();
            hit.clear();
            for &key in part {
                if let Some(row) = self.snap.get().seek_key(plan.table, key)? {
                    found.push(row);
                    hit.push(key);
                }
            }
            if found.is_empty() {
                continue;
            }
            let level0 = self.make_level(0, &found, &[], &hit)?;
            let mut set = ChunkSet::new(vec![level0]);
            self.run_ops(&plan.ops, &mut set)?;
            if rows_sink && self.stop.quota_met(idx, self.local_rows) {
                self.decisions.quota_stop += 1;
                break;
            }
        }
        Ok(())
    }

    /// One slice of a celebrity seed's frontier. The seed's level 0 is
    /// rebuilt per morsel, which is one row's gather, and the first
    /// expand is unrolled here so the morsel can own a range of the
    /// neighbor list instead of the whole of it. Slices are contiguous
    /// and claimed in order, so the batches stitch back into exactly
    /// the order one worker walking the list would have emitted.
    fn frontier_morsel(
        &mut self,
        seed: u64,
        rel: RelId,
        dir: Dir,
        to: usize,
        (lo, hi): (u64, u64),
    ) -> Result<()> {
        let plan = self.plan;
        let mut level0 = self.make_level(0, &[seed], &[], &[])?;
        level0.cur = Some(0);
        let mut set = ChunkSet::new(vec![level0]);
        // One list, and every worker on this seed wants the same one, so
        // pinning the group means each of them decoding a group's worth
        // of neighbors for the slice of one list it owns.
        let held = self.hold(rel, dir, seed, 1)?;
        let mut owned = self.row_pool.pop().unwrap_or_default();
        let list: &[u64] = match &held {
            Some(pin) => pin.list((seed % u64::from(GROUP_ROWS)) as usize),
            None => {
                owned.clear();
                self.snap.get().list_into(rel, seed, dir, &mut owned)?;
                &owned
            }
        };
        let mut result = Ok(());
        for part in list[lo as usize..hi as usize].chunks(zu_vector::VECTOR_SIZE) {
            let chunk = match self.make_level(to, part, &set.chunks, &[]) {
                Ok(c) => c,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            set.chunks.push(chunk);
            let res = self.run_ops(&plan.ops[1..], &mut set);
            set.chunks.pop();
            if let Err(e) = res {
                result = Err(e);
                break;
            }
            if self.stop.stopped() {
                break;
            }
        }
        self.row_pool.push(owned);
        result
    }

    fn scan_morsel(&mut self, idx: usize, (lo, hi): (u64, u64)) -> Result<()> {
        let plan = self.plan;
        let rows_sink = matches!(plan.sink, SinkSpec::Rows { .. });
        for chunk in lo / SCAN_ROWS as u64..hi.div_ceil(SCAN_ROWS as u64) {
            if self.stop.stopped() {
                break;
            }
            let Some(sc) = self.snap.get().scan(
                plan.table,
                chunk,
                &self.scan_cols,
                plan.zone(),
                &mut self.arena,
            )?
            else {
                // A zone-excluded chunk; nothing survives it.
                self.decisions.zone_skipped += 1;
                continue;
            };
            // A chunk the pushdown could not skip whole but still took
            // rows out of: the zone map said maybe and the values said
            // no, which is the half of the decision that costs a decode
            // and saves everything above it.
            if sc.sel.is_some() {
                self.decisions.zone_thinned += 1;
            }
            let mut vecs = Vec::with_capacity(1 + sc.columns.len());
            let mut ids =
                ValueVector::flat_uninit(&mut self.arena, PhysType::Int64, sc.rows as usize);
            for (i, slot) in ids.values_mut::<u64>().iter_mut().enumerate() {
                *slot = sc.row_base + i as u64;
            }
            vecs.push(ids);
            let mut level0 = DataChunk {
                vecs,
                sel: sc.sel,
                count: sc.rows,
                cur: None,
            };
            let mut stored = sc.columns.into_iter();
            for spec in &plan.levels[0].cols {
                let v = match spec {
                    // The scan handed these back in registration order,
                    // one per stored entry, so taking them in turn keeps
                    // every column at the position it was given.
                    ColSpec::Stored(..) => match stored.next() {
                        Some(v) => v,
                        None => break,
                    },
                    ColSpec::Computed(prog) => prog.eval(&level0, &mut self.arena)?,
                    // Level 0 has nothing below it to broadcast from,
                    // and the compiler only ever registers a broadcast
                    // off a level below the one it lands on.
                    ColSpec::Outer { .. } => unreachable!("the scan level reads no outer value"),
                    // A key column stands for the UNWIND variable a
                    // batch of seeks drives on, and a scan has no keys
                    // to answer with; the compiler only registers one
                    // under the seeks source.
                    ColSpec::Key => unreachable!("the scan level has no seek keys"),
                    // The kernel answered per node in row order, so
                    // the chunk's slice of it is the column.
                    ColSpec::Func => {
                        let col = plan.func.as_ref().expect("a call under a func column");
                        func_vec(col, sc.row_base, sc.rows as usize, &mut self.arena)
                    }
                };
                level0.vecs.push(v);
            }
            if level0.active_count() == 0 {
                continue;
            }
            let mut set = ChunkSet::new(vec![level0]);
            self.run_ops(&plan.ops, &mut set)?;
            if rows_sink && self.stop.quota_met(idx, self.local_rows) {
                self.decisions.quota_stop += 1;
                break;
            }
        }
        Ok(())
    }

    fn run_ops(&mut self, ops: &[Op], set: &mut ChunkSet) -> Result<()> {
        let Some((op, rest)) = ops.split_first() else {
            return self.push_sink(set);
        };
        match op {
            Op::Filter { prog } => {
                let last = set.chunks.last_mut().expect("a level under every filter");
                let bits = prog.eval_filter(last, &mut self.arena)?;
                let sel = match &last.sel {
                    Some(s) => SelVector::refine(&mut self.arena, s, &bits),
                    None => SelVector::from_bitmap(&mut self.arena, &bits),
                };
                if sel.is_empty() {
                    return Ok(());
                }
                last.sel = Some(sel);
                self.run_ops(rest, set)
            }
            Op::Expand {
                rel,
                dirs,
                to,
                batch,
                close,
                ..
            } => {
                // Only the product over the level this expand builds
                // fuses into it. One off a level below is a weight the
                // pin already holds, and the rows this expand emits are
                // what the sink reads, so that one runs where it sits.
                if let ([Op::DegreeProduct { steps, from }], None) = (rest, close)
                    && *from == *to
                {
                    // The concatenating path answers with one sum over
                    // the whole vector, which is the whole answer only
                    // where the answer is one number. A weighted sink
                    // reads keys off the row each weight belongs to, so
                    // it keeps the lists apart by source row instead.
                    return if matches!(self.plan.sink, SinkSpec::Count) {
                        self.expand_degree(*rel, *dirs, steps, set)
                    } else {
                        self.expand_weights(*rel, *dirs, steps, set)
                    };
                }
                let hop = Hop {
                    rel: *rel,
                    dirs: *dirs,
                    to: *to,
                    batch: *batch,
                    close: *close,
                };
                self.expand(hop, rest, set, None)
            }
            Op::Branch {
                rel,
                dirs,
                from,
                to,
            } => self.branch(*rel, *dirs, *from, *to, rest, set),
            Op::Join { table, key, to } => self.join(table, *key, *to, rest, set, None),
            Op::Sip { filter, key, slot } => {
                if !self.sip(filter, *key, *slot, set) {
                    return Ok(());
                }
                self.run_ops(rest, set)
            }
            Op::Intersect {
                seed,
                probe,
                probe_level,
                to,
            } => self.intersect(*seed, *probe, *probe_level, *to, rest, set),
            Op::Semi {
                rel,
                dirs,
                probe_level,
            } => self.semi(*rel, *dirs, *probe_level, rest, set),
            // The bracket is the expand under it, told what to do with
            // a source row it found nothing for. Keeping the two one
            // operator is what lets the group hold its CSR pins and
            // its scratch across outer rows, the same as any other
            // expand; a bracket that drove the group itself paid for
            // all of that once per outer row.
            Op::Optional { len, level } => {
                // A left join wears the same bracket: the probe is what
                // decides the row rather than the walk, and a probe
                // that lands on nothing is the miss.
                if let Op::Join { table, key, to } = &rest[0] {
                    let table = table.clone();
                    return self.join(
                        &table,
                        *key,
                        *to,
                        &rest[1..],
                        set,
                        Some((*level, &rest[*len..])),
                    );
                }
                let &Op::Expand {
                    rel,
                    dirs,
                    to,
                    close,
                    ..
                } = &rest[0]
                else {
                    unreachable!("the bracket compiles with its expand under it");
                };
                let hop = Hop {
                    rel,
                    dirs,
                    to,
                    batch: false,
                    close,
                };
                self.expand(hop, &rest[1..], set, Some((*level, &rest[*len..])))
            }
            Op::OptionalHit => {
                self.opt_hit = true;
                self.run_ops(rest, set)
            }
            Op::DegreeProduct { steps, from } => {
                if *from != set.chunks.len() - 1 {
                    return self.pinned_weight(steps, *from, set);
                }
                if !matches!(self.plan.sink, SinkSpec::Count) {
                    return self.weighted_sink(steps, set);
                }
                self.collect_rows(set.chunks.last().expect("a level under the count"));
                let mut rows = std::mem::take(&mut self.scratch);
                let sum = self.product_sum(steps, &mut rows);
                self.scratch = rows;
                self.sink.count += sum? as i64;
                Ok(())
            }
        }
    }

    /// The single row an OPTIONAL MATCH binds on a miss: every vector
    /// the level has, present and invalid, so a read off any of them
    /// answers null and the level still counts once. Built out of the
    /// morsel arena per miss, which is where it has to come from since
    /// the arena resets between morsels.
    fn null_level(&mut self, level: usize) -> DataChunk {
        let n = 1 + self.plan.levels[level].cols.len();
        let mut vecs = Vec::with_capacity(n);
        for _ in 0..n {
            let mut v = ValueVector::constant(&mut self.arena, PhysType::Int64, 0i64, 1);
            v.validity = Some(Bitmap::new_in(&mut self.arena, 1, false));
            vecs.push(v);
        }
        DataChunk::new(vecs, 1)
    }

    /// Walks one hop. `opt` turns the walk into an OPTIONAL MATCH
    /// bracket: the level the group introduces and the pipeline to run
    /// for a source row the group found nothing for, which gets one
    /// null row of that level instead of dropping out.
    fn expand(
        &mut self,
        hop: Hop,
        rest: &[Op],
        set: &mut ChunkSet,
        opt: Option<(usize, &[Op])>,
    ) -> Result<()> {
        let Hop {
            rel,
            dirs,
            to,
            batch,
            close,
        } = hop;
        let src = set.chunks.len() - 1;
        // A fused close reads its probe list once for the whole expand.
        // That level sits below the one being walked, so its pin does
        // not move while this runs, and a probe side with no edges at
        // all rejects every neighbor before any of them is built.
        let probe = match close {
            Some(c) => {
                let far = &set.chunks[c.probe_level];
                let prow = row_at(far, pinned_pos(far));
                let lists = self.row_lists(c.rel, c.dirs, prow)?;
                if lists.slices().iter().all(|l| l.is_empty()) {
                    self.recycle(lists);
                    self.decisions.empty_close += 1;
                    return Ok(());
                }
                Some(lists)
            }
            None => None,
        };
        let close_lists: Vec<&[u64]> = probe.as_ref().map(RowLists::slices).unwrap_or_default();
        // Copy the active rows out first: pinning mutates the chunk the
        // selection and values are read from.
        let mut idxs = self.idx_pool.pop().unwrap_or_default();
        let mut rows = self.row_pool.pop().unwrap_or_default();
        idxs.clear();
        rows.clear();
        {
            let chunk = &set.chunks[src];
            let vals = chunk.vecs[0].values::<u64>();
            match &chunk.sel {
                Some(s) => {
                    for &i in s.as_slice() {
                        idxs.push(u32::from(i));
                        rows.push(vals[i as usize]);
                    }
                }
                None => {
                    idxs.extend(0..chunk.count);
                    rows.extend_from_slice(vals);
                }
            }
        }
        // The hub weight: the pipeline under this expand is filters and
        // then a degree product off this expand's own source level, so
        // every row it emits stands for as many rows of the hop that was
        // fused away as its source row has neighbors. That count is one
        // degree read per source row, taken here for the whole vector,
        // and it rides the descent in the positions the rows land in.
        // A source row with no neighbors on the fused side is not
        // walked at all: the walk that is gone would have paired it
        // with nothing.
        let hub = match rest.last() {
            Some(Op::DegreeProduct { steps, from }) if batch && *from == src => {
                self.degree_products(steps, &rows)?;
                Some(std::mem::take(&mut self.prod))
            }
            _ => None,
        };
        let mut fillw = if hub.is_some() {
            std::mem::take(&mut self.bwts)
        } else {
            Vec::new()
        };
        fillw.clear();
        let mut result = Ok(());
        // One pin covers a whole storage group and the rows arrive in
        // row order, so the pin is held across rows the way the WCOJ
        // close holds its seed. Without this the loop pays a hash of
        // the pin key and two atomic refcount bumps per row per
        // direction, around a body whose real work is one slice of the
        // group's neighbor array. One slot per side, since an
        // undirected expand reads two pins that move together.
        // A pin is `None` where the group turned out to be worth
        // reading a list at a time instead, which is what a point
        // seeded walk wants: the pin would decode the whole group's
        // neighbor array for the dozen edges one seed has, and on a
        // graph bigger than the decoded pool it would not even hold on
        // to them until the next request.
        let mut held: [Option<(u32, Option<CsrPin>)>; 2] = [None, None];
        let mut point = self.row_pool.pop().unwrap_or_default();
        // The batched descent fills this across source rows and hands
        // it down whole. Nothing above the expand reads the source
        // level in that case, so the pin only has to keep the level's
        // multiplicity at one and any active row does that.
        let mut fill = self.row_pool.pop().unwrap_or_default();
        fill.clear();
        // Where a fused close puts the neighbors it kept.
        let mut masked = self.row_pool.pop().unwrap_or_default();
        let mut cursors = [0usize; 2];
        if batch {
            set.chunks[src].cur = idxs.first().copied();
        }
        'srcs: for (at, (&phys, &row)) in idxs.iter().zip(&rows).enumerate() {
            let weight = match &hub {
                Some(w) if w[at] == 0 => continue,
                Some(w) => w[at] as i64,
                None => 0,
            };
            if !batch {
                set.chunks[src].cur = Some(phys);
            }
            self.opt_hit = false;
            let group = (row / u64::from(GROUP_ROWS)) as u32;
            for (slot, dir) in sides(dirs).enumerate() {
                if held[slot].as_ref().is_none_or(|(g, _)| *g != group) {
                    match self.hold(rel, dir, row, rows.len()) {
                        Ok(p) => held[slot] = Some((group, p)),
                        Err(e) => {
                            result = Err(e);
                            break 'srcs;
                        }
                    }
                }
                let local = (row % u64::from(GROUP_ROWS)) as usize;
                let list = match &held[slot].as_ref().expect("just held").1 {
                    Some(pin) => pin.list(local),
                    None => {
                        point.clear();
                        if let Err(e) = self.snap.get().list_into(rel, row, dir, &mut point) {
                            result = Err(e);
                            break 'srcs;
                        }
                        &point[..]
                    }
                };
                // The close judges the neighbors here, where they are
                // still a sorted list and nothing has been built for
                // them. Both lists ascend, so the probe walks forward
                // and the whole source list costs one merge.
                let list = if close.is_some() {
                    masked.clear();
                    let cur = &mut cursors[..close_lists.len()];
                    cur.fill(0);
                    let mut prev = 0;
                    for &v in list {
                        if member(&close_lists, cur, &mut prev, v) {
                            masked.push(v);
                        }
                    }
                    &masked[..]
                } else {
                    list
                };
                if batch {
                    let mut tail = list;
                    while !tail.is_empty() {
                        let take = (zu_vector::VECTOR_SIZE - fill.len()).min(tail.len());
                        fill.extend_from_slice(&tail[..take]);
                        if hub.is_some() {
                            fillw.resize(fill.len(), weight);
                        }
                        tail = &tail[take..];
                        if fill.len() == zu_vector::VECTOR_SIZE {
                            self.bwts = std::mem::take(&mut fillw);
                            let res = self.descend(to, &fill, rest, set);
                            fillw = std::mem::take(&mut self.bwts);
                            fill.clear();
                            fillw.clear();
                            if let Err(e) = res {
                                result = Err(e);
                                break 'srcs;
                            }
                        }
                    }
                    continue;
                }
                for part in list.chunks(zu_vector::VECTOR_SIZE) {
                    if let Err(e) = self.descend(to, part, rest, set) {
                        result = Err(e);
                        break 'srcs;
                    }
                }
            }
            // Both sides of the hop are walked by here, so nothing
            // else can turn this row into a match. `opt_hit` is what
            // the group's own filters have to say about it too: they
            // sit between the descent and the flag, so a row whose
            // only neighbors they rejected is a miss.
            if let Some((level, cont)) = opt
                && !self.opt_hit
            {
                let chunk = match self.null.take() {
                    Some(c) => c,
                    None => self.null_level(level),
                };
                set.chunks.push(chunk);
                let res = self.run_ops(cont, set);
                // Back into the worker for the next miss, with
                // anything the pipeline below did to its selection
                // undone, since the next miss is a fresh row.
                let mut chunk = set.chunks.pop().expect("just pushed the null level");
                chunk.sel = None;
                chunk.cur = None;
                self.null = Some(chunk);
                if let Err(e) = res {
                    result = Err(e);
                    break 'srcs;
                }
            }
            if self.stop.stopped() {
                break;
            }
        }
        if result.is_ok() && !fill.is_empty() {
            self.bwts = std::mem::take(&mut fillw);
            result = self.descend(to, &fill, rest, set);
            fillw = std::mem::take(&mut self.bwts);
        }
        if let Some(w) = hub {
            self.prod = w;
            fillw.clear();
            self.bwts = fillw;
        }
        set.chunks[src].cur = None;
        drop(close_lists);
        if let Some(lists) = probe {
            self.recycle(lists);
        }
        self.idx_pool.push(idxs);
        self.row_pool.push(rows);
        self.row_pool.push(fill);
        self.row_pool.push(masked);
        result
    }

    /// The second pattern branch: a hop off a level below the newest
    /// one, which is what two patterns sharing a variable compile to
    /// when the far end of both is read.
    ///
    /// The source is pinned for the whole op, so its neighbor list is
    /// read once and every row of the newest level pairs with the whole
    /// of it. That pairing is the reason the newest level is pinned a
    /// row at a time here: everything below the branch reads the levels
    /// under it at their pins, so a vector of them cannot ride along.
    fn branch(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        from: usize,
        to: usize,
        rest: &[Op],
        set: &mut ChunkSet,
    ) -> Result<()> {
        let far = &set.chunks[from];
        let prow = row_at(far, pinned_pos(far));
        let held = self.row_lists(rel, dirs, prow)?;
        let lists = held.slices();
        if lists.iter().all(|l| l.is_empty()) {
            drop(lists);
            self.recycle(held);
            return Ok(());
        }
        let src = set.chunks.len() - 1;
        let mut idxs = self.idx_pool.pop().unwrap_or_default();
        idxs.clear();
        {
            let chunk = &set.chunks[src];
            match &chunk.sel {
                Some(s) => idxs.extend(s.as_slice().iter().map(|&i| u32::from(i))),
                None => idxs.extend(0..chunk.count),
            }
        }
        let mut result = Ok(());
        'srcs: for &phys in &idxs {
            set.chunks[src].cur = Some(phys);
            for list in &lists {
                for part in list.chunks(zu_vector::VECTOR_SIZE) {
                    if let Err(e) = self.descend(to, part, rest, set) {
                        result = Err(e);
                        break 'srcs;
                    }
                }
            }
            if self.stop.stopped() {
                break;
            }
        }
        set.chunks[src].cur = None;
        drop(lists);
        self.recycle(held);
        self.idx_pool.push(idxs);
        result
    }

    /// The sideways pass: drop the rows of the newest level whose key
    /// no build side under this one holds. Answers whether anything is
    /// left to run the rest of the pipeline on.
    ///
    /// The keys are gathered into one buffer first and tested out of
    /// it, because the test is a random read into the filter and the
    /// gather is a sequential one into a column: keeping them apart is
    /// what lets the filter's prefetch run several tests deep while the
    /// gather stays a straight walk. A row whose key is null is dropped
    /// here, which is what the probe would have done with it anyway,
    /// since an equality against null matches nothing.
    ///
    /// A filter earns its pass by rejecting rows. One that is not gets
    /// switched off after the first few vectors and the rest of the run
    /// pays nothing for it: the rows were going to reach the join
    /// either way, and the join tests them itself.
    fn sip(&mut self, filter: &SipFilter, key: ScalarRef, slot: usize, set: &mut ChunkSet) -> bool {
        if self.decisions.sip[slot].workers == 0 {
            self.decisions.sip[slot].workers = 1;
        }
        if !self.sips[slot].on {
            return true;
        }
        let last = set.chunks.last().expect("a level under every filter");
        debug_assert_eq!(
            key.level() + 1,
            set.chunks.len(),
            "a filter sits where its level is the newest one"
        );
        let mut keys = std::mem::take(&mut self.sip_keys);
        let mut rows = std::mem::take(&mut self.sip_rows);
        let mut out = std::mem::take(&mut self.sip_out);
        keys.clear();
        rows.clear();
        for pos in active_positions(last) {
            let k = match key {
                ScalarRef::Col { vec, .. } => {
                    if !last.vecs[vec].is_valid(pos) {
                        continue;
                    }
                    last.vecs[vec].values::<i64>()[pos] as u64
                }
                ScalarRef::RowId { .. } => row_at(last, pos),
                ScalarRef::Node { .. } => unreachable!("a node is not a join key"),
            };
            keys.push(k);
            rows.push(pos as u16);
        }
        out.resize(keys.len(), 0);
        let n = filter.select(&keys, &mut out);
        let state = &mut self.sips[slot];
        state.probes += keys.len() as u64;
        state.kept += n as u64;
        // Rejecting under a tenth of what it sees, over enough vectors
        // for that to be the filter and not the first chunk's luck.
        if state.probes >= SIP_TRIAL && state.kept * 10 > state.probes * 9 {
            state.on = false;
            self.decisions.sip[slot].dropped += 1;
        }
        let seen = &mut self.decisions.sip[slot];
        seen.probes += keys.len() as u64;
        seen.kept += n as u64;
        let all = n == rows.len();
        if n > 0 && !all {
            let mut sel = SelVector::with_capacity(&mut self.arena, n);
            for &i in &out[..n] {
                sel.push(rows[i as usize]);
            }
            set.chunks
                .last_mut()
                .expect("a level under every filter")
                .sel = Some(sel);
        }
        self.sip_keys = keys;
        self.sip_rows = rows;
        self.sip_out = out;
        n > 0
    }

    /// The value join's probe side: one lookup per row of the level the
    /// key reads, and the rows it matched become the newest level.
    ///
    /// The build table was filled once while the plan was compiled and
    /// every worker shares it, so this reads and never writes. A key
    /// that matched nothing costs one directory word, which is what
    /// makes an unmatched probe side cheap enough to leave the join
    /// where the query wrote it.
    ///
    /// A key on a level the pipeline has already left is pinned, so it
    /// is one value for the whole vector; the loop still walks the rows
    /// of the newest level, because each of them is a row of the answer
    /// and pairs with the whole of what the key matched.
    fn join(
        &mut self,
        table: &JoinTable,
        key: ScalarRef,
        to: usize,
        rest: &[Op],
        set: &mut ChunkSet,
        opt: Option<(usize, &[Op])>,
    ) -> Result<()> {
        let src = set.chunks.len() - 1;
        let mut idxs = self.idx_pool.pop().unwrap_or_default();
        idxs.clear();
        {
            let chunk = &set.chunks[src];
            match &chunk.sel {
                Some(s) => idxs.extend(s.as_slice().iter().map(|&i| u32::from(i))),
                None => idxs.extend(0..chunk.count),
            }
        }
        let mut result = Ok(());
        'srcs: for &phys in &idxs {
            set.chunks[src].cur = Some(phys);
            self.opt_hit = false;
            // A null key matches nothing on either engine, so it walks
            // no rows. Under a bracket it is still a source row, and a
            // source row that matched nothing is a miss.
            if let Some(k) = int_key(set, key, phys as usize) {
                for part in table.lookup(k).chunks(zu_vector::VECTOR_SIZE) {
                    if let Err(e) = self.descend(to, part, rest, set) {
                        result = Err(e);
                        break 'srcs;
                    }
                }
            }
            // The probe is done by here, and so are the group's own
            // predicates, which sit between the descent and the flag.
            if let Some((level, cont)) = opt
                && !self.opt_hit
            {
                let chunk = match self.null.take() {
                    Some(c) => c,
                    None => self.null_level(level),
                };
                set.chunks.push(chunk);
                let res = self.run_ops(cont, set);
                let mut chunk = set.chunks.pop().expect("just pushed the null level");
                chunk.sel = None;
                chunk.cur = None;
                self.null = Some(chunk);
                if let Err(e) = res {
                    result = Err(e);
                    break 'srcs;
                }
            }
            if self.stop.stopped() {
                break;
            }
        }
        set.chunks[src].cur = None;
        self.idx_pool.push(idxs);
        result
    }

    /// Pushes one vector of expanded rows through the rest of the
    /// pipeline as the newest level.
    fn descend(&mut self, to: usize, part: &[u64], rest: &[Op], set: &mut ChunkSet) -> Result<()> {
        let chunk = self.make_level(to, part, &set.chunks, &[])?;
        set.chunks.push(chunk);
        let res = self.run_ops(rest, set);
        set.chunks.pop();
        res
    }

    /// The WCOJ close: every row of the newest level is a wedge middle
    /// and the far end is pinned above, so the closing node is the
    /// intersection of two sorted neighbor lists. The far end's list is
    /// read once for the whole vector, both lists are borrowed out of
    /// their CSR pins with nothing copied, and the walk galloping past
    /// the runs it cannot match is what replaces a storage probe per
    /// candidate.
    fn intersect(
        &mut self,
        seed: (RelId, Dirs),
        probe: (RelId, Dirs),
        probe_level: usize,
        to: usize,
        rest: &[Op],
        set: &mut ChunkSet,
    ) -> Result<()> {
        let src = set.chunks.len() - 1;
        let far = &set.chunks[probe_level];
        let prow = row_at(far, pinned_pos(far));
        let pheld = self.row_lists(probe.0, probe.1, prow)?;
        let pl = pheld.slices();
        // An undirected probe end asks whether an edge exists either
        // way, so the two stored lists become one sorted set here and
        // the walk below stays a single leapfrog. The union is built
        // once for the whole vector, the same as the single list case.
        let mut union = self.row_pool.pop().unwrap_or_default();
        union.clear();
        let plist: &[u64] = match pl.as_slice() {
            [one] => one,
            [a, b] => {
                merge_sorted(a, b, &mut union);
                &union
            }
            _ => unreachable!("an expand walks one side or two"),
        };
        if plist.is_empty() {
            self.row_pool.push(union);
            drop(pl);
            self.recycle(pheld);
            return Ok(());
        }
        // Copy the active rows out first: pinning mutates the chunk the
        // selection and values are read from.
        let mut idxs = self.idx_pool.pop().unwrap_or_default();
        let mut rows = self.row_pool.pop().unwrap_or_default();
        idxs.clear();
        rows.clear();
        {
            let chunk = &set.chunks[src];
            let vals = chunk.vecs[0].values::<u64>();
            match &chunk.sel {
                Some(s) => {
                    for &i in s.as_slice() {
                        idxs.push(u32::from(i));
                        rows.push(vals[i as usize]);
                    }
                }
                None => {
                    idxs.extend(0..chunk.count);
                    rows.extend_from_slice(vals);
                }
            }
        }
        let mut hits = std::mem::take(&mut self.hits);
        let mut result = Ok(());
        // One seed pin covers a whole storage group, and a neighbor
        // list rarely leaves the group it started in, so the pin is
        // held across rows rather than looked up per row: the lookup
        // is a hash of the pin key and this loop runs once per edge
        // under the scan. One slot per side, since an undirected seed
        // reads two pins that move together.
        // A pin is `None` where the group is worth more than the rows
        // this vector holds in it, the same rule the expand follows,
        // and then the seed's list is read on its own.
        let mut held: [Option<(u32, Option<CsrPin>)>; 2] = [None, None];
        let mut point = self.row_pool.pop().unwrap_or_default();
        'srcs: for (&phys, &row) in idxs.iter().zip(&rows) {
            set.chunks[src].cur = Some(phys);
            let group = (row / u64::from(GROUP_ROWS)) as u32;
            let at = (row % u64::from(GROUP_ROWS)) as usize;
            hits.clear();
            // Each stored side is walked on its own. A node reachable
            // both ways is two edges and closes the wedge twice, which
            // is what the row by row expand this replaces would count.
            for (slot, dir) in sides(seed.1).enumerate() {
                if held[slot].as_ref().is_none_or(|(g, _)| *g != group) {
                    match self.hold(seed.0, dir, row, rows.len()) {
                        Ok(p) => held[slot] = Some((group, p)),
                        Err(e) => {
                            result = Err(e);
                            break 'srcs;
                        }
                    }
                }
                let slist = match &held[slot].as_ref().expect("just held").1 {
                    Some(pin) => pin.list(at),
                    None => {
                        point.clear();
                        if let Err(e) = self.snap.get().list_into(seed.0, row, dir, &mut point) {
                            result = Err(e);
                            break 'srcs;
                        }
                        &point[..]
                    }
                };
                leapfrog(slist, plist, &mut hits);
            }
            for part in hits.chunks(zu_vector::VECTOR_SIZE) {
                let chunk = match self.make_level(to, part, &set.chunks, &[]) {
                    Ok(c) => c,
                    Err(e) => {
                        result = Err(e);
                        break 'srcs;
                    }
                };
                set.chunks.push(chunk);
                let res = self.run_ops(rest, set);
                set.chunks.pop();
                if let Err(e) = res {
                    result = Err(e);
                    break 'srcs;
                }
            }
            if self.stop.stopped() {
                break;
            }
        }
        set.chunks[src].cur = None;
        self.hits = hits;
        self.idx_pool.push(idxs);
        self.row_pool.push(rows);
        self.row_pool.push(point);
        self.row_pool.push(union);
        drop(pl);
        self.recycle(pheld);
        result
    }

    /// The binary close: both ends of the edge are already bound, so
    /// the newest level keeps the rows with an edge back to the pinned
    /// end and nothing else changes. The pinned end's neighbor list is
    /// read once for the whole vector and each row galloped into it,
    /// the cursor carried across rows because an expand hands its rows
    /// over in list order.
    fn semi(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        probe_level: usize,
        rest: &[Op],
        set: &mut ChunkSet,
    ) -> Result<()> {
        let far = &set.chunks[probe_level];
        let prow = row_at(far, pinned_pos(far));
        let held = self.row_lists(rel, dirs, prow)?;
        let lists = held.slices();
        if lists.iter().all(|l| l.is_empty()) {
            drop(lists);
            self.recycle(held);
            self.decisions.empty_close += 1;
            return Ok(());
        }
        let last = set.chunks.len() - 1;
        let mut keep = std::mem::take(&mut self.keep);
        keep.clear();
        {
            let chunk = &set.chunks[last];
            let vals = chunk.vecs[0].values::<u64>();
            let mut cur = [0usize; 2];
            let mut prev = 0;
            let cur = &mut cur[..lists.len()];
            match &chunk.sel {
                Some(s) => {
                    for &i in s.as_slice() {
                        if member(&lists, cur, &mut prev, vals[i as usize]) {
                            keep.push(i);
                        }
                    }
                }
                None => {
                    for i in 0..chunk.count {
                        if member(&lists, cur, &mut prev, vals[i as usize]) {
                            keep.push(i as u16);
                        }
                    }
                }
            }
        }
        drop(lists);
        self.recycle(held);
        let mut result = Ok(());
        if !keep.is_empty() {
            let mut sel = SelVector::with_capacity(&mut self.arena, keep.len());
            for &i in &keep {
                sel.push(i);
            }
            set.chunks[last].sel = Some(sel);
            result = self.run_ops(rest, set);
        }
        self.keep = keep;
        result
    }

    /// The pin for a group, or `None` when the caller is better off
    /// reading its lists one at a time. `wanted` is how many source
    /// rows the caller holds, and the snapshot says how many it takes
    /// for the pin to pay: a scan holds a morsel and is always over
    /// it, a point lookup holds one row and is almost never.
    fn hold(&mut self, rel: RelId, dir: Dir, row: u64, wanted: usize) -> Result<Option<CsrPin>> {
        let group = (row / u64::from(GROUP_ROWS)) as u32;
        let key = (rel, matches!(dir, Dir::Bwd), group);
        if self.pins.contains_key(&key) {
            return Ok(Some(self.pins[&key].clone()));
        }
        if wanted < self.snap.get().list_threshold(rel, group, dir)? {
            self.decisions.point_reads += 1;
            return Ok(None);
        }
        Ok(Some(self.pin(rel, dir, row)?))
    }

    /// One row's neighbor lists, one per side of `dirs`, read the way
    /// the group they sit in is worth reading. Every caller of this
    /// holds one row's lists for a whole vector of work, so `wanted` is
    /// one list per side and the group gets pinned only where it is
    /// small enough for that to be the cheaper read.
    fn row_lists(&mut self, rel: RelId, dirs: Dirs, row: u64) -> Result<RowLists> {
        let mut out = RowLists {
            sides: Vec::with_capacity(2),
            at: (row % u64::from(GROUP_ROWS)) as usize,
        };
        for dir in sides(dirs) {
            out.sides.push(match self.hold(rel, dir, row, 1)? {
                Some(pin) => RowList::Pinned(pin),
                None => {
                    let mut list = self.row_pool.pop().unwrap_or_default();
                    list.clear();
                    self.snap.get().list_into(rel, row, dir, &mut list)?;
                    RowList::Read(list)
                }
            });
        }
        Ok(out)
    }

    /// Hands a row's read lists back to the pool they came from.
    fn recycle(&mut self, lists: RowLists) {
        for side in lists.sides {
            if let RowList::Read(list) = side {
                self.row_pool.push(list);
            }
        }
    }

    fn pin(&mut self, rel: RelId, dir: Dir, row: u64) -> Result<CsrPin> {
        let group = (row / u64::from(GROUP_ROWS)) as u32;
        let key = (rel, matches!(dir, Dir::Bwd), group);
        if let Some(p) = self.pins.get(&key) {
            return Ok(p.clone());
        }
        self.decisions.group_pins += 1;
        let p = self.snap.get().csr(rel, group, dir)?;
        self.pins.insert(key, p.clone());
        Ok(p)
    }

    /// Builds one level chunk from a neighbor slice: row ids plus every
    /// column the pipeline reads on this level, gathered if it is stored
    /// and computed over the vector if it is an expression. The list is
    /// in registration order and a program only loads columns registered
    /// ahead of it, so one pass fills the chunk.
    fn make_level(
        &mut self,
        level: usize,
        part: &[u64],
        below: &[DataChunk],
        keys: &[u64],
    ) -> Result<DataChunk> {
        let info = &self.plan.levels[level];
        let mut vecs = Vec::with_capacity(1 + info.cols.len());
        vecs.push(ValueVector::flat_from(
            &mut self.arena,
            PhysType::Int64,
            part,
        ));
        let mut chunk = DataChunk::new(vecs, part.len() as u32);
        for spec in &info.cols {
            let v = match spec {
                ColSpec::Stored(col, _) => {
                    self.snap
                        .get()
                        .gather(info.table, *col, part, &mut self.arena)?
                }
                ColSpec::Computed(prog) => prog.eval(&chunk, &mut self.arena)?,
                ColSpec::Outer { from, vec } => {
                    let src = &below[*from];
                    let at = pinned_pos(src);
                    broadcast(&src.vecs[*vec], at, part.len(), &mut self.arena)
                }
                // The keys arrive alongside the rows they found, so the
                // column is the batch as it was handed over. Only the
                // seeks source registers one, and only on level 0.
                ColSpec::Key => {
                    debug_assert_eq!(keys.len(), part.len(), "one key per row it found");
                    ValueVector::flat_from(&mut self.arena, PhysType::Int64, keys)
                }
                // Only level 0 carries a kernel's answer, and a level 0
                // built out of rows rather than scanned is a seek,
                // which the compiler never puts under a call.
                ColSpec::Func => unreachable!("a func column is scanned, not gathered"),
            };
            chunk.vecs.push(v);
        }
        Ok(chunk)
    }

    /// Copies a chunk's active row ids into the scratch buffer.
    fn collect_rows(&mut self, chunk: &DataChunk) {
        let vals = chunk.vecs[0].values::<u64>();
        self.scratch.clear();
        match &chunk.sel {
            Some(s) => {
                for &i in s.as_slice() {
                    self.scratch.push(vals[i as usize]);
                }
            }
            None => self.scratch.extend_from_slice(vals),
        }
    }

    /// The two-hop count fast path: an expand whose only consumer is
    /// the fused degree product never builds levels at all. The whole
    /// chunk's neighbor lists concatenate into one buffer and the
    /// product runs over that, so the per-source cost is a slice copy
    /// instead of a pipeline descent.
    fn expand_degree(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        steps: &[(RelId, Dirs)],
        set: &ChunkSet,
    ) -> Result<()> {
        self.collect_rows(set.chunks.last().expect("a level under the expand"));
        if self.scratch.is_empty() {
            return Ok(());
        }
        let mut rows = std::mem::take(&mut self.scratch);
        rows.sort_unstable();
        let mut neigh = std::mem::take(&mut self.neigh);
        neigh.clear();
        let sum = self
            .concat_lists(rel, dirs, &rows, &mut neigh)
            .and_then(|()| self.product_sum(steps, &mut neigh));
        self.scratch = rows;
        self.neigh = neigh;
        self.sink.count += sum? as i64;
        Ok(())
    }

    /// Appends every row's neighbor list, one pin per group run. A scan
    /// morsel stays inside one group and pins once, but a batch of
    /// seeks lands anywhere in the table, so the pin follows the group
    /// the row is in rather than the group the first row happened to be
    /// in. `rows` ascending is what keeps that one pin per group; what
    /// comes back is summed, so the caller is free to sort.
    ///
    /// How many rows of the run land in a group is known before it is
    /// read, since the rows arrive sorted, so a run too short to pay
    /// for the group reads its lists one at a time instead. That is the
    /// two hop count off a seed: a dozen neighbors spread over a few
    /// groups, none of which is worth decoding whole.
    fn concat_lists(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        rows: &[u64],
        out: &mut Vec<u64>,
    ) -> Result<()> {
        for dir in sides(dirs) {
            let mut at = 0;
            while at < rows.len() {
                let group = rows[at] / u64::from(GROUP_ROWS);
                let run = rows[at..].partition_point(|r| r / u64::from(GROUP_ROWS) == group);
                match self.hold(rel, dir, rows[at], run)? {
                    Some(pin) => {
                        for &row in &rows[at..at + run] {
                            out.extend_from_slice(pin.list((row % u64::from(GROUP_ROWS)) as usize));
                        }
                    }
                    None => {
                        for &row in &rows[at..at + run] {
                            self.snap.get().list_into(rel, row, dir, out)?;
                        }
                    }
                }
                at += run;
            }
        }
        Ok(())
    }

    /// Sum over `rows` of each row's per-step degree product, offsets
    /// only. Every level above the one the rows came from is pinned
    /// here, so each row counts exactly once. One step is a plain
    /// degree sum and stays on the bulk `degree_batch` read; several
    /// steps read per-row degrees and multiply.
    ///
    /// Both reads hold one group's offsets at a time, so a row list
    /// that jumps between groups reads a segment per row, which is what
    /// a batch of seeks hands over. The answer is a sum over the whole
    /// list and nothing above reads the list again, so the list is
    /// sorted in place first and every group is read once.
    fn product_sum(&mut self, steps: &[(RelId, Dirs)], rows: &mut [u64]) -> Result<u64> {
        rows.sort_unstable();
        if let [(rel, dirs)] = steps {
            let mut sum = 0;
            for dir in sides(*dirs) {
                sum += self.snap.get().degree_batch(*rel, rows, dir)?;
            }
            return Ok(sum);
        }
        self.prod.clear();
        self.prod.resize(rows.len(), 1);
        for &(rel, dirs) in steps {
            self.deg.clear();
            self.deg.resize(rows.len(), 0);
            for dir in sides(dirs) {
                self.snap.get().degrees(rel, rows, dir, &mut self.deg)?;
            }
            for (p, &d) in self.prod.iter_mut().zip(&self.deg) {
                *p *= d;
            }
        }
        Ok(self.prod.iter().sum())
    }

    /// Each row's product of per-step degrees, left where the row is.
    ///
    /// [`Worker::product_sum`] sorts the list itself, because its answer
    /// is one number over the whole of it. Here every weight has to stay
    /// next to the row it belongs to, since the sink reads that row's
    /// keys, so what gets sorted is an order over the list and the
    /// degrees are scattered back through it.
    ///
    /// The sort is the whole cost of this path when the rows are a hop's
    /// neighbors. A degree read that walks into a new storage group
    /// every row reads two offsets out of the file for each of them,
    /// where the same rows in order read the group once and answer the
    /// rest off it, so a few thousand ids sorted buys back a chunk
    /// decode per id.
    fn degree_products(&mut self, steps: &[(RelId, Dirs)], rows: &[u64]) -> Result<()> {
        self.prod.clear();
        self.prod.resize(rows.len(), 1);
        // A level's own rows arrive ascending off the scan and have
        // nothing to gain here, so only a hop's neighbors, which arrive
        // in the order their source rows did, pay for the order.
        let ascending = rows.is_sorted();
        if !ascending {
            self.order.clear();
            self.order.extend(0..rows.len() as u32);
            self.order.sort_unstable_by_key(|&i| rows[i as usize]);
            self.sorted.clear();
            self.sorted
                .extend(self.order.iter().map(|&i| rows[i as usize]));
        }
        for &(rel, dirs) in steps {
            self.deg.clear();
            self.deg.resize(rows.len(), 0);
            let read = if ascending { rows } else { &self.sorted };
            for dir in sides(dirs) {
                self.snap.get().degrees(rel, read, dir, &mut self.deg)?;
            }
            if ascending {
                for (p, &d) in self.prod.iter_mut().zip(&self.deg) {
                    *p *= d;
                }
            } else {
                for (&i, &d) in self.order.iter().zip(&self.deg) {
                    self.prod[i as usize] *= d;
                }
            }
        }
        Ok(())
    }

    /// The weighted answer for a hop that still walks: what a source
    /// row weighs is the sum over its neighbors of what each of those
    /// weighs, so the walk reads its lists here and the levels above it
    /// are never built.
    ///
    /// This is the shape a group by two hops out takes, the key sitting
    /// on the level the walk starts from. Running it as an ordinary
    /// descent instead costs a level, a key vector and a group probe
    /// per source row, over vectors as short as one node's neighbor
    /// list, which is most of the query on a graph with a small mean
    /// degree.
    fn expand_weights(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        steps: &[(RelId, Dirs)],
        set: &ChunkSet,
    ) -> Result<()> {
        let last = set.chunks.len() - 1;
        self.collect_rows(&set.chunks[last]);
        if self.scratch.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.scratch);
        let mut neigh = std::mem::take(&mut self.neigh);
        let mut ends = std::mem::take(&mut self.ends);
        neigh.clear();
        ends.clear();
        let res = self
            .lists_by_row(rel, dirs, &rows, &mut neigh, &mut ends)
            .and_then(|()| self.degree_products(steps, &neigh));
        self.scratch = rows;
        self.neigh = neigh;
        let mut wts = std::mem::take(&mut self.wts);
        let mut wsel = std::mem::take(&mut self.wsel);
        wts.clear();
        wsel.clear();
        if res.is_ok() {
            let mut at = 0;
            for (pos, &end) in active_positions(&set.chunks[last]).zip(&ends) {
                let w: u64 = self.prod[at..end as usize].iter().sum();
                at = end as usize;
                if w != 0 {
                    wts.push(w as i64);
                    wsel.push(pos as u16);
                }
            }
        }
        self.ends = ends;
        let out = res.and_then(|()| self.weighted_rows(set, &wsel, &wts));
        self.wts = wts;
        self.wsel = wsel;
        out
    }

    /// Every row's neighbor list, one after another, with the end of
    /// each row's stretch. [`Worker::concat_lists`] walks a direction at
    /// a time because its answer is one sum; here the lists have to stay
    /// with the row they came off, so the row is the outer loop and one
    /// pin per side follows the group.
    fn lists_by_row(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        rows: &[u64],
        out: &mut Vec<u64>,
        ends: &mut Vec<u32>,
    ) -> Result<()> {
        let mut held: [Option<(u64, Option<CsrPin>)>; 2] = [None, None];
        for &row in rows {
            let group = row / u64::from(GROUP_ROWS);
            for (slot, dir) in sides(dirs).enumerate() {
                if held[slot].as_ref().is_none_or(|(g, _)| *g != group) {
                    held[slot] = Some((group, self.hold(rel, dir, row, rows.len())?));
                }
                match &held[slot].as_ref().expect("just held").1 {
                    Some(pin) => {
                        out.extend_from_slice(pin.list((row % u64::from(GROUP_ROWS)) as usize));
                    }
                    None => self.snap.get().list_into(rel, row, dir, out)?,
                }
            }
            ends.push(out.len() as u32);
        }
        Ok(())
    }

    /// Feeds the sink the rows a fused expand would have built, as
    /// weights rather than rows. Every neighbor of a source row carries
    /// that row's keys and that row's arguments, so the group only
    /// needs to know how many neighbors there were, and that is a
    /// degree: offsets alone, with the neighbor array never read.
    ///
    /// A row the steps found nothing for weighs nothing and is dropped
    /// here, which is the difference between a group by over a hop and
    /// one over the source table: a key whose rows have no edge at all
    /// is not a group of the answer.
    fn weighted_sink(&mut self, steps: &[(RelId, Dirs)], set: &ChunkSet) -> Result<()> {
        let last = set.chunks.len() - 1;
        self.collect_rows(&set.chunks[last]);
        if self.scratch.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.scratch);
        let res = self.degree_products(steps, &rows);
        self.scratch = rows;
        res?;
        let mut wts = std::mem::take(&mut self.wts);
        let mut wsel = std::mem::take(&mut self.wsel);
        wts.clear();
        wsel.clear();
        for (pos, &w) in active_positions(&set.chunks[last]).zip(&self.prod) {
            if w != 0 {
                wts.push(w as i64);
                wsel.push(pos as u16);
            }
        }
        let res = self.weighted_rows(set, &wsel, &wts);
        self.wts = wts;
        self.wsel = wsel;
        res
    }

    /// The weight a fused hop off a level the pipeline has walked past
    /// contributes. That level is pinned while this runs, so the product
    /// of its degrees is one number for the whole vector and every row
    /// the sink sees stands for that many rows of the walk that is no
    /// longer there.
    ///
    /// A pinned row the steps found nothing for weighs nothing, and the
    /// walk they replaced would have paired the rows below with nothing
    /// at all, so the vector drops here.
    fn pinned_weight(
        &mut self,
        steps: &[(RelId, Dirs)],
        from: usize,
        set: &ChunkSet,
    ) -> Result<()> {
        if !self.bwts.is_empty() {
            return self.batched_weight(set);
        }
        let src = &set.chunks[from];
        let mut rows = std::mem::take(&mut self.scratch);
        rows.clear();
        rows.push(row_at(src, pinned_pos(src)));
        let w = self.product_sum(steps, &mut rows);
        self.scratch = rows;
        let w = w?;
        if w == 0 {
            return Ok(());
        }
        let last = set.chunks.len() - 1;
        if matches!(self.plan.sink, SinkSpec::Count) {
            let n = active_positions(&set.chunks[last]).count() as u64;
            self.sink.count += (w * n) as i64;
            return Ok(());
        }
        let mut wts = std::mem::take(&mut self.wts);
        let mut wsel = std::mem::take(&mut self.wsel);
        wts.clear();
        wsel.clear();
        for pos in active_positions(&set.chunks[last]) {
            wts.push(w as i64);
            wsel.push(pos as u16);
        }
        let res = self.weighted_rows(set, &wsel, &wts);
        self.wts = wts;
        self.wsel = wsel;
        res
    }

    /// The same weight when the expand above carried it down per row.
    /// The vector's rows come off many source rows there, so each of
    /// them stands for a different number, read at the position the row
    /// sits in rather than off a pin.
    fn batched_weight(&mut self, set: &ChunkSet) -> Result<()> {
        let last = set.chunks.len() - 1;
        let held = std::mem::take(&mut self.bwts);
        let out = if matches!(self.plan.sink, SinkSpec::Count) {
            let total: i64 = active_positions(&set.chunks[last]).map(|p| held[p]).sum();
            self.sink.count += total;
            Ok(())
        } else {
            let mut wts = std::mem::take(&mut self.wts);
            let mut wsel = std::mem::take(&mut self.wsel);
            wts.clear();
            wsel.clear();
            for pos in active_positions(&set.chunks[last]) {
                wts.push(held[pos]);
                wsel.push(pos as u16);
            }
            let res = self.weighted_rows(set, &wsel, &wts);
            self.wts = wts;
            self.wsel = wsel;
            res
        };
        self.bwts = held;
        out
    }

    /// The weighted rows into whichever sink the fusion allowed.
    fn weighted_rows(&mut self, set: &ChunkSet, wsel: &[u16], wts: &[i64]) -> Result<()> {
        if wts.is_empty() {
            return Ok(());
        }
        let plan = self.plan;
        let SinkSpec::Agg { keys, aggs, .. } = &plan.sink else {
            unreachable!("only a count or an aggregate fuses a degree product");
        };
        if !keys.is_empty() {
            return self.group_rows(set, keys, aggs, Some(wsel), wts.len(), Some(wts));
        }
        let last = set.chunks.len() - 1;
        let total: i64 = wts.iter().sum();
        if self.sink.bare.is_empty() {
            self.sink.bare = aggs.iter().map(Acc::new).collect();
        }
        for (spec, acc) in aggs.iter().zip(&mut self.sink.bare) {
            match spec.arg() {
                // Dense columns and required nodes are never null, so
                // count(x) counts rows exactly like star does.
                None => acc.add_star(total),
                Some(_) if matches!(spec, AggSpec::CountRef(_)) => acc.add_star(total),
                Some(r) if r.level() == last => {
                    for (&w, &pos) in wts.iter().zip(wsel) {
                        acc.add_int(int_scalar(set, r, pos as usize), w)?;
                    }
                }
                Some(r) => {
                    let pos = pinned_pos(&set.chunks[r.level()]);
                    acc.add_int(int_scalar(set, r, pos), total)?;
                }
            }
        }
        Ok(())
    }

    /// Keyed aggregation for one chunk set, a column at a time: pack the
    /// key vector, probe the whole vector in one call, then walk the
    /// group indices once per aggregate. Nothing in here dispatches on
    /// the plan per row, which is the point; the per-row work left is a
    /// hash, a probe, and an accumulator update.
    fn group_vector(&mut self, set: &ChunkSet, keys: &[ScalarRef], aggs: &[AggSpec]) -> Result<()> {
        let last = set.chunks.len() - 1;
        let live = &set.chunks[last];
        debug_assert!(live.cur.is_none(), "sink level is never pinned");
        // An unfiltered vector reads straight down its columns; only a
        // selection sends the reads through the survivor list.
        let sel = live.sel.as_ref().map(|s| s.as_slice());
        let rows = sel.map_or(live.count as usize, |s| s.len());
        self.group_rows(set, keys, aggs, sel, rows, None)
    }

    /// The same over a chosen set of rows, each standing for `w` of
    /// them when a fused degree product handed its weights over. A
    /// weight multiplies a count and a sum and leaves a min or a max
    /// alone, which is what the accumulator already does with the
    /// multiplicity a pinned level carries.
    fn group_rows(
        &mut self,
        set: &ChunkSet,
        keys: &[ScalarRef],
        aggs: &[AggSpec],
        sel: Option<&[u16]>,
        rows: usize,
        w: Option<&[i64]>,
    ) -> Result<()> {
        debug_assert!(w.is_none_or(|w| w.len() == rows), "one weight per row");
        if rows == 0 {
            return Ok(());
        }
        // Dense columns and required nodes are never null, so count(x)
        // counts rows exactly like count(*). One fixed-width key with
        // nothing but counters over it is the shape the table can hold
        // whole in its slots, which is most of what a GROUP BY is.
        let parts = key_parts(keys);
        let counting = parts.len() == 1
            && parts[0] == PartKind::Int
            && aggs
                .iter()
                .all(|s| s.arg().is_none() || matches!(s, AggSpec::CountRef(_)));
        let table = self.sink.groups.get_or_insert_with(|| {
            if counting {
                GroupTable::counting(parts, aggs.len())
            } else {
                GroupTable::new(parts, aggs.len())
            }
        });
        self.batch.reset(table.stride(), rows);
        let mut off = 0;
        for &r in keys {
            fill_key_col(self.plan, set, r, sel, rows, off, &mut self.batch)?;
            off += part_kind(r).words();
        }
        if counting {
            let (words, _) = self.batch.words_mut();
            match w {
                Some(w) => table.count_ints_weighted(words, w),
                None => table.count_ints(words),
            }
            return Ok(());
        }
        table.probe(&self.batch, aggs, &mut self.gids);
        let n = aggs.len();
        let weight = |i: usize| w.map_or(1, |w| w[i]);
        for (j, spec) in aggs.iter().enumerate() {
            // Dense columns and required nodes are never null, so
            // count(x) counts rows exactly like count(*).
            let counting = spec.arg().is_none() || matches!(spec, AggSpec::CountRef(_));
            if counting {
                let accs = table.accs_mut();
                // Warming the states the way the probe warms slots
                // measured slower here, so the loop stays plain: the
                // group indices repeat far more than slots do, and the
                // states are already in cache most of the time.
                for (i, &g) in self.gids.iter().enumerate() {
                    accs[g as usize * n + j].add_star(weight(i));
                }
                continue;
            }
            let r = spec
                .arg()
                .expect("a non counting aggregate has an argument");
            gather_ints(set, r, sel, rows, &mut self.args);
            let accs = table.accs_mut();
            for (i, (&g, &v)) in self.gids.iter().zip(&self.args).enumerate() {
                accs[g as usize * n + j].add_int(v, weight(i))?;
            }
        }
        Ok(())
    }

    fn push_sink(&mut self, set: &mut ChunkSet) -> Result<()> {
        match &self.plan.sink {
            SinkSpec::Count => {
                self.sink.count += set.multiplicity() as i64;
                Ok(())
            }
            // The distinct tuple is the group key and nothing
            // accumulates against it, so the table is built for its
            // group count alone.
            SinkSpec::CountDistinct { keys } => self.group_vector(set, keys, &[]),
            SinkSpec::Rows { items, .. } => {
                let plan = self.plan;
                let last = set.chunks.len() - 1;
                for pos in active_positions(&set.chunks[last]) {
                    let at = (self.morsel as u32, self.local_rows as u32);
                    self.local_rows += 1;
                    // Under a LIMIT the buffer judges the row on its
                    // sort keys alone, and a row that loses to the k it
                    // already holds is never built.
                    if let Some(top) = self.sink.top.as_mut() {
                        self.keybuf.clear();
                        for &(col, _) in top.keys() {
                            self.keybuf.push(scalar(plan, set, items[col], pos)?);
                        }
                        if !top.wants(&self.keybuf) {
                            continue;
                        }
                    }
                    let mut row = Vec::with_capacity(items.len());
                    for &r in items {
                        row.push(scalar(plan, set, r, pos)?);
                    }
                    match self.sink.top.as_mut() {
                        Some(top) => top.keep(&self.keybuf, at, row),
                        None => self.sink.rows.push(row),
                    }
                }
                Ok(())
            }
            SinkSpec::Agg { keys, aggs, .. } => {
                let last = set.chunks.len() - 1;
                let mult = set.multiplicity() as i64;
                if mult == 0 {
                    return Ok(());
                }
                if keys.is_empty() {
                    if self.sink.bare.is_empty() {
                        self.sink.bare = aggs.iter().map(Acc::new).collect();
                    }
                    for (spec, acc) in aggs.iter().zip(&mut self.sink.bare) {
                        match spec.arg() {
                            None => acc.add_star(mult),
                            Some(r) if matches!(spec, crate::compile::AggSpec::CountRef(_)) => {
                                // Dense columns and required nodes are
                                // never null; count(x) counts rows.
                                let _ = r;
                                acc.add_star(mult);
                            }
                            Some(r) if r.level() == last => {
                                for pos in active_positions(&set.chunks[last]) {
                                    acc.add_int(int_scalar(set, r, pos), 1)?;
                                }
                            }
                            Some(r) => {
                                let pos = pinned_pos(&set.chunks[r.level()]);
                                acc.add_int(int_scalar(set, r, pos), mult)?;
                            }
                        }
                    }
                    Ok(())
                } else {
                    self.group_vector(set, keys, aggs)
                }
            }
        }
    }
}

/// Intersects two sorted neighbor lists, galloping past the runs
/// neither side can match. A repeat in the seed list is a real
/// multi-edge row and emits again; a repeat in the probe list is only
/// the existence check answering twice, so it adds nothing. That is
/// the old engine's rule and the counts depend on it.
fn leapfrog(seed: &[u64], probe: &[u64], out: &mut Vec<u64>) {
    let (mut si, mut pi) = (0, 0);
    while si < seed.len() && pi < probe.len() {
        let (sv, pv) = (seed[si], probe[pi]);
        if sv < pv {
            si = gallop(seed, pv, si);
        } else if pv < sv {
            pi = gallop(probe, sv, pi);
        } else {
            while si < seed.len() && seed[si] == sv {
                out.push(sv);
                si += 1;
            }
            pi += 1;
        }
    }
}

/// Unions two sorted neighbor lists into `out`, ascending and without
/// repeats. The probe side of an intersection only answers whether an
/// edge exists, so a node the two sides share is one entry here.
fn merge_sorted(a: &[u64], b: &[u64], out: &mut Vec<u64>) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let v = a[i].min(b[j]);
        out.push(v);
        while i < a.len() && a[i] == v {
            i += 1;
        }
        while j < b.len() && b[j] == v {
            j += 1;
        }
    }
    for &v in a[i..].iter().chain(&b[j..]) {
        if out.last() != Some(&v) {
            out.push(v);
        }
    }
}

/// Whether `v` sits in any of the pinned lists, carrying each list's
/// cursor forward. Rows arrive in list order out of an expand, so the
/// cursor usually starts within a step or two of the answer; a row that
/// goes backwards rewinds it, which is what keeps a filtered or scanned
/// level correct here too.
fn member(lists: &[&[u64]], cur: &mut [usize], prev: &mut u64, v: u64) -> bool {
    if v < *prev {
        cur.fill(0);
    }
    *prev = v;
    let mut hit = false;
    for (list, c) in lists.iter().zip(cur.iter_mut()) {
        *c = gallop(list, v, *c);
        hit |= list.get(*c) == Some(&v);
    }
    hit
}

/// First position at or after `from` whose value is at least `target`,
/// found by doubling the step and then bisecting the bracket it
/// overshot. Doubling is what keeps a long list against a short one
/// logarithmic in the answer rather than linear in the list.
fn gallop(list: &[u64], target: u64, from: usize) -> usize {
    let mut step = 1;
    let mut lo = from;
    while lo + step < list.len() && list[lo + step] < target {
        lo += step;
        step *= 2;
    }
    let hi = (lo + step + 1).min(list.len());
    lo + list[lo..hi].partition_point(|&v| v < target)
}

/// Which CSR sides one expand walks, forward first.
fn sides(dirs: Dirs) -> impl Iterator<Item = Dir> {
    let (a, b) = match dirs {
        Dirs::One(d) => (Some(d), None),
        Dirs::Both => (Some(Dir::Fwd), Some(Dir::Bwd)),
    };
    a.into_iter().chain(b)
}

/// Physical positions of the active rows of an unpinned chunk.
fn active_positions(chunk: &DataChunk) -> impl Iterator<Item = usize> + '_ {
    debug_assert!(chunk.cur.is_none(), "sink level is never pinned");
    match &chunk.sel {
        Some(s) => Either::A(s.as_slice().iter().map(|&i| i as usize)),
        None => Either::B(0..chunk.count as usize),
    }
}

enum Either<A, B> {
    A(A),
    B(B),
}

impl<A: Iterator<Item = usize>, B: Iterator<Item = usize>> Iterator for Either<A, B> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            Either::A(a) => a.next(),
            Either::B(b) => b.next(),
        }
    }
}

fn pinned_pos(chunk: &DataChunk) -> usize {
    chunk.cur.expect("earlier levels are pinned") as usize
}

fn row_at(chunk: &DataChunk, pos: usize) -> u64 {
    chunk.vecs[0].values::<u64>()[pos]
}

/// One chunk's slice of a table function's answer, as a column. The
/// kernel yielded a value per node in row order, so a scan chunk takes
/// its own range of it; the rows the kernel reached nothing for come
/// out invalid, which is the null a projection reads and the false a
/// comparison against it gives.
fn func_vec(col: &FuncCol, base: u64, rows: usize, arena: &mut MorselArena) -> ValueVector {
    let lo = base as usize;
    let mut v = ValueVector::flat_from(arena, PhysType::Int64, &col.values[lo..lo + rows]);
    if col.nullable() {
        let mut valid = Bitmap::new_in(arena, rows, true);
        for (i, &null) in col.null[lo..lo + rows].iter().enumerate() {
            if null {
                valid.clear(i);
            }
        }
        v.validity = Some(valid);
    }
    v
}

/// One row of a pinned level's vector, standing for every row of the
/// level being built. Integers only, which is what the compiler
/// registers a broadcast for. A null end keeps the constant's value and
/// clears its validity: the compare kernel ands validity into its
/// result, so every row of the vector fails the predicate, which is
/// what a comparison against a missing property answers.
fn broadcast(src: &ValueVector, at: usize, len: usize, arena: &mut MorselArena) -> ValueVector {
    let value = match src.encoding {
        VecEncoding::Constant => src.constant_value::<i64>(),
        _ => src.values::<i64>()[at],
    };
    let mut v = ValueVector::constant(arena, src.phys, value, len);
    if !src.is_valid(at) {
        v.validity = Some(Bitmap::new_in(arena, len, false));
    }
    v
}

/// Reads one scalar for the sink: refs on the newest level read at
/// `pos`, refs on earlier levels read their pinned row.
fn scalar(plan: &ExecPlan, set: &ChunkSet, r: ScalarRef, pos: usize) -> Result<Value> {
    let level = r.level();
    let chunk = &set.chunks[level];
    let idx = if level + 1 == set.chunks.len() {
        pos
    } else {
        pinned_pos(chunk)
    };
    // The level an OPTIONAL MATCH bound on a miss has every vector
    // invalid, and a computed column clears validity on the rows it
    // divided by zero. Either way the answer is null, whatever the
    // ref names, and on a vector with no validity at all this is one
    // predictable branch per read.
    let at = match r {
        ScalarRef::Col { vec, .. } => vec,
        ScalarRef::Node { .. } | ScalarRef::RowId { .. } => 0,
    };
    if !chunk.vecs[at].is_valid(idx) {
        return Ok(Value::Null);
    }
    Ok(match r {
        ScalarRef::Node { .. } => Value::Node {
            table: plan.levels[level].table,
            offset: row_at(chunk, idx),
        },
        ScalarRef::RowId { .. } => Value::Int(row_at(chunk, idx) as i64),
        ScalarRef::Col { vec, ty, .. } => match ty {
            zu_query::snapshot::ColType::Int => Value::Int(chunk.vecs[vec].values::<i64>()[idx]),
            zu_query::snapshot::ColType::Str => Value::Str(str_at(&chunk.vecs[vec], idx)?),
        },
    })
}

/// The join key one row probes with, `None` where the row has no key
/// at all. A null never equals anything, so a row whose key column is
/// null matches nothing and the old engine's filter drops it too.
///
/// A key on the newest level is read at `pos`; one on a level below is
/// read at that level's pin, the same rule every other scalar read
/// follows.
fn int_key(set: &ChunkSet, r: ScalarRef, pos: usize) -> Option<u64> {
    let level = r.level();
    let chunk = &set.chunks[level];
    let idx = if level + 1 == set.chunks.len() {
        pos
    } else {
        pinned_pos(chunk)
    };
    match r {
        ScalarRef::Col { vec, .. } => chunk.vecs[vec]
            .is_valid(idx)
            .then(|| chunk.vecs[vec].values::<i64>()[idx] as u64),
        ScalarRef::RowId { .. } => Some(row_at(chunk, idx)),
        ScalarRef::Node { .. } => unreachable!("a node is not a join key"),
    }
}

/// Integer read for aggregate arguments; the compiler admits only
/// integer columns and row ids here.
fn int_scalar(set: &ChunkSet, r: ScalarRef, pos: usize) -> i64 {
    let chunk = &set.chunks[r.level()];
    match r {
        ScalarRef::Col { vec, .. } => chunk.vecs[vec].values::<i64>()[pos],
        ScalarRef::RowId { .. } => row_at(chunk, pos) as i64,
        ScalarRef::Node { .. } => unreachable!("nodes are not integer arguments"),
    }
}

/// What one key ref packs into: a node takes two words, a string a byte
/// range, everything else one word.
fn part_kind(r: ScalarRef) -> PartKind {
    match r {
        ScalarRef::Node { .. } => PartKind::Node,
        ScalarRef::RowId { .. } => PartKind::Int,
        ScalarRef::Col { ty, .. } => match ty {
            zu_query::snapshot::ColType::Int => PartKind::Int,
            zu_query::snapshot::ColType::Str => PartKind::Str,
        },
    }
}

fn key_parts(keys: &[ScalarRef]) -> Vec<PartKind> {
    keys.iter().copied().map(part_kind).collect()
}

/// Packs one key column of the whole vector. A ref on the newest level
/// reads one value per active position; a ref on a pinned level is the
/// same value for every row of the vector, and the fill says so.
fn fill_key_col(
    plan: &ExecPlan,
    set: &ChunkSet,
    r: ScalarRef,
    sel: Option<&[u16]>,
    rows: usize,
    off: usize,
    batch: &mut KeyBatch,
) -> Result<()> {
    let level = r.level();
    let chunk = &set.chunks[level];
    let live = level + 1 == set.chunks.len();
    match r {
        ScalarRef::Node { .. } | ScalarRef::RowId { .. } => {
            // The node's table is the level's table, one word for the
            // whole vector; the row id is the varying half.
            let at = match r {
                ScalarRef::Node { .. } => {
                    batch.fill_word(off, u64::from(plan.levels[level].table));
                    off + 1
                }
                _ => off,
            };
            if live {
                fill_col(batch, at, chunk.vecs[0].values::<u64>(), sel, rows, |v| v);
            } else {
                batch.fill_word(at, row_at(chunk, pinned_pos(chunk)));
            }
        }
        ScalarRef::Col { vec, ty, .. } => match ty {
            zu_query::snapshot::ColType::Int => {
                if live {
                    fill_col(
                        batch,
                        off,
                        chunk.vecs[vec].values::<i64>(),
                        sel,
                        rows,
                        |v| v as u64,
                    );
                } else {
                    let v = chunk.vecs[vec].values::<i64>()[pinned_pos(chunk)];
                    batch.fill_word(off, v as u64);
                }
            }
            zu_query::snapshot::ColType::Str => {
                // The view array and the string buffers are the same
                // for every row of the vector, so they are read once
                // here rather than per row through with_str_bytes.
                let v = &chunk.vecs[vec];
                let views = v.values::<StrView>();
                let bufs = v.str_buffers();
                for row in 0..rows {
                    let idx = match (live, sel) {
                        (false, _) => pinned_pos(chunk),
                        (true, None) => row,
                        (true, Some(pos)) => pos[row] as usize,
                    };
                    let view = views[idx];
                    let bytes = match bufs {
                        Some(b) => view.bytes(b),
                        None => view.inline_bytes(),
                    };
                    if std::str::from_utf8(bytes).is_err() {
                        return Err(invalid("string property is not UTF-8".to_string()));
                    }
                    batch.set_str(row, off, bytes);
                }
            }
        },
    }
    Ok(())
}

/// Writes one column of the key vector out of a column's values, either
/// straight down or through the selection.
fn fill_col<T: Copy>(
    batch: &mut KeyBatch,
    off: usize,
    vals: &[T],
    sel: Option<&[u16]>,
    rows: usize,
    word: impl Fn(T) -> u64,
) {
    let (words, stride) = batch.words_mut();
    let dst = words.iter_mut().skip(off).step_by(stride);
    match sel {
        None => {
            for (w, &v) in dst.zip(&vals[..rows]) {
                *w = word(v);
            }
        }
        Some(pos) => {
            for (w, &p) in dst.zip(pos) {
                *w = word(vals[p as usize]);
            }
        }
    }
}

/// Gathers an aggregate's integer argument for the whole vector, the
/// pinned case being one value repeated.
fn gather_ints(set: &ChunkSet, r: ScalarRef, sel: Option<&[u16]>, rows: usize, out: &mut Vec<i64>) {
    out.clear();
    let level = r.level();
    let chunk = &set.chunks[level];
    if level + 1 != set.chunks.len() {
        out.resize(rows, int_scalar(set, r, pinned_pos(chunk)));
        return;
    }
    match r {
        ScalarRef::Col { vec, .. } => {
            let vals = chunk.vecs[vec].values::<i64>();
            match sel {
                None => out.extend_from_slice(&vals[..rows]),
                Some(pos) => out.extend(pos.iter().map(|&p| vals[p as usize])),
            }
        }
        ScalarRef::RowId { .. } => {
            let ids = chunk.vecs[0].values::<u64>();
            match sel {
                None => out.extend(ids[..rows].iter().map(|&v| v as i64)),
                Some(pos) => out.extend(pos.iter().map(|&p| ids[p as usize] as i64)),
            }
        }
        ScalarRef::Node { .. } => unreachable!("nodes are not integer arguments"),
    }
}

/// Hands the bytes of a string cell to `f`, checked for UTF-8 first so
/// the group table can turn them back into a String without checking
/// again and so a bad property errors on the query the old engine errors
/// on. A short string lives inside the view, which is a value on the
/// stack here, so the bytes cannot outlive the call and every reader
/// takes a closure rather than a slice.
fn with_str_bytes<T>(v: &ValueVector, idx: usize, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
    let view = v.values::<StrView>()[idx];
    let bytes = match v.str_buffers() {
        Some(bufs) => view.bytes(bufs),
        None => view.inline_bytes(),
    };
    match std::str::from_utf8(bytes) {
        Ok(_) => Ok(f(bytes)),
        Err(_) => Err(invalid("string property is not UTF-8".to_string())),
    }
}

fn str_at(v: &ValueVector, idx: usize) -> Result<String> {
    with_str_bytes(v, idx, |b| String::from_utf8_lossy(b).into_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    #[test]
    fn gallop_finds_the_first_index_at_or_after_the_target() {
        let list = [2, 4, 4, 7, 11, 15, 15, 20];
        for from in 0..list.len() {
            for target in 0..25 {
                let want = (from..list.len())
                    .find(|&ix| list[ix] >= target)
                    .unwrap_or(list.len());
                assert_eq!(
                    gallop(&list, target, from),
                    want,
                    "target {target} from {from}"
                );
            }
        }
    }

    #[test]
    fn leapfrog_emits_one_hit_per_seed_occurrence() {
        let mut hits = Vec::new();
        leapfrog(&[2, 2, 3, 7, 9, 12], &[1, 2, 7, 7, 8, 12], &mut hits);
        assert_eq!(hits, [2, 2, 7, 12]);
        hits.clear();
        leapfrog(&[5], &[], &mut hits);
        leapfrog(&[], &[5], &mut hits);
        assert_eq!(hits, []);
    }

    use zu_query::snapshot::{ColId, GroupId, ScanChunk, TableId, ZonePred};
    use zu_vector::{ExprOp, OwnedValue};

    use super::*;
    use crate::compile::{AggSpec, ColSpec, Level, PostSpec};
    use zu_vector::{CmpOp, Program};

    /// One node table, integer column 0, one rel with in-memory CSRs.
    /// Age of row i is picked by the fixture; forks share the Arcs the
    /// way real forks share the block cache.
    #[derive(Clone)]
    struct Mock {
        rows: u64,
        age: Arc<Vec<i64>>,
        fwd: (Arc<Vec<u64>>, Arc<Vec<u64>>),
        bwd: (Arc<Vec<u64>>, Arc<Vec<u64>>),
        forkable: bool,
        /// Answers the threshold with something no caller can reach, so
        /// every list goes the one at a time way a point read does.
        point: bool,
        /// Lists served that way, shared with the forks.
        lists: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Mock {
        fn new(rows: u64, age: impl Fn(u64) -> i64, forkable: bool) -> Mock {
            // i knows i + 1, the last row knows nobody.
            let mut fo = vec![0u64];
            let mut fnb = Vec::new();
            let mut bo = vec![0u64];
            let mut bnb = Vec::new();
            for i in 0..rows {
                if i + 1 < rows {
                    fnb.push(i + 1);
                }
                fo.push(fnb.len() as u64);
                if i > 0 {
                    bnb.push(i - 1);
                }
                bo.push(bnb.len() as u64);
            }
            Mock {
                rows,
                age: Arc::new((0..rows).map(age).collect()),
                fwd: (Arc::new(fo), Arc::new(fnb)),
                bwd: (Arc::new(bo), Arc::new(bnb)),
                forkable,
                point: false,
                lists: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        /// The same graph read the way storage reads one when a group
        /// holds far more edges than the caller asked for.
        fn point(mut self) -> Mock {
            self.point = true;
            self
        }

        fn lists(&self) -> usize {
            self.lists.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn side(&self, dir: Dir) -> &(Arc<Vec<u64>>, Arc<Vec<u64>>) {
            match dir {
                Dir::Fwd => &self.fwd,
                Dir::Bwd => &self.bwd,
            }
        }
    }

    impl Snapshot for Mock {
        fn epoch(&self) -> u64 {
            0
        }

        fn table_rows(&mut self, _table: TableId) -> Result<u64> {
            Ok(self.rows)
        }

        fn resolve_col(
            &mut self,
            _table: TableId,
            name: &str,
        ) -> Result<Option<(ColId, zu_query::snapshot::ColType)>> {
            Ok((name == "age").then_some((0, zu_query::snapshot::ColType::Int)))
        }

        fn scan(
            &mut self,
            _table: TableId,
            chunk: u64,
            cols: &[ColId],
            pred: Option<&ZonePred>,
            arena: &mut MorselArena,
        ) -> Result<Option<ScanChunk>> {
            let base = chunk * SCAN_ROWS as u64;
            if base >= self.rows {
                return Ok(None);
            }
            let n = (self.rows - base).min(SCAN_ROWS as u64) as usize;
            let slice = &self.age[base as usize..base as usize + n];
            if let Some(p) = pred {
                let lo = slice.iter().map(|&v| v as u64).min().unwrap();
                let hi = slice.iter().map(|&v| v as u64).max().unwrap();
                if p.skips(lo, hi) {
                    return Ok(None);
                }
            }
            let columns = cols
                .iter()
                .map(|&c| {
                    assert_eq!(c, 0, "the mock has one column");
                    ValueVector::flat_from(arena, PhysType::Int64, slice)
                })
                .collect();
            Ok(Some(ScanChunk {
                row_base: base,
                rows: n as u32,
                sel: None,
                columns,
            }))
        }

        fn csr(&mut self, _rel: RelId, group: GroupId, dir: Dir) -> Result<CsrPin> {
            assert_eq!(group, 0, "the mock fits one group");
            assert!(!self.point, "a point mock pins nothing");
            let (offsets, neighbors) = self.side(dir).clone();
            Ok(CsrPin { offsets, neighbors })
        }

        fn list_threshold(&mut self, _rel: RelId, _group: GroupId, _dir: Dir) -> Result<usize> {
            Ok(if self.point { usize::MAX } else { 0 })
        }

        fn list_into(
            &mut self,
            _rel: RelId,
            node: u64,
            dir: Dir,
            out: &mut Vec<u64>,
        ) -> Result<()> {
            self.lists
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (offsets, neighbors) = self.side(dir);
            let (lo, hi) = (offsets[node as usize], offsets[node as usize + 1]);
            out.extend_from_slice(&neighbors[lo as usize..hi as usize]);
            Ok(())
        }

        fn gather(
            &mut self,
            _table: TableId,
            col: ColId,
            rows: &[u64],
            arena: &mut MorselArena,
        ) -> Result<ValueVector> {
            assert_eq!(col, 0);
            let vals: Vec<i64> = rows.iter().map(|&r| self.age[r as usize]).collect();
            Ok(ValueVector::flat_from(arena, PhysType::Int64, &vals))
        }

        fn seek_key(&mut self, _table: TableId, key: u64) -> Result<Option<u64>> {
            Ok((key < self.rows).then_some(key))
        }

        fn degree_batch(&mut self, _rel: RelId, nodes: &[u64], dir: Dir) -> Result<u64> {
            let offsets = &self.side(dir).0;
            Ok(nodes
                .iter()
                .map(|&n| offsets[n as usize + 1] - offsets[n as usize])
                .sum())
        }

        fn fork(&self) -> Option<Box<dyn Snapshot + Send>> {
            self.forkable
                .then(|| Box::new(self.clone()) as Box<dyn Snapshot + Send>)
        }
    }

    fn plan(levels: Vec<Level>, ops: Vec<Op>, sink: SinkSpec, columns: &[&str]) -> ExecPlan {
        ExecPlan {
            table: 0,
            source: Source::Scan(None),
            ops,
            sink,
            levels,
            columns: columns.iter().map(|s| s.to_string()).collect(),
            func: None,
        }
    }

    fn bare_level() -> Level {
        Level {
            table: 0,
            cols: Vec::new(),
        }
    }

    fn age_level() -> Level {
        Level {
            table: 0,
            cols: vec![ColSpec::Stored(0, zu_query::snapshot::ColType::Int)],
        }
    }

    /// age > c over chunk vector 1.
    fn gt_prog(c: i64) -> Program {
        Program {
            ops: vec![
                ExprOp::LoadCol { col: 1, dst: 0 },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(c),
                    dst: 1,
                },
                ExprOp::Compare {
                    op: CmpOp::Gt,
                    l: 0,
                    r: 1,
                    dst: 0,
                },
            ],
            regs: 2,
        }
    }

    fn seq() -> Options {
        Options {
            threads: 1,
            ..Options::default()
        }
    }

    #[test]
    fn morsels_cover_rows_and_stay_group_aligned() {
        let rows = u64::from(GROUP_ROWS) * 2 + 5000;
        let morsels = make_morsels(rows, 4);
        let mut expect = 0;
        for &(lo, hi) in &morsels {
            assert_eq!(lo, expect, "morsels tile the scan");
            assert!(hi > lo);
            assert_eq!(lo % SCAN_ROWS as u64, 0, "morsels start on a chunk");
            assert_eq!(
                lo / u64::from(GROUP_ROWS),
                (hi - 1) / u64::from(GROUP_ROWS),
                "a morsel never crosses a group"
            );
            expect = hi;
        }
        assert_eq!(expect, rows);
        assert!(morsels.len() >= 32, "four workers get several morsels each");
    }

    #[test]
    fn morsels_of_a_small_table_are_one() {
        assert_eq!(make_morsels(10, 8), vec![(0, 10)]);
        assert!(make_morsels(0, 8).is_empty());
    }

    #[test]
    fn stop_state_advances_a_contiguous_prefix() {
        let s = StopState::new(Some(10), 3);
        s.morsel_done(1, 5);
        assert!(!s.stopped(), "morsel 0 still open, no prefix yet");
        assert!(
            !s.quota_met(0, 6),
            "a later finished morsel counts only once the prefix reaches it"
        );
        assert!(
            s.quota_met(0, 12),
            "the prefix head alone can close the quota"
        );
        s.morsel_done(0, 6);
        assert!(s.stopped(), "prefix 0..2 holds 11 rows");
    }

    #[test]
    fn counts_every_row() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let p = plan(vec![bare_level()], Vec::new(), SinkSpec::Count, &["n"]);
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(10)]]);
    }

    #[test]
    fn filters_before_counting() {
        let mut snap = Mock::new(10, |i| i as i64 * 2, false);
        let p = plan(
            vec![age_level()],
            vec![Op::Filter { prog: gt_prog(6) }],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        // Ages 8, 10, 12, 14, 16, 18 pass.
        assert_eq!(r.rows, vec![vec![Value::Int(6)]]);
    }

    #[test]
    fn zone_pred_skips_chunks() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let mut p = plan(vec![age_level()], Vec::new(), SinkSpec::Count, &["n"]);
        p.source = Source::Scan(Some(ZonePred {
            col: 0,
            lo: 1000,
            hi: u64::MAX,
        }));
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(0)]], "every chunk zoned out");
    }

    /// A sideways filter over the age column of the driving level.
    fn sip_op(keys: &[u64], slot: usize) -> Op {
        Op::Sip {
            filter: Arc::new(SipFilter::over(keys)),
            key: ScalarRef::Col {
                level: 0,
                vec: 1,
                ty: zu_query::snapshot::ColType::Int,
            },
            slot,
        }
    }

    #[test]
    fn a_sideways_filter_drops_the_rows_that_cannot_match() {
        let mut snap = Mock::new(2000, |i| i as i64, false);
        let keys: Vec<u64> = (0..2000).step_by(4).collect();
        let p = plan(
            vec![age_level()],
            vec![sip_op(&keys, 0)],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(500)]], "one row in four");
    }

    #[test]
    fn the_run_reports_what_it_decided_on_the_way_through() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let mut p = plan(vec![age_level()], Vec::new(), SinkSpec::Count, &["n"]);
        p.source = Source::Scan(Some(ZonePred {
            col: 0,
            lo: 1000,
            hi: u64::MAX,
        }));
        let (_, d) = run(&p, &mut snap, &seq()).unwrap();
        assert!(d.zone_skipped > 0, "the chunks the pred emptied");
        assert!(
            d.render().contains("zone pushdown skipped"),
            "got:\n{}",
            d.render()
        );

        let mut snap = Mock::new(2000, |i| i as i64, false);
        let keys: Vec<u64> = (0..2000).step_by(4).collect();
        let p = plan(
            vec![age_level()],
            vec![sip_op(&keys, 0)],
            SinkSpec::Count,
            &["n"],
        );
        let (_, d) = run(&p, &mut snap, &seq()).unwrap();
        assert_eq!(d.sip.len(), 1, "one entry per filter in the plan");
        assert_eq!(d.sip[0].probes, 2000);
        assert_eq!(d.sip[0].kept, 500);
        assert_eq!(d.sip[0].dropped, 0, "it is rejecting three rows in four");
        assert!(
            d.render().contains("rejected 1500 of 2000 probe(s), 75.0%"),
            "got:\n{}",
            d.render()
        );
    }

    #[test]
    fn a_sideways_filter_that_keeps_nothing_ends_the_vector() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let p = plan(
            vec![age_level()],
            vec![sip_op(&[100, 200], 0)],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(0)]]);
    }

    #[test]
    fn a_sideways_filter_stops_testing_once_it_stops_rejecting() {
        // Keys covering the first eight chunks and nothing after, so
        // the trial window sees every row it tests survive and the
        // operator switches itself off. What comes through after that
        // is rows the filter would have rejected, which is sound
        // because the join behind it still has to match them.
        let rows = 20_000;
        let mut snap = Mock::new(rows, |i| i as i64, false);
        let keys: Vec<u64> = (0..SIP_TRIAL).collect();
        let p = plan(
            vec![age_level()],
            vec![sip_op(&keys, 0)],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(
            r.rows,
            vec![vec![Value::Int(rows as i64)]],
            "still filtering after the trial window"
        );
    }

    #[test]
    fn a_sideways_filter_that_earns_its_keep_stays_on() {
        // The same length of run as the test above, but rejecting
        // enough that the trial leaves it alone.
        let rows = 20_000;
        let mut snap = Mock::new(rows, |i| i as i64, false);
        let keys: Vec<u64> = (0..rows).step_by(2).collect();
        let p = plan(
            vec![age_level()],
            vec![sip_op(&keys, 0)],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(rows as i64 / 2)]]);
    }

    #[test]
    fn a_sideways_filter_composes_with_a_filter_over_the_same_level() {
        // The selection the operator writes is the one the next filter
        // reads, so the two have to compose rather than each start from
        // the whole vector.
        let mut snap = Mock::new(2000, |i| i as i64, false);
        let keys: Vec<u64> = (0..2000).step_by(4).collect();
        let p = plan(
            vec![age_level()],
            vec![sip_op(&keys, 0), Op::Filter { prog: gt_prog(999) }],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        // Multiples of four above 999: 1000, 1004, ... 1996.
        assert_eq!(r.rows, vec![vec![Value::Int(250)]]);
    }

    #[test]
    fn degree_count_matches_expand_count() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let expanded = plan(
            vec![bare_level(), bare_level()],
            vec![Op::Expand {
                rel: 0,
                dirs: Dirs::One(Dir::Fwd),
                from: 0,
                to: 1,
                batch: false,
                close: None,
            }],
            SinkSpec::Count,
            &["n"],
        );
        let fused = plan(
            vec![bare_level(), bare_level()],
            vec![Op::DegreeProduct {
                steps: vec![(0, Dirs::One(Dir::Fwd))],
                from: 0,
            }],
            SinkSpec::Count,
            &["n"],
        );
        let a = run(&expanded, &mut snap, &seq()).unwrap().0;
        let b = run(&fused, &mut snap, &seq()).unwrap().0;
        assert_eq!(a.rows, vec![vec![Value::Int(9)]], "everyone but the last");
        assert_eq!(a.rows, b.rows);
    }

    #[test]
    fn two_hop_walks_through_a_pinned_level() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let p = plan(
            vec![bare_level(), bare_level(), bare_level()],
            vec![
                Op::Expand {
                    rel: 0,
                    dirs: Dirs::One(Dir::Fwd),
                    from: 0,
                    to: 1,
                    batch: false,
                    close: None,
                },
                Op::DegreeProduct {
                    steps: vec![(0, Dirs::One(Dir::Fwd))],
                    from: 1,
                },
            ],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        // 0..8 reach a neighbor; of those neighbors 1..8 step again.
        assert_eq!(r.rows, vec![vec![Value::Int(8)]]);
    }

    #[test]
    fn hub_product_matches_nested_expands() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        // The hub two-hop: count in-degree times out-degree per row,
        // no expand ever runs. Rows 1..9 have one in-neighbor, rows
        // 0..9 one out-neighbor, so 8 rows see both sides.
        let fused = plan(
            vec![bare_level(), bare_level(), bare_level()],
            vec![Op::DegreeProduct {
                steps: vec![(0, Dirs::One(Dir::Bwd)), (0, Dirs::One(Dir::Fwd))],
                from: 0,
            }],
            SinkSpec::Count,
            &["n"],
        );
        let a = run(&fused, &mut snap, &seq()).unwrap().0;
        assert_eq!(a.rows, vec![vec![Value::Int(8)]]);
    }

    #[test]
    fn undirected_walks_both_sides_forward_first() {
        let mut snap = Mock::new(3, |i| i as i64, false);
        let p = plan(
            vec![bare_level(), bare_level()],
            vec![Op::Expand {
                rel: 0,
                dirs: Dirs::Both,
                from: 0,
                to: 1,
                batch: false,
                close: None,
            }],
            SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 1 }],
                post: Vec::new(),
            },
            &["m"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        let got: Vec<Value> = r.rows.into_iter().map(|mut v| v.remove(0)).collect();
        // Row 0 sees 1; row 1 sees 2 then 0; row 2 sees 1.
        assert_eq!(
            got,
            [1, 2, 0, 1].map(Value::Int).to_vec(),
            "forward list lands before the backward list"
        );
    }

    /// Reading a list at a time instead of pinning the group around it
    /// is a storage decision, and the walk above it is not allowed to
    /// notice: same neighbors, same order. What the point path exists
    /// for is the case a pin is ruinous at, one seed of a group holding
    /// millions of edges, and what this checks is that the two agree on
    /// a graph small enough to hold both answers in one assert.
    #[test]
    fn a_point_read_hands_down_what_a_pinned_group_does() {
        let shape = plan(
            vec![bare_level(), bare_level()],
            vec![Op::Expand {
                rel: 0,
                dirs: Dirs::Both,
                from: 0,
                to: 1,
                batch: false,
                close: None,
            }],
            SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 1 }],
                post: Vec::new(),
            },
            &["m"],
        );
        let mut pinned = Mock::new(64, |i| i as i64, false);
        let mut point = Mock::new(64, |i| i as i64, false).point();
        let a = run(&shape, &mut pinned, &seq()).unwrap().0;
        let b = run(&shape, &mut point, &seq()).unwrap().0;
        assert_eq!(a.rows.len(), 126, "63 forward neighbors and 63 backward");
        assert_eq!(b.rows, a.rows, "the point path walked a different graph");
        assert_eq!(pinned.lists(), 0, "a pinned group serves its own lists");
        assert_eq!(
            point.lists(),
            128,
            "one read per source row per direction, empty lists included"
        );
    }

    /// The batched descent is only worth having if it is invisible:
    /// same rows, same order, same count, whether the neighbors go
    /// down one source row at a time or packed into vectors. The
    /// filter is in the plan because it refines a selection over the
    /// packed vector, which is the part the row at a time shape never
    /// exercises.
    #[test]
    fn batching_an_expand_changes_nothing_it_hands_down() {
        let shape = |batch| {
            plan(
                vec![bare_level(), age_level()],
                vec![
                    Op::Expand {
                        rel: 0,
                        dirs: Dirs::Both,
                        from: 0,
                        to: 1,
                        batch,
                        close: None,
                    },
                    Op::Filter { prog: gt_prog(20) },
                ],
                SinkSpec::Rows {
                    items: vec![ScalarRef::RowId { level: 1 }],
                    post: Vec::new(),
                },
                &["m"],
            )
        };
        let mut snap = Mock::new(64, |i| i as i64, false);
        let one = run(&shape(false), &mut snap, &seq()).unwrap().0;
        let packed = run(&shape(true), &mut snap, &seq()).unwrap().0;
        // 43 forward neighbors over 20 and 42 backward ones.
        assert_eq!(one.rows.len(), 85, "the filter kept the wrong rows");
        assert_eq!(packed.rows, one.rows, "batching reordered or dropped rows");
    }

    /// The count sink reads no level, so the multiplicity is the only
    /// thing the source pin still carries and a batched expand has to
    /// keep it at one.
    #[test]
    fn a_batched_expand_still_counts_one_path_per_neighbor() {
        let shape = |batch| {
            plan(
                vec![bare_level(), age_level()],
                vec![
                    Op::Expand {
                        rel: 0,
                        dirs: Dirs::Both,
                        from: 0,
                        to: 1,
                        batch,
                        close: None,
                    },
                    Op::Filter { prog: gt_prog(-1) },
                ],
                SinkSpec::Count,
                &["n"],
            )
        };
        let mut snap = Mock::new(64, |i| i as i64, false);
        let one = run(&shape(false), &mut snap, &seq()).unwrap().0;
        let packed = run(&shape(true), &mut snap, &seq()).unwrap().0;
        assert_eq!(
            one.rows,
            vec![vec![Value::Int(126)]],
            "63 edges, both sides"
        );
        assert_eq!(packed.rows, one.rows, "batching changed the path count");
    }

    #[test]
    fn parallel_rows_come_back_in_scan_order() {
        let rows = 5000;
        let snap = Mock::new(rows, |i| i as i64, true);
        let p = plan(
            vec![bare_level()],
            Vec::new(),
            SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 0 }],
                post: Vec::new(),
            },
            &["n"],
        );
        let opts = Options {
            threads: 4,
            ..Options::default()
        };
        let r = run(&p, &mut snap.clone(), &opts).unwrap().0;
        assert_eq!(r.rows.len(), rows as usize);
        for (i, row) in r.rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64), "stitching keeps scan order");
        }
        let flat = run(&p, &mut snap.clone(), &seq()).unwrap().0;
        assert_eq!(flat.rows, r.rows, "sequential and parallel agree");
    }

    #[test]
    fn limit_stops_early_without_changing_the_answer() {
        let snap = Mock::new(5000, |i| i as i64, true);
        let p = plan(
            vec![bare_level()],
            Vec::new(),
            SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 0 }],
                post: vec![PostSpec::Skip(3), PostSpec::Limit(4)],
            },
            &["n"],
        );
        for threads in [1, 4] {
            let opts = Options {
                threads,
                ..Options::default()
            };
            let r = run(&p, &mut snap.clone(), &opts).unwrap().0;
            let got: Vec<Value> = r.rows.into_iter().map(|mut v| v.remove(0)).collect();
            assert_eq!(got, [3, 4, 5, 6].map(Value::Int).to_vec());
        }
    }

    #[test]
    fn keyed_groups_come_back_sorted() {
        let snap = Mock::new(10, |i| (i % 3) as i64, true);
        let p = plan(
            vec![age_level()],
            Vec::new(),
            SinkSpec::Agg {
                item_agg: vec![false, true],
                keys: vec![ScalarRef::Col {
                    level: 0,
                    vec: 1,
                    ty: zu_query::snapshot::ColType::Int,
                }],
                aggs: vec![AggSpec::CountStar],
                post: Vec::new(),
            },
            &["age", "n"],
        );
        for threads in [1, 4] {
            let opts = Options {
                threads,
                ..Options::default()
            };
            let r = run(&p, &mut snap.clone(), &opts).unwrap().0;
            assert_eq!(
                r.rows,
                vec![
                    vec![Value::Int(0), Value::Int(4)],
                    vec![Value::Int(1), Value::Int(3)],
                    vec![Value::Int(2), Value::Int(3)],
                ]
            );
        }
    }

    #[test]
    fn bare_aggregates_over_nothing_get_the_default_group() {
        let mut snap = Mock::new(0, |_| 0, false);
        let p = plan(
            vec![age_level()],
            Vec::new(),
            SinkSpec::Agg {
                item_agg: vec![true, true],
                keys: Vec::new(),
                aggs: vec![
                    AggSpec::Sum(ScalarRef::Col {
                        level: 0,
                        vec: 1,
                        ty: zu_query::snapshot::ColType::Int,
                    }),
                    AggSpec::Min(ScalarRef::Col {
                        level: 0,
                        vec: 1,
                        ty: zu_query::snapshot::ColType::Int,
                    }),
                ],
                post: Vec::new(),
            },
            &["s", "m"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap().0;
        // The old engine emits sum() of nothing as 0 and min() as null.
        assert_eq!(r.rows, vec![vec![Value::Int(0), Value::Null]]);
    }

    #[test]
    fn unforkable_snapshot_still_answers() {
        let mut snap = Mock::new(3000, |i| i as i64, false);
        let p = plan(vec![bare_level()], Vec::new(), SinkSpec::Count, &["n"]);
        let opts = Options {
            threads: 4,
            ..Options::default()
        };
        let r = run(&p, &mut snap, &opts).unwrap().0;
        assert_eq!(r.rows, vec![vec![Value::Int(3000)]]);
    }
}
