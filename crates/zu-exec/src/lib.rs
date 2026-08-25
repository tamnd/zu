//! Push-based morsel-parallel pipeline executor (Spec/2064g/perf/03).
//!
//! The old executor in zu-query pulls one chunk at a time through a
//! stage of operators and materializes every intermediate row set. This
//! crate runs the same logical plans the other way around: a compiler
//! turns the plan into one pipeline of push operators over factorized
//! [`ChunkSet`]s, a scheduler splits the driving scan into group-aligned
//! morsels, and workers push each morsel through the whole pipeline into
//! a thread-local sink partial, all reads going through the vectorized
//! [`Snapshot`] surface instead of the scalar Graph trait.
//!
//! The entry point is [`try_execute`], and the contract during the
//! migration is deliberate: it returns `Ok(None)` for any plan shape it
//! does not cover yet, and the caller falls back to the old executor.
//! That keeps the old engine as the differential oracle while coverage
//! grows, exactly the rollout docs/perf 03 section 6 describes. What it
//! does cover it must answer bit-for-bit like the old engine, including
//! row order, group order, and error cases; the parity suite in the zu
//! crate holds it to that.
//!
//! [`ChunkSet`]: zu_vector::ChunkSet
//! [`Snapshot`]: zu_query::snapshot::Snapshot

mod columns;
mod compile;
pub mod decide;
mod group;
pub mod join;
mod pool;
mod run;
mod sink;
pub mod sip;

use zu_common::Result;
use zu_query::binder::{BoundQuery, Schema};
use zu_query::exec::{Options, QueryResult, Sip, Streaming, Value, Wcoj};
use zu_query::plan::LogicalPlan;
use zu_query::snapshot::Snapshot;

use crate::compile::{ExecPlan, PostSpec, SinkSpec};

/// The physical plan one statement last compiled to, kept so that the
/// next run of that same statement does not compile it again.
///
/// Compiling is a fifth of the executor's time on a warm point read
/// and near half of the times it asks the allocator for memory, and
/// none of it is work about the row the caller asked for: the same
/// text against the same schema at the same epoch comes to the same
/// pipeline every time, and the only thing that differs is the
/// parameters, which the plan carries holes for.
///
/// One entry, because the caller holds one of these per compiled
/// statement and a statement has one physical plan. What the entry
/// remembers besides the plan is everything outside the text that the
/// compile read: the epoch and the two options that steer it. Text and
/// schema are the caller's to keep, since a cache of theirs is what
/// handed the statement over.
///
/// The epoch is not there for the rows. A plan is a shape and the rows
/// come out of the snapshot the run is handed, so a plan built before
/// a write and run after it reads what the write left. It is there for
/// the column and table ids the compile resolved, which are the one
/// thing on a plan that means something only against the catalog it
/// was resolved against. A caller that empties its own cache when the
/// epoch moves never reaches this check, and zu's session is such a
/// caller; it is here so that being right does not depend on that.
#[derive(Default)]
pub struct PlanCache {
    entry: Option<Entry>,
}

struct Entry {
    plan: ExecPlan,
    epoch: u64,
    wcoj: Wcoj,
    sip: Sip,
}

impl PlanCache {
    /// An empty cache.
    pub fn new() -> PlanCache {
        PlanCache::default()
    }

    /// Forgets whatever is held, which is what a caller does when the
    /// statement this belongs to is no longer the one it holds.
    pub fn clear(&mut self) {
        self.entry = None;
    }

    /// The held plan stamped with `params`, if it was compiled for
    /// this epoch and these options and its holes take them.
    fn take(&mut self, epoch: u64, options: &Options, params: &[Value]) -> Option<ExecPlan> {
        let entry = self.entry.take()?;
        if entry.epoch != epoch || entry.wcoj != options.wcoj || entry.sip != options.sip {
            return None;
        }
        let mut plan = entry.plan;
        plan.restamp(params).then_some(plan)
    }

