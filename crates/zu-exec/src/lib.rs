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
use zu_query::exec::{Options, QueryResult, Value};
use zu_query::plan::LogicalPlan;
use zu_query::snapshot::Snapshot;

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
