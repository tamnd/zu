//! Plan compiler: one supported LogicalPlan chain becomes one push
//! pipeline, anything else becomes `None` and the caller falls back to
//! the old executor.
//!
//! The supported shape today is the linear read pipeline: a single
//! non-optional node scan, filters, single-hop expands that walk off
//! the newest level or off a level below it, and one final Project or
//! Aggregate with its absorbed Distinct, Sort, Skip, and Limit. A hop
//! off a lower level is the second pattern branch, the shape two
//! patterns sharing a variable compile to, and it pairs every row of
//! the newest level with the whole of the pinned one. Everything the old
//! executor also covers, variable-length expands, optional groups,
//! closing joins, unwind, table functions, and rel values, falls back.
//! The bar for anything compiled here is exact old-engine output:
//! same rows, same order, same errors on overflow.

use std::collections::HashMap;
use std::sync::Arc;

use zu_common::Result;
use zu_query::ast::{BinaryOp, Literal, RelDirection, SortKey};
use zu_query::binder::{BoundClause, BoundExpr, BoundItem, BoundQuery, Func, Schema, TableFunc};
use zu_query::exec::{Options, Sip, Value, Wcoj};
use zu_query::plan::LogicalPlan;
use zu_query::snapshot::{ColId, ColType, Dir, FuncCol, RelId, Snapshot, TableId, ZonePred};
use zu_vector::{BinOp, CmpOp, ExprOp, MorselArena, OwnedValue, PhysType, Program, Reg};

use crate::join::JoinTable;
use crate::sip::SipFilter;

/// One compiled pipeline over one driving scan.
pub(crate) struct ExecPlan {
    pub table: TableId,
    pub source: Source,
    pub ops: Vec<Op>,
    pub sink: SinkSpec,
    pub levels: Vec<Level>,
    pub columns: Vec<String>,
    /// What a leading CALL yielded, one value per node of the domain.
    /// The kernel ran once while this plan was compiled, so every
    /// worker reads the same answer and none of them runs it again.
    pub func: Option<FuncCol>,
}

/// Where level 0 comes from.
pub(crate) enum Source {
    /// The driving scan, carrying any zone pushdown a level 0 filter
    /// gave up.
    Scan(Option<ZonePred>),
    /// The primary-key seek an `{id: k}` predicate folds into: one row
    /// or none, and no scan at all.
    Seek(u64),
    /// The batch of seeks a leading UNWIND folds into (docs/07): the
    /// list is known before the query runs, so the keys are resolved a
    /// vector at a time and level 0 is built out of the rows they hit,
    /// in list order, with a key that hits nothing dropping its row.
    Seeks(Vec<u64>),
}

impl ExecPlan {
    /// The zone pushdown the scan runs with; a seek has no chunks to
    /// skip.
    pub fn zone(&self) -> Option<&ZonePred> {
        match &self.source {
            Source::Scan(pred) => pred.as_ref(),
            Source::Seek(_) | Source::Seeks(_) => None,
        }
    }
}

/// One factorization level: the scan at index 0, one per expand after.
pub(crate) struct Level {
    pub table: TableId,
    /// Columns this level materializes; entry i lives at chunk vector
    /// i + 1, vector 0 is always the row id.
    pub cols: Vec<ColSpec>,
}

/// One column of a level's chunk: a stored property gathered from the
/// snapshot, or a value computed from the columns registered before it.
///
/// A computed column is the projection port (perf/05 section 5): the
/// expression compiles once into a register program and runs over the
/// whole vector where the level is built, so the sink and everything
/// above it read it as an ordinary column. A program only ever loads
/// columns registered ahead of it, which is what makes one pass over
/// this list enough to build the chunk.
pub(crate) enum ColSpec {
    Stored(ColId, ColType),
    Computed(Program),
    /// A value read off a level below, standing for every row of this
    /// one. A correlated predicate like `a.id < b.id` compares a column
    /// of the level it runs on against a level the pipeline pinned on
    /// the way here, and that pinned end is one value for the whole
    /// vector, so it enters the chunk as a constant column and the
    /// program loads it like any other. `from` is the level below,
    /// `vec` the chunk vector position to read there.
    Outer {
        from: usize,
        vec: usize,
    },
    /// The key that found each row of a batch of seeks, standing for
    /// the UNWIND variable the batch came from. The seek already knows
    /// it, so the variable is answered out of the column the source
    /// fills rather than gathered back off the row it found.
    Key,
    /// The value a table function yielded, read out of the plan's
    /// answer by row id. The kernel already holds one value per node
    /// of the domain, so this is a copy where a stored column would
    /// have been a decode.
    Func,
}

/// Traversal sides of one expand: `Both` is an undirected step over a
/// self-referencing rel, forward list first, matching the old engine's
/// emission order.
#[derive(Clone, Copy)]
pub(crate) enum Dirs {
    One(Dir),
    Both,
}

/// A semijoin folded into the expand that produces the rows it judges
/// (perf/13 section 1): the mask a join build passes sideways, in the
/// one shape the compiled pipeline can produce it today. The probe
/// side's list is the filter, the expand is the consumer, and the rows
/// it drops never reach the gather that would have read their columns.
#[derive(Clone, Copy)]
pub(crate) struct Close {
    pub rel: RelId,
    pub dirs: Dirs,
    /// The level holding the end that stays fixed, pinned for the whole
    /// expand because it sits below the level the expand walks off.
    pub probe_level: usize,
}

pub(crate) enum Op {
    /// Refine the newest level's selection by a predicate program.
    Filter { prog: Program },
    /// Pin each active row of level `from` and push its neighbor list
    /// as level `to`. The walk records any source level the plan
    /// names; validation at the end only keeps plans where `from` is
    /// the newest level at the op's position, everything else either
    /// fuses into a `DegreeProduct` or falls back.
    Expand {
        rel: RelId,
        dirs: Dirs,
        from: usize,
        to: usize,
        /// Whether the descent may carry whole vectors instead of one
        /// source row's neighbors. Set by `batch_expands` once the
        /// pipeline is final, false while the plan is still growing.
        batch: bool,
        /// A semijoin that judged this expand's rows one operator later
        /// and now judges them as they are emitted. Set by
        /// `fuse_closes`, None while the plan is still growing.
        close: Option<Close>,
    },
    /// A second pattern branch: a hop off a level the pipeline has
    /// already walked past, which is what a query writes when two
    /// patterns share a variable and read the far end of both.
    ///
    /// The source level sits below the newest one, so it is pinned for
    /// the whole op and its list is read once. What the newest level
    /// contributes is multiplicity: every one of its rows pairs with
    /// every neighbor of the pinned row, so the newest level is pinned
    /// a row at a time and the same list descends under each.
    Branch {
        rel: RelId,
        dirs: Dirs,
        from: usize,
        to: usize,
    },
    /// The WCOJ close (docs/07 section 4, perf/05 section 3): the
    /// expand that would build the closing node and the probe back
    /// into it are one intersection of two sorted neighbor lists. The
    /// seed list hangs off the newest level, the probe list off a
    /// level pinned above it, so a wedge closes in one leapfrog walk
    /// instead of a storage probe per candidate.
    ///
    /// An undirected end over a self-referencing rel reads both stored
    /// lists. On the probe side the two are unioned once for the
    /// vector; on the seed side each is walked in turn, which is the
    /// old engine's rule and keeps a two way edge counted twice.
    Intersect {
        seed: (RelId, Dirs),
        probe: (RelId, Dirs),
        /// The level holding the wedge's far end, always pinned when
        /// the op runs.
        probe_level: usize,
        to: usize,
    },
    /// The binary close (perf/05 section 6): both ends of the edge are
    /// already bound, so nothing is built and the newest level only
    /// loses the rows with no edge back to the pinned end. One list is
    /// read for the whole vector and every row is galloped into it.
    Semi {
        rel: RelId,
        dirs: Dirs,
        /// The level holding the end that stays fixed for the vector.
        probe_level: usize,
    },
    /// Top of an OPTIONAL MATCH bracket (docs/07): the `len` ops above
    /// this one are the group, the last of them being `OptionalHit`,
    /// and everything past that is the pipeline the group feeds. The
    /// group runs once per row of the level below it, with that row
    /// pinned, which is what makes a miss a per-row fact. A row the
    /// group matched nothing for still goes on, with `level` bound to
    /// one null row.
    Optional { len: usize, level: usize },
    /// Bottom of the bracket, reached only when the group produced a
    /// row. Setting the flag is all it does; the pipeline below runs
    /// off it either way.
    OptionalHit,
    /// Terminal fusion of expands feeding a bare count: each active row
    /// of level `from` contributes the product of its per-step degrees,
    /// read off the CSR offsets alone. One step is the plain
    /// expand-then-count fusion; several steps are a hub plan, expands
    /// fanning out of one level with nothing reading the far ends.
    ///
    /// `from` is the newest level when the fused expands were the last
    /// thing in the pipeline, and a level below it when they were not,
    /// which is the hop off a hub whose other pattern is still walked.
    /// A level below the newest one is pinned while this runs, so its
    /// product is one number and every row the sink sees carries it.
    DegreeProduct {
        steps: Vec<(RelId, Dirs)>,
        from: usize,
    },
    /// The value join (perf/05 section 2): a second pattern that shares
    /// no variable with the first and is tied to it by an equality
    /// instead, which is what a query writes when the edge between two
    /// node kinds is a property rather than a rel.
    ///
    /// The pattern's table is the build side. It is read once while the
    /// plan is compiled, into a hash table every worker then shares, so
    /// the rows a key matched are a slice of one buffer and a probe
    /// that matches nothing costs one directory word. The probe is one
    /// lookup per row of whatever level `key` reads, and the rows it
    /// matched become level `to` exactly the way an expand's neighbors
    /// do, in build order, which is the row order the old engine's
    /// nested loop over the same two patterns produces.
    Join {
        table: Arc<JoinTable>,
        key: ScalarRef,
        to: usize,
    },
    /// The sideways pass (perf/13 section 1): what a join's build side
    /// knows about its keys, applied to the level its probe reads, at
    /// the position that level is made. Everything between here and the
    /// join then runs on the rows that can still match, which today is
    /// the predicates over the level and the probe itself, and is a
    /// walk off it as soon as the planner puts one there.
    ///
    /// `slot` is this operator's place in the plan's filters, which is
    /// where the runner keeps the count of what it has rejected so far.
    /// A filter that rejects nothing is a pass over the probe side that
    /// buys nothing, and the runner is what notices.
    Sip {
        filter: Arc<SipFilter>,
        key: ScalarRef,
        slot: usize,
    },
}

/// A per-row scalar a sink reads out of the chunk set.
#[derive(Clone, Copy)]
pub(crate) enum ScalarRef {
    /// The level's node as a Value::Node.
    Node { level: usize },
    /// The level's row id as an integer, the dense id contract for a
    /// `.id` read with no stored id column.
    RowId { level: usize },
    /// A materialized property column.
    Col {
        level: usize,
        vec: usize,
        ty: ColType,
    },
}

impl ScalarRef {
    pub(crate) fn level(&self) -> usize {
        match *self {
            ScalarRef::Node { level }
            | ScalarRef::RowId { level }
            | ScalarRef::Col { level, .. } => level,
        }
    }
}

pub(crate) enum AggSpec {
    CountStar,
    /// count(x) for x that cannot be null here: dense property columns
    /// and non-optional nodes. Counts exactly like star.
    CountRef(ScalarRef),
    Sum(ScalarRef),
    Min(ScalarRef),
    Max(ScalarRef),
    Avg(ScalarRef),
}

impl AggSpec {
    /// The argument in place, for the renumbering a dropped level
    /// forces on everything that names one.
    pub(crate) fn arg_mut(&mut self) -> Option<&mut ScalarRef> {
        match self {
            AggSpec::CountStar => None,
            AggSpec::CountRef(r)
            | AggSpec::Sum(r)
            | AggSpec::Min(r)
            | AggSpec::Max(r)
            | AggSpec::Avg(r) => Some(r),
        }
    }

    pub(crate) fn arg(&self) -> Option<ScalarRef> {
        match *self {
            AggSpec::CountStar => None,
            AggSpec::CountRef(r)
            | AggSpec::Sum(r)
            | AggSpec::Min(r)
            | AggSpec::Max(r)
            | AggSpec::Avg(r) => Some(r),
        }
    }
}

/// Post steps above the sink, in plan order.
pub(crate) enum PostSpec {
    Distinct,
    /// ORDER BY over output columns: the column each key reads, which
    /// way round it goes and where its nulls sit. A key that names
    /// something the projection does not output falls back to the old
    /// engine.
    Sort(Vec<SortKey<usize>>),
    Skip(u64),
    Limit(u64),
}

