//! Plan compiler: one supported LogicalPlan chain becomes one push
//! pipeline, anything else becomes `None` and the caller falls back to
//! the old executor.
//!
//! The supported shape today is the linear read pipeline: a single
//! plain node scan, filters, single-hop expands that walk off the
//! newest level or off a level below it, and one final Project or
//! Aggregate with its absorbed Distinct, Sort, Skip, and Limit. A hop
//! off a lower level is the second pattern branch, the shape two
//! patterns sharing a variable compile to, and it pairs every row of
//! the newest level with the whole of the pinned one. Everything the old
//! executor also covers, variable-length expands, bracketed groups,
//! closing joins, unwind, table functions, and rel values, falls back.
//! The bar for anything compiled here is exact old-engine output:
//! same rows, same order, same errors on overflow.

use std::cell::Cell;
use std::sync::Arc;

use zu_common::types::LogicalType;
use zu_common::{IdMap, Result, Temporal};
use zu_query::ast::{BinaryOp, Literal, RelDirection, SortKey, UnaryOp};
use zu_query::binder::{
    BoundClause, BoundExpr, BoundItem, BoundQuery, Cut, Func, Math, Schema, TableFunc, Trim,
};
use zu_query::exec::{Options, Sip, Value, Wcoj};
use zu_query::plan::{Bracket, BracketKind, LogicalPlan};
use zu_query::snapshot::{
    ColId, ColType, Dir, FuncCol, RelId, SCAN_ROWS, Snapshot, TableId, TemporalLane, ZonePred,
};
use zu_vector::{
    BinOp, CmpOp, ExprOp, MathOp, MathPair, MorselArena, OwnedValue, PhysType, Program, Reg,
    StrCut, StrFold, StrLen, StrNorm, StrTrim, TrimSet,
};

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
    /// The values [`ScalarRef::Const`] names, one entry per constant
    /// the sink writes into its rows.
    pub consts: Vec<Value>,
    /// How to run this plan again on another set of parameters, `None`
    /// when there is no way to and it has to be compiled again.
    pub reuse: Option<Reuse>,
}

/// Where the parameters a compile read ended up in the plan it built,
/// which is what lets the plan be run again with other values in those
/// positions instead of compiled again.
///
/// A plan is only reusable if every parameter the compiler read landed
/// somewhere this can name. That is the whole of the check and it is
/// why the reads are counted rather than the holes: a parameter that
/// steered a decision, picked how many seeks a batch has or how far a
/// SKIP goes, leaves no hole and its bit stays uncovered, so the plan
/// is not offered for reuse and the next set of parameters compiles
/// its own. Nothing has to enumerate those cases or stay in step with
/// them as they change.
pub(crate) struct Reuse {
    /// Where the parameters landed, in no particular order.
    holes: Vec<Hole>,
}

/// One place in a compiled plan that a parameter's value was written
/// into, and the position it came from.
pub(crate) enum Hole {
    /// The primary key of [`Source::Seek`].
    SeekKey { param: usize },
}

impl Reuse {
    /// The bit of parameter `ix`, or every bit for one too far out to
    /// have its own. A plan reads a handful of parameters, so the far
    /// out case is a query written by a generator and not a hot path;
    /// giving it every bit costs it reuse and costs nothing to decide.
    fn bit(ix: usize) -> u64 {
        if ix < u64::BITS as usize {
            1 << ix
        } else {
            u64::MAX
        }
    }

    /// The reuse record of a finished compile, `None` when a parameter
    /// it read is not one of `holes`.
    fn of(read: u64, holes: Vec<Hole>) -> Option<Reuse> {
        let covered = holes.iter().fold(0u64, |acc, h| {
            acc | match h {
                Hole::SeekKey { param } => Reuse::bit(*param),
            }
        });
        (read & !covered == 0).then_some(Reuse { holes })
    }
}

impl ExecPlan {
    /// Writes `params` into the holes of a reusable plan, `false` when
    /// they do not fit and the caller has to compile instead.
    ///
    /// Not fitting is a parameter of the wrong type, since what a hole
    /// records is where a value went and not that any value goes
    /// there: `$id` bound to a string compiles to no seek at all, and
    /// the check here is the same one the compiler made, in the same
    /// order, so a set of parameters that would have been refused is
    /// refused rather than run against the last set's plan.
    pub fn restamp(&mut self, params: &[Value]) -> bool {
        let Some(reuse) = &self.reuse else {
            return false;
        };
        for hole in &reuse.holes {
            match hole {
                Hole::SeekKey { param } => {
                    let Some(Value::Int(n)) = params.get(*param) else {
                        return false;
                    };
                    let Ok(key) = u64::try_from(*n) else {
                        return false;
                    };
                    self.source = Source::Seek(key);
                }
            }
        }
        true
    }
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
    /// Whether the walk that builds this level has to carry the
    /// ordinal of every edge it stepped over, which it does when a
    /// column here reads one. The ordinals cost a second read of the
    /// list, so a level nothing asks it of does not pay for them.
    pub ords: bool,
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
    /// A property of the edge the walk stepped over to reach the row,
    /// gathered off the rel table by the edge's ordinal rather than
    /// off the node table by the row.
    ///
    /// It sits on the level the walk built because that is where the
    /// edges are: one per row of it, in the positions the rows landed
    /// in. A pair with several edges between it walks once per edge,
    /// so the copies are separate rows here and each reads its own
    /// value.
    RelStored(RelId, ColId, ColType),
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
    /// What an EXISTS block written under an OR answered, for every row
    /// of the level it was written on: one where the row has an edge of
    /// this rel, zero where it has none, with a NOT in front of the
    /// block folded in. The predicate around the block reads it back as
    /// an ordinary column, which is the whole point of a mark: the row
    /// survives either way and carries the answer instead of being
    /// decided by it.
    ///
    /// It is an integer column rather than a boolean one because the
    /// kernels pack booleans into bits and a bit column has no width to
    /// gather into; a compare against zero costs one pass and needs no
    /// new load.
    Mark {
        rel: RelId,
        dirs: Dirs,
        negated: bool,
    },
    /// The same answer where the block is tied to the pipeline by an
    /// equality rather than by a shared variable: whether the join's
    /// build side holds this row's key, one word of the directory per
    /// row and no payload read at all. `key` reads the level this
    /// column sits on, and it is registered after the column it reads,
    /// so one pass over the list still builds the chunk.
    JoinMark {
        table: Arc<JoinTable>,
        key: ScalarRef,
        negated: bool,
    },
    /// The same answer where the block asked more than the two above
    /// can: a predicate on the far node, a predicate over the held
    /// pattern. That has to be run as a group, once per row, so nothing
    /// fills this column while the chunk is built. The bracket that owns
    /// it writes a row of it as the group answers for that row, and by
    /// the time the group has been round the vector the column holds
    /// what the block said about every row of it.
    GroupMark,
}

/// Traversal sides of one expand: `Both` is an undirected step over a
/// self-referencing rel, forward list first, matching the old engine's
/// emission order.
#[derive(Clone, Copy, PartialEq, Eq)]
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
        /// Whether the closings of several seed rows ride down together
        /// instead of one vector per seed row. Set by `batch_walks` on
        /// the same rule an expand batches on, and it matters more
        /// here: a wedge closes about twice on a social graph, so
        /// without this the pipeline under the close runs on two rows
        /// at a time instead of the 2048 it is written for.
        batch: bool,
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
    /// Top of a bracket (docs/07): the `len` ops above this one are the
    /// group, the last of them being `BracketHit`, and everything past
    /// that is the pipeline the group feeds. The group runs once per
    /// row of the level below it, with that row pinned, which is what
    /// makes a match a per-row fact.
    ///
    /// What the outer row gets out of that depends on the kind. An
    /// OPTIONAL MATCH hands every match down and a row that matched
    /// nothing goes on once with `level` bound to a null row. An
    /// EXISTS block hands nothing down: the group stops at its first
    /// match and the outer row goes on once, on a match for a semi
    /// bracket and on a miss for an anti one, carrying the same null
    /// row since nothing above the block is allowed to read what it
    /// bound.
    ///
    /// A mark bracket hands nothing down either, and decides nothing:
    /// it writes what the group said into `mark`, the level and the
    /// chunk vector of the `GroupMark` column, a row at a time as the
    /// group answers. The pipeline past it runs once the group has
    /// been round the whole vector, on that vector, since every row of
    /// it is still there and the group's own level is gone by then.
    Bracket {
        len: usize,
        level: usize,
        kind: BracketKind,
        mark: Option<(usize, usize)>,
    },
    /// An EXISTS block over a bare pattern (docs/07): whether the row
    /// has an edge of this kind at all, which the CSR offsets answer
    /// on their own. No group runs and no level is built, so this is a
    /// filter over the level the block was written on and a hub costs
    /// what a leaf costs. `negated` is the anti bracket, where having
    /// one is what drops the row.
    ///
    /// `from` is the newest level where the block was written on the
    /// level the pipeline is standing on, and a lower one where it was
    /// written on a level the pipeline has walked past. That second
    /// one is a question about the row that level's pin holds, so the
    /// answer is one degree read and it decides for the whole vector:
    /// every row in hand came off that one row.
    HasEdge {
        rel: RelId,
        dirs: Dirs,
        from: usize,
        negated: bool,
    },
    /// Bottom of the bracket, reached only when the group produced a
    /// row. Recording the match is all it does. Under an OPTIONAL the
    /// pipeline below runs off it as well, since that match is a row;
    /// under an EXISTS block it is the end of the group's work for
    /// this outer row.
    BracketHit { kind: BracketKind },
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
    /// A held pattern the query pinned to rows of its own rather than
    /// to the pipeline: two patterns with no variable and no equality
    /// between them, each picked out by a predicate of its own, which
    /// is how a statement names the two ends of an edge it is about to
    /// write. That is a cross product, and the rows on this side of it
    /// are settled while the plan is compiled, by the key index where
    /// the predicate is on the pattern's id and by one zone pushed scan
    /// where it is on an integer column. Every row of the level below
    /// pairs with all of them, which is the same rows in the same order
    /// the old engine's nested loop produces, without its scan of the
    /// whole table per outer row.
    Product { rows: Arc<Vec<u64>>, to: usize },
    /// The sideways pass (perf/13 section 1): what a join's build side
    /// knows about its keys, applied to the level its probe reads, at
    /// the position that level is made. Everything between here and the
    /// join then runs on the rows that can still match, which today is
    /// the predicates over the level and the probe itself, and is a
    /// walk off it as soon as the optimizer puts one there.
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
    /// A value the query wrote or bound, held on the plan at `at` and
    /// copied into every row. It reads nothing off the row, which is
    /// what makes it the one ref with no level.
    Const { at: usize },
}

/// Whether two refs read the same thing. A ref carries its column type
/// along for the reader, and two refs at one position on one level are
/// the same column whatever either of them says the type is.
fn same_ref(a: ScalarRef, b: ScalarRef) -> bool {
    match (a, b) {
        (ScalarRef::Node { level: x }, ScalarRef::Node { level: y })
        | (ScalarRef::RowId { level: x }, ScalarRef::RowId { level: y }) => x == y,
        (
            ScalarRef::Col {
                level: x, vec: i, ..
            },
            ScalarRef::Col {
                level: y, vec: j, ..
            },
        ) => x == y && i == j,
        (ScalarRef::Const { at: x }, ScalarRef::Const { at: y }) => x == y,
        _ => false,
    }
}

impl ScalarRef {
    /// The level this reads off. A constant reads off none, and says so
    /// with a level no plan holds, so every `== level` test about it is
    /// false and anything that would index a chunk with it panics
    /// rather than reading the wrong one.
    pub(crate) fn level(&self) -> usize {
        match *self {
            ScalarRef::Node { level }
            | ScalarRef::RowId { level }
            | ScalarRef::Col { level, .. } => level,
            ScalarRef::Const { .. } => usize::MAX,
        }
    }
}

pub(crate) enum AggSpec {
    CountStar,
    /// count(x) for x that cannot be null here: dense property columns
    /// and unbracketed nodes. Counts exactly like star.
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
    /// A `WITH ... WHERE` over the groups rather than over the rows
    /// that made them, which is what a HAVING is.
    Having(PostPred),
    /// A second grouping over what the step below emitted: the columns
    /// it groups by, what it accumulates, and where each of the two
    /// lands in the row it writes. This is how a `count(DISTINCT x)`
    /// per group answers, the argument having joined the sink's own
    /// key, and how a clause that aggregates the groups again does.
    Regroup {
        keys: Vec<usize>,
        aggs: Vec<PostAgg>,
        item_agg: Vec<bool>,
    },
    /// The columns the answer is written from, in written order. A
    /// stage above the sink reads columns the answer does not carry,
    /// so this is what cuts the row back down to the clause.
    Emit(Vec<usize>),
}

/// A HAVING-style predicate over the columns the step below emits.
///
/// There are as many groups as the query grouped into and every value
/// in one is already boxed, so this is a small tree walked per row
/// rather than another vector program. What it covers is a column
/// against a constant and the two combinators, which is how a HAVING
/// is written; anything else falls back.
pub(crate) enum PostPred {
    /// `column op constant`, for the orderings and equality. An
    /// inequality is left out on purpose: this answers "unknown" where
    /// the two sides do not compare, an unknown drops the group, and
    /// for every operator here that is what the old engine does too.
    Cmp(BinaryOp, usize, Value),
    And(Vec<PostPred>),
    Or(Vec<PostPred>),
}

/// An aggregate of a second grouping stage, over the columns the step
/// below it emitted.
pub(crate) enum PostAgg {
    /// `count(*)`, and `count(x)` for the column x names, which counts
    /// the values that are not null.
    Count(Option<usize>),
    Sum(usize),
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
        Op::Bracket { level: opt, .. } => *opt == level,
        Op::DegreeProduct { from, .. } => *from == level,
        Op::Join { key, to, .. } => key.level() == level || *to == level,
        // The rows are settled, so the only level a product names is
        // the one it builds out of them.
        Op::Product { to, .. } => *to == level,
        Op::Sip { key, .. } => key.level() == level,
        Op::HasEdge { from, .. } => *from == level,
        Op::Filter { .. } | Op::BracketHit { .. } => false,
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
            // sideways one, which are the same read of the same rows.
            // A bare block reads rows the same way but says which
            // level it means, so it is names_level's business and not
            // this one's.
            Op::Filter { .. } | Op::Sip { .. } | Op::Semi { .. } | Op::Intersect { .. } => {
                return true;
            }
            Op::Expand { .. } | Op::Branch { .. } | Op::Join { .. } | Op::Product { .. } => {
                return false;
            }
            Op::Bracket { .. }
            | Op::BracketHit { .. }
            | Op::HasEdge { .. }
            | Op::DegreeProduct { .. } => {}
        }
    }
    false
}

