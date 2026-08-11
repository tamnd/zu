//! Plan compiler: one supported LogicalPlan chain becomes one push
//! pipeline, anything else becomes `None` and the caller falls back to
//! the old executor.
//!
//! The supported shape today is the linear read pipeline: a single
//! non-optional node scan, filters, single-hop expands that walk off
//! the newest level, and one final Project or Aggregate with its
//! absorbed Distinct, Sort, Skip, and Limit. Everything the old
//! executor also covers, variable-length expands, optional groups,
//! closing joins, unwind, table functions, and rel values, falls back.
//! The bar for anything compiled here is exact old-engine output:
//! same rows, same order, same errors on overflow.

use std::collections::HashMap;

use zu_common::Result;
use zu_query::ast::{BinaryOp, Literal, RelDirection};
use zu_query::binder::{BoundClause, BoundExpr, BoundItem, BoundQuery, Func, Schema};
use zu_query::exec::{Options, Value, Wcoj};
use zu_query::plan::LogicalPlan;
use zu_query::snapshot::{ColId, ColType, Dir, RelId, Snapshot, TableId, ZonePred};
use zu_vector::{BinOp, CmpOp, ExprOp, OwnedValue, PhysType, Program, Reg};

/// One compiled pipeline over one driving scan.
pub(crate) struct ExecPlan {
    pub table: TableId,
    pub source: Source,
    pub ops: Vec<Op>,
    pub sink: SinkSpec,
    pub levels: Vec<Level>,
    pub columns: Vec<String>,
}

/// Where level 0 comes from.
pub(crate) enum Source {
    /// The driving scan, carrying any zone pushdown a level 0 filter
    /// gave up.
    Scan(Option<ZonePred>),
    /// The primary-key seek an `{id: k}` predicate folds into: one row
    /// or none, and no scan at all.
    Seek(u64),
}

impl ExecPlan {
    /// The zone pushdown the scan runs with; a seek has no chunks to
    /// skip.
    pub fn zone(&self) -> Option<&ZonePred> {
        match &self.source {
            Source::Scan(pred) => pred.as_ref(),
            Source::Seek(_) => None,
        }
    }
}

/// One factorization level: the scan at index 0, one per expand after.
pub(crate) struct Level {
    pub table: TableId,
    /// Property columns this level materializes; entry i lives at chunk
    /// vector i + 1, vector 0 is always the row id.
    pub cols: Vec<(ColId, ColType)>,
}

/// Traversal sides of one expand: `Both` is an undirected step over a
/// self-referencing rel, forward list first, matching the old engine's
/// emission order.
#[derive(Clone, Copy)]
pub(crate) enum Dirs {
    One(Dir),
    Both,
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
    },
    /// The WCOJ close (docs/07 section 4, perf/05 section 3): the
    /// expand that would build the closing node and the probe back
    /// into it are one intersection of two sorted neighbor lists. The
    /// seed list hangs off the newest level, the probe list off a
    /// level pinned above it, so a wedge closes in one leapfrog walk
    /// instead of a storage probe per candidate.
    Intersect {
        seed: (RelId, Dir),
        probe: (RelId, Dir),
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
    /// Terminal fusion of trailing expands feeding a bare count: each
    /// active row of the newest level contributes the product of its
    /// per-step degrees, read off the CSR offsets alone. One step is
    /// the plain expand-then-count fusion; several steps are a hub
    /// plan, expands fanning out of one level with nothing reading
    /// the far ends.
    DegreeProduct { steps: Vec<(RelId, Dirs)> },
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
    /// ORDER BY over output columns: the column each key reads and
    /// whether it ascends. A key that names something the projection
    /// does not output falls back to the old engine.
    Sort(Vec<(usize, bool)>),
    Skip(u64),
    Limit(u64),
}