pub(crate) enum SinkSpec {
    /// The bare global count(*): one accumulator, fed by multiplicity
    /// or by a fused DegreeProduct.
    Count,
    /// The global `count(DISTINCT ...)`. A distinct count is a group by
    /// its own argument whose answer is how many groups there are, so
    /// the argument compiles to the group keys and the sink counts the
    /// table instead of reading an accumulator out of it. A list
    /// argument is the tuple written out, which is how a query asks to
    /// count each unordered triple once.
    CountDistinct { keys: Vec<ScalarRef> },
    Agg {
        /// One flag per output item in clause order: true takes the
        /// next aggregate, false the next key.
        item_agg: Vec<bool>,
        keys: Vec<ScalarRef>,
        aggs: Vec<AggSpec>,
        post: Vec<PostSpec>,
    },
    Rows {
        items: Vec<ScalarRef>,
        post: Vec<PostSpec>,
    },
}

/// Whether anything the sink reads sits on `level`.
fn sink_reads(sink: &SinkSpec, level: usize) -> bool {
    let named = |r: &ScalarRef| r.level() == level;
    match sink {
        SinkSpec::Count => false,
        SinkSpec::CountDistinct { keys } => keys.iter().any(named),
        SinkSpec::Agg { keys, aggs, .. } => {
            keys.iter().any(named) || aggs.iter().filter_map(AggSpec::arg).any(|r| named(&r))
        }
        SinkSpec::Rows { items, .. } => items.iter().any(named),
    }
}

/// Whether an op names `level` at all, as the end it walks off, the end
/// it builds, the end it probes back into, or the level a bracket
/// binds. An op that names a level is an op that stops it being fused
/// away.
fn names_level(op: &Op, level: usize) -> bool {
    match op {
        Op::Expand {
            from, to, close, ..
        } => *from == level || *to == level || close.is_some_and(|c| c.probe_level == level),
        Op::Branch { from, to, .. } => *from == level || *to == level,
        Op::Intersect {
            probe_level, to, ..
        } => *probe_level == level || *to == level,
        Op::Semi { probe_level, .. } => *probe_level == level,
        Op::Optional { level: opt, .. } => *opt == level,
        Op::DegreeProduct { from, .. } => *from == level,
        Op::Join { key, to, .. } => key.level() == level || *to == level,
        Op::Sip { key, .. } => key.level() == level,
        Op::Filter { .. } | Op::OptionalHit => false,
    }
}

/// Whether the ops right above an expand read the level it built
/// without naming it. A filter refines whatever level is newest where
/// it sits, a semi judges it, and an intersection seeds off it, and
/// right above an expand that level is the one the expand made. The
/// walk cannot go away under any of them. Anything that builds a level
/// of its own moves the newest on, so the scan stops there.
fn reads_newest(above: &[Op]) -> bool {
    for op in above {
        match op {
            // A filter refines the newest level and so does the
            // sideways one, which is the same read of the same rows.
            Op::Filter { .. } | Op::Sip { .. } | Op::Semi { .. } | Op::Intersect { .. } => {
                return true;
            }
            Op::Expand { .. } | Op::Branch { .. } | Op::Join { .. } => return false,
            Op::Optional { .. } | Op::OptionalHit | Op::DegreeProduct { .. } => {}
        }
    }
    false
}

/// Marks the expands that may descend on whole vectors.
///
/// An expand pins one source row at a time because the pin is how
/// everything above the expand knows which row the neighbors below
/// belong to. When nothing above reads that level, the pin carries no
/// information and the row by row descent is pure overhead: on an
/// average social degree the pipeline below runs on a dozen rows at a
/// time instead of the 2048 it is written for. Those expands
/// concatenate neighbor lists across source rows instead, which is the
/// same rows in the same order, just handed down in full vectors.
///
/// Only the expand's own level matters. Levels under it stay pinned by
/// the expands that built them, so a probe or a projection reaching
/// past this one is none of this decision's business.
fn batch_expands(ops: &mut [Op], sink: &SinkSpec, levels: &[LevelBuild]) {
    for i in 0..ops.len() {
        let Op::Expand { from, close, .. } = ops[i] else {
            continue;
        };
        // The hub weight is the one thing that reads a source level
        // without needing its pin: when everything above this expand is
        // a filter and then the degree product off this expand's own
        // source, the runner takes one degree per source row before it
        // descends and carries the weights down with the rows.
        let hub = matches!(ops.last(), Some(Op::DegreeProduct { from: f, .. }) if *f == from)
            && ops[i + 1..]
                .iter()
                .all(|op| matches!(op, Op::Filter { .. } | Op::DegreeProduct { .. }));
        let reads = |op: &Op| match op {
            Op::Intersect { probe_level, .. } | Op::Semi { probe_level, .. } => {
                *probe_level == from
            }
            Op::Expand { close: Some(c), .. } => c.probe_level == from,
            // A branch walks off the pin too, and its source level is
            // one the pipeline has already left behind, so it is read
            // at whatever row the pin holds.
            Op::Branch { from: src, .. } => *src == from,
            // So does a degree product whose source the pipeline has
            // walked past, unless this expand is the one carrying it:
            // otherwise its weight is that one pinned row's.
            Op::DegreeProduct { from: src, .. } => *src == from && !hub,
            // A join reads its key off a pinned row whenever the key
            // sits below the newest level, and this expand's source is
            // below the newest level from the moment it runs.
            Op::Join { key, .. } => key.level() == from,
            _ => false,
        };
        // The expand's own fused close counts: it reads the probe
        // level through that level's pin like a standalone semi does.
        let probed = close.is_some_and(|c| c.probe_level == from) || ops[i + 1..].iter().any(reads);
        if let Op::Expand { batch, .. } = &mut ops[i] {
            *batch = !probed && !sink_reads(sink, from) && !outer_reads(levels, from);
        }
    }
}

/// Whether any level broadcasts a value off `level`. The broadcast is
/// read at that level's pin, and a batched expand leaves the pin on the
/// first row of the vector it hands down, so an expand walking off a
/// level a correlated predicate reads stays row at a time.
fn outer_reads(levels: &[LevelBuild], level: usize) -> bool {
    levels.iter().any(|l| {
        l.cols
            .iter()
            .any(|(_, c)| matches!(c, ColSpec::Outer { from, .. } if *from == level))
    })
}

/// Folds a semijoin into the expand that produced the rows it judges.
///
/// A closing semi refines the newest level, and the expand right below
/// it is what built that level: every neighbor it emitted got a chunk
/// row and every property column the pipeline reads on that level, and
/// then the semi threw most of them away. perf/13 section 1 calls this
/// the mask flowing sideways into the expand and the gather, and here
/// the two are the same operator, so the mask goes in at the emit and
/// the columns are read for the survivors only.
///
/// The probe list is what the semi would have read anyway, once for the
/// vector, and it does not move while the expand runs because its level
/// sits below the one the expand walks off. So the fusion costs nothing
/// and saves whatever the close rejects.
///
/// Only an immediately following semi folds in. One with a filter
/// between them judges rows that filter has already thinned, and moving
/// it above the filter would run the probe on rows the pipeline no
/// longer cares about.
fn fuse_closes(ops: &mut Vec<Op>) {
    let mut i = 0;
    while i + 1 < ops.len() {
        let (
            Op::Expand { close: None, .. },
            &Op::Semi {
                rel,
                dirs,
                probe_level,
            },
        ) = (&ops[i], &ops[i + 1])
        else {
            i += 1;
            continue;
        };
        let Op::Expand { to, close, .. } = &mut ops[i] else {
            unreachable!("matched an expand just above");
        };
        // A semi probing the level the expand is building would have to
        // read rows that do not exist yet. Validation rejects that
        // shape, and this pass leaves it alone rather than relying on
        // the order the two run in.
        if probe_level == *to {
            i += 1;
            continue;
        }
        *close = Some(Close {
            rel,
            dirs,
            probe_level,
        });
        ops.remove(i + 1);
    }
}

/// Compiles a plan, `Ok(None)` for any shape not covered yet.
pub(crate) fn compile(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    snap: &mut dyn Snapshot,
    params: &[Value],
    options: &Options,
) -> Result<Option<ExecPlan>> {
    let mut c = Compiler {
        query,
        schema,
        snap,
        params,
        wcoj: options.wcoj,
        sip: options.sip,
        levels: Vec::new(),
        slot_level: HashMap::new(),
        sips: Vec::new(),
        sip_at: HashMap::new(),
        optional_level: None,
        unwind_slot: None,
        func_slot: None,
        func: None,
    };
    c.compile(plan)
}

/// Rows a value join will read into a build table. Sixteen bytes a row
/// go into the table itself, so this is a few hundred megabytes at the
/// ceiling, and a side larger than it falls back rather than building
/// something that size before the query has answered anything.
const BUILD_ROWS_MAX: u64 = 50_000_000;

/// A level under construction: the registry assigns chunk vector
/// positions as columns are demanded, so programs built mid-walk hold
/// stable indices.
struct LevelBuild {
    table: TableId,
    /// The property name a stored column answers to, so a second
    /// reader of it reuses the position instead of gathering twice.
    /// Computed columns carry no name: nothing looks them up by one.
    cols: Vec<(String, ColSpec)>,
}

struct Compiler<'a> {
    query: &'a BoundQuery,
    schema: &'a Schema,
    snap: &'a mut dyn Snapshot,
    params: &'a [Value],
    /// `Wcoj::Off` pins the binary join, the baseline the fused close
    /// is measured against, so the whole plan goes back to the old
    /// engine rather than closing the cycle here.
    wcoj: Wcoj,
    /// `Sip::Off` pins the plain probe, so a run with it measures the
    /// same plan without the filter a join would have published.
    sip: Sip,
    levels: Vec<LevelBuild>,
    slot_level: HashMap<usize, usize>,
    /// The joins that placed, each with the scalar its probe reads.
    /// The filter comes off the table at the end, once the plan is
    /// known to be one this pipeline runs at all.
    sips: Vec<(Arc<JoinTable>, ScalarRef)>,
    /// Where a level's filter goes: the op position right after the
    /// operator that made the level, which is the first place its rows
    /// exist and the last place they are all still there.
    ///
    /// Two kinds of level are in here, and they are the two a probe key
    /// can read today. The driving one, which the source makes, so its
    /// position is the front of the pipeline. And a level a join built,
    /// which a second join can probe off once the first one has placed
    /// it. A key on a level a walk made is a shape the compiler does
    /// not reach: the plan puts that walk under the tie, hanging off a
    /// pattern still held, and the pipeline declines before any of this
    /// runs. Levels made inside an optional bracket are left out on
    /// purpose, since dropping a row of one of those is dropping a
    /// match the bracket has to keep as a miss.
    sip_at: HashMap<usize, usize>,
    /// The level an OPTIONAL MATCH group introduced, once one is open.
    /// It is the level that binds null on a miss, so the sink is held
    /// to what it can answer with a null there, and nothing else
    /// compiles after the group closes.
    optional_level: Option<usize>,
    /// The variable a leading UNWIND bound, once its list has become
    /// the batch of seeks that drives the plan. Reading it is reading
    /// the key that found the row, which is level 0's key column.
    unwind_slot: Option<usize>,
    /// The value variable a leading CALL yielded. Reading it is reading
    /// level 0's func column, the kernel's answer for that row.
    func_slot: Option<usize>,
    /// That kernel's answer, held until it goes on the plan.
    func: Option<FuncCol>,
}