/// Marks the walks that may descend on whole vectors.
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
/// The WCOJ close is the same walk under a different name and takes the
/// same treatment, on a worse starting point: a wedge on a social graph
/// closes on about two nodes, so the row at a time descent there hands
/// the pipeline two rows and pays a whole level build for them.
///
/// Only the walk's own source level matters. Levels under it stay
/// pinned by the expands that built them, so a probe or a projection
/// reaching past this one is none of this decision's business.
fn batch_walks(ops: &mut [Op], sink: &SinkSpec, levels: &[LevelBuild]) {
    for i in 0..ops.len() {
        // An intersection seeds off the level right under the one it
        // builds, always, which is what makes its close a close. Levels
        // are already renumbered by the time this runs, so that level
        // is `to - 1` and no dropped one sits between them.
        let from = match ops[i] {
            Op::Expand { from, .. } => from,
            Op::Intersect { to, .. } => to - 1,
            _ => continue,
        };
        let close = match ops[i] {
            Op::Expand { close, .. } => close,
            _ => None,
        };
        // The hub weight is the one thing that reads a source level
        // without needing its pin: when everything above this expand is
        // a filter and then the degree product off this expand's own
        // source, the runner takes one degree per source row before it
        // descends and carries the weights down with the rows. Only the
        // expand does that, so only the expand gets the exemption.
        let hub = matches!(ops[i], Op::Expand { .. })
            && matches!(ops.last(), Some(Op::DegreeProduct { from: f, .. }) if *f == from)
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
            // A block on a level the pipeline has walked past is the
            // same read: whether that one pinned row has an edge.
            Op::HasEdge { from: src, .. } => *src == from,
            _ => false,
        };
        // The expand's own fused close counts: it reads the probe
        // level through that level's pin like a standalone semi does.
        let probed = close.is_some_and(|c| c.probe_level == from) || ops[i + 1..].iter().any(reads);
        let whole = !probed && !sink_reads(sink, from) && !outer_reads(levels, from);
        if let Op::Expand { batch, .. } | Op::Intersect { batch, .. } = &mut ops[i] {
            *batch = whole;
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
        let Op::Expand {
            from, to, close, ..
        } = &mut ops[i]
        else {
            unreachable!("matched an expand just above");
        };
        // A semi probing the level the expand is building would have to
        // read rows that do not exist yet. Validation rejects that
        // shape, and this pass leaves it alone rather than relying on
        // the order the two run in.
        //
        // One probing the level the expand walks off is the same
        // question a step further down: that level is the vector the
        // walk is reading, so it has no one row pinned while the walk
        // runs and the probe has nothing to read either. It stays an
        // operator of its own, where it runs per row after the walk has
        // built one. This is the shape a pattern list writes when two
        // of its patterns join both of their ends, `(a)-[e]->(b),
        // (a)-[f]->(b)`.
        if probe_level == *to || probe_level == *from {
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

/// Takes the projections that compute nothing out of the chain.
///
/// A `WITH n, d` in the middle of a query is not an operator: every
/// item is a variable the pipeline already carries, and the binder
/// keeps a projected variable's slot, so the clauses above it read the
/// same slots the clauses below it wrote. What the clause does do is
/// narrow what is visible, and that is a rule the binder enforced
/// before a plan existed. Dropping the node leaves the shape this
/// pipeline runs where there was a projection standing between an
/// operator and its sink.
///
/// The last node stays whatever it is: that one is the sink, and the
/// sink is the answer. An item that computes, aggregates or renames
/// into a fresh slot is a projection in earnest and stays too, which
/// leaves the query to the old engine.
fn drop_pass_through(chain: &mut Vec<&LogicalPlan>) {
    let last = chain.len().saturating_sub(1);
    let mut i = 0;
    chain.retain(|node| {
        let here = i;
        i += 1;
        if here == last {
            return true;
        }
        let LogicalPlan::Project { items, .. } = node else {
            return true;
        };
        let pass_through = !items.is_empty()
            && items
                .iter()
                .all(|item| matches!(item.expr, BoundExpr::Var(slot) if item.slot == Some(slot)));
        !pass_through
    });
}

/// Whether a node above the sink only narrows the answer the sink
/// wrote, rather than being a clause of its own over it. A window is
/// what the sink absorbs; anything else is a stage.
fn is_window(node: &LogicalPlan) -> bool {
    matches!(
        node,
        LogicalPlan::Distinct { .. }
            | LogicalPlan::Sort { .. }
            | LogicalPlan::Skip { .. }
            | LogicalPlan::Limit { .. }
    )
}

/// Whether a ref can stand as a group key.
///
/// A float cannot. A key here is packed as the bytes the value is
/// stored as, and the row at a time engine compares floats by their
/// order, which puts `0.0` and `-0.0` in one group and every NaN in
/// one group. Neither is a group anybody writes on purpose, and a
/// stored column can hold both, so the grouping goes back to the
/// engine that already agrees with itself about them.
/// A temporal value as a constant of its lane, `None` for a zoned one,
/// which is two numbers and does not fit a lane. The lane comes back
/// alongside, since a register that holds the constant carries it the
/// same way a register that read a column does.
fn lane_const(t: &Temporal) -> Option<(OwnedValue, TemporalLane)> {
    let lane = TemporalLane::of(&t.logical_type())?;
    let word = lane.word(t)?;
    Some((
        OwnedValue::Lane {
            phys: lane.phys(),
            word,
        },
        lane,
    ))
}

fn keyable(r: ScalarRef) -> bool {
    !matches!(
        r,
        ScalarRef::Col {
            ty: ColType::Float,
            ..
        } | ScalarRef::Const { .. }
    )
}

/// Whether two levels hold one element, as far as the tables can say
/// on their own. `None` is the pair the rows decide.
///
/// A level has one table for every row it will ever produce, so two
/// levels on different tables hold different nodes whatever their rows
/// turn out to be, and two names for one level hold the one node. What
/// is left is two levels on one table, and there the rows are the whole
/// of the answer.
fn settled_pair(levels: &[LevelBuild], l: usize, r: usize) -> Option<bool> {
    if l == r {
        return Some(true);
    }
    (levels[l].table != levels[r].table).then_some(false)
}

/// Whether anything above the sink reads column `at` of the row the
/// sink writes. Only the stages are asked: a window step indexes the
/// row the stage below it emitted, which is a different row and a
/// different numbering.
fn answers(post: &[PostSpec], at: usize) -> bool {
    post.iter().any(|step| match step {
        PostSpec::Having(pred) => pred_reads(pred, at),
        PostSpec::Emit(cols) => cols.contains(&at),
        PostSpec::Regroup { keys, aggs, .. } => {
            keys.contains(&at)
                || aggs.iter().any(|agg| match agg {
                    PostAgg::Count(col) => *col == Some(at),
                    PostAgg::Sum(col) => *col == at,
                })
        }
        _ => false,
    })
}

fn pred_reads(pred: &PostPred, at: usize) -> bool {
    match pred {
        PostPred::Cmp(_, col, _) => *col == at,
        PostPred::And(parts) | PostPred::Or(parts) => parts.iter().any(|part| pred_reads(part, at)),
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
    // A value query expression is answered by running a query of its
    // own before this plan starts, which is a thing the old engine
    // does and this one has no place for yet, so the whole statement
    // goes back there (GQ18).
    if !query.scalars.is_empty() {
        return Ok(None);
    }
    // A binding variable is the same story from the other side (GP05
    // through GP17): the definition is a query run before this plan
    // starts and its answer is written into a parameter position, so an
    // engine that does not run it reads a position the caller left
    // null. Reading it is what makes this worth declining rather than
    // leaving alone: `VALUE t = 3 MATCH (p:Person) RETURN t AS t`
    // compiles here, the null goes on the plan as a constant, and the
    // answer comes out null instead of three.
    if !query.bindings.is_empty() {
        return Ok(None);
    }
    let mut c = Compiler {
        query,
        schema,
        snap,
        params,
        wcoj: options.wcoj,
        sip: options.sip,
        levels: Vec::new(),
        slot_level: IdMap::default(),
        rel_level: IdMap::default(),
        sips: Vec::new(),
        sip_at: IdMap::default(),
        optional_level: None,
        exists_level: None,
        marked: None,
        unwind_slot: None,
        func_slot: None,
        func: None,
        marks: IdMap::default(),
        consts: Vec::new(),
        every_row_answers: false,
        params_read: Cell::new(0),
        holes: Vec::new(),
    };
    c.compile(plan)
}

/// Rows a value join will read into a build table. Sixteen bytes a row
/// go into the table itself, so this is a few hundred megabytes at the
/// ceiling, and a side larger than it falls back rather than building
/// something that size before the query has answered anything.
const BUILD_ROWS_MAX: u64 = 50_000_000;

/// Rows a pinned held pattern may settle on. The list rides in the plan
/// and pairs with every row under it, so a predicate that picks out
/// more than a vector of rows is a cross product wide enough to belong
/// on the old engine instead.
const PIN_ROWS_MAX: usize = 2048;

/// A level under construction: the registry assigns chunk vector
/// positions as columns are demanded, so programs built mid-walk hold
/// stable indices.
struct LevelBuild {
    table: TableId,
    /// The property name a stored column answers to, so a second
    /// reader of it reuses the position instead of gathering twice.
    /// Computed columns carry no name: nothing looks them up by one.
    /// An edge column shares the namespace and is told apart by its
    /// spec, since a node and the edge that reached it may well both
    /// have a `ts`.
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
    slot_level: IdMap<usize, usize>,
    /// Where a rel variable's edges are: the level the walk over that
    /// rel built, and the rel it walked. A property of the variable is
    /// then a column of that level, gathered by ordinal. Only a plain
    /// single hop registers one, so every other shape that names an
    /// edge still declines.
    rel_level: IdMap<usize, (usize, RelId)>,
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
    /// runs. Levels made inside a bracket are left out on purpose,
    /// since dropping a row of one of those is dropping a match the
    /// bracket has to keep as a miss.
    sip_at: IdMap<usize, usize>,
    /// The level an OPTIONAL MATCH group introduced, once one is open.
    /// It is the level that binds null on a miss, so the sink is held
    /// to what it can answer with a null there, and nothing else
    /// compiles after the group closes.
    optional_level: Option<usize>,
    /// The level an EXISTS block introduced, once one is open. The
    /// block's own variables are out of scope above it, so nothing
    /// reads this one and the sink is free of it; it is still a level
    /// the pipeline holds a chunk for, which is what the rules
    /// [`Compiler::bracketed`] gathers are about.
    exists_level: Option<usize>,
    /// The level a mark group left the pipeline standing on, once one
    /// has compiled. A mark group binds nothing and decides nothing, so
    /// the level it walked is gone by the time the rest of the pipeline
    /// runs and this is the level below it, which is where a predicate
    /// that reads the mark compiles. Nothing that makes a level goes
    /// after one, since the number the next level would take is the one
    /// the group's own level holds.
    marked: Option<usize>,
    /// The variable a leading UNWIND bound, once its list has become
    /// the batch of seeks that drives the plan. Reading it is reading
    /// the key that found the row, which is level 0's key column.
    unwind_slot: Option<usize>,
    /// The value variable a leading CALL yielded. Reading it is reading
    /// level 0's func column, the kernel's answer for that row.
    func_slot: Option<usize>,
    /// That kernel's answer, held until it goes on the plan.
    func: Option<FuncCol>,
    /// One bit per parameter position [`Self::param`] has been asked
    /// for, and where the ones that went straight into the plan went.
    /// Together they say whether the plan can be run again on other
    /// parameters; see [`Reuse`].
    params_read: Cell<u64>,
    holes: Vec<Hole>,
    /// Where each mark slot the binder made landed: the level its block
    /// was written on and the chunk vector position of the column that
    /// holds the answer. A predicate naming the slot compiles into a
    /// read of that column.
    marks: IdMap<usize, (usize, usize)>,
    /// The constants the sink writes, in the order they were asked for.
    consts: Vec<Value>,
    /// Whether every row the pipeline builds reaches the sink, so a
    /// computed column filled where the level is built is filled for
    /// exactly the rows the old engine projects.
    ///
    /// It is false while the pipeline is still growing and settled once
    /// the operators are known, which is before the sink compiles and
    /// therefore before anything reads it. A plan with no operator over
    /// the driving level and no slice above the sink is the shape it
    /// holds for: nothing drops a row between the level and the answer,
    /// so a function that can have no answer for a row is asked about
    /// the same rows on both engines and the two either raise together
    /// or agree.
    every_row_answers: bool,
}

impl Compiler<'_> {
    /// Whether a bracket is open. A group answers per outer row and the
    /// runner drives it off the pinned row of the level below, so the
    /// rules that hold for one hold for all three kinds: the pipeline
    /// ends where the group starts, nothing inside it batches, no walk
    /// inside it is fused into a degree read, and every expand still
    /// walks off the newest level rather than becoming a branch.
    fn bracketed(&self) -> bool {
        self.optional_level.is_some() || self.exists_level.is_some()
    }

    /// Whether a level is one a bracket already compiled here walks.
    /// The runner binds such a level to a single null row while it runs
    /// what stands past the bracket, so an operator that reads the
    /// level itself there is reading that null and not the row the
    /// group matched.
    fn walks_a_bracket_level(&self, level: usize) -> bool {
        self.optional_level == Some(level) || self.exists_level == Some(level)
    }

    /// The level the pipeline is standing on, which is where a
    /// predicate written after everything compiled so far reads its
    /// columns. That is the newest level everywhere except past a mark
    /// group, whose level the runner has popped by then.
    fn standing(&self) -> usize {
        self.marked.unwrap_or(self.levels.len() - 1)
    }

    fn compile(&mut self, plan: &LogicalPlan) -> Result<Option<ExecPlan>> {
        let mut chain = Vec::new();
        let mut cur = plan;
        loop {
            chain.push(cur);
            cur = match cur {
                LogicalPlan::Empty => break,
                // A write is not a shape this engine claims. The
                // elements arrive as arguments and binding one is an
                // unwind of a single value, which is exactly the kind
                // of source the pipeline does not have, so the whole
                // statement goes back to the old engine.
                LogicalPlan::Insert { .. }
                | LogicalPlan::Set { .. }
                | LogicalPlan::Delete { .. }
                | LogicalPlan::Rows { .. } => {
                    return Ok(None);
                }
                // A composite is two pipelines and an operator over
                // the pair, and this engine compiles one pipeline. The
                // statement goes back to the engine that runs the
                // operands and meets them.
                // A fork is one plan per way rather than a pipeline,
                // and the session runs each way as a part, so this
                // node never stands in front of a run.
                LogicalPlan::Conjoin { .. } | LogicalPlan::Fork { .. } => return Ok(None),
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
        drop_pass_through(&mut chain);
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
        if let Some(LogicalPlan::Unwind {
            expr,
            slot,
            ordinal,
            ..
        }) = it.peek()
        {
            // A counter is a second column this source does not make,
            // since the list here becomes the keys of a scan rather
            // than rows of its own, so the statement goes back whole.
            if ordinal.is_some() {
                return Ok(None);
            }
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
            // A traversal kernel is given a node id and walks from the
            // row behind it, the resolution the old engine does before
            // it calls the kernel. A key that names no node is an error
            // that engine owns, so an unresolved one falls back rather
            // than being answered here.
            if matches!(func, TableFunc::Bfs | TableFunc::Sssp) {
                let Some(row) = self.seek_arg(*table, vals.first().copied())? else {
                    return Ok(None);
                };
                vals[0] = row;
            }
            let Some(col) = self.snap.table_function(func.name(), *rel, &vals)? else {
                return Ok(None);
            };
            // A kernel answers for the rel table's node domain, which
            // is what the adjacency was built over, and the node table
            // may have grown since: a node inserted after the edges
            // were loaded is in the table and not in the domain. The
            // old engine yields what the kernel answered and stops
            // there, so a scan of the whole table beside a shorter
            // answer is a shape this engine does not have, and it
            // hands the statement back rather than reading past the
            // end of the column.
            if col.values.len() < self.snap.table_rows(*table)? as usize {
                return Ok(None);
            }
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
                bracket: None,
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
                bracket: None,
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
            bracket: None,
            ..
        }) = it.peek()
            && let Some(key) = id_point(expr, scan_slot)
        {
            let Some(k) = self.const_int(key).and_then(|k| u64::try_from(k).ok()) else {
                return Ok(None);
            };
            // A key that came from a parameter is the one place a
            // point read differs from run to run, so the plan records
            // where it went and the next read writes its own key there
            // rather than compiling the whole plan again.
            if let BoundExpr::Param(ix) = key {
                self.holes.push(Hole::SeekKey { param: *ix });
            }
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
    fn rest<'p, I: Iterator<Item = &'p LogicalPlan> + Clone>(
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
            // An open bracket all but ends the pipeline: the group's
            // level is the newest one and it may be null, so nothing
            // walks off it or filters on it here. Whatever is left goes
            // to the sink match below, which takes a projection or an
            // aggregate and sends anything else back to the old engine.
            //
            // A second block over a bare pattern is the exception, and
            // it is a common enough way to write two of them. What it
            // reads is a pinned row and the degrees under it, never the
            // newest level, so the null the group left there is nothing
            // to it. The arm below settles whether the block really is
            // that shape and falls back where it is not, so this only
            // has to be right about what is worth trying.
            if self.bracketed() && !bare_block(&mut it.clone()) {
                break;
            }
            // Past a mark group the pipeline stands on the level below
            // the newest one, and the only thing left that compiles
            // there is a predicate, which is what the mark was written
            // for. Anything that makes a level would take the number
            // the group's level is holding.
            if self.marked.is_some()
                && !matches!(it.peek(), Some(LogicalPlan::Filter { bracket: None, .. }))
            {
                break;
            }
            match it.peek() {
                Some(LogicalPlan::Filter {
                    expr,
                    bracket: None,
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
                    let level = self.standing();
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
                    bracket: None,
                    ..
                }) => {
                    it.next();
                    let &[build] = self.query.variables[*slot].node_tables.as_slice() else {
                        return Ok(None);
                    };
                    pending.push((*slot, build));
                }
                // A bracketed pattern that shares no variable with the
                // pipeline and is tied to it by an equality. Where a
                // hop would walk, this probes a table built off the
                // other side, and what the bracket does with the answer
                // is the kind's business: an OPTIONAL MATCH is a left
                // join, binding the level to one null row where the
                // probe finds nothing instead of dropping the outer
                // row, and an existence block is the semi or the anti,
                // where the outer row is what survives and the probe
                // only decides whether it does.
                //
                // The bracket is the same one a hop uses. What sits
                // under it is a join rather than an expand, and the
                // group's other predicates sit between the two, so a
                // row whose only matches they reject is a miss and not
                // a dropped row.
                Some(LogicalPlan::ScanNodes {
                    slot,
                    bracket: Some(group),
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
                        bracket: Some(g),
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
                    // Without one the bracketed pattern is a cross
                    // product against the whole of the other side, and
                    // the old engine's nested loop is where that
                    // belongs.
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
                    // The mark a join writes. A block under an OR cannot
                    // decide the row it was written about, so the answer
                    // is written down per row instead of acted on, and
                    // here the answer is whether the build side holds
                    // the row's key, which the table knows already. So
                    // it is a column of the level the key is read off
                    // and no operator at all, the same shape a bare
                    // block's degree read takes and for the same reason.
                    //
                    // Only a block with nothing else in it is answered
                    // this cheaply. Another predicate over the held
                    // pattern decides which build rows count, and there
                    // is nothing in the table that answers that, so it
                    // goes on to the group below, which runs the probe
                    // and the predicate per outer row and writes down
                    // what it found instead of acting on it.
                    let mut mark = None;
                    if let BracketKind::Mark {
                        slot: mark_slot,
                        negated,
                    } = group.kind
                    {
                        if group_filters.is_empty() {
                            let level = key.level();
                            let vec = self.register_join_mark(level, table, key, negated);
                            self.marks.insert(mark_slot, (level, vec));
                            continue;
                        }
                        let held = !pending.is_empty() || !waiting.is_empty();
                        let Some(at) = self
                            .levels
                            .len()
                            .checked_sub(1)
                            .and_then(|outer| self.group_mark(mark_slot, outer, held))
                        else {
                            return Ok(None);
                        };
                        mark = Some(at);
                    }
                    let to_level = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: build,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(slot, to_level);
                    // Only the semi passes its keys sideways. What the
                    // filter says about an outer row is that no build
                    // key can match it, and only under a semi is that
                    // the same as dropping the row: the anti keeps
                    // exactly those rows and the left join carries them
                    // on with a null bound to the level, so a filter
                    // there would be answering the query rather than
                    // narrowing it.
                    if self.sip == Sip::On && group.kind == BracketKind::Semi {
                        self.sips.push((table.clone(), key));
                    }
                    let head = ops.len();
                    ops.push(Op::Bracket {
                        len: 0,
                        level: to_level,
                        kind: group.kind,
                        mark,
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
                    ops.push(Op::BracketHit { kind: group.kind });
                    let len = ops.len() - head - 1;
                    let Op::Bracket { len: slot, .. } = &mut ops[head] else {
                        unreachable!("just pushed the bracket");
                    };
                    *slot = len;
                    self.close_bracket(group.kind, to_level, mark);
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
                    bracket: None,
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
                    // The edges this hop steps over are one per row of
                    // the level it makes, so a property of the rel
                    // variable is a column of that level. Registering
                    // it here is what lets a query that reads
                    // `t.ts` compile at all.
                    self.rel_level.insert(*rel, (to_level, rel_id));
                    ops.push(Op::Expand {
                        rel: rel_id,
                        dirs,
                        from: src,
                        to: to_level,
                        batch: false,
                        close: None,
                    });
                    // A join probing off this level wants its filter
                    // here, where the level is made, and the walk is
                    // what makes it.
                    self.sip_at.insert(to_level, ops.len());
                }
                // A bracketed hop, in the one shape the bracket covers:
                // a single hop introducing the far node, with the
                // group's own filters over it. That is what an
                // OPTIONAL MATCH is in nearly every query that has
                // one, and it is the whole of an EXISTS block over a
                // pattern, and it is where those queries used to go
                // back to the old executor.
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: false,
                    bracket: Some(group),
                    ..
                }) => {
                    let kind = group.kind;
                    let group_id = *group;
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
                    // An EXISTS block over a bare pattern asks whether
                    // the row has an edge, and the CSR offsets answer
                    // that: nothing inside the block reads the far
                    // node, so there is no level to build and no group
                    // to run per outer row. The block is a filter over
                    // the level it was written on, which is the whole
                    // of it on a hub, where the walk it replaces would
                    // have read a list to look at its first entry.
                    //
                    // Bare means the group ends at this hop: another
                    // operator inside it, a second step or a predicate
                    // on the far node, is a group and goes below. Which
                    // level the block was written on does not have to
                    // be the one the pipeline is standing on: a block
                    // on a level below asks about the row that level's
                    // pin holds, which is one degree read for the whole
                    // vector, since every row in hand came off it.
                    let alone = it.peek().and_then(|p| node_bracket(p)) != Some(group_id);
                    let def = self.schema.rel_by_id(rel_id);
                    let far = def.and_then(|d| match dirs {
                        Dirs::One(Dir::Fwd) => Some(d.to),
                        Dirs::One(Dir::Bwd) => Some(d.from),
                        // Both ends are walked, so the block is only
                        // this simple where the rel has the same table
                        // at each of them.
                        Dirs::Both => (d.to == d.from).then_some(d.to),
                    });
                    let bare = alone && far == Some(to_table);
                    // A mark is that same degree read written down
                    // instead of acted on, so it is a column of the
                    // level the block was written on and no operator at
                    // all. Only the bare shape has an answer this
                    // cheap. Anything else has to run a group per outer
                    // row, and what makes it a mark rather than a semi
                    // is that the row goes on either way and the group
                    // writes its answer to a column of the level below
                    // instead of deciding the row on it.
                    let mut mark = None;
                    if let BracketKind::Mark {
                        slot: mark_slot,
                        negated,
                    } = kind
                    {
                        if bare {
                            let vec = self.register_mark(src, rel_id, dirs, negated);
                            self.marks.insert(mark_slot, (src, vec));
                            continue;
                        }
                        let held = !pending.is_empty() || !waiting.is_empty();
                        let Some(at) = self.group_mark(mark_slot, src, held) else {
                            return Ok(None);
                        };
                        mark = Some(at);
                    }
                    if kind != BracketKind::Optional && bare {
                        // A degree read off a level some bracket walks
                        // would read the null row the runner stands
                        // there while it runs that bracket's
                        // continuation, and answer the question about a
                        // row that is not the one the block matched.
                        // The pattern is a second statement of a block
                        // or a predicate over what an OPTIONAL bound,
                        // and either way it is the interpreter's.
                        if self.walks_a_bracket_level(src) {
                            return Ok(None);
                        }
                        ops.push(Op::HasEdge {
                            rel: rel_id,
                            dirs,
                            from: src,
                            negated: kind == BracketKind::Anti,
                        });
                        continue;
                    }
                    // Anything else the block wants is a group, and a
                    // group standing in another bracket's continuation
                    // would walk off the null level that one left as
                    // the newest.
                    if self.bracketed() {
                        return Ok(None);
                    }
                    let to_level = self.levels.len();
                    self.levels.push(LevelBuild {
                        table: to_table,
                        cols: Vec::new(),
                    });
                    self.slot_level.insert(*to, to_level);
                    let head = ops.len();
                    ops.push(Op::Bracket {
                        len: 0,
                        level: to_level,
                        kind,
                        mark,
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
                        bracket: Some(g),
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
                    ops.push(Op::BracketHit { kind });
                    let len = ops.len() - head - 1;
                    let Op::Bracket { len: slot, .. } = &mut ops[head] else {
                        unreachable!("just pushed the bracket");
                    };
                    *slot = len;
                    self.close_bracket(kind, to_level, mark);
                }
                Some(LogicalPlan::Expand {
                    rel,
                    from,
                    to,
                    direction,
                    range: None,
                    into: true,
                    wcoj: true,
                    bracket: None,
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
                    bracket: None,
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

        // A held pattern nothing tied to the pipeline and nothing
        // pinned to rows of its own is a cross product against a whole
        // table, and one of those belongs on the old engine: it has a
        // nested loop for it and this pipeline would have to build a
        // table of the whole side to answer the same thing. A predicate
        // still waiting names one of those patterns, so it goes back
        // for the same reason.
        if !pending.is_empty() || !waiting.is_empty() {
            return Ok(None);
        }

        // The sink and everything the query wrote above it. Most of
        // that is a window on the answer the sink absorbs; a grouped
        // aggregate can also carry whole clauses above it, and those
        // are stages over a table the sink already holds.
        let sink_node = it.next();
        let tail: Vec<&LogicalPlan> = it.collect();

        // Whether the rows the level carries are the rows the answer
        // is built from. An operator between the two drops some of
        // them, an expand multiplies them and drops the rows with
        // nothing to walk to, and a slice above the sink stops reading
        // partway; each of those is a row the old engine never projects
        // and this one would have computed. With none of them written,
        // the level's rows and the answer's rows are the same rows.
        self.every_row_answers = ops.is_empty()
            && !tail
                .iter()
                .any(|node| matches!(node, LogicalPlan::Skip { .. } | LogicalPlan::Limit { .. }));

        let mut sink = match sink_node {
            Some(LogicalPlan::Project { items, .. }) => {
                let Some(post) = self.window_post(&tail)? else {
                    return Ok(None);
                };
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
            Some(LogicalPlan::Aggregate {
                keys,
                aggs,
                order_aggs,
                ..
            }) => {
                // GF20. An aggregate a sort key asked for and no column
                // carries finalizes into a slot of its own, which this
                // sink has no room for: its rows are the columns and
                // nothing else. The old executor keeps those values
                // beside the row, so the plan goes there whole rather
                // than being sorted here by a column that is not in it.
                if !order_aggs.is_empty() {
                    return Ok(None);
                }
                let mut key_refs = Vec::with_capacity(keys.len());
                for item in keys {
                    let Some(r) = self.item_ref(&item.expr)? else {
                        return Ok(None);
                    };
                    if !keyable(r) {
                        return Ok(None);
                    }
                    key_refs.push(r);
                }
                // A clause of its own above the aggregate reads the
                // groups rather than the rows under them, and where
                // one is written the whole answer is built in stages.
                if tail.iter().any(|node| !is_window(node)) {
                    let Some(sink) = self.staged_agg(keys, aggs, key_refs, &tail)? else {
                        return Ok(None);
                    };
                    sink
                } else {
                    let Some(post) = self.window_post(&tail)? else {
                        return Ok(None);
                    };
                    let Some(item_agg) = self.item_order(keys, aggs) else {
                        return Ok(None);
                    };
                    // A distinct count brings its own grouping and
                    // answers out of the table rather than out of an
                    // accumulator, so it is decided before the
                    // ordinary aggregate specs.
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
                    // A projection carries only the window steps: the
                    // stages are built over a grouped sink alone.
                    _ => true,
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
                    _ => true,
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
            // nothing, so a bracket keeps its walk.
            SinkSpec::Agg { .. } => !self.bracketed(),
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
            if !self.bracketed() {
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
                // a degree read off that would answer row zero's, so the
                // bracket puts the expands back and walks them.
                if from != newest_after && self.bracketed() {
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
        // a bracket keeps the old rule and falls back.
        let bracketed = self.bracketed();
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
                // A product reads nothing off the pipeline, so there is
                // nothing to check about where it sits; what it builds
                // is the newest level from here on like any other.
                Op::Product { to, .. } => newest = *to,
                _ => {}
            }
        }

        fuse_closes(&mut ops);
        batch_walks(&mut ops, &sink, &self.levels);
        // An edge column is read by the ordinal the walk carried down
        // beside the row, and only a plain expand carries one. Every
        // other operator that builds a level, a branch, an
        // intersection, a join, makes rows the walk did not step to
        // one at a time, so a level of theirs that ended up with an
        // edge column on it is a shape this executor cannot run.
        for (level, l) in self.levels.iter().enumerate() {
            if !l
                .cols
                .iter()
                .any(|(_, c)| matches!(c, ColSpec::RelStored(..)))
            {
                continue;
            }
            let walked = ops
                .iter()
                .any(|op| matches!(op, Op::Expand { to, .. } if *to == level));
            if !walked {
                return Ok(None);
            }
        }
        // The bracket runs its group one outer row at a time, because
        // whether the group matched is a fact about that row. A
        // batched descent drops the pin and concatenates neighbors
        // across source rows, which loses exactly that, so nothing
        // inside the bracket batches.
        if let Some(head) = ops.iter().position(|op| matches!(op, Op::Bracket { .. })) {
            for op in &mut ops[head..] {
                if let Op::Expand { batch, .. } | Op::Intersect { batch, .. } = op {
                    *batch = false;
                }
            }
        }

        // A leading CALL ran its kernel while this plan was built and
        // the answer is on the plan, so the plan is an answer about
        // this graph at this moment and not a shape to fill in later.
        let func = self.func.take();
        let reuse = if func.is_some() {
            None
        } else {
            Reuse::of(self.params_read.get(), std::mem::take(&mut self.holes))
        };

        Ok(Some(ExecPlan {
            table,
            source: match (seek, seeks) {
                (Some(key), _) => Source::Seek(key),
                (None, Some(keys)) => Source::Seeks(keys),
                (None, None) => Source::Scan(pred),
            },
            ops,
            sink,
            consts: std::mem::take(&mut self.consts),
            levels: self
                .levels
                .drain(..)
                .map(|l| {
                    let cols: Vec<ColSpec> = l.cols.into_iter().map(|(_, spec)| spec).collect();
                    Level {
                        table: l.table,
                        ords: cols.iter().any(|c| matches!(c, ColSpec::RelStored(..))),
                        cols,
                    }
                })
                .collect(),
            columns: self.query.columns.clone(),
            func,
            reuse,
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
                    match col {
                        ColSpec::Outer { from, .. } => *from = map[*from],
                        ColSpec::JoinMark { key, .. } => match key {
                            ScalarRef::Node { level }
                            | ScalarRef::RowId { level }
                            | ScalarRef::Col { level, .. } => *level = map[*level],
                            ScalarRef::Const { .. } => {}
                        },
                        _ => {}
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
                Op::Bracket { level, mark, .. } => {
                    *level = map[*level];
                    if let Some((at, _)) = mark {
                        *at = map[*at];
                    }
                }
                Op::DegreeProduct { from, .. } => *from = map[*from],
                Op::Join { key, to, .. } => {
                    match key {
                        ScalarRef::Node { level }
                        | ScalarRef::RowId { level }
                        | ScalarRef::Col { level, .. } => *level = map[*level],
                        ScalarRef::Const { .. } => {}
                    }
                    *to = map[*to];
                }
                Op::Product { to, .. } => *to = map[*to],
                Op::Sip { key, .. } => match key {
                    ScalarRef::Node { level }
                    | ScalarRef::RowId { level }
                    | ScalarRef::Col { level, .. } => *level = map[*level],
                    ScalarRef::Const { .. } => {}
                },
                Op::HasEdge { from, .. } => *from = map[*from],
                Op::Filter { .. } | Op::BracketHit { .. } => {}
            }
        }
        // A constant sits beside the levels rather than on one, so
        // there is nothing in it for the remap to move.
        let fix = |r: &mut ScalarRef| match r {
            ScalarRef::Node { level }
            | ScalarRef::RowId { level }
            | ScalarRef::Col { level, .. } => {
                *level = map[*level];
            }
            ScalarRef::Const { .. } => {}
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
            batch: false,
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

    /// The window steps above the sink, in plan order: a dedup, an
    /// ordering, and a slice of the answer. `None` for anything else,
    /// which is a clause the sink does not run.
    fn window_post(&self, tail: &[&LogicalPlan]) -> Result<Option<Vec<PostSpec>>> {
        let mut post = Vec::with_capacity(tail.len());
        for node in tail {
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
                _ => return Ok(None),
            }
        }
        Ok(Some(post))
    }

    /// A grouped aggregate the query did not stop at, with the clauses
    /// above it compiled into stages over its own table.
    ///
    /// The shape covered is the one a `WITH` writes: the aggregate,
    /// then its `WHERE` over the groups, then the clause that names
    /// them again or groups them a second time, then the usual window.
    /// A group count is a fraction of the rows that made it, so every
    /// stage here walks values rather than vectors.
    ///
    /// Whichever way the sink gets there, the stages above it read one
    /// layout: the keys the query wrote, then one value per aggregate
    /// item, then the columns a stage asked for off a key. Only the
    /// last of those three grows while the tail compiles, so a
    /// position settled early stays where it was put.
    fn staged_agg(
        &mut self,
        keys: &[BoundItem],
        aggs: &[BoundItem],
        key_refs: Vec<ScalarRef>,
        tail: &[&LogicalPlan],
    ) -> Result<Option<SinkSpec>> {
        // What the sink itself groups and accumulates. A grouped
        // `count(DISTINCT x)` has no accumulator: x joins the key, so
        // the table holds one group per distinct pair, and a second
        // stage counts the pairs each group of the written keys got.
        let mut dargs = Vec::new();
        let mut agg_specs = Vec::new();
        let mut counts_pairs = false;
        if aggs.len() == 1
            && let Some(parts) = self.distinct_count_keys(&aggs[0].expr)?
        {
            dargs = parts;
            counts_pairs = true;
        } else {
            for item in aggs {
                let Some(spec) = self.agg_spec(&item.expr)? else {
                    return Ok(None);
                };
                agg_specs.push(spec);
            }
        }
        let k = keys.len();
        // One value per aggregate item either way: the pair count
        // stands in for the distinct count it was lowered from.
        let base = k + if counts_pairs { 1 } else { agg_specs.len() };
        let mut hidden: Vec<ScalarRef> = Vec::new();
        let mut post = Vec::new();

        let mut it = tail.iter();
        let mut node = it.next();
        // The clause's own WHERE, which judges the groups and not the
        // rows under them. One written with an AND in it arrives as
        // one filter per conjunct, so this takes the whole run.
        while let Some(LogicalPlan::Filter {
            expr,
            bracket: None,
            ..
        }) = node
        {
            let Some(pred) = self.post_pred(expr, keys, aggs, &key_refs, base, &mut hidden)? else {
                return Ok(None);
            };
            post.push(PostSpec::Having(pred));
            node = it.next();
        }
        // The clause above it, which either names the groups again or
        // groups them a second time. One of the two has to be there:
        // the sink's row carries columns these stages asked for and
        // the answer does not.
        match node {
            Some(LogicalPlan::Project { items, .. }) => {
                let mut cols = Vec::with_capacity(items.len());
                for item in items {
                    if item.aggregate {
                        return Ok(None);
                    }
                    let Some(at) =
                        self.tuple_pos(&item.expr, keys, aggs, &key_refs, base, &mut hidden)?
                    else {
                        return Ok(None);
                    };
                    cols.push(at);
                }
                post.push(PostSpec::Emit(cols));
            }
            Some(LogicalPlan::Aggregate {
                keys: over,
                aggs: again,
                ..
            }) => {
                // This one is the query's last clause, so the order
                // its items were written in is the order the answer
                // wants them in.
                let Some(item_agg) = self.item_order(over, again) else {
                    return Ok(None);
                };
                let mut cols = Vec::with_capacity(over.len());
                for item in over {
                    let Some(at) =
                        self.tuple_pos(&item.expr, keys, aggs, &key_refs, base, &mut hidden)?
                    else {
                        return Ok(None);
                    };
                    cols.push(at);
                }
                let mut specs = Vec::with_capacity(again.len());
                for item in again {
                    let Some(spec) =
                        self.post_agg(&item.expr, keys, aggs, &key_refs, base, &mut hidden)?
                    else {
                        return Ok(None);
                    };
                    specs.push(spec);
                }
                post.push(PostSpec::Regroup {
                    keys: cols,
                    aggs: specs,
                    item_agg,
                });
            }
            _ => return Ok(None),
        }
        // What is left reads the answer's own columns, so it resolves
        // the way it does over any other sink.
        let rest: Vec<&LogicalPlan> = it.copied().collect();
        let Some(mut window) = self.window_post(&rest)? else {
            return Ok(None);
        };
        post.append(&mut window);

        // A node the query grouped by that the answer never carries
        // out is being asked for its identity and nothing else, and
        // its row says that on its own: a level holds one table, so
        // the table word beside the row is the same word for every
        // group. Dropping it halves that key and leaves the groups
        // exactly where they were.
        let mut key_refs = key_refs;
        for (at, r) in key_refs.iter_mut().enumerate() {
            if let ScalarRef::Node { level } = *r
                && !answers(&post, at)
            {
                *r = ScalarRef::RowId { level };
            }
        }

        // The sink's own row, laid out so the stages above read the
        // layout they compiled against. The hidden columns sit last
        // because that is the one part of it that grew.
        let h = hidden.len();
        let d = dargs.len();
        let mut sink_keys = key_refs;
        sink_keys.extend(hidden);
        sink_keys.extend(dargs);
        let item_agg = if counts_pairs {
            // Nothing accumulated: every column of the sink's row is
            // a key, and the pair count arrives with the stage below.
            vec![false; k + h + d]
        } else {
            let mut order = vec![false; k];
            order.extend(std::iter::repeat_n(true, agg_specs.len()));
            order.extend(std::iter::repeat_n(false, h));
            order
        };
        if counts_pairs {
            let mut order = vec![false; k];
            order.push(true);
            order.extend(std::iter::repeat_n(false, h));
            post.insert(
                0,
                PostSpec::Regroup {
                    keys: (0..k + h).collect(),
                    aggs: vec![PostAgg::Count(None)],
                    item_agg: order,
                },
            );
        }
        Ok(Some(SinkSpec::Agg {
            item_agg,
            keys: sink_keys,
            aggs: agg_specs,
            post,
        }))
    }

    /// Where a stage above the sink reads an expression from: one of
    /// the items the aggregate clause wrote, or a property of a node
    /// that is one of its keys.
    #[allow(clippy::too_many_arguments)]
    fn tuple_pos(
        &mut self,
        expr: &BoundExpr,
        keys: &[BoundItem],
        aggs: &[BoundItem],
        key_refs: &[ScalarRef],
        base: usize,
        hidden: &mut Vec<ScalarRef>,
    ) -> Result<Option<usize>> {
        if let Some(at) = keys.iter().position(|item| self.names_item(item, expr)) {
            return Ok(Some(at));
        }
        if let Some(at) = aggs.iter().position(|item| self.names_item(item, expr)) {
            return Ok(Some(keys.len() + at));
        }
        // Something read off a node the query grouped by. Its value is
        // the same for every row of the group, so grouping by it as
        // well leaves the groups exactly as they were and puts the
        // value where this stage can read it. Read off anything else
        // it is one of many values the group holds and there is no
        // such column to add.
        let Some(r) = self.item_ref(expr)? else {
            return Ok(None);
        };
        let pinned = key_refs
            .iter()
            .any(|k| matches!(*k, ScalarRef::Node { level } if level == r.level()));
        if !pinned {
            return Ok(None);
        }
        let at = match hidden.iter().position(|h| same_ref(*h, r)) {
            Some(at) => at,
            None => {
                hidden.push(r);
                hidden.len() - 1
            }
        };
        Ok(Some(base + at))
    }

    /// Whether a projected item is what an expression above it names,
    /// the same rule ORDER BY resolves by: the alias it bound, or the
    /// expression written again.
    fn names_item(&self, item: &BoundItem, expr: &BoundExpr) -> bool {
        item.expr == *expr
            || matches!(expr, BoundExpr::Var(slot) if self.item_slot(item) == Some(*slot))
    }

    /// A HAVING over the groups. Covers a column against a constant
    /// and the two combinators, which is how one is written.
    #[allow(clippy::too_many_arguments)]
    fn post_pred(
        &mut self,
        expr: &BoundExpr,
        keys: &[BoundItem],
        aggs: &[BoundItem],
        key_refs: &[ScalarRef],
        base: usize,
        hidden: &mut Vec<ScalarRef>,
    ) -> Result<Option<PostPred>> {
        let BoundExpr::Binary { op, lhs, rhs } = expr else {
            return Ok(None);
        };
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let Some(l) = self.post_pred(lhs, keys, aggs, key_refs, base, hidden)? else {
                return Ok(None);
            };
            let Some(r) = self.post_pred(rhs, keys, aggs, key_refs, base, hidden)? else {
                return Ok(None);
            };
            return Ok(Some(match op {
                BinaryOp::And => PostPred::And(vec![l, r]),
                _ => PostPred::Or(vec![l, r]),
            }));
        }
        // The constant may be written either side of the operator, and
        // moving it to the right turns the operator round with it.
        let (op, col, want) = match self.const_value(rhs) {
            Some(v) => (*op, lhs, v),
            None => {
                let Some(v) = self.const_value(lhs) else {
                    return Ok(None);
                };
                let Some(op) = flip_cmp(*op) else {
                    return Ok(None);
                };
                (op, rhs, v)
            }
        };
        if !matches!(
            op,
            BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        ) {
            return Ok(None);
        }
        let Some(at) = self.tuple_pos(col, keys, aggs, key_refs, base, hidden)? else {
            return Ok(None);
        };
        Ok(Some(PostPred::Cmp(op, at, want)))
    }

    /// An aggregate of a second grouping stage, over what the stage
    /// below it emitted.
    #[allow(clippy::too_many_arguments)]
    fn post_agg(
        &mut self,
        expr: &BoundExpr,
        keys: &[BoundItem],
        aggs: &[BoundItem],
        key_refs: &[ScalarRef],
        base: usize,
        hidden: &mut Vec<ScalarRef>,
    ) -> Result<Option<PostAgg>> {
        let BoundExpr::Call {
            func,
            distinct: false,
            star,
            args,
            ..
        } = expr
        else {
            return Ok(None);
        };
        if *star {
            return Ok(match func {
                Func::Count => Some(PostAgg::Count(None)),
                _ => None,
            });
        }
        let [arg] = args.as_slice() else {
            return Ok(None);
        };
        let Some(at) = self.tuple_pos(arg, keys, aggs, key_refs, base, hidden)? else {
            return Ok(None);
        };
        Ok(match func {
            Func::Count => Some(PostAgg::Count(Some(at))),
            Func::Sum => Some(PostAgg::Sum(at)),
            _ => None,
        })
    }

    /// Puts a value on the plan and answers the ref that reads it back.
    /// Two items that wrote the same constant take two entries, because
    /// a `Value` is cheaper than the walk that would find the first
    /// one.
    /// The value bound at parameter position `ix`, noted as read.
    ///
    /// Every look at a parameter goes through here, which is what
    /// makes [`Reuse`] safe to build: a plan is offered for reuse only
    /// if each bit this set has a hole against it, so a parameter read
    /// for any purpose other than being written into the plan keeps
    /// the plan out of the cache by doing nothing at all.
    fn param(&self, ix: usize) -> Option<&Value> {
        self.params_read
            .set(self.params_read.get() | Reuse::bit(ix));
        self.params.get(ix)
    }

    fn push_const(&mut self, value: Value) -> ScalarRef {
        self.consts.push(value);
        ScalarRef::Const {
            at: self.consts.len() - 1,
        }
    }

    /// A written constant or a parameter, as the value it stands for.
    fn const_value(&self, expr: &BoundExpr) -> Option<Value> {
        match expr {
            BoundExpr::Literal(Literal::Int(n)) => Some(Value::Int(*n)),
            BoundExpr::Literal(Literal::Float(f)) => Some(Value::Float(*f)),
            BoundExpr::Literal(Literal::Str(s)) => Some(Value::Str(s.clone())),
            BoundExpr::Literal(Literal::Bool(b)) => Some(Value::Bool(*b)),
            BoundExpr::Param(ix) => match self.param(*ix)? {
                v @ (Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)) => {
                    Some(v.clone())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// A projected item that stands for one value however the row it
    /// lands on came about: a written constant, a bound parameter, or a
    /// list or a record built out of those.
    ///
    /// [`Self::const_value`] answers the scalar half of this and is
    /// what a comparison against a column wants, so the two are kept
    /// apart: a record is a fine thing to project and not a thing a
    /// column predicate knows how to test against.
    fn const_item(&self, expr: &BoundExpr) -> Option<Value> {
        match expr {
            BoundExpr::Literal(Literal::Null) => Some(Value::Null),
            // Whatever the caller bound, whole. A comparison is picky
            // about this and a projection is not: the old engine hands
            // the bound value straight out here too.
            BoundExpr::Param(ix) => self.param(*ix).cloned(),
            BoundExpr::List(items) => items
                .iter()
                .map(|item| self.const_item(item))
                .collect::<Option<_>>()
                .map(Value::List),
            // GV45, and the shape `SET p = {age: 41}` writes. The
            // fields sort on the way in here exactly as they do in the
            // old engine, so a record is one value whichever engine
            // built it.
            BoundExpr::Map(pairs) => pairs
                .iter()
                .map(|(name, item)| Some((name.clone(), self.const_item(item)?)))
                .collect::<Option<Vec<_>>>()
                .map(Value::record),
            _ => self.const_value(expr),
        }
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
            BoundExpr::Param(ix) => match self.param(*ix)? {
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
            BoundExpr::Param(ix) => match self.param(*ix)? {
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
        if let Some((ix, ty)) =
            self.levels[level]
                .cols
                .iter()
                .enumerate()
                .find_map(|(ix, (k, c))| match c {
                    ColSpec::Stored(_, ty) if k == key => Some((ix, *ty)),
                    _ => None,
                })
        {
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

    /// The same for a property of the edge a level's walk stepped over,
    /// which is the rel table's column read by the edge's ordinal.
    ///
    /// A node column and an edge column can carry the same name on one
    /// level, so the match is on the spec as well as the name.
    fn register_rel_col(
        &mut self,
        level: usize,
        rel: RelId,
        key: &str,
    ) -> Result<Option<(usize, ColType)>> {
        if let Some((ix, ty)) =
            self.levels[level]
                .cols
                .iter()
                .enumerate()
                .find_map(|(ix, (k, c))| match c {
                    ColSpec::RelStored(_, _, ty) if k == key => Some((ix, *ty)),
                    _ => None,
                })
        {
            return Ok(Some((ix + 1, ty)));
        }
        let Some((id, ty)) = self.snap.resolve_rel_col(rel, key)? else {
            return Ok(None);
        };
        self.levels[level]
            .cols
            .push((key.to_string(), ColSpec::RelStored(rel, id, ty)));
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

    /// Whether vector `vec` of `level` is read straight out of storage,
    /// which is where a value cannot be null: a stored column holding a
    /// null does not resolve into a plan at all, so one that did resolve
    /// has a value in every row. Vector 0 is the level's row ids, which
    /// are not null either.
    fn stored_col(&self, level: usize, vec: usize) -> bool {
        if vec == 0 {
            return true;
        }
        matches!(
            self.levels[level].cols.get(vec - 1).map(|(_, c)| c),
            Some(ColSpec::Stored(..) | ColSpec::RelStored(..))
        )
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

    /// Registers an EXISTS block's answer as a column on the level the
    /// block was written on, returning its chunk vector position. Two
    /// blocks asking the same question share the column the way two
    /// readers of a property share its gather.
    fn register_mark(&mut self, level: usize, rel: RelId, dirs: Dirs, negated: bool) -> usize {
        let same = |c: &ColSpec| {
            matches!(c, ColSpec::Mark { rel: r, dirs: d, negated: n }
                if *r == rel && *d == dirs && *n == negated)
        };
        if let Some(ix) = self.levels[level].cols.iter().position(|(_, c)| same(c)) {
            return ix + 1;
        }
        self.levels[level]
            .cols
            .push((String::new(), ColSpec::Mark { rel, dirs, negated }));
        self.levels[level].cols.len()
    }

    /// Registers a join's answer about each row of `level` as a column
    /// on it, returning its chunk vector position. Two blocks probing
    /// the same table on the same key share the column, and the table
    /// itself is shared whatever happens, since the build reads it once
    /// while the plan is compiled.
    fn register_join_mark(
        &mut self,
        level: usize,
        table: Arc<JoinTable>,
        key: ScalarRef,
        negated: bool,
    ) -> usize {
        let same = |c: &ColSpec| {
            matches!(c, ColSpec::JoinMark { table: t, key: k, negated: n }
                if Arc::ptr_eq(t, &table) && same_ref(*k, key) && *n == negated)
        };
        if let Some(ix) = self.levels[level].cols.iter().position(|(_, c)| same(c)) {
            return ix + 1;
        }
        self.levels[level].cols.push((
            String::new(),
            ColSpec::JoinMark {
                table,
                key,
                negated,
            },
        ));
        self.levels[level].cols.len()
    }

    /// Reserves the column a group's mark is written to, on the level
    /// the pipeline is standing on, and returns where the bracket has to
    /// write it. `None` declines the whole plan.
    ///
    /// The group walks the newest level, so a block written about any
    /// other one is not this shape. Nor is one where a bracket is
    /// already open, since a group inside another group's continuation
    /// would walk off the null level that one left as the newest, or
    /// where a second one has already written a mark, since the two
    /// would want the same level number. A pattern still held is the
    /// same objection one step later: settling it builds a level, and
    /// the only place left to build one is where this group's level
    /// already is.
    /// Records what a group the compiler has just finished leaves
    /// behind.
    ///
    /// Only an OPTIONAL leaves a level the rest of the query can read,
    /// so only it holds the sink to what a null in that level answers.
    /// What an EXISTS block bound is out of scope above it, which is why
    /// the two are tracked apart. A mark leaves no level at all: the
    /// runner pops the group's chunk when the group is done with the
    /// vector, and what the block had to say is a column of the level
    /// below by then.
    fn close_bracket(&mut self, kind: BracketKind, level: usize, mark: Option<(usize, usize)>) {
        match kind {
            BracketKind::Optional => self.optional_level = Some(level),
            BracketKind::Mark { .. } => {
                let (at, _) = mark.expect("a mark group reserved its column");
                self.marked = Some(at);
            }
            BracketKind::Semi | BracketKind::Anti => self.exists_level = Some(level),
        }
    }

    fn group_mark(&mut self, slot: usize, level: usize, held: bool) -> Option<(usize, usize)> {
        if self.bracketed() || self.marked.is_some() || held || level + 1 != self.levels.len() {
            return None;
        }
        self.levels[level]
            .cols
            .push((String::new(), ColSpec::GroupMark));
        let vec = self.levels[level].cols.len();
        self.marks.insert(slot, (level, vec));
        Some((level, vec))
    }

    /// Compiles an arithmetic projection into a computed column on the
    /// level its properties come from, returning the scalar the sink
    /// reads it back through. The program is registered after every
    /// column it reads, which is what lets one walk of the list build
    /// the chunk.
    ///
    /// A number, of either width. An integer column and an integer
    /// written beside it answer an integer, and a float anywhere in the
    /// expression answers a float, which is the rule the row engine
    /// follows and the one the numeric functions need: the answer of a
    /// root or an angle is a float whatever arrived, so a projection
    /// holding one would have nowhere to land without this.
    ///
    /// Or a string, which a fold and a trim answer. A computed string
    /// column is read back exactly the way a stored one is, the vector
    /// carrying the bytes its kernel made along with the views into
    /// them, so nothing downstream of here has to know which of the two
    /// it is looking at.
    ///
    /// Anything that could have no answer for a row declines where
    /// something between the level and the answer drops rows. A
    /// computed column is filled before the filter that would have
    /// dropped the offending row, so a condition raised there is a
    /// condition the old engine never reached, and the two answers
    /// would differ on which engine took the plan. A divisor written as
    /// a number that is not nought cannot raise and neither can most of
    /// the numeric functions, so those shapes arrive whatever else the
    /// query wrote.
    ///
    /// Where nothing drops a row the question does not come up, and
    /// that is the shape a column of roots or logarithms is written in:
    /// `MATCH (p:person) RETURN ln(p.weight)` reads every row and
    /// answers for every row, so the rows the kernel measures are the
    /// rows the old engine measures and a value neither has an answer
    /// for raises on both. `every_row_answers` is that condition, and
    /// two things it does not settle: a conjunction, which decides per
    /// row which of its own operands are measured at all, and a second
    /// condition in the same program, which the two engines reach in
    /// different orders.
    fn register_expr(&mut self, expr: &BoundExpr) -> Result<Option<ScalarRef>> {
        if !matches!(
            expr,
            BoundExpr::Binary { .. }
                | BoundExpr::Call {
                    func: Func::Math(_)
                        | Func::CharLength
                        | Func::OctetLength
                        | Func::Size
                        | Func::Upper
                        | Func::Lower
                        | Func::Trim(_)
                        | Func::Cut(_)
                        | Func::Normalize(_)
                        | Func::ElementId,
                    ..
                }
        ) {
            return Ok(None);
        }
        let Some(level) = self.expr_level(expr) else {
            return Ok(None);
        };
        let mut b = ProgBuilder::new();
        let Some(root) = self.value_reg(&mut b, expr, level, false)? else {
            return Ok(None);
        };
        let ty = match b.types[root as usize] {
            PhysType::Int64 => ColType::Int,
            PhysType::Float64 => ColType::Float,
            // A string, which a fold answers. The vector carries the
            // bytes it made along with the views, so the sink reads one
            // back exactly the way it reads a stored column.
            PhysType::Str => ColType::Str,
            // A temporal value, whose word is a count of something the
            // physical type alone does not name: an interval counts
            // months or nanoseconds and a timestamp counts nanoseconds
            // into a day or since the epoch. The lane names it, and a
            // register only carries one where the compiler knew it the
            // whole way through, so a program that lost the lane
            // declines here rather than handing back the bare number.
            PhysType::Date | PhysType::Timestamp | PhysType::Interval => match b.lane(root) {
                Some(lane) => ColType::Temporal(lane),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        if may_raise(&b.ops)
            && !(self.every_row_answers && !short_circuits(&b.ops) && conditions(&b) <= 1)
        {
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
            ty,
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
            // A call reads what its arguments read. Without this the
            // level of `abs(p.x)` is no level at all, and a projection
            // that named one would be registered against nothing.
            BoundExpr::Call { args, .. } => {
                for arg in args {
                    self.walk_slots(arg, f);
                }
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

    /// Settles a held pattern no equality tied to the pipeline, where
    /// the pattern's own predicate names rows of its table instead.
    ///
    /// What is left after every join that was going to happen has is a
    /// cross product, and the reason to compile one at all is the
    /// statement that writes an edge: `MATCH (a:Obj {id: 1}), (b:Obj
    /// {id: 2})` names both ends and ties neither to the other, and
    /// sending that back a row at a time costs a scan of the table per
    /// end, which is the whole cost of the write at any size worth
    /// measuring.
    ///
    /// The product goes in where the predicate that pinned it was
    /// written, which is what lets a walk off the pinned pattern
    /// compile as well, and true is that having happened. A pattern
    /// nothing pins stays held and the caller falls back with it, so
    /// the product this compiles always has a settled side.
    fn pin_held(
        &mut self,
        ops: &mut Vec<Op>,
        pending: &mut Vec<(usize, TableId)>,
        waiting: &mut Vec<&BoundExpr>,
    ) -> Result<bool> {
        // Inside a bracket the newest level is the group's and may be
        // null, and past a mark the runner has popped it. A product
        // built on either would pair its rows with a level that is not
        // there, so those go back whole.
        if self.bracketed() || self.marked.is_some() {
            return Ok(false);
        }
        for p in 0..pending.len() {
            let (slot, build) = pending[p];
            let mut pinned = None;
            for (i, expr) in waiting.iter().enumerate() {
                let Some(rows) = self.pin_rows(expr, slot, build)? else {
                    continue;
                };
                pinned = Some((i, rows));
                break;
            }
            let Some((at, rows)) = pinned else {
                continue;
            };
            waiting.remove(at);
            pending.remove(p);
            let to = self.levels.len();
            self.levels.push(LevelBuild {
                table: build,
                cols: Vec::new(),
            });
            self.slot_level.insert(slot, to);
            ops.push(Op::Product {
                rows: Arc::new(rows),
                to,
            });
            self.sip_at.insert(to, ops.len());
            return Ok(true);
        }
        Ok(false)
    }

    /// The rows of `build` a predicate over the held pattern at `slot`
    /// picks out, `None` when it is not a predicate that names rows on
    /// its own.
    ///
    /// Two shapes qualify. A point on the pattern's id is one key index
    /// lookup and no scan at all, which is what the write statements
    /// write. An equality on a stored integer column is one scan of the
    /// table with the zone map pushed down, so a column the rows are
    /// laid out by touches a chunk and the rest of the table is never
    /// decoded.
    fn pin_rows(
        &mut self,
        expr: &BoundExpr,
        slot: usize,
        build: TableId,
    ) -> Result<Option<Vec<u64>>> {
        // Whatever it says about the held pattern, it has to say it
        // about that pattern alone: a predicate reading another
        // variable is answered where both levels exist and not here.
        let mut named = 0;
        let mut mine = true;
        self.walk_slots(expr, &mut |s| {
            named += 1;
            mine &= s == slot;
        });
        if named == 0 || !mine {
            return Ok(None);
        }
        if let Some(key) = id_point(expr, slot) {
            let Some(k) = self.const_int(key) else {
                return Ok(None);
            };
            // A key that names no row is not a failure, it is a match
            // of nothing, and an empty side is what says so.
            return Ok(Some(
                self.seek_arg(build, Some(k))?
                    .into_iter()
                    .map(|row| row as u64)
                    .collect(),
            ));
        }
        let BoundExpr::Binary {
            op: BinaryOp::Eq,
            lhs,
            rhs,
        } = expr
        else {
            return Ok(None);
        };
        let (col_expr, key) = match (self.const_int(rhs), self.const_int(lhs)) {
            (Some(k), _) => (lhs.as_ref(), k),
            (None, Some(k)) => (rhs.as_ref(), k),
            (None, None) => return Ok(None),
        };
        let BoundExpr::Property { base, key: name } = col_expr else {
            return Ok(None);
        };
        if !matches!(base.as_ref(), BoundExpr::Var(s) if *s == slot) {
            return Ok(None);
        }
        let Some((col, ColType::Int)) = self.snap.resolve_col(build, name)? else {
            return Ok(None);
        };
        let Ok(wanted) = u64::try_from(key) else {
            // The zones compare unsigned and a negative bound has no
            // place in them, so the scan below would answer the wrong
            // rows rather than none.
            return Ok(None);
        };
        let pred = ZonePred {
            col,
            lo: wanted,
            hi: wanted,
        };
        let mut rows = Vec::new();
        let mut arena = MorselArena::new();
        // A chunk the zones ruled out answers with nothing, the same as
        // a chunk past the end of the table, so the walk counts the
        // chunks out rather than stopping at the first empty one: the
        // pushdown is what makes this read cheap and the whole point is
        // that most chunks come back empty.
        let chunks = self.snap.table_rows(build)?.div_ceil(SCAN_ROWS as u64);
        for chunk in 0..chunks {
            let Some(sc) = self
                .snap
                .scan(build, chunk, &[col], Some(&pred), &mut arena)?
            else {
                continue;
            };
            let vec = &sc.columns[0];
            let vals = vec.values::<i64>();
            let mut take = |i: usize| {
                if vec.is_valid(i) && vals[i] == key {
                    rows.push(sc.row_base + i as u64);
                }
            };
            match &sc.sel {
                Some(sel) => sel.as_slice().iter().for_each(|&i| take(usize::from(i))),
                None => (0..sc.rows as usize).for_each(take),
            }
            arena.reset();
            // The rows sit in the plan and every one of them is paired
            // with every row of the level below, so a predicate this
            // loose is a cross product the old engine should own rather
            // than one to hold a list this long for.
            if rows.len() > PIN_ROWS_MAX {
                return Ok(None);
            }
        }
        Ok(Some(rows))
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
                // Nothing an equality could move, so a held pattern
                // whose own predicate names its rows is settled on
                // those instead. That opens the join and filter passes
                // again, since a predicate reading two held patterns
                // has one of them now.
                if self.pin_held(ops, pending, waiting)? {
                    continue;
                }
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
            // Through the selection when the scan built one: a chunk a
            // delete took rows out of hands them back like any other
            // row, and a build side that read them would answer a probe
            // with a row the rest of the query cannot see.
            let mut take = |i: usize| {
                if vec.is_valid(i) {
                    keys.push(vals[i] as u64);
                    payload.push(sc.row_base + i as u64);
                }
            };
            match &sc.sel {
                Some(sel) => sel.as_slice().iter().for_each(|&i| take(usize::from(i))),
                None => (0..sc.rows as usize).for_each(take),
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
            // A value the query wrote or bound is the same value on
            // every row, so it goes on the plan once and the sink
            // copies it out. Statements that write are full of these:
            // the read half of `SET p.age = 41` projects the row and
            // the 41 beside it, and before this the 41 alone was enough
            // to send the whole statement back to the old engine.
            BoundExpr::Literal(_)
            | BoundExpr::Param(_)
            | BoundExpr::List(_)
            | BoundExpr::Map(_) => match self.const_item(expr) {
                Some(value) => Ok(Some(self.push_const(value))),
                None => self.register_expr(expr),
            },
            BoundExpr::Property { base, key } => {
                let BoundExpr::Var(slot) = base.as_ref() else {
                    return Ok(None);
                };
                let Some(&level) = self.slot_level.get(slot) else {
                    // Not a node the walk bound, so it may be the edge
                    // of a hop, whose properties live on the level that
                    // hop built.
                    let Some(&(level, rel)) = self.rel_level.get(slot) else {
                        return Ok(None);
                    };
                    return Ok(self
                        .register_rel_col(level, rel, key)?
                        .map(|(vec, ty)| ScalarRef::Col { level, vec, ty }));
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
            // ID of a node, which is the number `.id` answers written
            // as a call, so it resolves to the same row.
            //
            // The argument goes through this function rather than being
            // read as a variable, which is what turns the shapes that
            // are not a node away: a yielded value and a seek key are
            // columns of level 0 and come back as columns, an edge
            // variable belongs to no level here, and the old engine
            // answers null for an edge anyway.
            BoundExpr::Call {
                func: Func::Id,
                distinct: false,
                star: false,
                args,
                ..
            } => {
                let [arg] = args.as_slice() else {
                    return Ok(None);
                };
                Ok(match self.item_ref(arg)? {
                    Some(ScalarRef::Node { level }) => Some(ScalarRef::RowId { level }),
                    _ => None,
                })
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
            ..
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
            if !keyable(r) {
                return Ok(None);
            }
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
            ..
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
        let mut b = ProgBuilder::new();
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
        // A mark slot is not a value the query wrote, it is the column
        // an EXISTS block left on the level it was written on. Zero
        // there is the miss, so the predicate the row is judged by is a
        // compare against zero.
        if let BoundExpr::Var(slot) = expr
            && let Some(&(from, vec)) = self.marks.get(slot)
        {
            return self.mark_reg(b, from, vec, level);
        }
        // A null test asks a question the column already answers. A
        // kernel that misses a node writes null there, and the vector
        // that carries the answer carries a validity bitmap beside it,
        // so IS NOT NULL is that bitmap and IS NULL is its complement.
        // Only a column has validity to read: a test over a computed
        // value, or over a mark, goes back to the old engine.
        if let BoundExpr::IsNull { expr, negated } = expr {
            if !matches!(**expr, BoundExpr::Property { .. } | BoundExpr::Var(_)) {
                return Ok(None);
            }
            if let BoundExpr::Var(slot) = &**expr
                && self.marks.contains_key(slot)
            {
                return Ok(None);
            }
            // Not `outer`: a level below broadcasts one row into this
            // one, and the broadcast does not carry that row's validity
            // with it, so the test would read a bitmap that is not the
            // column's.
            let Some(src) = self.value_reg(b, expr, level, false)? else {
                return Ok(None);
            };
            let dst = b.push_type(PhysType::Bool)?;
            b.ops.push(ExprOp::IsNull {
                src,
                negated: *negated,
                dst,
            });
            return Ok(Some(dst));
        }
        // A type test the column already answers. GV65 to GV68 put IS
        // TYPED in filter position, and in general the answer is a
        // question about the value: `x IS TYPED INT8` has to look at
        // every number, because whether it fits in eight bits is not
        // something the column's type settles. The case that is settled
        // is the test that asks a column for the type it has, or for
        // one that holds it whole, and there the answer is the same for
        // every row before a row is read. Null included: a null belongs
        // to every nullable type, and the target has to be nullable to
        // get past `widens` at all.
        if let BoundExpr::IsTyped { expr, ty, negated } = expr {
            let Some(src) = self.value_reg(b, expr, level, false)? else {
                return Ok(None);
            };
            if !widens(b.types[src as usize], ty) {
                return Ok(None);
            }
            let dst = b.push_type(PhysType::Bool)?;
            b.ops.push(ExprOp::All { on: !*negated, dst });
            return Ok(Some(dst));
        }
        // GF08's other half. Whether a string is in a normal form is
        // the one string function whose answer is a truth value, and a
        // chunk of truth values is not a column this executor carries,
        // so the answer is written where a comparison writes its own:
        // straight into a predicate register.
        //
        // The NOT that the negated spelling binds to is read here and
        // handed to the kernel rather than run as an op of its own. A
        // predicate bitmap has room for two answers and the language
        // has three, so a row holding null is off in the bitmap either
        // way, and a complement would have called it unnormalized.
        {
            let (inner, negated) = match expr {
                BoundExpr::Unary {
                    op: UnaryOp::Not,
                    expr,
                } => (&**expr, true),
                other => (other, false),
            };
            if let BoundExpr::Call {
                func: Func::IsNormalized(form),
                args,
                distinct: false,
                star: false,
                ..
            } = inner
                && args.len() == 1
            {
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                if StrNorm::Test(*form)
                    .answer_type(b.types[src as usize])
                    .is_none()
                {
                    return Ok(None);
                }
                let dst = b.push_type(PhysType::Bool)?;
                b.ops.push(ExprOp::StrNormalized {
                    form: *form,
                    negated,
                    src,
                    dst,
                });
                return Ok(Some(dst));
            }
        }
        // GF11, the two questions about which elements a row bound.
        // Both are the same pairwise walk over the arguments, and both
        // answer a truth value, so they are written straight into a
        // predicate register the way a comparison is.
        if let BoundExpr::Call {
            func: func @ (Func::Same | Func::AllDifferent),
            args,
            distinct: false,
            star: false,
            ..
        } = expr
        {
            return self.identity_reg(b, *func, args, level);
        }
        let BoundExpr::Binary { op, lhs, rhs } = expr else {
            return Ok(None);
        };
        if let Some(cmp) = cmp_op(*op) {
            if let Some(dst) = self.narrowed_reg(b, *op, lhs, rhs, level)? {
                return Ok(Some(dst));
            }
            let Some(l) = self.value_reg(b, lhs, level, true)? else {
                return Ok(None);
            };
            let Some(r) = self.value_reg(b, rhs, level, true)? else {
                return Ok(None);
            };
            // The kernels compare within one physical type, and the two
            // pairs a query writes on purpose are moved into one type,
            // the integer column against a written float above and a
            // float against a written whole number here. What is left
            // is a mistyped compare and it keeps old-engine semantics
            // by falling back.
            let Some((l, r)) = self.matched(b, l, r, lhs, rhs)? else {
                return Ok(None);
            };
            let dst = b.push_type(PhysType::Bool)?;
            b.ops.push(ExprOp::Compare { op: cmp, l, r, dst });
            return Ok(Some(dst));
        }
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let Some(l) = self.pred_reg(b, lhs, level)? else {
                    return Ok(None);
                };
                let second = b.ops.len();
                let Some(r) = self.pred_reg(b, rhs, level)? else {
                    return Ok(None);
                };
                // The row engine stops at a conjunct that decided the
                // row, so anything written behind one is something it
                // never runs, and `n <> 0 AND 100 / n > 5` is how a
                // query says which rows the division is for. The
                // program has no such order: every op runs over the
                // whole chunk, so a divisor of nought behind the guard
                // would raise where the query said it could not. Such a
                // plan goes back to the row engine whole. A divisor
                // written as a number that is not nought cannot raise,
                // which is the other way people write it, and most of
                // the numeric functions cannot raise at all, so those
                // shapes stay here.
                if may_raise(&b.ops[second..]) {
                    return Ok(None);
                }
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

    /// SAME and ALL_DIFFERENT over the nodes a row bound, compiled into
    /// a predicate register.
    ///
    /// An element is its table and its row, and a level has one table
    /// for every row it will ever produce. So half of each pair is
    /// settled before a row is read: two levels on different tables
    /// hold different nodes whatever the rows turn out to be, and two
    /// names for one level hold the same node. Those pairs answer
    /// without a compare, and either function is false outright as soon
    /// as one pair goes the wrong way. What is left is a row against a
    /// row, which is one integer comparison.
    ///
    /// An edge argument declines. An edge is its table, the pair it
    /// runs between and which copy of that pair it is, and none of
    /// those is a number this level carries.
    fn identity_reg(
        &mut self,
        b: &mut ProgBuilder,
        func: Func,
        args: &[BoundExpr],
        level: usize,
    ) -> Result<Option<Reg>> {
        let same = matches!(func, Func::Same);
        let mut levels = Vec::with_capacity(args.len());
        for arg in args {
            let Some(ScalarRef::Node { level }) = self.item_ref(arg)? else {
                return Ok(None);
            };
            if self.levels.get(level).is_none() {
                return Ok(None);
            }
            levels.push(level);
        }
        if levels.len() < 2 {
            return Ok(None);
        }
        let mut compares = Vec::new();
        for (at, &l) in levels.iter().enumerate() {
            for &r in &levels[at + 1..] {
                match settled_pair(&self.levels, l, r) {
                    Some(held) if held == same => continue,
                    Some(_) => {
                        let dst = b.push_type(PhysType::Bool)?;
                        b.ops.push(ExprOp::All { on: false, dst });
                        return Ok(Some(dst));
                    }
                    None => compares.push((l, r)),
                }
            }
        }
        let op = if same { CmpOp::Eq } else { CmpOp::Ne };
        let mut acc: Option<Reg> = None;
        for (l, r) in compares {
            // A row that a level below broadcasts in may be null,
            // which an OPTIONAL MATCH that missed leaves behind. The
            // compare kernel clears the answer's validity there and
            // the row is off, which is the null the old engine
            // answers for an argument it could not read.
            let Some(l) = self.ref_reg(b, ScalarRef::RowId { level: l }, level, true)? else {
                return Ok(None);
            };
            let Some(r) = self.ref_reg(b, ScalarRef::RowId { level: r }, level, true)? else {
                return Ok(None);
            };
            let dst = b.push_type(PhysType::Bool)?;
            b.ops.push(ExprOp::Compare { op, l, r, dst });
            acc = Some(match acc {
                None => dst,
                Some(prev) => {
                    let both = b.push_type(PhysType::Bool)?;
                    b.ops.push(ExprOp::And {
                        l: prev,
                        r: dst,
                        dst: both,
                    });
                    both
                }
            });
        }
        // Every pair was settled by the tables, and settled the way the
        // function asked, so the answer is the same for every row. It
        // is written as an op rather than left out because the program
        // hands back its last op's register and the filter above wants
        // one to read.
        Ok(Some(match acc {
            Some(reg) => reg,
            None => {
                let dst = b.push_type(PhysType::Bool)?;
                b.ops.push(ExprOp::All { on: true, dst });
                dst
            }
        }))
    }

    /// Reads a mark column back as a predicate register. The block may
    /// have been written on a level the pipeline has since walked off,
    /// and that level is pinned to one row for the whole vector here,
    /// so its answer enters this level as a constant column the same
    /// way a correlated end does.
    fn mark_reg(
        &mut self,
        b: &mut ProgBuilder,
        from: usize,
        vec: usize,
        level: usize,
    ) -> Result<Option<Reg>> {
        let col = match from == level {
            true => vec,
            false => {
                if from > level {
                    return Ok(None);
                }
                self.register_outer(level, from, vec)
            }
        };
        let Ok(col) = u8::try_from(col) else {
            return Ok(None);
        };
        let dst = b.push_type(PhysType::Int64)?;
        b.ops.push(ExprOp::LoadCol { col, dst });
        let zero = b.push_const(OwnedValue::Int(0))?;
        let out = b.push_type(PhysType::Bool)?;
        b.ops.push(ExprOp::Compare {
            op: CmpOp::Ne,
            l: dst,
            r: zero,
            dst: out,
        });
        Ok(Some(out))
    }

    /// An integer column against a float constant, compiled as one
    /// integer comparison, or `None` when the predicate is not that
    /// shape.
    ///
    /// This is the only pair of physical types a query writes on
    /// purpose: `p.age > 30.5` reads a stored integer and compares it
    /// with a literal the lexer had to make a float. The kernels
    /// compare within one type, so before this the whole query went
    /// back to the old engine, which reads the tag of every value of
    /// every row to find out that one side is an integer and the other
    /// is not. The types are known at compile time, so the constant is
    /// moved into the column's type here and the comparison runs in the
    /// monomorphic integer kernel.
    fn narrowed_reg(
        &mut self,
        b: &mut ProgBuilder,
        op: BinaryOp,
        lhs: &BoundExpr,
        rhs: &BoundExpr,
        level: usize,
    ) -> Result<Option<Reg>> {
        // The constant may be written on either side, and a flipped
        // operator is what makes the two spellings one shape.
        let (col_expr, c, op) = match (self.const_float(lhs), self.const_float(rhs)) {
            (None, Some(c)) => (lhs, c, op),
            (Some(c), None) => match flip_cmp(op) {
                Some(op) => (rhs, c, op),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let Some((cmp, k)) = narrow_float(op, c) else {
            return Ok(None);
        };
        let Some(l) = self.value_reg(b, col_expr, level, true)? else {
            return Ok(None);
        };
        if b.types[l as usize] != PhysType::Int64 {
            return Ok(None);
        }
        let r = b.push_const(OwnedValue::Int(k))?;
        let dst = b.push_type(PhysType::Bool)?;
        b.ops.push(ExprOp::Compare { op: cmp, l, r, dst });
        Ok(Some(dst))
    }

    /// A resolved item read into a register: the chunk vector it names,
    /// broadcast in from the level that holds it when that is not the
    /// one the program runs on.
    ///
    /// The row a level carries is vector 0 there, which is why a row id
    /// and a stored column are one read with two positions rather than
    /// two kinds of thing.
    fn ref_reg(
        &mut self,
        b: &mut ProgBuilder,
        r: ScalarRef,
        level: usize,
        outer: bool,
    ) -> Result<Option<Reg>> {
        let (from, mut col, ty, lane) = match r {
            // A property is never a constant, so this is the arm
            // nothing reaches rather than a shape to build a program
            // out of.
            ScalarRef::Const { .. } => return Ok(None),
            ScalarRef::RowId { level } => (level, 0, PhysType::Int64, None),
            ScalarRef::Col { level, vec, ty } => match ty {
                ColType::Int => (level, vec, PhysType::Int64, None),
                ColType::Float => (level, vec, PhysType::Float64, None),
                ColType::Str => (level, vec, PhysType::Str, None),
                // The column knows what its words are a count of and
                // the register takes it from there, so an operator over
                // two of them can check they agree and the sink can
                // hand back what it read rather than the number under
                // it.
                ColType::Temporal(lane) => (level, vec, lane.phys(), Some(lane)),
            },
            ScalarRef::Node { .. } => return Ok(None),
        };
        if from != level {
            // A level above this one is not built yet, and a string end
            // would have to carry its buffers into the broadcast, so
            // both go back to the old engine.
            //
            // Under a comparison the broadcast may be null: the kernel
            // clears the whole column's validity and that is the answer
            // the old engine gives. Inside arithmetic it may not,
            // because the arith kernels do not propagate validity and a
            // null end would come back a number. A stored column is the
            // case where that cannot arise, since a column holding a
            // null does not resolve at all.
            let numeric = matches!(ty, PhysType::Int64 | PhysType::Float64);
            let never_null = self.stored_col(from, col);
            if from > level || !numeric || !(outer || never_null) {
                return Ok(None);
            }
            col = self.register_outer(level, from, col);
        }
        let Ok(col) = u8::try_from(col) else {
            return Ok(None);
        };
        let dst = b.push_reg(ty, lane)?;
        b.ops.push(ExprOp::LoadCol { col, dst });
        Ok(Some(dst))
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
            // A temporal literal is the word its lane rides in, which is
            // what the column it is compared against holds. A zoned one
            // has no lane and declines here, the same way a zoned column
            // never resolves in the first place.
            BoundExpr::Literal(Literal::Temporal(t)) => match lane_const(t) {
                Some((v, lane)) => b.push_const_lane(v, lane).map(Some),
                None => Ok(None),
            },
            BoundExpr::Param(ix) => match self.param(*ix) {
                Some(Value::Int(n)) => b.push_const(OwnedValue::Int(*n)).map(Some),
                Some(Value::Float(f)) => b.push_const(OwnedValue::Float(*f)).map(Some),
                Some(Value::Str(s)) => b.push_const(OwnedValue::Str(s.as_bytes().into())).map(Some),
                Some(Value::Temporal(t)) => match lane_const(t) {
                    Some((v, lane)) => b.push_const_lane(v, lane).map(Some),
                    None => Ok(None),
                },
                _ => Ok(None),
            },
            // A property, or a variable that stands for a column: the
            // value a CALL yielded and the key a batch of seeks found
            // are both columns of level 0, and a variable naming a node
            // is not a value at all, which the node arm below declines.
            //
            // ID of a node joins them because it is the same read: the
            // number it answers is the row the level already carries,
            // and `item_ref` hands it back as that row.
            BoundExpr::Property { .. }
            | BoundExpr::Var(_)
            | BoundExpr::Call { func: Func::Id, .. } => {
                let Some(r) = self.item_ref(expr)? else {
                    return Ok(None);
                };
                self.ref_reg(b, r, level, outer)
            }
            // ELEMENT_ID of a node, which is the same two numbers ID
            // reads written as the string the standard asks for. The
            // table is the level's and is the same for every row it
            // will ever produce, so the kernel writes it once for the
            // chunk and a row's own number after it.
            //
            // An edge declines here as it declines above, and for a
            // longer reason: an edge is its table, the pair it runs
            // between and which copy of that pair it is, and a level
            // carries none of those as a column.
            BoundExpr::Call {
                func: Func::ElementId,
                distinct: false,
                star: false,
                args,
                ..
            } => {
                let [arg] = args.as_slice() else {
                    return Ok(None);
                };
                let Some(ScalarRef::Node { level: node }) = self.item_ref(arg)? else {
                    return Ok(None);
                };
                let Some(table) = self.levels.get(node).map(|l| l.table) else {
                    return Ok(None);
                };
                let Some(src) = self.ref_reg(b, ScalarRef::RowId { level: node }, level, outer)?
                else {
                    return Ok(None);
                };
                let dst = b.push_type(PhysType::Str)?;
                b.ops.push(ExprOp::ElementId { table, src, dst });
                Ok(Some(dst))
            }
            // A cast the values cannot notice. GA05 put CAST in filter
            // position and a cast is a per-row conversion with a
            // condition behind it, so it belongs to the row engine in
            // general. The exception is the cast that does nothing: a
            // column stored as a 64 bit integer asked for as a 64 bit
            // or wider integer is the value it already was, and the
            // same holds for a float column asked for as a float of at
            // least the width it has. Peeling those keeps a filter
            // written that way on the kernel.
            // A cast carrying a label set is never one of those: it
            // reads the graph and can raise, so it stays on the row
            // engine whatever the register holds.
            BoundExpr::Cast {
                expr,
                ty,
                constrained: None,
            } => {
                let Some(src) = self.value_reg(b, expr, level, outer)? else {
                    return Ok(None);
                };
                if widens(b.types[src as usize], ty) {
                    Ok(Some(src))
                } else {
                    Ok(None)
                }
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
                let Some((l, r)) = self.matched(b, l, r, lhs, rhs)? else {
                    return Ok(None);
                };
                let ty = b.types[l as usize];
                // Two durations of one kind add and subtract, and that
                // is the whole of the temporal arithmetic that is a
                // kernel. The words are counts of the same unit, so the
                // sum is the integer sum and the answer is a duration
                // of the kind both sides were.
                //
                // Two of different kinds do not add, which is what the
                // kinds exist to say, and a lane on each register is
                // what lets this notice. An instant shifted by a
                // duration is not here either: it clamps a month onto
                // a day, refuses a time of day landing on a date, and
                // stops at the end of the calendar, none of which the
                // arith kernel has an op for.
                let lane = match (b.lane(l), b.lane(r)) {
                    (None, None) => None,
                    (Some(a), Some(c))
                        if a == c
                            && matches!(a, TemporalLane::Duration(_))
                            && matches!(bin, BinOp::Add | BinOp::Sub) =>
                    {
                        Some(a)
                    }
                    _ => return Ok(None),
                };
                if !matches!(ty, PhysType::Int64 | PhysType::Float64 | PhysType::Interval) {
                    return Ok(None);
                }
                let dst = b.push_reg(ty, lane)?;
                b.ops.push(ExprOp::Binary { op: bin, l, r, dst });
                Ok(Some(dst))
            }
            // GF01 to GF03, the numeric functions. They are the
            // numeric library's first kernels because they are the
            // ones a filter is written over: `abs(a.x - b.x) < 3` is a
            // scan the row engine used to take back for the sake of
            // one call.
            BoundExpr::Call {
                func: Func::Math(math),
                args,
                distinct: false,
                star: false,
                ..
            } => {
                if let Some(op) = self.math_op(*math, args) {
                    let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                        return Ok(None);
                    };
                    let Some(ty) = op.answer_type(b.types[src as usize]) else {
                        return Ok(None);
                    };
                    let dst = b.push_type(ty)?;
                    b.ops.push(ExprOp::Math { op, src, dst });
                    return Ok(Some(dst));
                }
                let Some(op) = math_pair(*math, args) else {
                    return Ok(None);
                };
                let Some(l) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(r) = self.value_reg(b, &args[1], level, false)? else {
                    return Ok(None);
                };
                // The kernel answers one pair of types, so a whole
                // number the statement wrote beside an approximate
                // column becomes an approximate number here rather
                // than a branch in the loop.
                let Some((l, r)) = self.matched(b, l, r, &args[0], &args[1])? else {
                    return Ok(None);
                };
                let Some(ty) = op.answer_type(b.types[l as usize], b.types[r as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::MathPair { op, l, r, dst });
                Ok(Some(dst))
            }
            // GF04, the questions about a string whose answer is a
            // number. They come before the rest of the string library
            // because an answer that is a count needs no room in the
            // arena for bytes, which is the question the folds and the
            // trims still have to settle. `SIZE` is here because a
            // string is one of the things it counts, and over a string
            // it counts exactly what `CHAR_LENGTH` counts, so the two
            // are the same kernel under two spellings. Over a list it
            // is something else and there is no kernel for it yet,
            // which needs no rule here: the length of anything that is
            // not a string has no answer type, so those arguments fall
            // back the way an unhandled expression does.
            BoundExpr::Call {
                func: func @ (Func::CharLength | Func::OctetLength | Func::Size),
                args,
                distinct: false,
                star: false,
                ..
            } if args.len() == 1 => {
                let op = match func {
                    Func::CharLength | Func::Size => StrLen::Chars,
                    _ => StrLen::Octets,
                };
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(ty) = op.answer_type(b.types[src as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::StrLen { op, src, dst });
                Ok(Some(dst))
            }
            // DURATION_BETWEEN, ISO 20.28, over two instants of the
            // same type. Both operands are counts and the answer is a
            // count, so the whole call is arithmetic over words once
            // the type is known, and the type is known here rather than
            // in the loop: the op carries it and the kernel picks its
            // loop once a chunk.
            //
            // The two sides have to agree on a lane, which is what says
            // they are instants of one type. A date and a datetime do
            // not have a duration between them, the row engine refusing
            // that pair rather than reading one as the other, and two
            // registers that disagree decline here for the same reason.
            // A duration is not an instant either, so a lane that is
            // one is not this call.
            BoundExpr::Call {
                func: Func::DurationBetween(kind),
                args,
                distinct: false,
                star: false,
                ..
            } if args.len() == 2 => {
                let Some(from) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(to) = self.value_reg(b, &args[1], level, false)? else {
                    return Ok(None);
                };
                let (Some(a), Some(c)) = (b.lane(from), b.lane(to)) else {
                    return Ok(None);
                };
                if a != c || matches!(a, TemporalLane::Duration(_)) {
                    return Ok(None);
                }
                let dst = b.push_lane(TemporalLane::Duration(*kind))?;
                b.ops.push(ExprOp::Between {
                    of: a.logical_type(),
                    kind: *kind,
                    from,
                    to,
                    dst,
                });
                Ok(Some(dst))
            }
            // GF04's other half, the two folds. These are the first
            // calls whose answer is a string, so the vector they write
            // carries bytes of its own rather than numbers alone.
            BoundExpr::Call {
                func: func @ (Func::Upper | Func::Lower),
                args,
                distinct: false,
                star: false,
                ..
            } if args.len() == 1 => {
                let op = match func {
                    Func::Upper => StrFold::Upper,
                    _ => StrFold::Lower,
                };
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(ty) = op.answer_type(b.types[src as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::StrFold { op, src, dst });
                Ok(Some(dst))
            }
            // GF06 and GF05, the trim family: six spellings of one
            // loop. Two of the six differences are settled here rather
            // than in the kernel. A set that is not written out sends
            // the query back, since a set that is a column would be a
            // different set a row and the kernel prepares one; and a
            // TRIM handed more than a single character raises `22027`,
            // which the old engine says in its own words, so that shape
            // goes back as well rather than being said twice.
            BoundExpr::Call {
                func: Func::Trim(trim),
                args,
                distinct: false,
                star: false,
                ..
            } if (1..=2).contains(&args.len()) => {
                let chars = match args.get(1) {
                    None => " ".to_string(),
                    Some(arg) => match self.const_value(arg) {
                        Some(Value::Str(s)) => s,
                        _ => return Ok(None),
                    },
                };
                let ends = match trim {
                    Trim::Both | Trim::Btrim => StrTrim::Both,
                    Trim::Leading | Trim::Ltrim => StrTrim::Leading,
                    Trim::Trailing | Trim::Rtrim => StrTrim::Trailing,
                };
                let one_character = matches!(trim, Trim::Both | Trim::Leading | Trim::Trailing);
                if one_character && chars.chars().count() != 1 {
                    return Ok(None);
                }
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(ty) = ends.answer_type(b.types[src as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::StrTrim {
                    ends,
                    set: Arc::new(TrimSet::new(&chars)),
                    src,
                    dst,
                });
                Ok(Some(dst))
            }
            // ISO 20.24's substring function, which in GQL is LEFT and
            // RIGHT and nothing else. The count is an ordinary argument
            // rather than a word the statement wrote, so it compiles to
            // a register of its own and a column is one of the things
            // that can arrive in it, which is what separates this from
            // the trim's set and the normalization's form.
            BoundExpr::Call {
                func: Func::Cut(cut),
                args,
                distinct: false,
                star: false,
                ..
            } if args.len() == 2 => {
                let end = match cut {
                    Cut::Left => StrCut::Left,
                    Cut::Right => StrCut::Right,
                };
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(n) = self.value_reg(b, &args[1], level, false)? else {
                    return Ok(None);
                };
                let Some(ty) = end.answer_type(b.types[src as usize], b.types[n as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::StrCut { end, src, n, dst });
                Ok(Some(dst))
            }
            // GF08, the half of it that answers a string. NORMALIZE is
            // the string function whose answer is neither a part of the
            // argument nor the same length as it, so the vector it
            // writes carries bytes of the kernel's own and nothing in
            // it points back at the column it read.
            BoundExpr::Call {
                func: Func::Normalize(form),
                args,
                distinct: false,
                star: false,
                ..
            } if args.len() == 1 => {
                let Some(src) = self.value_reg(b, &args[0], level, false)? else {
                    return Ok(None);
                };
                let Some(ty) = StrNorm::Into(*form).answer_type(b.types[src as usize]) else {
                    return Ok(None);
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::StrNorm {
                    form: *form,
                    src,
                    dst,
                });
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
    ///
    /// A bound written as a float narrows to an integer bound first, so
    /// `age > 30.5` skips the chunks `age > 30` skips. Which chunks a
    /// query reads is then a question about the bound rather than about
    /// how the bound was spelled.
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
        let flipped = flip_cmp(*op).and_then(|f| self.const_bound(f, lhs));
        let (col_expr, cmp, c) = match (self.const_bound(*op, rhs), flipped) {
            (Some((cmp, c)), _) => (lhs, cmp, c),
            (None, Some((cmp, c))) => (rhs, cmp, c),
            (None, None) => return Ok(None),
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
        let (lo, hi) = match cmp {
            CmpOp::Eq => (c, c),
            CmpOp::Ge => (c, u64::MAX),
            CmpOp::Gt => match c.checked_add(1) {
                Some(lo) => (lo, u64::MAX),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(Some(ZonePred { col, lo, hi }))
    }

    /// The integer bound `expr` stands for on the right of `op`, read
    /// off an integer constant or off a float constant that narrows to
    /// one, and `None` when `expr` is not a constant the zones can use.
    fn const_bound(&self, op: BinaryOp, expr: &BoundExpr) -> Option<(CmpOp, i64)> {
        match self.const_int(expr) {
            Some(c) => cmp_op(op).map(|cmp| (cmp, c)),
            None => narrow_float(op, self.const_float(expr)?),
        }
    }

    /// Which vector op a numeric call is, or `None` where the kernels
    /// have nothing for it.
    ///
    /// ROUND is the one with a second argument, and the digit count has
    /// to be a number the statement wrote or bound: a count read off a
    /// column would be a branch inside the loop these kernels exist to
    /// keep branch free, and a query that rounds every row to a
    /// different place is rare enough to leave to the row engine.
    fn math_op(&self, math: Math, args: &[BoundExpr]) -> Option<MathOp> {
        match (math, args.len()) {
            (Math::Abs, 1) => Some(MathOp::Abs),
            (Math::Ceil, 1) => Some(MathOp::Ceil),
            (Math::Floor, 1) => Some(MathOp::Floor),
            (Math::Sign, 1) => Some(MathOp::Sign),
            (Math::Round, 1) => Some(MathOp::Round(0)),
            (Math::Round, 2) => self.const_int(&args[1]).map(MathOp::Round),
            (Math::Sqrt, 1) => Some(MathOp::Sqrt),
            (Math::Exp, 1) => Some(MathOp::Exp),
            (Math::Ln, 1) => Some(MathOp::Ln),
            (Math::Log10, 1) => Some(MathOp::Log10),
            (Math::Sin, 1) => Some(MathOp::Sin),
            (Math::Cos, 1) => Some(MathOp::Cos),
            (Math::Tan, 1) => Some(MathOp::Tan),
            (Math::Cot, 1) => Some(MathOp::Cot),
            (Math::Asin, 1) => Some(MathOp::Asin),
            (Math::Acos, 1) => Some(MathOp::Acos),
            (Math::Atan, 1) => Some(MathOp::Atan),
            (Math::Degrees, 1) => Some(MathOp::Degrees),
            (Math::Radians, 1) => Some(MathOp::Radians),
            // POWER, LOG and MOD take two numbers, so each of them is a
            // second column and the op below rather than this one.
            _ => None,
        }
    }

    /// The two sides of an operator as registers of one physical type,
    /// or None where they are two types and nothing may be done about
    /// it.
    ///
    /// The pair that arrives here is a value the kernels answer as a
    /// float against a whole number the query wrote: `sqrt(p.x) > 10`
    /// and `sqrt(p.x) * 2` are the ordinary spellings, and nobody
    /// writes the ten as `10.0` because the value it stands for is ten
    /// either way. The row engine reads such a pair by widening the
    /// integer to a float and answering in floats, so the constant is
    /// widened here and the comparison or the sum runs in the float
    /// kernel. Widening the constant rather than the column is what
    /// makes the two engines agree exactly: `as f64` is the conversion
    /// the row engine performs on the same value.
    fn matched(
        &self,
        b: &mut ProgBuilder,
        l: Reg,
        r: Reg,
        lhs: &BoundExpr,
        rhs: &BoundExpr,
    ) -> Result<Option<(Reg, Reg)>> {
        match (b.types[l as usize], b.types[r as usize]) {
            (x, y) if x == y => Ok(Some((l, r))),
            (PhysType::Float64, PhysType::Int64) => match self.const_int(rhs) {
                Some(n) => {
                    let r = b.push_const(OwnedValue::Float(n as f64))?;
                    Ok(Some((l, r)))
                }
                None => Ok(None),
            },
            (PhysType::Int64, PhysType::Float64) => match self.const_int(lhs) {
                Some(n) => {
                    let l = b.push_const(OwnedValue::Float(n as f64))?;
                    Ok(Some((l, r)))
                }
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn const_int(&self, expr: &BoundExpr) -> Option<i64> {
        match expr {
            BoundExpr::Literal(Literal::Int(n)) => Some(*n),
            BoundExpr::Param(ix) => match self.param(*ix) {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        }
    }

    /// A float the query wrote or bound, whichever way it wrote it.
    fn const_float(&self, expr: &BoundExpr) -> Option<f64> {
        match expr {
            BoundExpr::Literal(Literal::Float(f)) => Some(*f),
            BoundExpr::Param(ix) => match self.param(*ix) {
                Some(Value::Float(f)) => Some(*f),
                _ => None,
            },
            _ => None,
        }
    }
}

/// `x op c` over an integer `x` and a float `c`, rewritten as a
/// comparison against an integer.
///
/// A float that is not a whole number falls between two integers, so
/// the bound moves to the integer on the closed side and the operator
/// closes with it: `x > 30.5` is `x > 30` and `x < 30.5` is `x <= 30`.
/// A whole number keeps both the bound and the operator. Equality
/// against a fraction holds for no integer and its negation holds for
/// every one, and `i64::MIN` is the bound that says so without a
/// second kind of op: nothing is below it and everything is at or
/// above it.
///
/// The rewrite is refused above 2^53, which is where the two domains
/// stop agreeing. Under it every integer the comparison can meet
/// converts to a float without losing a digit, so the integer answer
/// and the float answer are the same answer; over it they are not, and
/// an engine that quietly gave a different one here than the row
/// engine gives would have made this an optimization with a result.
fn narrow_float(op: BinaryOp, c: f64) -> Option<(CmpOp, i64)> {
    const EXACT: f64 = (1u64 << 53) as f64;
    // A NaN is refused with them: it is not a bound at all, and every
    // comparison against it is false, which is an answer the kernel
    // has no integer to give.
    if c.is_nan() || c.abs() >= EXACT {
        return None;
    }
    let floor = c.floor();
    let k = floor as i64;
    if c == floor {
        return cmp_op(op).map(|cmp| (cmp, k));
    }
    Some(match op {
        BinaryOp::Lt | BinaryOp::Le => (CmpOp::Le, k),
        BinaryOp::Gt | BinaryOp::Ge => (CmpOp::Gt, k),
        BinaryOp::Eq => (CmpOp::Lt, i64::MIN),
        BinaryOp::Ne => (CmpOp::Ge, i64::MIN),
        _ => return None,
    })
}

/// Register and type bookkeeping for one program build.
///
/// Every register has a physical type and some of them have a lane
/// besides. The physical type is what the kernels read: it says how
/// wide the register is and how it compares. The lane is what the sink
/// reads, and only the temporal registers carry one, because
/// `PhysType::Interval` is a word of either months or nanoseconds and
/// `PhysType::Timestamp` is a word of either a time of day or an
/// instant. Nothing about the word says which, so the answer of a
/// program that ends on one would be a bare number without this.
struct ProgBuilder {
    ops: Vec<ExprOp>,
    types: Vec<PhysType>,
    lanes: Vec<Option<TemporalLane>>,
}

impl ProgBuilder {
    fn new() -> ProgBuilder {
        ProgBuilder {
            ops: Vec::new(),
            types: Vec::new(),
            lanes: Vec::new(),
        }
    }

    fn push_type(&mut self, ty: PhysType) -> Result<Reg> {
        self.push_reg(ty, None)
    }

    /// A register holding a temporal value of a known lane.
    fn push_lane(&mut self, lane: TemporalLane) -> Result<Reg> {
        self.push_reg(lane.phys(), Some(lane))
    }

    fn push_reg(&mut self, ty: PhysType, lane: Option<TemporalLane>) -> Result<Reg> {
        if self.types.len() >= usize::from(Reg::MAX) {
            // A 255-register filter is not a query anyone writes; the
            // fallback handles it if one ever shows up.
            return Err(zu_common::ZuError::InvalidArgument(
                "filter expression too large".into(),
            ));
        }
        self.types.push(ty);
        self.lanes.push(lane);
        Ok((self.types.len() - 1) as Reg)
    }

    /// The lane of a register, or `None` for one that holds a number,
    /// a string or a temporal value nobody named the lane of.
    fn lane(&self, reg: Reg) -> Option<TemporalLane> {
        self.lanes[reg as usize]
    }

    fn push_const(&mut self, v: OwnedValue) -> Result<Reg> {
        let ty = match &v {
            OwnedValue::Int(_) => PhysType::Int64,
            OwnedValue::Float(_) => PhysType::Float64,
            OwnedValue::Str(_) => PhysType::Str,
            OwnedValue::Lane { phys, .. } => *phys,
            _ => PhysType::Int64,
        };
        let dst = self.push_type(ty)?;
        self.ops.push(ExprOp::LoadConst { v, dst });
        Ok(dst)
    }

    /// A temporal constant, which carries its lane the way a temporal
    /// column does, so the two can meet in an operator.
    fn push_const_lane(&mut self, v: OwnedValue, lane: TemporalLane) -> Result<Reg> {
        let dst = self.push_lane(lane)?;
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

/// Whether a cast of a register holding `from` to `to` is the identity,
/// so the cast can be dropped and the filter stay on a kernel.
///
/// The bar is high on purpose. A cast is a conversion with a condition
/// behind it: a narrowing one raises 22003, an unsigned one raises 22003
/// on a negative, a declared precision is a digit count the engine owes
/// the user a check on, and a target written NOT NULL raises 22004 on a
/// null. Every one of those is a per-row decision the kernels have no op
/// for, so every one of them says no here. What is left is the cast that
/// asks a value for the type it already has, or for one that holds it
/// whole: an i64 read as INT64 or wider, and an f64 read as FLOAT64 or
/// wider. Those change no answer downstream, because the register keeps
/// the same physical width either way and the comparison it feeds sees
/// the same bits.
fn widens(from: PhysType, to: &LogicalType) -> bool {
    // A target written without NOT NULL is nullable, and a nullable
    // target is the only one that cannot raise on a null end.
    let LogicalType::Nullable(inner) = to else {
        return false;
    };
    match (from, inner.as_ref()) {
        (
            PhysType::Int64,
            LogicalType::Int {
                signed: true,
                bits,
                precision: None,
            },
        ) => bits.bits() >= 64,
        (
            PhysType::Float64,
            LogicalType::Float {
                bits,
                precision: None,
            },
        ) => bits.bits() >= 64,
        _ => false,
    }
}

/// Which vector op a numeric call of two arguments is. Unlike the
/// single argument table this one is a free function, since none of the
/// three needs anything the compiler knows.
fn math_pair(math: Math, args: &[BoundExpr]) -> Option<MathPair> {
    match (math, args.len()) {
        (Math::Power, 2) => Some(MathPair::Power),
        (Math::Log, 2) => Some(MathPair::Log),
        (Math::Mod, 2) => Some(MathPair::Mod),
        _ => None,
    }
}

/// Whether these ops hold something a row could have no answer for: a
/// division whose divisor is not written as a number that is not
/// nought, or a numeric function with a condition behind it.
fn may_raise(ops: &[ExprOp]) -> bool {
    ops.iter().enumerate().any(|(i, op)| match op {
        ExprOp::Binary {
            op: BinOp::Div | BinOp::Mod,
            r,
            ..
        } => !written_nonzero(&ops[..i], *r),
        ExprOp::Math { op, .. } => op.may_raise(),
        // MOD under its function spelling is the operator's answers, so
        // it is the operator's question too: a written divisor that is
        // not nought has a remainder for every row. A power and a
        // logarithm have conditions no fold over one written number
        // settles, so both of them stand behind a filter and nowhere
        // else.
        ExprOp::MathPair {
            op: MathPair::Mod,
            r,
            ..
        } => !written_nonzero(&ops[..i], *r),
        ExprOp::MathPair { .. } => true,
        // A length of time between two instants, which has a condition
        // no fold over the operands settles: a date the calendar has is
        // not always a number of nanoseconds, so a pair of perfectly
        // ordinary dates can have no day-time duration between them.
        // The call stands behind a filter, where the row engine
        // evaluates it too, and not in a computed column, which is
        // filled before the filter that would have dropped the row.
        ExprOp::Between { .. } => true,
        // A count has an answer for every string there is, so it is a
        // computed column like a floor or an angle. So does a fold, and
        // so does a trim: the trim family's one condition is about the
        // set a statement wrote and the compiler settles it there. So
        // do both normalizations, every string having a normal form and
        // either being in it or not. And so does an identifier: a node
        // has one, and the two numbers it is written from are a level's
        // own rather than anything a row can be wrong about.
        ExprOp::StrLen { .. }
        | ExprOp::StrFold { .. }
        | ExprOp::StrTrim { .. }
        | ExprOp::StrNorm { .. }
        | ExprOp::StrNormalized { .. }
        | ExprOp::ElementId { .. } => false,
        // A cut is the one string op whose condition is about a value
        // rather than about what the statement wrote, a string having
        // no negative number of characters and a column being able to
        // hold one. A count written out is settled here the way a
        // written divisor is, and every other count stands behind a
        // filter.
        ExprOp::StrCut { n, .. } => !written_nonneg(&ops[..i], *n),
        _ => false,
    })
}

/// Whether these ops hold a connective, which is what decides per row
/// how much of the expression around it is measured at all. The old
/// engine stops at the operand that decided the row and the program
/// runs every op over the whole chunk, so `n <> 0 AND 100 / n > 5` is
/// the one shape where a plan that drops nothing still asks a question
/// the query said it would not.
fn short_circuits(ops: &[ExprOp]) -> bool {
    ops.iter()
        .any(|op| matches!(op, ExprOp::And { .. } | ExprOp::Or { .. }))
}

/// How many of a program's ops have a condition, meaning a row they
/// could have no answer for. Arithmetic over whole numbers is one of
/// them, an answer that does not fit being a condition rather than an
/// infinity, and so is every function `may_raise` names.
///
/// The count is what matters and not the list. One condition in a
/// program is raised by the first row that has it, which is the same
/// row on both engines and so the same message. Two are raised in
/// different orders: the program runs an op over the whole chunk
/// before the next op sees a row, so it finds the earlier op's
/// condition on a later row, while the old engine finishes each row
/// before it starts the next and finds whichever condition the first
/// offending row has. `abs(-9223372036854775807 - p.id)` over a column
/// of small numbers is the shape, the difference an integer that left
/// the range against a distance that did.
fn conditions(b: &ProgBuilder) -> usize {
    b.ops
        .iter()
        .filter(|op| match op {
            ExprOp::Binary {
                op: BinOp::Add | BinOp::Sub | BinOp::Mul,
                l,
                ..
            } => matches!(b.types[*l as usize], PhysType::Int64),
            ExprOp::Binary {
                op: BinOp::Div | BinOp::Mod,
                ..
            } => true,
            ExprOp::Math { op, .. } => op.may_raise(),
            ExprOp::MathPair { .. } | ExprOp::Between { .. } | ExprOp::StrCut { .. } => true,
            _ => false,
        })
        .count()
}

/// Whether a register was last loaded with a number that is not
/// nought. The builder gives every op a register of its own, so the
/// last load into one is the whole of what it holds.
fn written_nonzero(before: &[ExprOp], reg: Reg) -> bool {
    before
        .iter()
        .rev()
        .find_map(|op| match op {
            ExprOp::LoadConst {
                v: OwnedValue::Int(n),
                dst,
            } if *dst == reg => Some(*n != 0),
            ExprOp::LoadConst {
                v: OwnedValue::Float(f),
                dst,
            } if *dst == reg => Some(*f != 0.0),
            _ => None,
        })
        .unwrap_or(false)
}

/// Whether a register was last loaded with a number at nought or
/// above, which is the question a substring's count asks. A count the
/// statement wrote is the ordinary case and the only one that can be
/// settled without looking at a row.
fn written_nonneg(before: &[ExprOp], reg: Reg) -> bool {
    before
        .iter()
        .rev()
        .find_map(|op| match op {
            ExprOp::LoadConst {
                v: OwnedValue::Int(n),
                dst,
            } if *dst == reg => Some(*n >= 0),
            _ => None,
        })
        .unwrap_or(false)
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
    direction.flip()
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

/// The bracket a plan node was written inside, for the three kinds of
/// node that can carry one.
/// Whether the next node is an existence block over a bare pattern:
/// one hop under a semi or an anti bracket with nothing else in the
/// group. That is the block the degrees answer on their own, and the
/// only one that can stand inside another bracket's continuation.
fn bare_block<'a>(it: &mut impl Iterator<Item = &'a LogicalPlan>) -> bool {
    let Some(LogicalPlan::Expand {
        range: None,
        into: false,
        bracket: Some(group),
        ..
    }) = it.next()
    else {
        return false;
    };
    group.kind != BracketKind::Optional && it.next().and_then(node_bracket) != Some(*group)
}

fn node_bracket(plan: &LogicalPlan) -> Option<Bracket> {
    match plan {
        LogicalPlan::ScanNodes { bracket, .. }
        | LogicalPlan::Expand { bracket, .. }
        | LogicalPlan::Filter { bracket, .. } => *bracket,
        _ => None,
    }
}

fn expand_dirs(
    schema: &Schema,
    rel: RelId,
    src_table: TableId,
    direction: RelDirection,
) -> Option<Dirs> {
    let def = schema.rel_by_id(rel)?;
    // An undirected table answers both ways round, and a pattern that
    // does not admit it walks nothing here at all (GH02).
    let direction = direction.resolve(def.undirected)?;
    let fwd = src_table == def.from && direction.walks_out();
    let bwd = src_table == def.to && direction.walks_in();
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
    use zu_common::types::{FloatBits, IntBits};

    /// Every integer the rewritten predicate answers the same way the
    /// float one does, which is the whole claim the rewrite makes.
    #[test]
    fn a_float_bound_moves_into_the_integer_domain_without_moving_the_answer() {
        let ops = [
            BinaryOp::Lt,
            BinaryOp::Le,
            BinaryOp::Gt,
            BinaryOp::Ge,
            BinaryOp::Eq,
            BinaryOp::Ne,
        ];
        for c in [30.5, 30.0, -0.5, -30.5, 0.0, -1.0, 1e15, 8.5e15] {
            for op in ops {
                let (cmp, k) = narrow_float(op, c).unwrap_or_else(|| panic!("{op:?} {c}"));
                for x in [-31i64, -1, 0, 30, 31, 1_000_000_000_000_000] {
                    let float_answer = match op {
                        BinaryOp::Lt => (x as f64) < c,
                        BinaryOp::Le => (x as f64) <= c,
                        BinaryOp::Gt => (x as f64) > c,
                        BinaryOp::Ge => (x as f64) >= c,
                        BinaryOp::Eq => (x as f64) == c,
                        BinaryOp::Ne => (x as f64) != c,
                        _ => unreachable!("the list above is comparisons"),
                    };
                    assert_eq!(cmp.holds(x, k), float_answer, "{x} {op:?} {c}");
                }
            }
        }
    }

    #[test]
    fn a_bound_the_two_domains_disagree_about_is_refused() {
        // 2^53 and up, where an integer stops surviving the conversion,
        // and the two values that are not bounds at all.
        for c in [
            9.007199254740992e15,
            -9.007199254740992e15,
            1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            assert!(narrow_float(BinaryOp::Lt, c).is_none(), "{c}");
        }
        // Not a comparison, so there is nothing to move.
        assert!(narrow_float(BinaryOp::Add, 1.5).is_none());
    }

    fn int(signed: bool, bits: IntBits, precision: Option<u16>) -> LogicalType {
        LogicalType::Int {
            signed,
            bits,
            precision,
        }
    }

    fn nullable(ty: LogicalType) -> LogicalType {
        LogicalType::Nullable(Box::new(ty))
    }

    /// The cast that can be dropped is the one that asks a value for a
    /// type it already fits in whole, and nothing else. Each refusal
    /// here stands for a per row decision the kernels cannot make: a
    /// narrower width or an unsigned target raises 22003 on a value
    /// that does not fit, a declared precision is a digit count the
    /// engine owes the user a check on, and a target written NOT NULL
    /// raises 22004 on a null.
    #[test]
    fn only_a_cast_that_cannot_change_a_value_is_dropped() {
        for bits in [IntBits::B64, IntBits::B128, IntBits::B256] {
            assert!(widens(PhysType::Int64, &nullable(int(true, bits, None))));
        }
        for bits in [IntBits::B8, IntBits::B16, IntBits::B32] {
            assert!(!widens(PhysType::Int64, &nullable(int(true, bits, None))));
        }
        assert!(!widens(
            PhysType::Int64,
            &nullable(int(false, IntBits::B64, None))
        ));
        assert!(!widens(
            PhysType::Int64,
            &nullable(int(true, IntBits::B64, Some(18)))
        ));
        assert!(!widens(PhysType::Int64, &int(true, IntBits::B64, None)));

        let f64_ty = |bits| LogicalType::Float {
            bits,
            precision: None,
        };
        for bits in [FloatBits::B64, FloatBits::B128, FloatBits::B256] {
            assert!(widens(PhysType::Float64, &nullable(f64_ty(bits))));
        }
        for bits in [FloatBits::B16, FloatBits::B32] {
            assert!(!widens(PhysType::Float64, &nullable(f64_ty(bits))));
        }
        // The two towers do not cross: an integer read as a float is a
        // conversion the register would have to carry out, and a float
        // read as an integer rounds.
        assert!(!widens(PhysType::Int64, &nullable(f64_ty(FloatBits::B64))));
        assert!(!widens(
            PhysType::Float64,
            &nullable(int(true, IntBits::B64, None))
        ));
        // A string is stored as a view and every string type carries a
        // length bound, so no string cast is free.
        assert!(!widens(
            PhysType::Str,
            &nullable(LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            })
        ));
    }

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

    /// Half of what SAME and ALL_DIFFERENT ask is answered by the
    /// tables before a row is read, and the fixtures the parity suite
    /// runs over hold one table each, so this is where the other table
    /// gets a say.
    #[test]
    fn a_pair_of_levels_on_different_tables_is_two_elements() {
        let mut people = level(Vec::new());
        people.table = 7;
        let mut places = level(Vec::new());
        places.table = 9;
        let levels = vec![people, places];
        // One level named twice is one element, whatever its table.
        assert_eq!(settled_pair(&levels, 0, 0), Some(true));
        assert_eq!(settled_pair(&levels, 1, 1), Some(true));
        // Two tables cannot hold one node.
        assert_eq!(settled_pair(&levels, 0, 1), Some(false));
        assert_eq!(settled_pair(&levels, 1, 0), Some(false));
        // Two levels on one table, which is the pair the rows decide.
        let same_table = vec![level(Vec::new()), level(Vec::new())];
        assert_eq!(settled_pair(&same_table, 0, 1), None);
    }

    fn closes(ops: &[Op]) -> Vec<Option<usize>> {
        ops.iter()
            .filter_map(|op| match op {
                Op::Expand { close, .. } => Some(close.map(|c| c.probe_level)),
                _ => None,
            })
            .collect()
    }

    /// A bare EXISTS block reads the rows of the level it sits on, the
    /// same as a filter, so the walk that built them cannot be fused
    /// into a degree read and taken away underneath it. It says which
    /// level that is, so the question is names_level's and the answer
    /// holds for the level it names and no other.
    #[test]
    fn a_bare_block_reads_the_level_it_names() {
        let block = |from| Op::HasEdge {
            rel: 0,
            dirs: Dirs::One(Dir::Fwd),
            from,
            negated: false,
        };
        assert!(names_level(&block(1), 1));
        assert!(!names_level(&block(1), 0));
        // A block on a level the pipeline has walked past leaves the
        // newest one alone, so an expand that built it is still free
        // to fuse away.
        assert!(!reads_newest(&[block(0)]));
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
        batch_walks(&mut ops, &SinkSpec::Count, &[]);
        assert_eq!(
            batched(&ops),
            [true, false],
            "the close reads level 1 through its pin, fused or not"
        );
    }

    #[test]
    fn an_expand_batches_when_nothing_above_reads_its_source() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_walks(&mut ops, &SinkSpec::Count, &[]);
        assert_eq!(batched(&ops), [true, true], "a bare count reads no level");

        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_walks(
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

    /// The close is a walk off the level under the one it builds, so
    /// the same rule applies to it, and it is the one that needs it:
    /// a wedge closes on a node or two and the row at a time descent
    /// builds a level for those two.
    #[test]
    fn a_close_batches_on_the_same_rule_the_expand_does() {
        let wcoj = |probe_level, to| Op::Intersect {
            seed: (0, Dirs::One(Dir::Fwd)),
            probe: (0, Dirs::One(Dir::Fwd)),
            probe_level,
            to,
            batch: false,
        };
        let closed = |ops: &[Op]| match ops.last() {
            Some(Op::Intersect { batch, .. }) => *batch,
            _ => unreachable!("the close is the last op here"),
        };

        let mut ops = vec![hop(0, 1), wcoj(0, 2)];
        batch_walks(&mut ops, &SinkSpec::Count, &[]);
        assert!(closed(&ops), "a bare count reads neither end of the wedge");

        // The seed level is level 1 here, which is `to` minus one and
        // never the probe level, so probing the far end is no reason
        // for the close itself to stay row at a time.
        let mut ops = vec![hop(0, 1), wcoj(0, 2)];
        batch_walks(
            &mut ops,
            &SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 0 }],
                post: Vec::new(),
            },
            &[],
        );
        assert!(closed(&ops), "the projected level is the pinned far end");

        let mut ops = vec![hop(0, 1), wcoj(0, 2)];
        batch_walks(
            &mut ops,
            &SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 1 }],
                post: Vec::new(),
            },
            &[],
        );
        assert!(
            !closed(&ops),
            "the wedge middle is projected, so the close keeps its pin"
        );
    }

    #[test]
    fn an_expand_whose_source_is_read_above_it_keeps_its_pin() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_walks(
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
        batch_walks(&mut ops, &SinkSpec::Count, &[]);
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
        batch_walks(&mut ops, &SinkSpec::Count, &levels);
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
        batch_walks(&mut ops, &SinkSpec::Count, &levels);
        assert_eq!(
            batched(&ops),
            [false, true],
            "reaching past a level is the business of the expand that walks off the one it names"
        );
    }

    #[test]
    fn an_aggregate_argument_counts_as_a_read() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_walks(
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