pub(crate) enum SinkSpec {
    /// The bare global count(*): one accumulator, fed by multiplicity
    /// or by a fused DegreeProduct.
    Count,
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
        SinkSpec::Agg { keys, aggs, .. } => {
            keys.iter().any(named) || aggs.iter().filter_map(AggSpec::arg).any(|r| named(&r))
        }
        SinkSpec::Rows { items, .. } => items.iter().any(named),
    }
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
fn batch_expands(ops: &mut [Op], sink: &SinkSpec) {
    for i in 0..ops.len() {
        let Op::Expand { from, .. } = ops[i] else {
            continue;
        };
        let probed = ops[i + 1..].iter().any(|op| match op {
            Op::Intersect { probe_level, .. } | Op::Semi { probe_level, .. } => {
                *probe_level == from
            }
            _ => false,
        });
        if let Op::Expand { batch, .. } = &mut ops[i] {
            *batch = !probed && !sink_reads(sink, from);
        }
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
        levels: Vec::new(),
        slot_level: HashMap::new(),
    };
    c.compile(plan)
}

/// A level under construction: the registry assigns chunk vector
/// positions as columns are demanded, so programs built mid-walk hold
/// stable indices.
struct LevelBuild {
    table: TableId,
    cols: Vec<(String, ColId, ColType)>,
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
    levels: Vec<LevelBuild>,
    slot_level: HashMap<usize, usize>,
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
        if let Some(LogicalPlan::Filter {
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

        // Filters and expands, in written order, always off the newest
        // level.
        let mut ops = Vec::new();
        let mut pred = None;
        loop {
            match it.peek() {
                Some(LogicalPlan::Filter {
                    expr,
                    optional: None,
                    ..
                }) => {
                    it.next();
                    let level = self.levels.len() - 1;
                    let Some(prog) = self.build_prog(expr, level)? else {
                        return Ok(None);
                    };
                    if level == 0 && ops.is_empty() && pred.is_none() && seek.is_none() {
                        pred = self.zone_pred(expr)?;
                    }
                    ops.push(Op::Filter { prog });
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
                    });
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
                    let Some(op) = self.fuse_close(&ops, *rel, *from, *to, *direction) else {
                        return Ok(None);
                    };
                    it.next();
                    ops.pop();
                    ops.push(op);
                    ops.extend(held.into_iter().rev());
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

        let sink = match sink_node {
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
            _ => return Ok(None),
        };

        // Fuse trailing expands feeding a bare count into one degree
        // product when nothing reads the expanded levels' rows or
        // columns. The steps must fan out of one source level: a
        // single popped expand is the plain expand-then-count fusion,
        // several are a hub, the shape the optimizer picks for an
        // unseeded two-hop, where the count per source row is the
        // product of its per-step degrees.
        if matches!(sink, SinkSpec::Count) {
            let mut steps = Vec::new();
            let mut step_from = None;
            while let Some(Op::Expand {
                rel,
                dirs,
                from,
                to,
                ..
            }) = ops.last()
            {
                if !self.levels[*to].cols.is_empty() || step_from.is_some_and(|f| f != *from) {
                    break;
                }
                step_from = Some(*from);
                steps.push((*rel, *dirs));
                ops.pop();
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
                if from != newest_after {
                    // The steps hang off a level the surviving
                    // pipeline does not end on; validation below
                    // rejects the unfused shape too.
                    return Ok(None);
                }
                ops.push(Op::DegreeProduct { steps });
            }
        }

        // After fusion every surviving expand must walk off the newest
        // level, the invariant the runner's pin-and-descend loop is
        // built on. The hub shapes fusion could not absorb fall back
        // to the old engine here.
        let mut newest = 0;
        for op in &ops {
            match op {
                Op::Expand { from, to, .. } => {
                    if *from != newest {
                        return Ok(None);
                    }
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
                _ => {}
            }
        }

        batch_expands(&mut ops, &sink);

        Ok(Some(ExecPlan {
            table,
            source: match seek {
                Some(key) => Source::Seek(key),
                None => Source::Scan(pred),
            },
            ops,
            sink,
            levels: self
                .levels
                .drain(..)
                .map(|l| Level {
                    table: l.table,
                    cols: l.cols.into_iter().map(|(_, id, ty)| (id, ty)).collect(),
                })
                .collect(),
            columns: self.query.columns.clone(),
        }))
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
    /// covers: both sides one direction over one rel table, one of the
    /// two lists hanging off the newest level so it is walked row by
    /// row, and the other hanging off a level below it so it is pinned
    /// and read once for the whole vector.
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
            dirs: Dirs::One(built_dir),
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
        let Dirs::One(close_dir) = expand_dirs(
            self.schema,
            close_rel,
            self.levels[far_level].table,
            far_dir,
        )?
        else {
            return None;
        };
        if far_table(self.schema, close_rel, close_dir)? != self.levels[built_to].table {
            return None;
        }
        // The newest level is the one the pipeline ends on before this
        // pair, and the other list has to sit strictly below it.
        let newest = built_to - 1;
        let (seed, probe, probe_level) = if built_from == newest && far_level < newest {
            ((built_rel, built_dir), (close_rel, close_dir), far_level)
        } else if far_level == newest && built_from < newest {
            ((close_rel, close_dir), (built_rel, built_dir), built_from)
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
    fn sort_cols(&self, keys: &[(BoundExpr, bool)]) -> Option<Vec<(usize, bool)>> {
        let BoundClause::Project { items, .. } = self.query.clauses.last()? else {
            return None;
        };
        let mut cols = Vec::with_capacity(keys.len());
        for (expr, asc) in keys {
            let at = items.iter().position(|item| {
                item.expr == *expr
                    || matches!(expr, BoundExpr::Var(slot) if self.item_slot(item) == Some(*slot))
            })?;
            cols.push((at, *asc));
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
        if let Some(ix) = self.levels[level].cols.iter().position(|(k, ..)| k == key) {
            let (_, _, ty) = self.levels[level].cols[ix];
            return Ok(Some((ix + 1, ty)));
        }
        let Some((id, ty)) = self.snap.resolve_col(self.levels[level].table, key)? else {
            return Ok(None);
        };
        self.levels[level].cols.push((key.to_string(), id, ty));
        Ok(Some((self.levels[level].cols.len(), ty)))
    }

    /// Maps a projection, key, or argument expression to a scalar the
    /// sink can read: a node slot, a property column, or the dense id.
    fn item_ref(&mut self, expr: &BoundExpr) -> Result<Option<ScalarRef>> {
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
            _ => Ok(None),
        }
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
            let Some(l) = self.value_reg(b, lhs, level)? else {
                return Ok(None);
            };
            let Some(r) = self.value_reg(b, rhs, level)? else {
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

    fn value_reg(
        &mut self,
        b: &mut ProgBuilder,
        expr: &BoundExpr,
        level: usize,
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
            BoundExpr::Property { .. } => {
                let Some(r) = self.item_ref(expr)? else {
                    return Ok(None);
                };
                if r.level() != level {
                    return Ok(None);
                }
                let (col, ty) = match r {
                    ScalarRef::RowId { .. } => (0, PhysType::Int64),
                    ScalarRef::Col { vec, ty, .. } => (
                        vec,
                        match ty {
                            ColType::Int => PhysType::Int64,
                            ColType::Str => PhysType::Str,
                        },
                    ),
                    ScalarRef::Node { .. } => return Ok(None),
                };
                let dst = b.push_type(ty)?;
                b.ops.push(ExprOp::LoadCol {
                    col: col as u8,
                    dst,
                });
                Ok(Some(dst))
            }
            BoundExpr::Binary { op, lhs, rhs } => {
                let Some(bin) = bin_op(*op) else {
                    return Ok(None);
                };
                let Some(l) = self.value_reg(b, lhs, level)? else {
                    return Ok(None);
                };
                let Some(r) = self.value_reg(b, rhs, level)? else {
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
        let Some(ScalarRef::Col {
            level: 0,
            vec,
            ty: ColType::Int,
        }) = self.item_ref(col_expr)?
        else {
            return Ok(None);
        };
        let col = self.levels[0].cols[vec - 1].1;
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
        if on_id && matches!(key.as_ref(), BoundExpr::Literal(_) | BoundExpr::Param(_)) {
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

    #[test]
    fn an_expand_batches_when_nothing_above_reads_its_source() {
        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(&mut ops, &SinkSpec::Count);
        assert_eq!(batched(&ops), [true, true], "a bare count reads no level");

        let mut ops = vec![hop(0, 1), hop(1, 2)];
        batch_expands(
            &mut ops,
            &SinkSpec::Rows {
                items: vec![ScalarRef::RowId { level: 2 }],
                post: Vec::new(),
            },
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
        batch_expands(&mut ops, &SinkSpec::Count);
        assert_eq!(
            batched(&ops),
            [true, false],
            "the semi join probes level 1 through its pin"
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
        );
        assert_eq!(batched(&ops), [true, false], "sum reads level 1 per path");
    }
}
