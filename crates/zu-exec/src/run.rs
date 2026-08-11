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
use zu_query::snapshot::{ColId, CsrPin, Dir, RelId, SCAN_ROWS, Snapshot};
use zu_vector::{ChunkSet, DataChunk, MorselArena, PhysType, SelVector, StrView, ValueVector};

use crate::compile::{AggSpec, Dirs, ExecPlan, Op, PostSpec, ScalarRef, SinkSpec};
use crate::group::{GroupTable, KeyBatch, PartKind};
use crate::pool;
use crate::sink::{self, Acc, SinkState};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// Runs a compiled pipeline to completion.
pub(crate) fn run(
    plan: &ExecPlan,
    snap: &mut dyn Snapshot,
    options: &Options,
) -> Result<QueryResult> {
    let total_rows = snap.table_rows(plan.table)?;
    let threads = match options.threads {
        // A scan under one storage group is a handful of morsels;
        // forking snapshots and spawning workers costs more than the
        // scan, so auto stays sequential and only an explicit thread
        // count forces the parallel path.
        0 if total_rows <= u64::from(GROUP_ROWS) => 1,
        0 => std::thread::available_parallelism().map_or(1, |n| n.get().min(8)),
        n => n,
    };
    let morsels = make_morsels(total_rows, threads.max(1));
    let quota = match &plan.sink {
        SinkSpec::Rows { post, .. } => quota_of(post),
        _ => None,
    };
    let stop = StopState::new(quota, morsels.len());
    let claim = AtomicUsize::new(0);

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
    let slots: Vec<Mutex<Option<Result<SinkState>>>> =
        forks.iter().map(|_| Mutex::new(None)).collect();
    let main = {
        let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = forks
            .into_iter()
            .zip(&slots)
            .map(|(f, slot)| {
                let (stop, claim, morsels) = (&stop, &claim, morsels.as_slice());
                Box::new(move || {
                    let mut w = Worker::new(plan, SnapHandle::Fork(f), stop);
                    let res = w.work(morsels, claim).map(|()| w.sink);
                    *slot.lock().unwrap() = Some(res);
                }) as Box<dyn FnOnce() + Send + '_>
            })
            .collect();
        let pending = pool::submit(jobs);
        let mut w = Worker::new(plan, SnapHandle::Main(snap), &stop);
        let main = w.work(&morsels, &claim).map(|()| w.sink);
        pending.wait();
        main
    };
    let mut out = Vec::with_capacity(slots.len() + 1);
    let mut first_err = None;
    for res in std::iter::once(main).chain(slots.into_iter().map(|slot| {
        slot.into_inner()
            .unwrap()
            .unwrap_or_else(|| Err(invalid("executor worker panicked".into())))
    })) {
        match res {
            Ok(p) => out.push(p),
            Err(e) => first_err = first_err.or(Some(e)),
        }
    }
    let partials = match first_err {
        Some(e) => return Err(e),
        None => out,
    };

    match &plan.sink {
        SinkSpec::Count => {
            let total: i64 = partials.iter().map(|p| p.count).sum();
            Ok(QueryResult {
                columns: plan.columns.clone(),
                rows: vec![vec![Value::Int(total)]],
            })
        }
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
    }
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
    /// Per-row degree and running product scratch for hub counts.
    deg: Vec<u64>,
    prod: Vec<u64>,
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
    /// Rows emitted in the morsel in flight, for the quota check.
    local_rows: u64,
}

impl<'a> Worker<'a> {
    fn new(plan: &'a ExecPlan, snap: SnapHandle<'a>, stop: &'a StopState) -> Self {
        Worker {
            plan,
            snap,
            arena: MorselArena::new(),
            pins: HashMap::new(),
            scan_cols: plan.levels[0].cols.iter().map(|&(id, _)| id).collect(),
            scratch: Vec::new(),
            neigh: Vec::new(),
            deg: Vec::new(),
            prod: Vec::new(),
            idx_pool: Vec::new(),
            row_pool: Vec::new(),
            batch: KeyBatch::default(),
            gids: Vec::new(),
            args: Vec::new(),
            sink: SinkState::default(),
            stop,
            local_rows: 0,
        }
    }

    fn work(&mut self, morsels: &[(u64, u64)], claim: &AtomicUsize) -> Result<()> {
        loop {
            let m = claim.fetch_add(1, Ordering::Relaxed);
            if m >= morsels.len() || self.stop.stopped() {
                return Ok(());
            }
            if let Err(e) = self.run_morsel(m, morsels[m]) {
                self.stop.abort();
                return Err(e);
            }
        }
    }