    /// Holds `plan` for the next run, if it is one that can be reused.
    fn put(&mut self, plan: ExecPlan, epoch: u64, options: &Options) {
        if plan.reuse.is_none() {
            return;
        }
        self.entry = Some(Entry {
            plan,
            epoch,
            wcoj: options.wcoj,
            sip: options.sip,
        });
    }
}

/// [`try_execute`] reusing the physical plan `cache` holds when it
/// covers this run, and refilling it when it does not.
///
/// The answer is the same either way. A hit skips the compile and
/// nothing else: the plan a hit produces is the plan a compile would
/// have produced, which is what the reuse record is for, and the
/// parity suite runs both paths against the old engine.
pub fn try_execute_cached(
    cache: &mut PlanCache,
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    params: &[Value],
    options: &Options,
) -> Result<Option<QueryResult>> {
    if options.flat {
        return Ok(None);
    }
    let epoch = snap.epoch();
    let exec_plan = match cache.take(epoch, options, params) {
        Some(plan) => plan,
        None => match compile::compile(plan, query, schema, snap, params, options)? {
            Some(plan) => plan,
            None => return Ok(None),
        },
    };
    let out = run::run(&exec_plan, snap, options).map(|(rows, _)| Some(rows));
    cache.put(exec_plan, epoch, options);
    out
}

/// Runs an optimized plan through the pipeline executor when the plan
/// is a shape it supports, `Ok(None)` when the caller should use the
/// old executor instead. `options.flat` always returns `None`: flat
/// runs are the sequential oracle and stay on the old engine.
pub fn try_execute(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    params: &[Value],
    options: &Options,
) -> Result<Option<QueryResult>> {
    if options.flat {
        return Ok(None);
    }
    let Some(exec_plan) = compile::compile(plan, query, schema, snap, params, options)? else {
        return Ok(None);
    };
    run::run(&exec_plan, snap, options).map(|(rows, _)| Some(rows))
}

/// Runs a plan through the pipeline executor handing its rows to `st`
/// in batches as they are made, `Ok(false)` when this is not a plan it
/// can stream and the caller should stream it some other way.
///
/// Two things have to hold before it says yes. The pipeline has to
/// cover the plan at all, which is [`try_execute`]'s own question, and
/// the answer has to be rows in scan order: an aggregate has no row
/// until its groups close, and ORDER BY and DISTINCT have none until
/// the last one is read, so those arrive whole through the buffered
/// entry point and are cut into batches afterwards. SKIP and LIMIT do
/// stream, spent across the batches by the handoff.
pub fn try_execute_streaming(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    params: &[Value],
    options: &Options,
    st: &mut Streaming,
) -> Result<bool> {
    if options.flat {
        return Ok(false);
    }
    let Some(exec_plan) = compile::compile(plan, query, schema, snap, params, options)? else {
        return Ok(false);
    };
    let SinkSpec::Rows { post, .. } = &exec_plan.sink else {
        return Ok(false);
    };
    if post
        .iter()
        .any(|p| matches!(p, PostSpec::Distinct | PostSpec::Sort(_)))
    {
        return Ok(false);
    }
    run::run_streamed(&exec_plan, snap, options, st)?;
    Ok(true)
}

/// The same run handing back the decisions it made as well, which is
/// what EXPLAIN ANALYZE prints under the plan. Every run keeps the
/// record, since it is a handful of counters a worker already had to
/// have; this entry point is only about whether the caller is handed
/// it, and [`try_execute`] drops it because a query run for its answer
/// has nothing to say about it.
pub fn try_execute_profiled(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    params: &[Value],
    options: &Options,
) -> Result<Option<(QueryResult, decide::Decisions)>> {
    if options.flat {
        return Ok(None);
    }
    let Some(exec_plan) = compile::compile(plan, query, schema, snap, params, options)? else {
        return Ok(None);
    };
    run::run(&exec_plan, snap, options).map(Some)
}