impl Compiler<'_> {
    fn compile(&mut self, plan: &LogicalPlan) -> Result<Option<ExecPlan>> {
        let mut chain = Vec::new();
        let mut cur = plan;
        loop {
            chain.push(cur);
            cur = match cur {
                LogicalPlan::Empty => break,
                LogicalPlan::ScanNodes { input, .. }
                | LogicalPlan::Expand { input, .. }
                | LogicalPlan::Filter { input, .. }
                | LogicalPlan::Unwind { input, .. }
                | LogicalPlan::TableFunction { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Distinct { input }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Skip { input, .. }
                | LogicalPlan::Limit { input, .. } => input,
            };
        }
        chain.reverse();
        let mut it = chain.iter().copied().peekable();
        if !matches!(it.next(), Some(LogicalPlan::Empty)) {
            return Ok(None);
        }

        // A leading UNWIND over a list that is already known, which is
        // the batch point read: a client hands a page of ids and wants
        // the rows behind them. The list is held here and only becomes
        // the source once the filter under the scan says the values are
        // keys of the scanned table; an UNWIND of anything else, or one
        // whose variable is not what the scan seeks on, goes back to
        // the old engine whole.
        let mut unwound = None;
        if let Some(LogicalPlan::Unwind { expr, slot, .. }) = it.peek() {
            let Some(keys) = self.const_keys(expr) else {
                return Ok(None);
            };
            it.next();
            unwound = Some((*slot, keys));
        }

        // A leading CALL is the other thing that drives a pipeline: the
        // kernel runs over the whole rel and yields one row per node of
        // its domain, so level 0 is that node table read in row order
        // and the yielded value is a column of it. The rest of the
        // query, the hops off the yielded node and everything that
        // aggregates over them, then compiles like any other pipeline
        // instead of going back a row at a time.
        if let Some(LogicalPlan::TableFunction {
            func,
            rel,
            table,
            args,
            slots,
            ..
        }) = it.peek()
        {
            // A batch of seeks and a kernel are two different sources
            // and the plan only has room for one.
            if unwound.is_some() {
                return Ok(None);
            }
            let &[node_slot, value_slot] = slots.as_slice() else {
                return Ok(None);
            };
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                let Some(v) = self.const_int(arg) else {
                    return Ok(None);
                };
                vals.push(v);
            }
            // sssp is given a node id and walks from the row behind it,
            // the resolution the old engine does before it calls the
            // kernel. A key that names no node is an error that engine
            // owns, so an unresolved one falls back rather than being
            // answered here.
            if matches!(func, TableFunc::Sssp) {
                let Some(row) = self.seek_arg(*table, vals.first().copied())? else {
                    return Ok(None);
                };
                vals[0] = row;
            }
            let Some(col) = self.snap.table_function(func.name(), *rel, &vals)? else {
                return Ok(None);
            };
            it.next();
            self.slot_level.insert(node_slot, 0);
            self.slot_level.insert(value_slot, 0);
            self.func_slot = Some(value_slot);
            self.func = Some(col);
            self.levels.push(LevelBuild {
                table: *table,
                cols: Vec::new(),
            });
            return self.rest(it, *table, None, None);
        }

        // The driving scan: one required node slot over one table.
        let (table, scan_slot) = match it.next() {
            Some(LogicalPlan::ScanNodes {
                slot,
                optional: None,
                ..
            }) => {
                let tables = &self.query.variables[*slot].node_tables;
                let &[table] = tables.as_slice() else {
                    return Ok(None);
                };
                self.slot_level.insert(*slot, 0);
                (table, *slot)
            }
            _ => return Ok(None),
        };
        // A point predicate on the driving slot's id is the one shape
        // that never scans: the key index answers it with one row, so
        // the filter turns into the source instead of running over
        // every chunk. A key that is not a constant here, or one that
        // does not fit a row id, goes back to the old engine rather
        // than reading the whole table to find one row.
        let mut seek = None;
        let mut seeks = None;
        if let Some((slot, keys)) = unwound {
            // The list is the driving source only if the scan seeks on
            // the variable it bound. Anything else that an UNWIND could
            // be doing over a scan is a join the pipeline has no shape
            // for, so the query goes back rather than scanning the
            // table once per element.
            let Some(LogicalPlan::Filter {
                expr,
                optional: None,
                ..
            }) = it.peek()
            else {
                return Ok(None);
            };
            let Some(&BoundExpr::Var(key)) = id_point(expr, scan_slot) else {
                return Ok(None);
            };
            if key != slot {
                return Ok(None);
            }
            it.next();
            self.unwind_slot = Some(slot);
            seeks = Some(keys);
        } else if let Some(LogicalPlan::Filter {
            expr,
            optional: None,
            ..
        }) = it.peek()
            && let Some(key) = id_point(expr, scan_slot)
        {
            let Some(k) = self.const_int(key).and_then(|k| u64::try_from(k).ok()) else {
                return Ok(None);
            };
            it.next();
            seek = Some(k);
        }
        self.levels.push(LevelBuild {
            table,
            cols: Vec::new(),
        });
        self.rest(it, table, seek, seeks)
    }

    /// Everything above the source: the filters and expands in written
    /// order, then the sink with its absorbed post steps. Level 0 is
    /// already registered when this runs, so what drives the plan, a
    /// scan, a seek, a batch of them or a kernel, only shows up in the
    /// source it ends up carrying.
    fn rest<'p, I: Iterator<Item = &'p LogicalPlan>>(
        &mut self,
        mut it: std::iter::Peekable<I>,
        table: TableId,
        seek: Option<u64>,
        seeks: Option<Vec<u64>>,
    ) -> Result<Option<ExecPlan>> {
        // Filters and expands, in written order, always off the newest
        // level.
        let mut ops = Vec::new();
        let mut pred = None;
        // The driving level is made by the source, so the front of the
        // pipeline is where a filter over it goes.
        self.sip_at.insert(0, 0);
        // The patterns that share no variable with the first one, each
        // held until a predicate says what ties it to the pipeline. A
        // held pattern is a cross product until then, and the equality
        // that turns it into a join is written in the WHERE, which the
        // plan puts above every scan. Predicates that name a held
        // pattern wait with it, since the level they read does not
        // exist until its join builds it.
        let mut pending: Vec<(usize, TableId)> = Vec::new();
        let mut waiting: Vec<&BoundExpr> = Vec::new();
        loop {
            // An open bracket ends the pipeline: the group's level is
            // the newest one and it may be null, so nothing walks off
            // it or filters on it here. Whatever is left goes to the
            // sink match below, which takes a projection or an
            // aggregate and sends anything else back to the old
            // engine.
            if self.optional_level.is_some() {
                break;
            }
            match it.peek() {
                Some(LogicalPlan::Filter {
                    expr,
                    optional: None,
                    ..
                }) => {
                    it.next();
                    // A predicate that names a held pattern waits: it
                    // either ties that pattern to the pipeline, which
                    // is the join, or reads a level the join has not
                    // built yet. Settling decides which, and one join
                    // can be what lets the next predicate become one.
                    if pending.iter().any(|&(slot, _)| self.names_slot(expr, slot)) {
                        waiting.push(expr);
                        if self.settle(&mut ops, &mut pending, &mut waiting)?.is_none() {
                            return Ok(None);
                        }
                        continue;
                    }
                    let level = self.levels.len() - 1;
                    let Some(prog) = self.build_prog(expr, level)? else {
                        return Ok(None);
                    };
                    if level == 0
                        && ops.is_empty()
                        && pred.is_none()
                        && seek.is_none()
                        && seeks.is_none()
                    {
                        pred = self.zone_pred(expr)?;
                    }
                    ops.push(Op::Filter { prog });
                }
                // A second driving scan, which is a second pattern with
                // no variable in common with the first. Nothing is
                // emitted here: the scan is held until the equality
                // that joins it turns up, and a plan where none does is
                // a cross product this pipeline has no shape for.
                Some(LogicalPlan::ScanNodes {
                    slot,
                    optional: None,
                    ..
                }) => {
                    it.next();
                    let &[build] = self.query.variables[*slot].node_tables.as_slice() else {
                        return Ok(None);
                    };
                    pending.push((*slot, build));
                }
                // An OPTIONAL MATCH over a pattern that shares no
                // variable with the pipeline, tied by an equality. That
                // is a left join: probe the held side and, where the
                // probe finds nothing, bind the level to one null row
                // instead of dropping the outer one.
                //
                // The bracket is the same one a hop uses. What sits
                // under it is a join rather than an expand, and the
                // group's other predicates sit between the two, so a
                // row whose only matches they reject is a miss and not
                // a dropped row.
                Some(LogicalPlan::ScanNodes {
                    slot,
                    optional: Some(group),
                    ..
                }) => {
                    let group = *group;
                    let slot = *slot;
                    it.next();
                    let &[build] = self.query.variables[slot].node_tables.as_slice() else {
                        return Ok(None);
                    };
                    // The clause's inline props and its WHERE arrive as
                    // one conjunction, and the tie is one term of it, so
                    // the ands are split before anything looks for it.
                    let mut group_filters: Vec<&BoundExpr> = Vec::new();
                    while let Some(LogicalPlan::Filter {
                        expr,
                        optional: Some(g),
                        ..
                    }) = it.peek()
                    {
                        if *g != group {
                            break;
                        }
                        conjuncts(expr, &mut group_filters);
                        it.next();
                    }
                    // One of the group's predicates has to be the tie.
                    // Without one the optional pattern is a cross
                    // product that keeps every outer row, and the old
                    // engine's nested loop is where that belongs.
                    let Some(at) = ({
                        let mut found = None;
                        for (i, expr) in group_filters.iter().enumerate() {
                            if self.join_tie(expr, slot, build)?.is_some() {
                                found = Some(i);
                                break;
                            }
                        }
                        found
                    }) else {
                        return Ok(None);
                    };
                    let tie = group_filters.remove(at);
                    let Some((table, key)) = self.join_tie(tie, slot, build)? else {
                        unreachable!("the tie was just resolved");
                    };
                    let to_level = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: build,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(slot, to_level);
                    let head = ops.len();
                    ops.push(Op::Optional {
                        len: 0,
                        level: to_level,
                    });
                    ops.push(Op::Join {
                        table,
                        key,
                        to: to_level,
                    });
                    for expr in group_filters {
                        let Some(prog) = self.build_prog(expr, to_level)? else {
                            return Ok(None);
                        };
                        ops.push(Op::Filter { prog });
                    }
                    ops.push(Op::OptionalHit);
                    let len = ops.len() - head - 1;
                    let Op::Optional { len: slot, .. } = &mut ops[head] else {
                        unreachable!("just pushed the bracket");
                    };
                    *slot = len;
                    self.optional_level = Some(to_level);
                }
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: false,
                    asp: false,
                    wcoj: false,
                    optional: None,
                    ..
                }) => {
                    it.next();
                    let Some(&src) = self.slot_level.get(from) else {
                        return Ok(None);
                    };
                    let &[rel_id] = self.query.variables[*rel].rel_tables.as_slice() else {
                        return Ok(None);
                    };
                    let &[to_table] = self.query.variables[*to].node_tables.as_slice() else {
                        return Ok(None);
                    };
                    let Some(dirs) =
                        expand_dirs(self.schema, rel_id, self.levels[src].table, *direction)
                    else {
                        return Ok(None);
                    };
                    let to_level = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: to_table,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(*to, to_level);
                    ops.push(Op::Expand {
                        rel: rel_id,
                        dirs,
                        from: src,
                        to: to_level,
                        batch: false,
                        close: None,
                    });
                }
                // OPTIONAL MATCH, in the one shape the bracket covers:
                // a single hop introducing the far node, with the
                // group's own filters over it. That is what an
                // OPTIONAL MATCH is in nearly every query that has
                // one, and it is where the whole query used to go
                // back to the old executor.
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: false,
                    optional: Some(group),
                    ..
                }) => {
                    it.next();
                    let Some(&src) = self.slot_level.get(from) else {
                        return Ok(None);
                    };
                    let &[rel_id] = self.query.variables[*rel].rel_tables.as_slice() else {
                        return Ok(None);
                    };
                    let &[to_table] = self.query.variables[*to].node_tables.as_slice() else {
                        return Ok(None);
                    };
                    let Some(dirs) =
                        expand_dirs(self.schema, rel_id, self.levels[src].table, *direction)
                    else {
                        return Ok(None);
                    };
                    let to_level = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: to_table,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(*to, to_level);
                    let head = ops.len();
                    ops.push(Op::Optional {
                        len: 0,
                        level: to_level,
                    });
                    ops.push(Op::Expand {
                        rel: rel_id,
                        dirs,
                        from: src,
                        to: to_level,
                        batch: false,
                        close: None,
                    });
                    // The clause's inline props and its WHERE gate the
                    // match inside the group: a row they reject is a
                    // miss, not a dropped row, which is why they have
                    // to sit above the expand and below the bracket.
                    while let Some(LogicalPlan::Filter {
                        expr,
                        optional: Some(g),
                        ..
                    }) = it.peek()
                    {
                        if g != group {
                            break;
                        }
                        it.next();
                        let Some(prog) = self.build_prog(expr, to_level)? else {
                            return Ok(None);
                        };
                        ops.push(Op::Filter { prog });
                    }
                    ops.push(Op::OptionalHit);
                    let len = ops.len() - head - 1;
                    let Op::Optional { len: slot, .. } = &mut ops[head] else {
                        unreachable!("just pushed the bracket");
                    };
                    *slot = len;
                    self.optional_level = Some(to_level);
                }
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: true,
                    wcoj: true,
                    optional: None,
                    ..
                }) => {
                    // Filters the optimizer pushed onto the closing
                    // node sit between the two halves of the pair.
                    // They read the level the intersection produces,
                    // so they step aside and go back on top of it.
                    let mut held = Vec::new();
                    while matches!(ops.last(), Some(Op::Filter { .. })) {
                        held.push(ops.pop().expect("just matched"));
                    }
                    it.next();
                    match self.fuse_close(&ops, *rel, *from, *to, *direction) {
                        Some(op) => {
                            ops.pop();
                            ops.push(op);
                            ops.extend(held.into_iter().rev());
                        }
                        // The mark says the pair is worth intersecting,
                        // not that this executor can. The plain probe
                        // is still correct, and taking it here beats
                        // sending the whole query back to the old
                        // engine over one operator.
                        None => {
                            ops.extend(held.into_iter().rev());
                            let Some(op) = self.close_semi(*rel, *from, *to, *direction) else {
                                return Ok(None);
                            };
                            ops.push(op);
                        }
                    }
                }
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: true,
                    wcoj: false,
                    optional: None,
                    ..
                }) => {
                    let Some(op) = self.close_semi(*rel, *from, *to, *direction) else {
                        return Ok(None);
                    };
                    it.next();
                    ops.push(op);
                }
                _ => break,
            }
        }

        // A held pattern nothing ever tied to the pipeline is a cross
        // product, and one of those belongs on the old engine: it has a
        // nested loop for it and this pipeline would have to build a
        // table of the whole side to answer the same thing. A predicate
        // still waiting names one of those patterns, so it goes back
        // for the same reason.
        if !pending.is_empty() || !waiting.is_empty() {
            return Ok(None);
        }

        // The sink and its absorbed post steps.
        let sink_node = it.next();
        let mut post = Vec::new();
        for node in it {
            match node {
                LogicalPlan::Distinct { .. } => post.push(PostSpec::Distinct),
                LogicalPlan::Skip { expr, .. } => {
                    let Some(n) = self.const_count(expr) else {
                        return Ok(None);
                    };
                    post.push(PostSpec::Skip(n));
                }
                LogicalPlan::Limit { expr, .. } => {
                    let Some(n) = self.const_count(expr) else {
                        return Ok(None);
                    };
                    post.push(PostSpec::Limit(n));
                }
                LogicalPlan::Sort { keys, .. } => {
                    let Some(cols) = self.sort_cols(keys) else {
                        return Ok(None);
                    };
                    post.push(PostSpec::Sort(cols));
                }
                // HAVING-style filters above the sink still fall back.
                _ => return Ok(None),
            }
        }

        let mut sink = match sink_node {
            Some(LogicalPlan::Project { items, .. }) => {
                let mut refs = Vec::with_capacity(items.len());
                for item in items {
                    if item.aggregate {
                        return Ok(None);
                    }
                    let Some(r) = self.item_ref(&item.expr)? else {
                        return Ok(None);
                    };
                    refs.push(r);
                }
                SinkSpec::Rows { items: refs, post }
            }
            Some(LogicalPlan::Aggregate { keys, aggs, .. }) => {
                let Some(item_agg) = self.item_order(keys, aggs) else {
                    return Ok(None);
                };
                let mut key_refs = Vec::with_capacity(keys.len());
                for item in keys {
                    let Some(r) = self.item_ref(&item.expr)? else {
                        return Ok(None);
                    };
                    key_refs.push(r);
                }
                // A distinct count brings its own grouping and answers
                // out of the table rather than out of an accumulator,
                // so it is decided before the ordinary aggregate specs.
                let mut tuple = None;
                if key_refs.is_empty() && post.is_empty() && aggs.len() == 1 {
                    tuple = self.distinct_count_keys(&aggs[0].expr)?;
                }
                match tuple {
                    Some(keys) => SinkSpec::CountDistinct { keys },
                    None => {
                        let mut agg_specs = Vec::with_capacity(aggs.len());
                        for item in aggs {
                            let Some(spec) = self.agg_spec(&item.expr)? else {
                                return Ok(None);
                            };
                            agg_specs.push(spec);
                        }
                        if key_refs.is_empty()
                            && agg_specs.len() == 1
                            && matches!(agg_specs[0], AggSpec::CountStar | AggSpec::CountRef(_))
                            && post.is_empty()
                        {
                            SinkSpec::Count
                        } else {
                            SinkSpec::Agg {
                                item_agg,
                                keys: key_refs,
                                aggs: agg_specs,
                                post,
                            }
                        }
                    }
                }
            }
            _ => return Ok(None),
        };

        // The bracket's level binds null on a miss, and only some
        // sinks answer with a null in them. A projection carries one
        // out as a value, and a count over a miss counts the outer row
        // once because a null level still has one row. Deduplicating
        // and ordering on a null, and aggregating over one, follow
        // rules the sink does not implement yet, so those go back to
        // the old engine.
        if let Some(opt) = self.optional_level {
            let ok = match &sink {
                SinkSpec::Count => true,
                SinkSpec::Rows { items, post } => post.iter().all(|p| match p {
                    PostSpec::Distinct => false,
                    PostSpec::Sort(cols) => cols.iter().all(|k| items[k.expr].level() != opt),
                    PostSpec::Skip(_) | PostSpec::Limit(_) => true,
                }),
                SinkSpec::CountDistinct { keys } => keys.iter().all(|k| k.level() != opt),
                SinkSpec::Agg { keys, aggs, .. } => {
                    keys.iter().all(|k| k.level() != opt)
                        && aggs
                            .iter()
                            .filter_map(AggSpec::arg)
                            .all(|r| r.level() != opt)
                }
            };
            if !ok {
                return Ok(None);
            }
        }

        // A kernel that answers null for some rows, which sssp does for
        // every node it did not reach, is read the way any other
        // nullable value is: a projection carries the null out and a
        // comparison against it is false, both of which match the old
        // engine. Grouping on one, ordering by one, deduplicating on
        // one and aggregating over one follow rules the packed key and
        // the accumulators do not implement, so those go back.
        if let Some(null) = self.null_func_vec() {
            let reads =
                |r: &ScalarRef| matches!(*r, ScalarRef::Col { level: 0, vec, .. } if vec == null);
            let ok = match &sink {
                SinkSpec::Count => true,
                SinkSpec::Rows { items, post } => post.iter().all(|p| match p {
                    PostSpec::Distinct => !items.iter().any(reads),
                    PostSpec::Sort(cols) => !cols.iter().any(|k| reads(&items[k.expr])),
                    PostSpec::Skip(_) | PostSpec::Limit(_) => true,
                }),
                SinkSpec::CountDistinct { keys } => !keys.iter().any(reads),
                SinkSpec::Agg { keys, aggs, .. } => {
                    !keys.iter().any(reads)
                        && !aggs.iter().filter_map(AggSpec::arg).any(|r| reads(&r))
                }
            };
            if !ok {
                return Ok(None);
            }
        }

        // The sideways pass (perf/13 section 1). A join knows which keys
        // its build side holds, and the level it probes from was made
        // somewhere below it, with a walk, a gather and a predicate or
        // two in between. Putting the filter where that level is made
        // is what lets all of that run on the rows that can still
        // match, rather than on every row the scan produced.
        //
        // The range goes further down than the operator does, into the
        // scan's zone pushdown, where a chunk whose values all sit
        // outside the build side's range is skipped without being
        // decoded at all.
        let mut inserts: Vec<(usize, Op)> = Vec::new();
        for (table, key) in std::mem::take(&mut self.sips) {
            let Some(&at) = self.sip_at.get(&key.level()) else {
                continue;
            };
            let filter = table.sip();
            if let ScalarRef::Col { level: 0, vec, .. } = key
                && pred.is_none()
                && seek.is_none()
                && seeks.is_none()
                && let ColSpec::Stored(col, _) = self.levels[0].cols[vec - 1].1
            {
                pred = filter.zone(col);
            }
            // A filter with no gaps in it rejects what the range
            // rejects and nothing else, so testing every probe row
            // against it is a pass that cannot drop one. The range it
            // published above is the whole of what that build side had
            // to say.
            if filter.gapless() {
                continue;
            }
            // An inexact test does not pay for itself here. What the
            // operator saves on a rejected row is one probe, which is
            // one random read of the join's directory, and what it
            // costs is a random read of its own. So it has to be both
            // cheaper than the probe and right about the row, and only
            // the mask is: a shift and a bit test in a bitmap an order
            // of magnitude smaller than the directory. On the join
            // bench the mask runs 0.9 to 1.1x against the same plan
            // with ZU_SIP=0 and the bloom 0.8 to 0.9x. The range is
            // worth publishing either way and that part is done above.
            //
            // This is where a scan that took the filter would change
            // the arithmetic, since then a rejected row would cost no
            // column decode at all and an inexact test would have
            // something real to save.
            if !filter.exact() {
                continue;
            }
            let slot = inserts.len();
            inserts.push((
                at,
                Op::Sip {
                    filter: Arc::new(filter),
                    key,
                    slot,
                },
            ));
        }
        // Back to front, so an insertion never moves a position that
        // has not been used yet.
        inserts.sort_by_key(|&(at, _)| std::cmp::Reverse(at));
        for (at, op) in inserts {
            ops.insert(at, op);
        }

        // Fuse trailing expands feeding a count or a grouped aggregate
        // into one degree product when nothing reads the expanded
        // levels' rows or columns. The steps must fan out of one source
        // level: a single popped expand is the plain expand-then-count
        // fusion, several are a hub, the shape the optimizer picks for
        // an unseeded two-hop, where the count per source row is the
        // product of its per-step degrees.
        //
        // An aggregate takes the product as a weight. What the walk
        // would have built is one row per neighbor carrying the same
        // key and the same argument as the source row it came off, so
        // the group only ever needed to know how many of them there
        // were, and a degree read is offsets alone: the neighbor array
        // is never touched. A source row the steps found nothing for
        // weighs nothing and drops out, which is what keeps a group
        // from opening on a row that has no answer.
        let fusable = match &sink {
            SinkSpec::Count => true,
            // A bracket's level binds one invalid row on a miss, and a
            // degree read off it would be row zero's degree rather than
            // nothing, so an optional keeps its walk.
            SinkSpec::Agg { .. } => self.optional_level.is_none(),
            SinkSpec::CountDistinct { .. } | SinkSpec::Rows { .. } => false,
        };
        if fusable {
            let mut steps = Vec::new();
            let mut taken = Vec::new();
            let mut step_from = None;
            while let Some(Op::Expand {
                rel,
                dirs,
                from,
                to,
                ..
            }) = ops.last()
            {
                if !self.levels[*to].cols.is_empty()
                    || sink_reads(&sink, *to)
                    || step_from.is_some_and(|f| f != *from)
                {
                    break;
                }
                step_from = Some(*from);
                steps.push((*rel, *dirs));
                taken.push(ops.pop().expect("just matched"));
            }
            // The same fusion for a hop sitting in the middle of the
            // pipeline. Two patterns off one variable put the read end
            // last as often as first, and the unread one is a weight
            // wherever it sits: the pipeline below it never looked at
            // its rows, so the only thing they contributed was how many
            // there were. Taking it out is what keeps the walk below
            // from running once per neighbor of a level nobody reads.
            //
            // Its level goes away with it, so everything above it moves
            // down one and the level indices the plan is written in are
            // renumbered before the plan is handed over.
            let mut dropped = Vec::new();
            if self.optional_level.is_none() {
                for i in (0..ops.len()).rev() {
                    let Op::Expand {
                        rel,
                        dirs,
                        from,
                        to,
                        ..
                    } = ops[i]
                    else {
                        continue;
                    };
                    if !self.levels[to].cols.is_empty()
                        || sink_reads(&sink, to)
                        || outer_reads(&self.levels, to)
                        || step_from.is_some_and(|f| f != from)
                        || ops
                            .iter()
                            .enumerate()
                            .any(|(j, op)| j != i && names_level(op, to))
                        || reads_newest(&ops[i + 1..])
                    {
                        continue;
                    }
                    step_from = Some(from);
                    steps.push((rel, dirs));
                    ops.remove(i);
                    dropped.push(to);
                }
            }
            if let Some(from) = step_from {
                let newest_after = ops
                    .iter()
                    .filter_map(|op| match op {
                        Op::Expand { to, .. } => Some(*to),
                        _ => None,
                    })
                    .next_back()
                    .unwrap_or(0);
                // Steps off a level the surviving pipeline does not end
                // on are read at that level's pin instead of over a
                // vector of rows, which the runner does and a bracket
                // does not: its level binds an invalid row on a miss and
                // a degree read off that would answer row zero's. So an
                // optional puts the expands back and walks them.
                if from != newest_after && self.optional_level.is_some() {
                    steps.clear();
                    while let Some(op) = taken.pop() {
                        ops.push(op);
                    }
                }
            }
            if !dropped.is_empty() {
                let map = self.renumber(&dropped, &mut ops, &mut sink);
                step_from = step_from.map(|from| map[from]);
            }
            if !steps.is_empty() {
                let from = step_from.expect("every step came off a level");
                ops.push(Op::DegreeProduct { steps, from });
            }
        }

        // After fusion every surviving expand either walks off the
        // newest level, the invariant the runner's pin-and-descend loop
        // is built on, or off a level below it, which is a branch and
        // runs as one. The bracket is the exception: a branch under it
        // would walk off a level whose pin the miss path rewrites, so
        // an optional keeps the old rule and falls back.
        let bracketed = self.optional_level.is_some();
        let mut newest = 0;
        for op in &mut ops {
            match op {
                Op::Expand {
                    rel,
                    dirs,
                    from,
                    to,
                    ..
                } => {
                    if *from != newest {
                        if bracketed || *from > newest {
                            return Ok(None);
                        }
                        *op = Op::Branch {
                            rel: *rel,
                            dirs: *dirs,
                            from: *from,
                            to: *to,
                        };
                    }
                    let (Op::Expand { to, .. } | Op::Branch { to, .. }) = op else {
                        unreachable!("just matched one of the two");
                    };
                    newest = *to;
                }
                Op::Intersect {
                    probe_level, to, ..
                } => {
                    if *probe_level >= newest {
                        return Ok(None);
                    }
                    newest = *to;
                }
                Op::Semi { probe_level, .. } if *probe_level >= newest => return Ok(None),
                // A join's key is read off a level the pipeline has
                // already built, at its pin when it is not the newest
                // one, so a key naming a level above is a plan the
                // runner has no row for.
                Op::Join { key, to, .. } => {
                    if key.level() > newest {
                        return Ok(None);
                    }
                    newest = *to;
                }
                _ => {}
            }
        }

        fuse_closes(&mut ops);
        batch_expands(&mut ops, &sink, &self.levels);
        // The bracket runs its group one outer row at a time, because
        // whether the group matched is a fact about that row. A
        // batched descent drops the pin and concatenates neighbors
        // across source rows, which loses exactly that, so nothing
        // inside the bracket batches.
        if let Some(head) = ops.iter().position(|op| matches!(op, Op::Optional { .. })) {
            for op in &mut ops[head..] {
                if let Op::Expand { batch, .. } = op {
                    *batch = false;
                }
            }
        }

        Ok(Some(ExecPlan {
            table,
            source: match (seek, seeks) {
                (Some(key), _) => Source::Seek(key),
                (None, Some(keys)) => Source::Seeks(keys),
                (None, None) => Source::Scan(pred),
            },
            ops,
            sink,
            levels: self
                .levels
                .drain(..)
                .map(|l| Level {
                    table: l.table,
                    cols: l.cols.into_iter().map(|(_, spec)| spec).collect(),
                })
                .collect(),
            columns: self.query.columns.clone(),
            func: self.func.take(),
        }))
    }

    /// Drops the levels a fused hop took away and moves everything
    /// above them down one, in the levels themselves, in the ops, and
    /// in the sink. A level is its position in the runner's chunk
    /// stack, so a plan that keeps a hole in the numbering reads the
    /// wrong chunk. Answers the old level to new level map, which the
    /// fusion still needs for the source it kept.
    fn renumber(&mut self, dropped: &[usize], ops: &mut [Op], sink: &mut SinkSpec) -> Vec<usize> {
        let mut map = vec![usize::MAX; self.levels.len()];
        let mut next = 0;
        for (old, slot) in map.iter_mut().enumerate() {
            if dropped.contains(&old) {
                continue;
            }
            *slot = next;
            next += 1;
        }
        let kept: Vec<_> = self
            .levels
            .drain(..)
            .enumerate()
            .filter(|(old, _)| map[*old] != usize::MAX)
            .map(|(_, mut level)| {
                for (_, col) in &mut level.cols {
                    if let ColSpec::Outer { from, .. } = col {
                        *from = map[*from];
                    }
                }
                level
            })
            .collect();
        self.levels = kept;
        for op in ops {
            match op {
                Op::Expand {
                    from, to, close, ..
                } => {
                    *from = map[*from];
                    *to = map[*to];
                    if let Some(c) = close {
                        c.probe_level = map[c.probe_level];
                    }
                }
                Op::Branch { from, to, .. } => {
                    *from = map[*from];
                    *to = map[*to];
                }
                Op::Intersect {
                    probe_level, to, ..
                } => {
                    *probe_level = map[*probe_level];
                    *to = map[*to];
                }
                Op::Semi { probe_level, .. } => *probe_level = map[*probe_level],
                Op::Optional { level, .. } => *level = map[*level],
                Op::DegreeProduct { from, .. } => *from = map[*from],
                Op::Join { key, to, .. } => {
                    match key {
                        ScalarRef::Node { level }
                        | ScalarRef::RowId { level }
                        | ScalarRef::Col { level, .. } => *level = map[*level],
                    }
                    *to = map[*to];
                }
                Op::Sip { key, .. } => match key {
                    ScalarRef::Node { level }
                    | ScalarRef::RowId { level }
                    | ScalarRef::Col { level, .. } => *level = map[*level],
                },
                Op::Filter { .. } | Op::OptionalHit => {}
            }
        }
        let fix = |r: &mut ScalarRef| match r {
            ScalarRef::Node { level }
            | ScalarRef::RowId { level }
            | ScalarRef::Col { level, .. } => {
                *level = map[*level];
            }
        };
        match sink {
            SinkSpec::Count => {}
            SinkSpec::CountDistinct { keys } => keys.iter_mut().for_each(fix),
            SinkSpec::Rows { items, .. } => items.iter_mut().for_each(fix),
            SinkSpec::Agg { keys, aggs, .. } => {
                keys.iter_mut().for_each(fix);
                for arg in aggs.iter_mut().filter_map(AggSpec::arg_mut) {
                    fix(arg);
                }
            }
        }
        map
    }

    /// Reconstructs the output item order of the final projection: one
    /// flag per clause item, true for aggregates. The plan node splits
    /// keys from aggregates but the result row interleaves them in
    /// written order.
    fn item_order(&self, keys: &[BoundItem], aggs: &[BoundItem]) -> Option<Vec<bool>> {
        let Some(BoundClause::Project { items, .. }) = self.query.clauses.last() else {
            return None;
        };
        if items.len() != keys.len() + aggs.len() {
            return None;
        }
        Some(items.iter().map(|it| it.aggregate).collect())
    }

    /// Fuses a closing expand into the expand that built the node it
    /// closes on, or None when the pair is not a shape the intersection
    /// covers: both sides over one rel table, one of the two lists
    /// hanging off the newest level so it is walked row by row, and the
    /// other hanging off a level below it so it is pinned and read once
    /// for the whole vector.
    ///
    /// Which of the two is which depends on where the optimizer put the
    /// scan. Starting at the wedge tip leaves the built expand walking
    /// off the newest level and the close reaching back down; starting
    /// at the closing node itself mirrors that, the close walks off the
    /// newest level and the built expand reaches down. Both are the
    /// same intersection with the roles swapped.
    fn fuse_close(
        &self,
        ops: &[Op],
        rel: usize,
        from: usize,
        to: usize,
        direction: RelDirection,
    ) -> Option<Op> {
        let &Op::Expand {
            rel: built_rel,
            dirs: built_dirs,
            from: built_from,
            to: built_to,
            ..
        } = ops.last()?
        else {
            return None;
        };
        if matches!(self.wcoj, Wcoj::Off) || built_to + 1 != self.levels.len() {
            return None;
        }
        // The end of the closing pattern that names the node the expand
        // above just built, and the walk direction read from its other
        // end, which is the one already on a level.
        let (far_slot, far_dir) = if self.slot_level.get(&to) == Some(&built_to) {
            (from, direction)
        } else if self.slot_level.get(&from) == Some(&built_to) {
            (to, flip(direction))
        } else {
            return None;
        };
        let &far_level = self.slot_level.get(&far_slot)?;
        let &[close_rel] = self.query.variables[rel].rel_tables.as_slice() else {
            return None;
        };
        let close_dirs = expand_dirs(
            self.schema,
            close_rel,
            self.levels[far_level].table,
            far_dir,
        )?;
        // The close has to land on the table the built expand's level
        // holds, otherwise the two lists name different things and
        // there is nothing to intersect. A both-sides walk stays on one
        // table by construction, so its far side is the near side.
        let lands = match close_dirs {
            Dirs::One(d) => far_table(self.schema, close_rel, d)? == self.levels[built_to].table,
            Dirs::Both => self.levels[far_level].table == self.levels[built_to].table,
        };
        if !lands {
            return None;
        }
        // The newest level is the one the pipeline ends on before this
        // pair, and the other list has to sit strictly below it.
        let newest = built_to - 1;
        let (seed, probe, probe_level) = if built_from == newest && far_level < newest {
            ((built_rel, built_dirs), (close_rel, close_dirs), far_level)
        } else if far_level == newest && built_from < newest {
            ((close_rel, close_dirs), (built_rel, built_dirs), built_from)
        } else {
            return None;
        };
        Some(Op::Intersect {
            seed,
            probe,
            probe_level,
            to: built_to,
        })
    }

    /// Compiles a closing expand the intersection did not take into a
    /// semijoin, or None when neither end sits on the newest level.
    /// Both the storage probe and the accumulated edge set the old
    /// engine picks between are the same test here, whether an edge
    /// exists, and the answer is one row either way, so the two plan
    /// flavors compile to the same operator.
    fn close_semi(
        &self,
        rel: usize,
        from: usize,
        to: usize,
        direction: RelDirection,
    ) -> Option<Op> {
        let newest = self.levels.len() - 1;
        let &from_level = self.slot_level.get(&from)?;
        let &to_level = self.slot_level.get(&to)?;
        let &[rel_id] = self.query.variables[rel].rel_tables.as_slice() else {
            return None;
        };
        // The pinned end is the one below the newest level: its list is
        // read once and the rows of the newest level are probed into it.
        let (probe_level, probe_dir) = if from_level < newest && to_level == newest {
            (from_level, direction)
        } else if to_level < newest && from_level == newest {
            (to_level, flip(direction))
        } else {
            return None;
        };
        let dirs = expand_dirs(
            self.schema,
            rel_id,
            self.levels[probe_level].table,
            probe_dir,
        )?;
        // The walk has to land on the table the probed rows come from.
        let lands = match dirs {
            Dirs::One(d) => far_table(self.schema, rel_id, d)? == self.levels[newest].table,
            Dirs::Both => self.levels[probe_level].table == self.levels[newest].table,
        };
        lands.then_some(Op::Semi {
            rel: rel_id,
            dirs,
            probe_level,
        })
    }

    /// Resolves ORDER BY keys to output columns. A key either names a
    /// projected item, which is how `ORDER BY alias` binds, or repeats
    /// the item's own expression, which is how `ORDER BY p.name` binds
    /// when p is still in scope. Anything else, an expression over a
    /// column the query does not return, needs the key materialized
    /// next to the row and goes back to the old engine.
    fn sort_cols(&self, keys: &[SortKey<BoundExpr>]) -> Option<Vec<SortKey<usize>>> {
        let BoundClause::Project { items, .. } = self.query.clauses.last()? else {
            return None;
        };
        let mut cols = Vec::with_capacity(keys.len());
        for key in keys {
            let expr = &key.expr;
            let at = items.iter().position(|item| {
                item.expr == *expr
                    || matches!(expr, BoundExpr::Var(slot) if self.item_slot(item) == Some(*slot))
            })?;
            cols.push(key.with_expr(at));
        }
        Some(cols)
    }

    /// The slot a projected item answers to after the projection, the
    /// same rule the old engine's sink uses: WITH items carry it, RETURN
    /// items lose theirs in the binder and get it back by name.
    fn item_slot(&self, item: &BoundItem) -> Option<usize> {
        if item.slot.is_some() {
            return item.slot;
        }
        if let BoundExpr::Var(slot) = item.expr {
            return Some(slot);
        }
        self.query
            .variables
            .iter()
            .rposition(|v| v.name == item.name)
    }

    /// The keys a leading UNWIND yields, when the list is written out
    /// or arrives whole as a parameter and every element is a key a
    /// seek could use.
    ///
    /// One element that is not a non-negative integer sends the whole
    /// query back. The old engine skips such an element, since no row
    /// answers to it, so declining costs nothing but the fallback and
    /// keeps the rule here to one line.
    fn const_keys(&self, expr: &BoundExpr) -> Option<Vec<u64>> {
        let ints: Vec<i64> = match expr {
            BoundExpr::List(items) => items
                .iter()
                .map(|item| match item {
                    BoundExpr::Literal(Literal::Int(n)) => Some(*n),
                    _ => None,
                })
                .collect::<Option<_>>()?,
            BoundExpr::Param(ix) => match self.params.get(*ix)? {
                Value::List(items) => items
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => Some(*n),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
                _ => return None,
            },
            _ => return None,
        };
        ints.into_iter().map(|n| u64::try_from(n).ok()).collect()
    }

    /// A SKIP or LIMIT count that is a plain non-negative integer.
    fn const_count(&self, expr: &BoundExpr) -> Option<u64> {
        let v = match expr {
            BoundExpr::Literal(Literal::Int(n)) => *n,
            BoundExpr::Param(ix) => match self.params.get(*ix)? {
                Value::Int(n) => *n,
                _ => return None,
            },
            _ => return None,
        };
        u64::try_from(v).ok()
    }

    /// Registers a property column on a level, returning its chunk
    /// vector position.
    fn register_col(&mut self, level: usize, key: &str) -> Result<Option<(usize, ColType)>> {
        if let Some(ix) = self.levels[level].cols.iter().position(|(k, _)| k == key) {
            let ColSpec::Stored(_, ty) = self.levels[level].cols[ix].1 else {
                unreachable!("only stored columns carry a name");
            };
            return Ok(Some((ix + 1, ty)));
        }
        let Some((id, ty)) = self.snap.resolve_col(self.levels[level].table, key)? else {
            return Ok(None);
        };
        self.levels[level]
            .cols
            .push((key.to_string(), ColSpec::Stored(id, ty)));
        Ok(Some((self.levels[level].cols.len(), ty)))
    }

    /// Registers level 0's key column, or finds the one already there,
    /// and returns the scalar that reads it back.
    fn key_col(&mut self) -> ScalarRef {
        let at = match self.levels[0]
            .cols
            .iter()
            .position(|(_, c)| matches!(c, ColSpec::Key))
        {
            Some(ix) => ix + 1,
            None => {
                self.levels[0].cols.push((String::new(), ColSpec::Key));
                self.levels[0].cols.len()
            }
        };
        ScalarRef::Col {
            level: 0,
            vec: at,
            ty: ColType::Int,
        }
    }

    /// Registers level 0's func column, or finds the one already there,
    /// and returns the scalar that reads it back. A yielded value is
    /// the kernel's answer for the row, so it enters the chunk beside
    /// the row id rather than being looked up anywhere.
    fn func_col(&mut self) -> ScalarRef {
        let at = match self.levels[0]
            .cols
            .iter()
            .position(|(_, c)| matches!(c, ColSpec::Func))
        {
            Some(ix) => ix + 1,
            None => {
                self.levels[0].cols.push((String::new(), ColSpec::Func));
                self.levels[0].cols.len()
            }
        };
        ScalarRef::Col {
            level: 0,
            vec: at,
            ty: ColType::Int,
        }
    }

    /// Level 0's func column position when the kernel's answer has
    /// nulls in it, `None` when every row has a value or when nothing
    /// read the column at all.
    fn null_func_vec(&self) -> Option<usize> {
        if !self.func.as_ref().is_some_and(FuncCol::nullable) {
            return None;
        }
        self.levels[0]
            .cols
            .iter()
            .position(|(_, c)| matches!(c, ColSpec::Func))
            .map(|ix| ix + 1)
    }

    /// The dense row a table function's node id argument names, `None`
    /// when there is no argument or the key names no node.
    fn seek_arg(&mut self, table: TableId, key: Option<i64>) -> Result<Option<i64>> {
        let Some(key) = key.and_then(|k| u64::try_from(k).ok()) else {
            return Ok(None);
        };
        Ok(self.snap.seek_key(table, key)?.map(|row| row as i64))
    }

    /// Registers a lower level's pinned value as a constant column on
    /// `level`, returning its chunk vector position. Two predicates
    /// reading the same end share the column, the way two readers of a
    /// property share its gather.
    fn register_outer(&mut self, level: usize, from: usize, vec: usize) -> usize {
        let same = |c: &ColSpec| matches!(c, ColSpec::Outer { from: f, vec: v } if *f == from && *v == vec);
        if let Some(ix) = self.levels[level].cols.iter().position(|(_, c)| same(c)) {
            return ix + 1;
        }
        self.levels[level]
            .cols
            .push((String::new(), ColSpec::Outer { from, vec }));
        self.levels[level].cols.len()
    }

    /// Compiles an arithmetic projection into a computed column on the
    /// level its properties come from, returning the scalar the sink
    /// reads it back through. The program is registered after every
    /// column it reads, which is what lets one walk of the list build
    /// the chunk.
    ///
    /// Integer results only. A property is an int or a string here, so
    /// a float can only come from a literal, and a literal float
    /// against an int column already fails the program builder's type
    /// check; restricting the column keeps the sink reading the two
    /// types it knows.
    ///
    /// Division and modulo decline. The kernel is total and clears the
    /// row's validity when the divisor is zero, the old engine raises,
    /// and a computed column runs before the filter that would have
    /// dropped the offending row, so there is no way to match the old
    /// answer here. They stay with the old engine until the error is
    /// carried out of the kernel.
    fn register_expr(&mut self, expr: &BoundExpr) -> Result<Option<ScalarRef>> {
        if !matches!(expr, BoundExpr::Binary { .. }) {
            return Ok(None);
        }
        let Some(level) = self.expr_level(expr) else {
            return Ok(None);
        };
        let mut b = ProgBuilder {
            ops: Vec::new(),
            types: Vec::new(),
        };
        let Some(root) = self.value_reg(&mut b, expr, level, false)? else {
            return Ok(None);
        };
        if b.types[root as usize] != PhysType::Int64 {
            return Ok(None);
        }
        if b.ops.iter().any(|op| {
            matches!(
                op,
                ExprOp::Binary {
                    op: BinOp::Div | BinOp::Mod,
                    ..
                }
            )
        }) {
            return Ok(None);
        }
        let prog = Program {
            ops: b.ops,
            regs: b.types.len() as Reg,
        };
        self.levels[level]
            .cols
            .push((String::new(), ColSpec::Computed(prog)));
        Ok(Some(ScalarRef::Col {
            level,
            vec: self.levels[level].cols.len(),
            ty: ColType::Int,
        }))
    }

    /// The one level an expression reads, or None when it reads none or
    /// spans two. A program runs against a single level's chunk, and a
    /// join of two levels' columns is not a projection.
    fn expr_level(&self, expr: &BoundExpr) -> Option<usize> {
        let mut level = None;
        let mut ok = true;
        self.walk_slots(expr, &mut |slot| {
            let found = self.slot_level.get(&slot).copied();
            match (level, found) {
                (_, None) => ok = false,
                (None, Some(l)) => level = Some(l),
                (Some(l), Some(f)) if l != f => ok = false,
                _ => {}
            }
        });
        ok.then_some(level).flatten()
    }

    /// Every variable an expression names, in no particular order.
    fn walk_slots(&self, expr: &BoundExpr, f: &mut impl FnMut(usize)) {
        match expr {
            BoundExpr::Var(slot) => f(*slot),
            BoundExpr::Property { base, .. } => self.walk_slots(base, f),
            BoundExpr::Binary { lhs, rhs, .. } => {
                self.walk_slots(lhs, f);
                self.walk_slots(rhs, f);
            }
            _ => {}
        }
    }

    /// Whether an expression reads a variable at all.
    fn names_slot(&self, expr: &BoundExpr, slot: usize) -> bool {
        let mut found = false;
        self.walk_slots(expr, &mut |s| found |= s == slot);
        found
    }

    /// The equality that ties a held pattern to the pipeline, as the
    /// build table it produces and the probe scalar it reads, `None`
    /// when this predicate is not that equality.
    ///
    /// One side has to read a property of the held pattern and nothing
    /// else, and the other has to read only levels the pipeline has
    /// already built. That second side is the probe key, one value per
    /// row wherever it sits, and the first is the build key, read off
    /// the held table once here.
    ///
    /// Integer columns on both sides. A string key would have to carry
    /// its bytes into the table and hash them there, and comparing a
    /// property against a node is not an equality either engine
    /// answers, so both go back.
    fn join_tie(
        &mut self,
        expr: &BoundExpr,
        slot: usize,
        build: TableId,
    ) -> Result<Option<(Arc<JoinTable>, ScalarRef)>> {
        let BoundExpr::Binary {
            op: BinaryOp::Eq,
            lhs,
            rhs,
        } = expr
        else {
            return Ok(None);
        };
        // Whichever side reads the held pattern is the build key, and
        // it has to read only that.
        let (mine, theirs) = match (self.names_slot(lhs, slot), self.names_slot(rhs, slot)) {
            (true, false) => (lhs.as_ref(), rhs.as_ref()),
            (false, true) => (rhs.as_ref(), lhs.as_ref()),
            _ => return Ok(None),
        };
        let BoundExpr::Property { base, key } = mine else {
            return Ok(None);
        };
        if !matches!(base.as_ref(), BoundExpr::Var(s) if *s == slot) {
            return Ok(None);
        }
        // The probe side has to be readable where the join runs, which
        // means every variable in it is already a level. A side that
        // names nothing at all is a constant, and that is a predicate
        // on the held pattern rather than a join.
        let mut probes = 0;
        let mut ready = true;
        self.walk_slots(theirs, &mut |s| {
            probes += 1;
            ready &= self.slot_level.contains_key(&s);
        });
        if probes == 0 || !ready {
            return Ok(None);
        }
        let Some((col, ColType::Int)) = self.snap.resolve_col(build, key)? else {
            return Ok(None);
        };
        let Some(probe) = self.item_ref(theirs)? else {
            return Ok(None);
        };
        // Everything cheap says yes before the build reads a table, so
        // a predicate that comes back here after another join bound the
        // level it probes does not pay for the read twice.
        match probe {
            ScalarRef::Col {
                ty: ColType::Int, ..
            }
            | ScalarRef::RowId { .. } => {}
            _ => return Ok(None),
        }
        let Some(table) = self.build_join(build, col)? else {
            return Ok(None);
        };
        Ok(Some((table, probe)))
    }

    /// Places every join the predicates seen so far allow, and compiles
    /// the ones that are not joins as filters once nothing they read is
    /// still held.
    ///
    /// One join can be what lets the next predicate become one, since
    /// its probe side may read a level an earlier join built, so this
    /// runs to a fixpoint rather than once per predicate. What is left
    /// over stays where it is: the caller has more plan to walk, and
    /// the equality that settles it may not have turned up yet.
    ///
    /// `None` is a shape this pipeline has no plan for, same as
    /// everywhere else.
    fn settle(
        &mut self,
        ops: &mut Vec<Op>,
        pending: &mut Vec<(usize, TableId)>,
        waiting: &mut Vec<&BoundExpr>,
    ) -> Result<Option<()>> {
        loop {
            let mut moved = false;
            let mut i = 0;
            while i < waiting.len() {
                let expr = waiting[i];
                let held: Vec<usize> = (0..pending.len())
                    .filter(|&p| self.names_slot(expr, pending[p].0))
                    .collect();
                // Nothing it reads is held any more, so every level it
                // wants exists and it is a filter over the newest one.
                if held.is_empty() {
                    waiting.remove(i);
                    let level = self.levels.len() - 1;
                    let Some(prog) = self.build_prog(expr, level)? else {
                        return Ok(None);
                    };
                    ops.push(Op::Filter { prog });
                    moved = true;
                    continue;
                }
                let mut tied = false;
                for p in held {
                    let (slot, build) = pending[p];
                    let Some((table, key)) = self.join_tie(expr, slot, build)? else {
                        continue;
                    };
                    let to = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: build,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(slot, to);
                    if self.sip == Sip::On {
                        self.sips.push((table.clone(), key));
                    }
                    ops.push(Op::Join { table, key, to });
                    self.sip_at.insert(to, ops.len());
                    pending.remove(p);
                    waiting.remove(i);
                    tied = true;
                    moved = true;
                    break;
                }
                if !tied {
                    i += 1;
                }
            }
            if !moved {
                return Ok(Some(()));
            }
        }
    }

    /// Reads one integer column of a table into a hash table keyed by
    /// its values, carrying the row each value came from.
    ///
    /// This runs once, here, rather than per worker: the old engine's
    /// join has every worker sweep the whole side to fill its own
    /// membership set, and the whole point of the shared table is that
    /// the build happens a single time and the probe side is what
    /// scales out.
    ///
    /// A null in the column is not a key. An equality against null is
    /// null, which drops the row on both engines, so those rows are
    /// left out of the table instead of being probed and rejected.
    fn build_join(&mut self, table: TableId, col: ColId) -> Result<Option<Arc<JoinTable>>> {
        let rows = self.snap.table_rows(table)?;
        // The table is two words a row and it is built before the query
        // has returned anything, so a build side past this is not a
        // plan to run quietly: it goes back to the old engine, whose
        // nested loop is slow but bounded.
        if rows > BUILD_ROWS_MAX {
            return Ok(None);
        }
        let mut keys = Vec::with_capacity(rows as usize);
        let mut payload = Vec::with_capacity(rows as usize);
        let mut arena = MorselArena::new();
        let mut chunk = 0;
        while let Some(sc) = self.snap.scan(table, chunk, &[col], None, &mut arena)? {
            let vec = &sc.columns[0];
            let vals = vec.values::<i64>();
            for (i, &v) in vals.iter().enumerate().take(sc.rows as usize) {
                if !vec.is_valid(i) {
                    continue;
                }
                keys.push(v as u64);
                payload.push(sc.row_base + i as u64);
            }
            chunk += 1;
            arena.reset();
        }
        Ok(Some(Arc::new(JoinTable::build(&keys, &payload))))
    }

    /// Maps a projection, key, or argument expression to a scalar the
    /// sink can read: a node slot, a property column, the dense id, or
    /// a value computed from one level's columns.
    fn item_ref(&mut self, expr: &BoundExpr) -> Result<Option<ScalarRef>> {
        // The UNWIND variable a batch of seeks drives on. A row exists
        // because a key found it, so the key is a column of level 0 and
        // not something to gather back off the row: a table whose keys
        // are not its row ids would give a different answer that way.
        if let BoundExpr::Var(slot) = expr
            && self.unwind_slot == Some(*slot)
        {
            return Ok(Some(self.key_col()));
        }
        // The value a CALL yielded, which is the same story: the kernel
        // answered for the row, so it is a column of level 0.
        if let BoundExpr::Var(slot) = expr
            && self.func_slot == Some(*slot)
        {
            return Ok(Some(self.func_col()));
        }
        match expr {
            BoundExpr::Var(slot) => Ok(self
                .slot_level
                .get(slot)
                .map(|&level| ScalarRef::Node { level })),
            BoundExpr::Property { base, key } => {
                let BoundExpr::Var(slot) = base.as_ref() else {
                    return Ok(None);
                };
                let Some(&level) = self.slot_level.get(slot) else {
                    return Ok(None);
                };
                match self.register_col(level, key)? {
                    Some((vec, ty)) => Ok(Some(ScalarRef::Col { level, vec, ty })),
                    // No stored column: `.id` is the dense row id, the
                    // contract the old engine keeps; anything else is
                    // an error it owns.
                    None if key == "id" => Ok(Some(ScalarRef::RowId { level })),
                    None => Ok(None),
                }
            }
            _ => self.register_expr(expr),
        }
    }

    /// The group keys of a global `count(DISTINCT ...)`, `None` when
    /// the item is not that call.
    ///
    /// A list argument is the tuple spelled out and each element
    /// becomes a key part, which is how a query counts each unordered
    /// triple once. Anything else is the one part case, `count(DISTINCT
    /// c)` over a node or a column.
    ///
    /// The old engine skips a null argument before it reaches the set,
    /// and nothing that compiles to a key part here can be null: node
    /// and row id refs never are, and a stored column is dense, which
    /// is the same assumption `CountRef` already counts on.
    fn distinct_count_keys(&mut self, expr: &BoundExpr) -> Result<Option<Vec<ScalarRef>>> {
        let BoundExpr::Call {
            func: Func::Count,
            distinct: true,
            star: false,
            args,
        } = expr
        else {
            return Ok(None);
        };
        let [arg] = args.as_slice() else {
            return Ok(None);
        };
        let parts = match arg {
            BoundExpr::List(items) => items.as_slice(),
            other => std::slice::from_ref(other),
        };
        if parts.is_empty() {
            return Ok(None);
        }
        let mut keys = Vec::with_capacity(parts.len());
        for part in parts {
            let Some(r) = self.item_ref(part)? else {
                return Ok(None);
            };
            // A node key carries its table beside its row, and a level
            // has one table, so the table word is the same for every
            // row the query will ever hash. Counting rows counts the
            // same nodes, and it halves the key: one word decides a
            // slot on its own, where two send every probe back to the
            // stored key to compare it. Nothing reads these keys back,
            // so the node itself is never wanted.
            keys.push(match r {
                ScalarRef::Node { level } => ScalarRef::RowId { level },
                other => other,
            });
        }
        Ok(Some(keys))
    }

    /// Maps one aggregate item to its spec. Restricted to integer
    /// arguments today; string min/max and collect fall back.
    fn agg_spec(&mut self, expr: &BoundExpr) -> Result<Option<AggSpec>> {
        let BoundExpr::Call {
            func,
            distinct: false,
            star,
            args,
        } = expr
        else {
            return Ok(None);
        };
        if *star {
            return Ok(match func {
                Func::Count => Some(AggSpec::CountStar),
                _ => None,
            });
        }
        let [arg] = args.as_slice() else {
            return Ok(None);
        };
        let Some(r) = self.item_ref(arg)? else {
            return Ok(None);
        };
        // An argument off an OPTIONAL MATCH group is null on a miss,
        // and every aggregate here skips nulls: count(x) is zero over
        // one, not one, and the counting spec below would count it
        // like a star. That is the rule the old engine has and the
        // accumulators do not, so the shape goes back there.
        if self.optional_level == Some(r.level()) {
            return Ok(None);
        }
        // Same rule for a kernel that answered null for some rows: a
        // count over one is not a count of rows, and the sums and
        // extremes have nothing to skip a null with. The sink gate
        // below cannot see this one, since a bare count collapses to
        // the sink that carries no argument at all.
        if matches!(r, ScalarRef::Col { level: 0, vec, .. } if Some(vec) == self.null_func_vec()) {
            return Ok(None);
        }
        let is_int = matches!(r, ScalarRef::RowId { .. })
            || matches!(
                r,
                ScalarRef::Col {
                    ty: ColType::Int,
                    ..
                }
            );
        Ok(match func {
            Func::Count => Some(AggSpec::CountRef(r)),
            Func::Sum if is_int => Some(AggSpec::Sum(r)),
            Func::Min if is_int => Some(AggSpec::Min(r)),
            Func::Max if is_int => Some(AggSpec::Max(r)),
            Func::Avg if is_int => Some(AggSpec::Avg(r)),
            _ => None,
        })
    }

    /// Compiles a filter over one level into a kernel program, `None`
    /// when the expression needs anything beyond comparisons, boolean
    /// combinators, and integer arithmetic over this level's columns.
    fn build_prog(&mut self, expr: &BoundExpr, level: usize) -> Result<Option<Program>> {
        let mut b = ProgBuilder {
            ops: Vec::new(),
            types: Vec::new(),
        };
        let root = self.pred_reg(&mut b, expr, level)?;
        match root {
            Some(_) => Ok(Some(Program {
                ops: b.ops,
                regs: b.types.len() as Reg,
            })),
            None => Ok(None),
        }
    }

    fn pred_reg(
        &mut self,
        b: &mut ProgBuilder,
        expr: &BoundExpr,
        level: usize,
    ) -> Result<Option<Reg>> {
        let BoundExpr::Binary { op, lhs, rhs } = expr else {
            return Ok(None);
        };
        if let Some(cmp) = cmp_op(*op) {
            let Some(l) = self.value_reg(b, lhs, level, true)? else {
                return Ok(None);
            };
            let Some(r) = self.value_reg(b, rhs, level, true)? else {
                return Ok(None);
            };
            // The kernels compare within one physical type; mixed
            // int/float and mistyped compares keep old-engine
            // semantics by falling back.
            if b.types[l as usize] != b.types[r as usize] {
                return Ok(None);
            }
            let dst = b.push_type(PhysType::Bool)?;
            b.ops.push(ExprOp::Compare { op: cmp, l, r, dst });
            return Ok(Some(dst));
        }
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let Some(l) = self.pred_reg(b, lhs, level)? else {
                    return Ok(None);
                };
                let Some(r) = self.pred_reg(b, rhs, level)? else {
                    return Ok(None);
                };
                let dst = b.push_type(PhysType::Bool)?;
                let op = if matches!(op, BinaryOp::And) {
                    ExprOp::And { l, r, dst }
                } else {
                    ExprOp::Or { l, r, dst }
                };
                b.ops.push(op);
                Ok(Some(dst))
            }
            _ => Ok(None),
        }
    }

    /// One operand of a predicate, compiled into a register.
    ///
    /// `outer` says whether a property of a level below `level` may be
    /// read here. It holds directly under a comparison, where the value
    /// enters the kernel as a constant vector and a null end clears the
    /// whole column's validity, which is the answer the old engine
    /// gives. It does not hold inside arithmetic: the arith kernels do
    /// not propagate validity yet, so `a.x + 1` over a null end would
    /// come out as a number instead of null.
    fn value_reg(
        &mut self,
        b: &mut ProgBuilder,
        expr: &BoundExpr,
        level: usize,
        outer: bool,
    ) -> Result<Option<Reg>> {
        match expr {
            BoundExpr::Literal(Literal::Int(n)) => b.push_const(OwnedValue::Int(*n)).map(Some),
            BoundExpr::Literal(Literal::Float(f)) => b.push_const(OwnedValue::Float(*f)).map(Some),
            BoundExpr::Literal(Literal::Str(s)) => {
                b.push_const(OwnedValue::Str(s.as_bytes().into())).map(Some)
            }
            BoundExpr::Param(ix) => match self.params.get(*ix) {
                Some(Value::Int(n)) => b.push_const(OwnedValue::Int(*n)).map(Some),
                Some(Value::Float(f)) => b.push_const(OwnedValue::Float(*f)).map(Some),
                Some(Value::Str(s)) => b.push_const(OwnedValue::Str(s.as_bytes().into())).map(Some),
                _ => Ok(None),
            },
            // A property, or a variable that stands for a column: the
            // value a CALL yielded and the key a batch of seeks found
            // are both columns of level 0, and a variable naming a node
            // is not a value at all, which the node arm below declines.
            BoundExpr::Property { .. } | BoundExpr::Var(_) => {
                let Some(r) = self.item_ref(expr)? else {
                    return Ok(None);
                };
                let (from, mut col, ty) = match r {
                    ScalarRef::RowId { level } => (level, 0, PhysType::Int64),
                    ScalarRef::Col { level, vec, ty } => (
                        level,
                        vec,
                        match ty {
                            ColType::Int => PhysType::Int64,
                            ColType::Str => PhysType::Str,
                        },
                    ),
                    ScalarRef::Node { .. } => return Ok(None),
                };
                if from != level {
                    // A level above this one is not built yet, and a
                    // string end would have to carry its buffers into
                    // the broadcast, so both go back to the old engine.
                    if !outer || from > level || ty != PhysType::Int64 {
                        return Ok(None);
                    }
                    col = self.register_outer(level, from, col);
                }
                let Ok(col) = u8::try_from(col) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::LoadCol { col, dst });
                Ok(Some(dst))
            }
            BoundExpr::Binary { op, lhs, rhs } => {
                let Some(bin) = bin_op(*op) else {
                    return Ok(None);
                };
                let Some(l) = self.value_reg(b, lhs, level, false)? else {
                    return Ok(None);
                };
                let Some(r) = self.value_reg(b, rhs, level, false)? else {
                    return Ok(None);
                };
                let ty = b.types[l as usize];
                if b.types[r as usize] != ty || !matches!(ty, PhysType::Int64 | PhysType::Float64) {
                    return Ok(None);
                }
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::Binary { op: bin, l, r, dst });
                Ok(Some(dst))
            }
            _ => Ok(None),
        }
    }

    /// Extracts a zone pushdown from a level 0 filter: one conjunct of
    /// the form `col cmp non-negative-int`. Only Eq, Gt, and Ge (and
    /// their flipped forms) are sound here: the zones compare unsigned,
    /// so an upper bound could skip chunks whose matches are stored as
    /// negative values. The residual program still runs either way.
    fn zone_pred(&mut self, expr: &BoundExpr) -> Result<Option<ZonePred>> {
        let BoundExpr::Binary { op, lhs, rhs } = expr else {
            return Ok(None);
        };
        if matches!(op, BinaryOp::And) {
            if let Some(p) = self.zone_pred(lhs)? {
                return Ok(Some(p));
            }
            return self.zone_pred(rhs);
        }
        let (col_expr, const_expr, op) = if self.const_int(rhs).is_some() {
            (lhs, rhs, *op)
        } else if self.const_int(lhs).is_some() {
            let Some(flipped) = flip_cmp(*op) else {
                return Ok(None);
            };
            (rhs, lhs, flipped)
        } else {
            return Ok(None);
        };
        let Some(c) = self.const_int(const_expr) else {
            return Ok(None);
        };
        if c < 0 {
            return Ok(None);
        }
        // A stored property and nothing else. The zone map holds the
        // min and max of what is on disk, so a computed value has no
        // summary to answer against, and asking `item_ref` for one
        // would register a column no one reads.
        if !matches!(**col_expr, BoundExpr::Property { .. }) {
            return Ok(None);
        }
        let Some(ScalarRef::Col {
            level: 0,
            vec,
            ty: ColType::Int,
        }) = self.item_ref(col_expr)?
        else {
            return Ok(None);
        };
        let &ColSpec::Stored(col, _) = &self.levels[0].cols[vec - 1].1 else {
            unreachable!("a property registers as a stored column");
        };
        let c = c as u64;
        let (lo, hi) = match op {
            BinaryOp::Eq => (c, c),
            BinaryOp::Ge => (c, u64::MAX),
            BinaryOp::Gt => match c.checked_add(1) {
                Some(lo) => (lo, u64::MAX),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(ZonePred { col, lo, hi }))
    }

    fn const_int(&self, expr: &BoundExpr) -> Option<i64> {
        match expr {
            BoundExpr::Literal(Literal::Int(n)) => Some(*n),
            BoundExpr::Param(ix) => match self.params.get(*ix) {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Register and type bookkeeping for one program build.
struct ProgBuilder {
    ops: Vec<ExprOp>,
    types: Vec<PhysType>,
}

impl ProgBuilder {
    fn push_type(&mut self, ty: PhysType) -> Result<Reg> {
        if self.types.len() >= usize::from(Reg::MAX) {
            // A 255-register filter is not a query anyone writes; the
            // fallback handles it if one ever shows up.
            return Err(zu_common::ZuError::InvalidArgument(
                "filter expression too large".into(),
            ));
        }
        self.types.push(ty);
        Ok((self.types.len() - 1) as Reg)
    }

    fn push_const(&mut self, v: OwnedValue) -> Result<Reg> {
        let ty = match &v {
            OwnedValue::Int(_) => PhysType::Int64,
            OwnedValue::Float(_) => PhysType::Float64,
            OwnedValue::Str(_) => PhysType::Str,
            _ => PhysType::Int64,
        };
        let dst = self.push_type(ty)?;
        self.ops.push(ExprOp::LoadConst { v, dst });
        Ok(dst)
    }
}

fn cmp_op(op: BinaryOp) -> Option<CmpOp> {
    match op {
        BinaryOp::Eq => Some(CmpOp::Eq),
        BinaryOp::Ne => Some(CmpOp::Ne),
        BinaryOp::Lt => Some(CmpOp::Lt),
        BinaryOp::Le => Some(CmpOp::Le),
        BinaryOp::Gt => Some(CmpOp::Gt),
        BinaryOp::Ge => Some(CmpOp::Ge),
        _ => None,
    }
}

fn bin_op(op: BinaryOp) -> Option<BinOp> {
    match op {
        BinaryOp::Add => Some(BinOp::Add),
        BinaryOp::Sub => Some(BinOp::Sub),
        BinaryOp::Mul => Some(BinOp::Mul),
        BinaryOp::Div => Some(BinOp::Div),
        BinaryOp::Mod => Some(BinOp::Mod),
        _ => None,
    }
}

/// The key side of an `{id: k}` point predicate, the shape both engines
/// answer with a seek: `slot.id = k` or `id(slot) = k` in either operand
/// order against a constant. The old engine accepts any key expression
/// that names no slot; a literal or a parameter is what people write,
/// and a wider key only costs the seek, never an answer.
///
/// A variable comes back too, since that is what a leading UNWIND
/// leaves behind, and the caller decides whether it is the one the
/// batch of seeks binds. Every other variable fails that check and the
/// query goes back to the old engine.
fn id_point(expr: &BoundExpr, slot: usize) -> Option<&BoundExpr> {
    let BoundExpr::Binary {
        op: BinaryOp::Eq,
        lhs,
        rhs,
    } = expr
    else {
        return None;
    };
    for (side, key) in [(lhs, rhs), (rhs, lhs)] {
        let on_id = match side.as_ref() {
            BoundExpr::Property { base, key } => {
                key == "id" && matches!(base.as_ref(), BoundExpr::Var(s) if *s == slot)
            }
            BoundExpr::Call {
                func: Func::Id,
                star: false,
                args,
                ..
            } => matches!(args.as_slice(), [BoundExpr::Var(s)] if *s == slot),
            _ => false,
        };
        if on_id
            && matches!(
                key.as_ref(),
                BoundExpr::Literal(_) | BoundExpr::Param(_) | BoundExpr::Var(_)
            )
        {
            return Some(key);
        }
    }
    None
}

/// `c op x` rewritten as `x op' c`.
fn flip_cmp(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Eq => Some(BinaryOp::Eq),
        BinaryOp::Ne => Some(BinaryOp::Ne),
        BinaryOp::Lt => Some(BinaryOp::Gt),
        BinaryOp::Le => Some(BinaryOp::Ge),
        BinaryOp::Gt => Some(BinaryOp::Lt),
        BinaryOp::Ge => Some(BinaryOp::Le),
        _ => None,
    }
}

/// The table on the far side of one CSR walk.
fn far_table(schema: &Schema, rel: RelId, dir: Dir) -> Option<TableId> {
    let def = schema.rel_by_id(rel)?;
    Some(match dir {
        Dir::Fwd => def.to,
        Dir::Bwd => def.from,
    })
}

/// The same pattern edge read from its other end.
fn flip(direction: RelDirection) -> RelDirection {
    match direction {
        RelDirection::Out => RelDirection::In,
        RelDirection::In => RelDirection::Out,
        RelDirection::Undirected => RelDirection::Undirected,
    }
}

/// The traversal sides of one expand step, matching the old engine's
/// orientation checks: forward applies when the source table is the
/// rel's from side, backward when it is the to side, and an undirected
/// step over a self-referencing rel walks both, forward first.
/// The terms of a conjunction, flattened. A predicate the binder built
/// out of several is one `And` tree here, and the join tie is one leaf
/// of it rather than the whole thing.
fn conjuncts<'e>(expr: &'e BoundExpr, out: &mut Vec<&'e BoundExpr>) {
    if let BoundExpr::Binary {
        op: BinaryOp::And,
        lhs,
        rhs,
    } = expr
    {
        conjuncts(lhs, out);
        conjuncts(rhs, out);
        return;
    }
    out.push(expr);
}

fn expand_dirs(
    schema: &Schema,
    rel: RelId,
    src_table: TableId,
    direction: RelDirection,
) -> Option<Dirs> {
    let def = schema.rel_by_id(rel)?;
    let fwd =
        src_table == def.from && matches!(direction, RelDirection::Out | RelDirection::Undirected);
    let bwd =
        src_table == def.to && matches!(direction, RelDirection::In | RelDirection::Undirected);
    match (fwd, bwd) {
        (true, true) => Some(Dirs::Both),
        (true, false) => Some(Dirs::One(Dir::Fwd)),
        (false, true) => Some(Dirs::One(Dir::Bwd)),
        // No side applies: the old engine yields empty configs here;
        // rare enough to leave with the oracle.
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(from: usize, to: usize) -> Op {
        Op::Expand {
            rel: 0,
            dirs: Dirs::One(Dir::Fwd),
            from,
            to,
            batch: false,
            close: None,
        }
    }

    fn batched(ops: &[Op]) -> Vec<bool> {
        ops.iter()
            .filter_map(|op| match op {
                Op::Expand { batch, .. } => Some(*batch),
                _ => None,
            })
            .collect()
    }

    fn semi(probe_level: usize) -> Op {
        Op::Semi {
            rel: 0,
            dirs: Dirs::One(Dir::Fwd),
            probe_level,
        }
    }

    fn level(cols: Vec<ColSpec>) -> LevelBuild {
        LevelBuild {
            table: 0,
            cols: cols.into_iter().map(|c| (String::new(), c)).collect(),
        }
    }

    fn closes(ops: &[Op]) -> Vec<Option<usize>> {
        ops.iter()
            .filter_map(|op| match op {
                Op::Expand { close, .. } => Some(close.map(|c| c.probe_level)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_semi_right_above_an_expand_folds_into_it() {
        let mut ops = vec![hop(0, 1), hop(1, 2), semi(0)];
        fuse_closes(&mut ops);
        assert_eq!(ops.len(), 2, "the semi is gone from the chain");
        assert_eq!(
            closes(&ops),
            [None, Some(0)],
            "the close belongs to the expand that built the rows it judges"
        );
    }

    #[test]
    fn a_filter_between_them_keeps_the_semi_where_it_is() {
        let mut ops = vec![
            hop(0, 1),
            hop(1, 2),
            Op::Filter {
                prog: Program {
                    ops: Vec::new(),
                    regs: 0,
                },
            },
            semi(0),
        ];
        fuse_closes(&mut ops);
        assert_eq!(ops.len(), 4, "the chain is untouched");
        assert_eq!(closes(&ops), [None, None], "neither expand took the close");
    }

    #[test]
    fn a_folded_close_keeps_the_probe_levels_pin() {
        let mut ops = vec![hop(0, 1), hop(1, 2), semi(1)];
        fuse_closes(&mut ops);
        batch_expands(&mut ops, &SinkSpec::Count, &[]);
        assert_eq!(
            batched(&ops),
            [true, false],
            "the close reads level 1 through its pin, fused or not"
        );
    }

    #[test]
    fn an_expand_batches_when_nothing_above_reads_its_source() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(&mut ops, &SinkSpec::Count, &[]);
        assert_eq!(batched(&ops), [true, true], "a bare count reads no level");

        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(
            &mut ops,
            &SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 2 }],
                post: Vec::new(),
            },
            &[],
        );
        assert_eq!(
            batched(&ops),
            [true, true],
            "the projected level is the one the last expand builds, not the one it walks off"
        );
    }

    #[test]
    fn an_expand_whose_source_is_read_above_it_keeps_its_pin() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(
            &mut ops,
            &SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 1 }],
                post: Vec::new(),
            },
            &[],
        );
        assert_eq!(
            batched(&ops),
            [true, false],
            "level 1 is projected, so the expand that walks off it stays row at a time"
        );

        let mut ops = vec![
            hop(0, 1),
            hop(1, 2),
            Op::Semi {
                rel: 0,
                dirs: Dirs::One(Dir::Fwd),
                probe_level: 1,
            },
        ];
        batch_expands(&mut ops, &SinkSpec::Count, &[]);
        assert_eq!(
            batched(&ops),
            [true, false],
            "the semi join probes level 1 through its pin"
        );
    }

    #[test]
    fn a_broadcast_counts_as_a_read_of_the_level_it_comes_from() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        let levels = [
            level(Vec::new()),
            level(Vec::new()),
            level(vec![ColSpec::Outer { from: 1, vec: 0 }]),
        ];
        batch_expands(&mut ops, &SinkSpec::Count, &levels);
        assert_eq!(
            batched(&ops),
            [true, false],
            "level 2 reads level 1 at its pin, so that expand stays row at a time"
        );

        let mut ops = vec![hop(0, 1), hop(1, 2)];
        let levels = [
            level(Vec::new()),
            level(Vec::new()),
            level(vec![ColSpec::Outer { from: 0, vec: 0 }]),
        ];
        batch_expands(&mut ops, &SinkSpec::Count, &levels);
        assert_eq!(
            batched(&ops),
            [false, true],
            "reaching past a level is the business of the expand that walks off the one it names"
        );
    }

    #[test]
    fn an_aggregate_argument_counts_as_a_read() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(
            &mut ops,
            &SinkSpec::Agg {
                item_agg: vec![true],
                keys: Vec::new(),
                aggs: vec![AggSpec::Sum(ScalarRef::Col {
                    level: 1,
                    vec: 1,
                    ty: ColType::Int,
                })],
                post: Vec::new(),
            },
            &[],
        );
        assert_eq!(batched(&ops), [true, false], "sum reads level 1 per path");
    }
}