    fn run_morsel(&mut self, idx: usize, (lo, hi): (u64, u64)) -> Result<()> {
        self.arena.reset();
        self.local_rows = 0;
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
                plan.pred.as_ref(),
                &mut self.arena,
            )?
            else {
                // A zone-excluded chunk; nothing survives it.
                continue;
            };
            let mut vecs = Vec::with_capacity(1 + sc.columns.len());
            let mut ids =
                ValueVector::flat_uninit(&mut self.arena, PhysType::Int64, sc.rows as usize);
            for (i, slot) in ids.values_mut::<u64>().iter_mut().enumerate() {
                *slot = sc.row_base + i as u64;
            }
            vecs.push(ids);
            vecs.extend(sc.columns);
            let level0 = DataChunk {
                vecs,
                sel: sc.sel,
                count: sc.rows,
                cur: None,
            };
            if level0.active_count() == 0 {
                continue;
            }
            let mut set = ChunkSet::new(vec![level0]);
            self.run_ops(&plan.ops, &mut set)?;
            if rows_sink && self.stop.quota_met(idx, self.local_rows) {
                break;
            }
        }
        if rows_sink {
            let batch = std::mem::take(&mut self.sink.rows);
            self.sink.batches.push((idx, batch));
        }
        self.stop.morsel_done(idx, self.local_rows);
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
            Op::Expand { rel, dirs, to, .. } => {
                if let [Op::DegreeProduct { steps }] = rest {
                    return self.expand_degree(*rel, *dirs, steps, set);
                }
                self.expand(*rel, *dirs, *to, rest, set)
            }
            Op::DegreeProduct { steps } => {
                self.collect_rows(set.chunks.last().expect("a level under the count"));
                let rows = std::mem::take(&mut self.scratch);
                let sum = self.product_sum(steps, &rows);
                self.scratch = rows;
                self.sink.count += sum? as i64;
                Ok(())
            }
        }
    }

    fn expand(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        to: usize,
        rest: &[Op],
        set: &mut ChunkSet,
    ) -> Result<()> {
        let src = set.chunks.len() - 1;
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
        let mut result = Ok(());
        'srcs: for (&phys, &row) in idxs.iter().zip(&rows) {
            set.chunks[src].cur = Some(phys);
            for dir in sides(dirs) {
                let pin = match self.pin(rel, dir, row) {
                    Ok(p) => p,
                    Err(e) => {
                        result = Err(e);
                        break 'srcs;
                    }
                };
                let list = pin.list((row % u64::from(GROUP_ROWS)) as usize);
                for part in list.chunks(zu_vector::VECTOR_SIZE) {
                    let chunk = match self.make_level(to, part) {
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
            }
            if self.stop.stopped() {
                break;
            }
        }
        set.chunks[src].cur = None;
        self.idx_pool.push(idxs);
        self.row_pool.push(rows);
        result
    }

    fn pin(&mut self, rel: RelId, dir: Dir, row: u64) -> Result<CsrPin> {
        let group = (row / u64::from(GROUP_ROWS)) as u32;
        let key = (rel, matches!(dir, Dir::Bwd), group);
        if let Some(p) = self.pins.get(&key) {
            return Ok(p.clone());
        }
        let p = self.snap.get().csr(rel, group, dir)?;
        self.pins.insert(key, p.clone());
        Ok(p)
    }

    /// Builds one level chunk from a neighbor slice: row ids plus every
    /// property column the pipeline reads on this level.
    fn make_level(&mut self, level: usize, part: &[u64]) -> Result<DataChunk> {
        let info = &self.plan.levels[level];
        let mut vecs = Vec::with_capacity(1 + info.cols.len());
        vecs.push(ValueVector::flat_from(
            &mut self.arena,
            PhysType::Int64,
            part,
        ));
        for &(col, _) in &info.cols {
            vecs.push(
                self.snap
                    .get()
                    .gather(info.table, col, part, &mut self.arena)?,
            );
        }
        Ok(DataChunk::new(vecs, part.len() as u32))
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
    /// instead of a pipeline descent. A morsel never crosses a group,
    /// so one pin per direction covers every source row in the chunk.
    fn expand_degree(
        &mut self,
        rel: RelId,
        dirs: Dirs,
        steps: &[(RelId, Dirs)],
        set: &ChunkSet,
    ) -> Result<()> {
        self.collect_rows(set.chunks.last().expect("a level under the expand"));
        let Some(&first) = self.scratch.first() else {
            return Ok(());
        };
        self.neigh.clear();
        for dir in sides(dirs) {
            let pin = self.pin(rel, dir, first)?;
            for &row in &self.scratch {
                self.neigh
                    .extend_from_slice(pin.list((row % u64::from(GROUP_ROWS)) as usize));
            }
        }
        let neigh = std::mem::take(&mut self.neigh);
        let sum = self.product_sum(steps, &neigh);
        self.neigh = neigh;
        self.sink.count += sum? as i64;
        Ok(())
    }

    /// Sum over `rows` of each row's per-step degree product, offsets
    /// only. Every level above the one the rows came from is pinned
    /// here, so each row counts exactly once. One step is a plain
    /// degree sum and stays on the bulk `degree_batch` read; several
    /// steps read per-row degrees and multiply.
    fn product_sum(&mut self, steps: &[(RelId, Dirs)], rows: &[u64]) -> Result<u64> {
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
        if rows == 0 {
            return Ok(());
        }
        let table = self
            .sink
            .groups
            .get_or_insert_with(|| GroupTable::new(key_parts(keys), aggs.len()));
        self.batch.reset(table.stride(), rows);
        let mut off = 0;
        for &r in keys {
            fill_key_col(self.plan, set, r, sel, rows, off, &mut self.batch)?;
            off += part_kind(r).words();
        }
        table.probe(&self.batch, aggs, &mut self.gids);
        let n = aggs.len();
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
                for &g in &self.gids {
                    accs[g as usize * n + j].add_star(1);
                }
                continue;
            }
            let r = spec.arg().expect("a non counting aggregate has an argument");
            gather_ints(set, r, sel, rows, &mut self.args);
            let accs = table.accs_mut();
            for (&g, &v) in self.gids.iter().zip(&self.args) {
                accs[g as usize * n + j].add_int(v, 1)?;
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
            SinkSpec::Rows { items, .. } => {
                let last = set.chunks.len() - 1;
                for pos in active_positions(&set.chunks[last]) {
                    let mut row = Vec::with_capacity(items.len());
                    for &r in items {
                        row.push(scalar(self.plan, set, r, pos)?);
                    }
                    self.sink.rows.push(row);
                    self.local_rows += 1;
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
                    fill_col(batch, off, chunk.vecs[vec].values::<i64>(), sel, rows, |v| {
                        v as u64
                    });
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

    use zu_query::snapshot::{ColId, GroupId, ScanChunk, TableId, ZonePred};
    use zu_vector::{ExprOp, OwnedValue};

    use super::*;
    use crate::compile::{AggSpec, Level, PostSpec};
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
            }
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
            let (offsets, neighbors) = self.side(dir).clone();
            Ok(CsrPin { offsets, neighbors })
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

        fn lookup_pk(&mut self, _rel: RelId, _key: u64) -> Result<Option<u64>> {
            Ok(None)
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
            pred: None,
            ops,
            sink,
            levels,
            columns: columns.iter().map(|s| s.to_string()).collect(),
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
            cols: vec![(0, zu_query::snapshot::ColType::Int)],
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
        let r = run(&p, &mut snap, &seq()).unwrap();
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
        let r = run(&p, &mut snap, &seq()).unwrap();
        // Ages 8, 10, 12, 14, 16, 18 pass.
        assert_eq!(r.rows, vec![vec![Value::Int(6)]]);
    }

    #[test]
    fn zone_pred_skips_chunks() {
        let mut snap = Mock::new(10, |i| i as i64, false);
        let mut p = plan(vec![age_level()], Vec::new(), SinkSpec::Count, &["n"]);
        p.pred = Some(ZonePred {
            col: 0,
            lo: 1000,
            hi: u64::MAX,
        });
        let r = run(&p, &mut snap, &seq()).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int(0)]], "every chunk zoned out");
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
            }],
            SinkSpec::Count,
            &["n"],
        );
        let fused = plan(
            vec![bare_level(), bare_level()],
            vec![Op::DegreeProduct {
                steps: vec![(0, Dirs::One(Dir::Fwd))],
            }],
            SinkSpec::Count,
            &["n"],
        );
        let a = run(&expanded, &mut snap, &seq()).unwrap();
        let b = run(&fused, &mut snap, &seq()).unwrap();
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
                },
                Op::DegreeProduct {
                    steps: vec![(0, Dirs::One(Dir::Fwd))],
                },
            ],
            SinkSpec::Count,
            &["n"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap();
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
            }],
            SinkSpec::Count,
            &["n"],
        );
        let a = run(&fused, &mut snap, &seq()).unwrap();
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
            }],
            SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 1 }],
                post: Vec::new(),
            },
            &["m"],
        );
        let r = run(&p, &mut snap, &seq()).unwrap();
        let got: Vec<Value> = r.rows.into_iter().map(|mut v| v.remove(0)).collect();
        // Row 0 sees 1; row 1 sees 2 then 0; row 2 sees 1.
        assert_eq!(
            got,
            [1, 2, 0, 1].map(Value::Int).to_vec(),
            "forward list lands before the backward list"
        );
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
        let r = run(&p, &mut snap.clone(), &opts).unwrap();
        assert_eq!(r.rows.len(), rows as usize);
        for (i, row) in r.rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64), "stitching keeps scan order");
        }
        let flat = run(&p, &mut snap.clone(), &seq()).unwrap();
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
            let r = run(&p, &mut snap.clone(), &opts).unwrap();
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
            let r = run(&p, &mut snap.clone(), &opts).unwrap();
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
        let r = run(&p, &mut snap, &seq()).unwrap();
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
        let r = run(&p, &mut snap, &opts).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int(3000)]]);
    }
}
