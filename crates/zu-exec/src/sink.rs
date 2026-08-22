//! Sink partials and the final merge.
//!
//! Every worker owns one [`SinkState`] and pushes into it lock-free;
//! the merge at the end of the run folds the partials and reproduces
//! the old executor's output exactly: group rows in ascending key
//! order, distinct keeping the first occurrence, skip and limit in
//! plan order. Aggregate accumulators mirror the old `Acc` semantics
//! including the empty-input rows and the sum overflow error.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::ast::{BinaryOp, SortKey};
use zu_query::exec::{OrdValue, QueryResult, Value};

use crate::columns::ColumnSink;
use crate::compile::{AggSpec, PostAgg, PostPred, PostSpec};
use crate::group::{GroupTable, KeyBatch, PartKind};

/// Rows per DISTINCT probe vector, the pipeline's vector width.
const VECTOR: usize = 2048;

/// One aggregate accumulator, the integer subset of the old engine's
/// Acc with identical finalize semantics: sum of nothing is 0, avg of
/// nothing is null, min and max of nothing are null.
///
/// There is deliberately no counterpart here to the old engine's
/// `01G11 null value eliminated in set function`. This accumulator
/// never sees a null: `Compiler::agg_spec` declines an argument off an
/// optional level or off a kernel column that answers null, and dense
/// stored columns have a value on every row. So a statement that could
/// raise the warning is one this executor refused to compile, and the
/// old engine raises it there. If that gate ever loosens, the warning
/// has to arrive here at the same time, or the two executors will
/// answer the same query with two different envelopes.
#[derive(Clone, Copy)]
pub(crate) enum Acc {
    Count(i64),
    Sum(Option<i64>),
    Avg { sum: f64, n: i64 },
    Min(Option<i64>),
    Max(Option<i64>),
}

impl Acc {
    pub(crate) fn new(spec: &AggSpec) -> Acc {
        match spec {
            AggSpec::CountStar | AggSpec::CountRef(_) => Acc::Count(0),
            AggSpec::Sum(_) => Acc::Sum(None),
            AggSpec::Avg(_) => Acc::Avg { sum: 0.0, n: 0 },
            AggSpec::Min(_) => Acc::Min(None),
            AggSpec::Max(_) => Acc::Max(None),
        }
    }

    /// Count contribution of `mult` logical rows.
    pub(crate) fn add_star(&mut self, mult: i64) {
        if let Acc::Count(n) = self {
            *n += mult;
        }
    }

    /// One integer argument value standing for `mult` logical rows.
    pub(crate) fn add_int(&mut self, v: i64, mult: i64) -> Result<()> {
        match self {
            Acc::Count(n) => *n += mult,
            Acc::Sum(acc) => {
                let scaled = v
                    .checked_mul(mult)
                    .ok_or_else(|| ZuError::gql(codes::C22003, "integer overflow in sum()"))?;
                *acc = Some(match *acc {
                    None => scaled,
                    Some(prev) => prev
                        .checked_add(scaled)
                        .ok_or_else(|| ZuError::gql(codes::C22003, "integer overflow in sum()"))?,
                });
            }
            Acc::Avg { sum, n } => {
                *sum += v as f64 * mult as f64;
                *n += mult;
            }
            Acc::Min(cur) => {
                if cur.is_none_or(|c| v < c) {
                    *cur = Some(v);
                }
            }
            Acc::Max(cur) => {
                if cur.is_none_or(|c| v > c) {
                    *cur = Some(v);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: &Acc) -> Result<()> {
        match (self, *other) {
            (Acc::Count(n), Acc::Count(m)) => *n += m,
            (Acc::Sum(a), Acc::Sum(b)) => {
                *a =
                    match (*a, b) {
                        (Some(x), Some(y)) => Some(x.checked_add(y).ok_or_else(|| {
                            ZuError::gql(codes::C22003, "integer overflow in sum()")
                        })?),
                        (x, y) => x.or(y),
                    };
            }
            (Acc::Avg { sum, n }, Acc::Avg { sum: s2, n: n2 }) => {
                *sum += s2;
                *n += n2;
            }
            (Acc::Min(a), Acc::Min(b)) => {
                if let Some(v) = b
                    && a.is_none_or(|c| v < c)
                {
                    *a = Some(v);
                }
            }
            (Acc::Max(a), Acc::Max(b)) => {
                if let Some(v) = b
                    && a.is_none_or(|c| v > c)
                {
                    *a = Some(v);
                }
            }
            _ => unreachable!("partials built from one spec list"),
        }
        Ok(())
    }

    fn finalize(self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum(acc) => Value::Int(acc.unwrap_or(0)),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(sum / n as f64),
            Acc::Min(cur) | Acc::Max(cur) => cur.map_or(Value::Null, Value::Int),
        }
    }
}

/// One worker's sink partial. Which fields carry data depends on the
/// plan's sink kind; keeping one struct spares the driver a generic.
#[derive(Default)]
pub(crate) struct SinkState {
    /// The bare count sink, also fed directly by DegreeCount.
    pub count: i64,
    /// Keyed aggregation partials, built on the worker's first keyed
    /// row so a plan that never groups never pays for the table.
    pub groups: Option<GroupTable>,
    /// States of the single group a bare aggregate has.
    pub bare: Vec<Acc>,
    /// Rows of the morsel in flight.
    pub rows: Vec<Vec<Value>>,
    /// Finished morsels: (morsel index, its rows).
    pub batches: Vec<(usize, Vec<Vec<Value>>)>,
    /// The bounded buffer, on the plans whose ORDER BY sits under a
    /// LIMIT. A worker running with one never fills `rows`.
    pub top: Option<TopN>,
    /// The columns, on the plans with nothing above the projection. A
    /// worker running with these never fills `rows` either, and the
    /// answer they make is the one a columnar client reads without a
    /// transpose and a row reader builds its rows out of.
    pub cols: Option<ColumnSink>,
}

/// Post steps over materialized rows, exactly the old apply_post.
pub(crate) fn apply_post(post: &[PostSpec], mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    for (at, op) in post.iter().enumerate() {
        match op {
            PostSpec::Distinct => rows = distinct(rows),
            PostSpec::Sort(keys) => sort_rows(&mut rows, keys, needed_after(&post[at + 1..])),
            PostSpec::Skip(n) => {
                let n = (*n as usize).min(rows.len());
                rows.drain(..n);
            }
            PostSpec::Limit(n) => rows.truncate(*n as usize),
            // A group the predicate cannot answer for is a group it
            // does not keep, which is what a null does to a WHERE.
            PostSpec::Having(pred) => rows.retain(|row| holds(pred, row) == Some(true)),
            PostSpec::Regroup {
                keys,
                aggs,
                item_agg,
            } => rows = regroup(keys, aggs, item_agg, rows),
            PostSpec::Emit(cols) => {
                for row in &mut rows {
                    *row = cols.iter().map(|&c| row[c].clone()).collect();
                }
            }
        }
    }
    rows
}

/// What a HAVING says about one group: true, false, or nothing at all
/// where the two sides of a comparison do not compare.
fn holds(pred: &PostPred, row: &[Value]) -> Option<bool> {
    match pred {
        PostPred::Cmp(op, at, want) => compares(*op, &row[*at], want),
        PostPred::And(parts) => {
            let mut all = Some(true);
            for p in parts {
                match holds(p, row) {
                    Some(false) => return Some(false),
                    None => all = None,
                    Some(true) => {}
                }
            }
            all
        }
        PostPred::Or(parts) => {
            let mut any = Some(false);
            for p in parts {
                match holds(p, row) {
                    Some(true) => return Some(true),
                    None => any = None,
                    Some(false) => {}
                }
            }
            any
        }
    }
}

/// One comparison of a group's value against the constant the query
/// wrote. Two values of different kinds do not compare, and neither
/// does a null, so those answer with nothing.
fn compares(op: BinaryOp, have: &Value, want: &Value) -> Option<bool> {
    let ord = match (have, want) {
        (Value::Int(p), Value::Int(q)) => p.cmp(q),
        (Value::Float(p), Value::Float(q)) => p.partial_cmp(q)?,
        (Value::Int(p), Value::Float(q)) => (*p as f64).partial_cmp(q)?,
        (Value::Float(p), Value::Int(q)) => p.partial_cmp(&(*q as f64))?,
        (Value::Str(p), Value::Str(q)) => p.cmp(q),
        (Value::Bool(p), Value::Bool(q)) => p.cmp(q),
        _ => return None,
    };
    Some(match op {
        BinaryOp::Eq => ord == Ordering::Equal,
        BinaryOp::Lt => ord == Ordering::Less,
        BinaryOp::Le => ord != Ordering::Greater,
        BinaryOp::Gt => ord == Ordering::Greater,
        BinaryOp::Ge => ord != Ordering::Less,
        _ => return None,
    })
}

/// Groups rows that already came out of the sink a second time.
///
/// There are as many of these as the query grouped into, which is a
/// fraction of the rows that made them, so this sorts and walks the
/// runs rather than building a second hash table. It costs one compare
/// per row per key and hands the answer back in ascending key order,
/// which is the order the sink's own groups arrive in.
fn regroup(
    keys: &[usize],
    aggs: &[PostAgg],
    item_agg: &[bool],
    mut rows: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    // No key at all is one group over everything, and it answers even
    // when nothing reached it: a count over no rows is zero.
    if keys.is_empty() {
        return vec![staged_row(item_agg, &[], &fold(aggs, &rows))];
    }
    let same = |a: &Vec<Value>, b: &Vec<Value>| {
        keys.iter()
            .map(|&c| val_cmp(&a[c], &b[c]))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };
    rows.sort_by(same);
    let mut out = Vec::new();
    let mut at = 0;
    while at < rows.len() {
        let mut end = at + 1;
        while end < rows.len() && same(&rows[end], &rows[at]) == Ordering::Equal {
            end += 1;
        }
        let vals = fold(aggs, &rows[at..end]);
        let keyvals: Vec<Value> = keys.iter().map(|&c| rows[at][c].clone()).collect();
        out.push(staged_row(item_agg, &keyvals, &vals));
        at = end;
    }
    out
}

/// One group's keys and accumulators woven back into the order the
/// clause wrote its items in, the same weave [`finish_agg`] does.
fn staged_row(item_agg: &[bool], keys: &[Value], aggs: &[Value]) -> Vec<Value> {
    let mut kit = keys.iter();
    let mut ait = aggs.iter();
    item_agg
        .iter()
        .map(|&is_agg| {
            if is_agg { ait.next() } else { kit.next() }
                .expect("one value per item")
                .clone()
        })
        .collect()
}

/// The accumulators of one group of already-grouped rows.
fn fold(aggs: &[PostAgg], rows: &[Vec<Value>]) -> Vec<Value> {
    aggs.iter()
        .map(|spec| match spec {
            PostAgg::Count(None) => Value::Int(rows.len() as i64),
            PostAgg::Count(Some(c)) => Value::Int(
                rows.iter()
                    .filter(|row| !matches!(row[*c], Value::Null))
                    .count() as i64,
            ),
            PostAgg::Sum(c) => sum_of(rows.iter().map(|row| &row[*c])),
        })
        .collect()
}

/// A sum over group values, integer while every value is one and real
/// once any of them is. Nulls are skipped and a sum of nothing is
/// zero, which is what the old engine answers.
fn sum_of<'a>(vals: impl Iterator<Item = &'a Value>) -> Value {
    let mut ints: i64 = 0;
    let mut reals = 0f64;
    let mut any_real = false;
    for v in vals {
        match v {
            Value::Int(n) => ints = ints.wrapping_add(*n),
            Value::Float(f) => {
                reals += f;
                any_real = true;
            }
            _ => {}
        }
    }
    if any_real {
        Value::Float(ints as f64 + reals)
    } else {
        Value::Int(ints)
    }
}

/// The OrdValue total order taken by reference. Ordering through
/// OrdValue itself means cloning both values, which on a hundred
/// thousand groups or a wide sort costs more than the compare does, so
/// the types a sink actually produces are matched here and only the
/// rest, lists and paths, pay the clone.
fn val_cmp(x: &Value, y: &Value) -> Ordering {
    match (x, y) {
        (Value::Int(p), Value::Int(q)) => p.cmp(q),
        (Value::Str(p), Value::Str(q)) => p.cmp(q),
        (Value::Float(p), Value::Float(q)) => p.total_cmp(q),
        (Value::Int(p), Value::Float(q)) => (*p as f64).total_cmp(q),
        (Value::Float(p), Value::Int(q)) => p.total_cmp(&(*q as f64)),
        (Value::Bool(p), Value::Bool(q)) => p.cmp(q),
        (Value::Null, Value::Null) => Ordering::Equal,
        (
            Value::Node {
                table: t1,
                offset: o1,
            },
            Value::Node {
                table: t2,
                offset: o2,
            },
        ) => (t1, o1).cmp(&(t2, o2)),
        _ => OrdValue(x.clone()).cmp(&OrdValue(y.clone())),
    }
}

/// Group order: the key columns compared left to right.
fn key_cmp(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = val_cmp(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// How many of the sorted rows the steps above the sort can still use.
/// A SKIP of n under a LIMIT of k needs n + k of them and nothing more,
/// which is what turns an ORDER BY under a LIMIT into a selection.
///
/// A step that can drop a row or make one takes the bound off again,
/// because the rows it hands on are no longer the rows the LIMIT was
/// counting. `Emit` is the one step here that cannot: it rewrites a row
/// into the columns the query asked for and hands on exactly the rows
/// it was given, in the order it was given them. It sits above the sort
/// whenever the ORDER BY names something the RETURN does not, which is
/// what `ORDER BY n.id DESC LIMIT 3` over `RETURN n.id AS id` is, so
/// reading it as a barrier is what kept the commonest bounded shape
/// there is on the materializing path.
fn needed_after(post: &[PostSpec]) -> usize {
    let mut need = usize::MAX;
    for op in post.iter().rev() {
        need = match op {
            PostSpec::Limit(k) => need.min(*k as usize),
            PostSpec::Skip(n) => need.saturating_add(*n as usize),
            PostSpec::Emit(_) => need,
            _ => usize::MAX,
        };
    }
    need
}

/// The widest answer a bounded sink will hold. Past this every worker
/// is carrying more rows than the fan usually has, and a selection
/// over the whole fan costs about what pruning a buffer that wide
/// does, so the plan keeps the materializing path.
const TOPN_MAX: usize = 16384;

/// The ORDER BY under a LIMIT a bounded sink can serve: its keys and
/// the number of ordered rows anything above the sort can still use.
/// The sort has to be the first step above the sink, because a dedup
/// under it changes which rows the limit lands on, and the limit has
/// to be one a worker can hold.
pub(crate) fn topn_of(post: &[PostSpec]) -> Option<(&[SortKey<usize>], usize)> {
    let PostSpec::Sort(keys) = post.first()? else {
        return None;
    };
    let need = needed_after(&post[1..]);
    (need <= TOPN_MAX).then_some((keys.as_slice(), need))
}

/// A row a worker is still holding for the answer, with where it sat
/// in the scan: the morsel it came out of and its position inside it.
/// That pair is the order the stitched result would have had, so it is
/// what breaks ties between equal keys, and the answer reads the same
/// whichever worker happened to claim which morsel.
pub(crate) struct Kept {
    key: Vec<Value>,
    at: (u32, u32),
    row: Vec<Value>,
    /// Where in the chunk in flight the row sits, and whether `row` is
    /// still empty because nobody has built it yet. Both are true only
    /// between the settle that admitted the entry and the hand-back
    /// that fills it.
    pos: u32,
    owing: bool,
}

/// One worker's bounded buffer for ORDER BY under a LIMIT.
///
/// The sink knows k before the first row arrives, so a row that loses
/// to the k rows already held is dead on arrival: the buffer reads its
/// key columns, drops it, and the row itself is never built. On the IS
/// shape that is ten rows materialized out of ten thousand candidates,
/// and the ordered query ends up doing less work than the same query
/// with no ORDER BY on it.
///
/// The buffer holds up to 2k rows and prunes back to k by selection
/// instead of sifting a heap on every winner. Both are linear in the
/// rows and both spend one compare on a loser, which is the compare
/// that matters here; the buffer gets to reuse the comparator the full
/// sort already uses rather than carry a second one.
///
/// A loser costs one compare, and the shape that decides how fast this
/// runs is therefore the one where nothing loses. `ORDER BY id DESC`
/// over a table whose ids climb with the scan beats the worst kept row
/// every single time, and that is not a rare case: it is what a bounded
/// sort over an ordered column looks like. Building a row for each of
/// those winners costs the same hundred thousand row builds the full
/// sort costs, and the LIMIT saves nothing at all.
///
/// So a winner is not built when it wins. It is staged by its key
/// alone, and the chunk it came out of is cut to the k best before any
/// row is built: the caller settles the chunk, is told which positions
/// survived it, and builds those. That is k rows per chunk rather than
/// one per winner, and the ordered-column shape stops being the
/// expensive one.
pub(crate) struct TopN {
    keys: Vec<SortKey<usize>>,
    need: usize,
    kept: Vec<Kept>,
    /// Keys staged out of the chunk in flight, laid end to end, one
    /// run of `keys.len()` values per staged row.
    staged: Vec<Value>,
    /// Where each staged key's row sat, in the same order: the stitch
    /// position that breaks its ties, and the position in the chunk the
    /// caller builds the row from.
    staged_at: Vec<((u32, u32), u32)>,
    /// Scratch for the selection over the staged keys, so the chunk
    /// does not allocate one per settle.
    order: Vec<u32>,
    /// Kept entries whose row is still owed, by their place in `kept`.
    owed: Vec<u32>,
    /// The positions those entries sit at, which is what the caller
    /// reads to build the rows.
    owed_pos: Vec<u32>,
    /// Key and row buffers a prune dropped, emptied of their values but
    /// not of their capacity, waiting to be filled again.
    spare: Vec<Vec<Value>>,
    /// The key of the k-th best row so far, once k rows are in hand. A
    /// row that does not beat it cannot reach the answer: k rows
    /// already order ahead of it and none of them leave.
    worst: Option<Vec<Value>>,
}

impl TopN {
    pub(crate) fn new(keys: &[SortKey<usize>], need: usize) -> TopN {
        TopN {
            keys: keys.to_vec(),
            need,
            kept: Vec::new(),
            staged: Vec::new(),
            staged_at: Vec::new(),
            order: Vec::new(),
            owed: Vec::new(),
            owed_pos: Vec::new(),
            spare: Vec::new(),
            worst: None,
        }
    }

    /// A buffer for a row the caller is about to build, out of the ones
    /// a prune dropped where there is one to hand.
    pub(crate) fn row_buffer(&mut self, width: usize) -> Vec<Value> {
        match self.spare.pop() {
            Some(buf) => buf,
            None => Vec::with_capacity(width),
        }
    }

    /// The columns a row has to produce before the buffer can judge it,
    /// in the order the key is built.
    pub(crate) fn keys(&self) -> &[SortKey<usize>] {
        &self.keys
    }

    /// How many rows the buffer can still use. Nothing a chunk holds
    /// past its own best `need` can reach the answer, which is what
    /// lets a caller cut a chunk down before it stages anything.
    pub(crate) fn need(&self) -> usize {
        self.need
    }

    /// Whether a row with this key, arriving after everything the
    /// buffer holds, can still reach the answer. Equal to the worst
    /// kept key is not good enough: the tie breaks on scan position
    /// and this row sits behind every row already held.
    pub(crate) fn wants(&self, key: &[Value]) -> bool {
        self.need > 0
            && self
                .worst
                .as_ref()
                .is_none_or(|w| key_order(&self.keys, key, w) == Ordering::Less)
    }

    /// Stages a key the buffer wants, with the stitch position that
    /// breaks its ties and the position in the chunk its row would be
    /// built from. The row is not built here and may never be.
    pub(crate) fn stage(&mut self, key: &[Value], at: (u32, u32), pos: u32) {
        self.staged.extend_from_slice(key);
        self.staged_at.push((at, pos));
    }

    /// Ends the chunk: cuts everything staged against everything kept,
    /// and reports the positions whose rows the caller now has to build.
    /// Those rows come back through [`TopN::owe`], in this order.
    pub(crate) fn settle(&mut self) -> &[u32] {
        self.owed.clear();
        self.owed_pos.clear();
        if self.staged_at.is_empty() {
            return &self.owed_pos;
        }
        if self.need == 0 {
            self.staged.clear();
            self.staged_at.clear();
            return &self.owed_pos;
        }
        // The chunk's own best k first, over indices rather than over
        // the keys themselves, so nothing is moved that a row might
        // still be built from.
        let stride = self.keys.len().max(1);
        self.order.clear();
        self.order.extend(0..self.staged_at.len() as u32);
        if self.order.len() > self.need {
            let (keys, staged, staged_at) = (&self.keys, &self.staged, &self.staged_at);
            let run = |i: u32| &staged[i as usize * stride..(i as usize + 1) * stride];
            self.order.select_nth_unstable_by(self.need - 1, |x, y| {
                key_order(keys, run(*x), run(*y))
                    .then(staged_at[*x as usize].0.cmp(&staged_at[*y as usize].0))
            });
            self.order.truncate(self.need);
        }
        for &i in &self.order {
            let (at, pos) = self.staged_at[i as usize];
            let mut buf = match self.spare.pop() {
                Some(buf) => buf,
                None => Vec::with_capacity(stride),
            };
            buf.extend_from_slice(&self.staged[i as usize * stride..(i as usize + 1) * stride]);
            let row = self.spare.pop().unwrap_or_default();
            self.kept.push(Kept {
                key: buf,
                at,
                row,
                pos,
                owing: true,
            });
        }
        self.staged.clear();
        self.staged_at.clear();
        self.prune();
        for (at, k) in self.kept.iter().enumerate() {
            if k.owing {
                self.owed.push(at as u32);
                self.owed_pos.push(k.pos);
            }
        }
        &self.owed_pos
    }

    /// The nth position [`TopN::settle`] asked for.
    pub(crate) fn owed_at(&self, nth: usize) -> usize {
        self.owed_pos[nth] as usize
    }

    /// Hands back the row for the nth position [`TopN::settle`] asked
    /// for.
    pub(crate) fn owe(&mut self, nth: usize, row: Vec<Value>) {
        let at = self.owed[nth] as usize;
        self.kept[at].row = row;
        self.kept[at].owing = false;
    }

    /// Cuts the buffer back to the k best rows and records the new
    /// worst of them, which is the reject test every later row meets.
    fn prune(&mut self) {
        if self.need == 0 {
            self.kept.clear();
            return;
        }
        if self.kept.len() < self.need {
            return;
        }
        let TopN {
            keys,
            need,
            kept,
            spare,
            worst,
            ..
        } = self;
        let by = |x: &Kept, y: &Kept| key_order(keys, &x.key, &y.key).then(x.at.cmp(&y.at));
        kept.select_nth_unstable_by(*need - 1, by);
        for mut dropped in kept.drain(*need..) {
            dropped.key.clear();
            dropped.row.clear();
            spare.push(dropped.key);
            spare.push(dropped.row);
        }
        *worst = Some(kept[*need - 1].key.clone());
    }
}

/// The answer out of the workers' buffers: every row they kept, put in
/// key order and then in scan order, cut to k. The tie break is the
/// order the materializing path would have stitched, so both paths
/// return the same rows the same way round.
pub(crate) fn merge_topn(keys: &[SortKey<usize>], need: usize, tops: Vec<TopN>) -> Vec<Vec<Value>> {
    let mut kept: Vec<Kept> = tops
        .into_iter()
        .flat_map(|mut t| {
            t.prune();
            t.kept
        })
        .collect();
    kept.sort_unstable_by(|x, y| key_order(keys, &x.key, &y.key).then(x.at.cmp(&y.at)));
    kept.truncate(need);
    kept.into_iter().map(|k| k.row).collect()
}

/// Two sort keys compared the way the ORDER BY reads them. The values
/// are already in key order, so the column each one came from is spent
/// and the direction and the null placement are what is left.
fn key_order(keys: &[SortKey<usize>], a: &[Value], b: &[Value]) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let ord = one_key(key, &a[at], &b[at]);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Two values compared the way one ORDER BY key reads them.
///
/// A null sits outside the direction: NULLS FIRST is the head of the
/// result and not the small end of the order, so a descending key with
/// NULLS FIRST still leads with its nulls. The direction covers only
/// what is left, which is every pair of values that are both there.
fn one_key(key: &SortKey<usize>, a: &Value, b: &Value) -> Ordering {
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return if key.nulls_first() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        (false, true) => {
            return if key.nulls_first() {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        (false, false) => {}
    }
    let ord = val_cmp(a, b);
    if key.ascending { ord } else { ord.reverse() }
}

/// The longest string a normalized key encodes. Every row pays the
/// longest string in its column, so one long value in a text column
/// widens the key for the whole set; past this the padding costs more
/// than the compare it saves and the row comparator is the better deal.
const NORM_STR_MAX: usize = 24;

/// The widest key that still fits one machine word pair. At or under
/// this a key sorts as a u128, which moves the key and the row position
/// together in one 16 byte compare; over it the sort compares bytes in
/// place and moves only the position.
const NORM_WORD: usize = 16;

/// The widest key at all. The buffer costs this per row on top of the
/// rows themselves, and an ORDER BY this long is deciding on its first
/// few columns anyway, so past here the row comparator is the honest
/// answer: it reads the columns it needs and stops.
const NORM_MAX: usize = 64;

/// How a sort column turns into bytes that sort the way its values do.
#[derive(Clone, Copy, PartialEq)]
enum NormKind {
    /// An i64 keeps its order as a u64 once the sign bit is flipped.
    Int,
    /// A negative f64 reverses under its own bits, so it inverts whole
    /// and a positive one only flips its sign bit. That is the order
    /// total_cmp gives, NaN ends included.
    Float,
    Bool,
    /// The table and then the offset, which is how two nodes compare.
    Node,
    /// The text padded out to the longest string the column holds, with
    /// the length behind it so a string sorts ahead of the one it is a
    /// prefix of even when the padding is part of that longer string.
    Str,
}

/// Where one sort column sits in the key and what it costs. The kind
/// is spent by the time the layout is built: the encoder reads the
/// value itself, and the layout is what says every row agrees on it.
struct NormField {
    col: usize,
    asc: bool,
    /// One when the column holds a null somewhere, in which case every
    /// row carries a byte in front of its value saying whether the
    /// value is there, and zero when the column holds none.
    pad: usize,
    /// The leading byte a row holding nothing gets. A row holding a
    /// value gets the other one, so the byte orders the two groups the
    /// way the key's null ordering asks and the direction never enters
    /// into it.
    null_byte: u8,
    at: usize,
    width: usize,
}

fn norm_kind(v: &Value) -> Option<NormKind> {
    match v {
        Value::Int(_) => Some(NormKind::Int),
        Value::Float(_) => Some(NormKind::Float),
        Value::Bool(_) => Some(NormKind::Bool),
        Value::Node { .. } => Some(NormKind::Node),
        Value::Str(_) => Some(NormKind::Str),
        _ => None,
    }
}

/// The key layout for these sort columns, or None when the set has no
/// rows, a column holds a type with no byte form, a column changes type
/// from row to row, a text column runs past what padding is worth, or
/// the columns together run past what a key is worth carrying.
/// The type has to hold for every row because a column of integers and
/// floats mixed compares through f64, which no fixed byte form gives.
fn norm_plan(rows: &[Vec<Value>], keys: &[SortKey<usize>]) -> Option<Vec<NormField>> {
    if keys.is_empty() {
        return None;
    }
    if rows.is_empty() {
        return None;
    }
    let mut fields = Vec::with_capacity(keys.len());
    let mut at = 0;
    for key in keys {
        let col = key.expr;
        // One pass over the column settles all three questions: what
        // kind it is, whether it holds a null, and how long its longest
        // string is. The kind comes from the rows holding a value,
        // since a null has no byte form of its own, and a column of
        // nothing but nulls has no layout at all and goes to the
        // comparator.
        let mut kind: Option<NormKind> = None;
        let mut nullable = false;
        let mut longest = 0;
        for row in rows {
            match row.get(col)? {
                Value::Null => nullable = true,
                value => {
                    let k = norm_kind(value)?;
                    if *kind.get_or_insert(k) != k {
                        return None;
                    }
                    if let Value::Str(text) = value {
                        longest = longest.max(text.len());
                    }
                }
            }
        }
        let pad = usize::from(nullable);
        let width = pad
            + match kind? {
                NormKind::Int | NormKind::Float => 8,
                NormKind::Bool => 1,
                NormKind::Node => 12,
                NormKind::Str => {
                    if longest > NORM_STR_MAX {
                        return None;
                    }
                    longest + 1
                }
            };
        fields.push(NormField {
            col,
            asc: key.ascending,
            pad,
            null_byte: u8::from(!key.nulls_first()),
            at,
            width,
        });
        at += width;
        if at > NORM_MAX {
            return None;
        }
    }
    Some(fields)
}

/// One order-preserving byte string per row, laid out back to back at a
/// fixed stride. The bytes compare the way the values compare, so the
/// sort reads one contiguous buffer instead of chasing a Vec per row,
/// and a descending column is its own field inverted. This is the IS
/// and IC shape, ORDER BY a date or a score with an id to break the
/// ties, and it holds up just as well on a name and three columns.
fn norm_keys(rows: &[Vec<Value>], fields: &[NormField], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; rows.len() * width];
    for (row, key) in rows.iter().zip(out.chunks_exact_mut(width)) {
        for f in fields {
            let cell = &mut key[f.at..][..f.width];
            if f.pad == 1 {
                let missing = matches!(row[f.col], Value::Null);
                cell[0] = if missing {
                    f.null_byte
                } else {
                    1 - f.null_byte
                };
                // The rest of the cell stays zero, which is what makes
                // two nulls tie and leaves the position to break them.
                if missing {
                    continue;
                }
            }
            let cell = &mut cell[f.pad..];
            let last = cell.len() - 1;
            match &row[f.col] {
                Value::Int(n) => cell.copy_from_slice(&((*n as u64) ^ (1 << 63)).to_be_bytes()),
                Value::Float(x) => {
                    let bits = x.to_bits();
                    let flip = if bits >> 63 == 1 {
                        !bits
                    } else {
                        bits ^ (1 << 63)
                    };
                    cell.copy_from_slice(&flip.to_be_bytes());
                }
                Value::Bool(b) => cell[0] = u8::from(*b),
                Value::Node { table, offset } => {
                    cell[..4].copy_from_slice(&table.to_be_bytes());
                    cell[4..].copy_from_slice(&offset.to_be_bytes());
                }
                Value::Str(s) => {
                    cell[..s.len()].copy_from_slice(s.as_bytes());
                    cell[last] = s.len() as u8;
                }
                other => unreachable!("the layout matched every row, not {other:?}"),
            }
            if !f.asc {
                for b in cell.iter_mut() {
                    *b = !*b;
                }
            }
        }
    }
    out
}

/// The row order a key that fits one word gives. The key and the row
/// position travel as one pair, so the sort never reads back into the
/// buffer, and the position breaks ties into input order.
fn order_by_word(norm: &[u8], width: usize, need: usize) -> Vec<u32> {
    let word = |key: &[u8]| {
        let mut buf = [0u8; NORM_WORD];
        buf[..key.len()].copy_from_slice(key);
        u128::from_be_bytes(buf)
    };
    let mut order: Vec<(u128, u32)> = norm
        .chunks_exact(width)
        .zip(0u32..)
        .map(|(key, at)| (word(key), at))
        .collect();
    if need < order.len() {
        order.select_nth_unstable(need);
        order.truncate(need);
    }
    order.sort_unstable();
    order.into_iter().map(|(_, at)| at).collect()
}

/// The row order a key too wide for a word gives. The compare is a
/// memcmp of two fixed slices, which still beats walking a row of
/// values, and only the four byte position moves.
fn order_by_bytes(norm: &[u8], width: usize, need: usize) -> Vec<u32> {
    let key = |at: &u32| &norm[*at as usize * width..][..width];
    let by = |x: &u32, y: &u32| key(x).cmp(key(y)).then(x.cmp(y));
    let mut order: Vec<u32> = (0..(norm.len() / width) as u32).collect();
    if need < order.len() {
        order.select_nth_unstable_by(need, by);
        order.truncate(need);
    }
    order.sort_unstable_by(by);
    order
}

/// ORDER BY over materialized rows, ordering the whole set only when
/// something above wants the whole set. Under a LIMIT it partitions
/// once around the k-th row and orders the surviving prefix, which is
/// O(n) plus O(k log k) instead of O(n log n). The comparator breaks
/// ties on the row's position so the answer is the stable sort's, the
/// order the old engine's sort_by produces.
fn sort_rows(rows: &mut Vec<Vec<Value>>, keys: &[SortKey<usize>], need: usize) {
    if let Some(fields) = norm_plan(rows, keys) {
        let width = fields.iter().map(|f| f.width).sum();
        let norm = norm_keys(rows, &fields, width);
        let order = if width <= NORM_WORD {
            order_by_word(&norm, width, need)
        } else {
            order_by_bytes(&norm, width, need)
        };
        let mut out = Vec::with_capacity(order.len());
        for i in order {
            out.push(std::mem::take(&mut rows[i as usize]));
        }
        *rows = out;
        return;
    }
    let by_keys = |a: &Vec<Value>, b: &Vec<Value>| {
        for key in keys {
            let ord = one_key(key, &a[key.expr], &b[key.expr]);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    };
    if need >= rows.len() {
        rows.sort_by(by_keys);
        return;
    }
    let mut order: Vec<u32> = (0..rows.len() as u32).collect();
    {
        let by = |x: &u32, y: &u32| by_keys(&rows[*x as usize], &rows[*y as usize]).then(x.cmp(y));
        order.select_nth_unstable_by(need, by);
        order.truncate(need);
        order.sort_unstable_by(by);
    }
    let mut out = Vec::with_capacity(need);
    for i in order {
        out.push(std::mem::take(&mut rows[i as usize]));
    }
    *rows = out;
}

/// The key layout of a row set, or None when some column holds a type
/// the group table has no part for or the columns are not the same type
/// on every row. Rows out of one sink always are, the check is cheap,
/// and it keeps the hashed path from misreading a key.
fn row_parts(rows: &[Vec<Value>]) -> Option<Vec<PartKind>> {
    let parts: Vec<PartKind> = rows.first()?.iter().map(part_of).collect::<Option<_>>()?;
    let same = rows.iter().all(|row| {
        row.len() == parts.len() && row.iter().zip(&parts).all(|(v, &p)| part_of(v) == Some(p))
    });
    same.then_some(parts)
}

fn part_of(v: &Value) -> Option<PartKind> {
    match v {
        Value::Int(_) => Some(PartKind::Int),
        Value::Str(_) => Some(PartKind::Str),
        Value::Node { .. } => Some(PartKind::Node),
        _ => None,
    }
}

/// DISTINCT over materialized rows, keeping the first occurrence. Rows
/// of integers, strings, and nodes go through the group table a vector
/// at a time, one hash and no allocation each. Anything else, a float or
/// a null or a mixed column, falls back to the ordered set, which clones
/// a row's worth of values per row but handles every type there is.
fn distinct(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let Some(parts) = row_parts(&rows) else {
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert(row.iter().cloned().map(OrdValue).collect::<Vec<_>>()));
        return rows;
    };
    let mut table = GroupTable::new(parts, 0);
    let stride = table.stride();
    let mut batch = KeyBatch::default();
    let mut gids = Vec::new();
    // A row is the first sight of its key exactly when the table hands
    // it the next index it had not handed out yet.
    let mut next = 0;
    let mut keep = Vec::with_capacity(rows.len());
    for block in rows.chunks(VECTOR) {
        batch.reset(stride, block.len());
        for (row, values) in block.iter().enumerate() {
            let mut off = 0;
            for v in values {
                match v {
                    Value::Int(n) => {
                        let (words, stride) = batch.words_mut();
                        words[row * stride + off] = *n as u64;
                    }
                    Value::Node { table, offset } => {
                        let (words, stride) = batch.words_mut();
                        words[row * stride + off] = u64::from(*table);
                        words[row * stride + off + 1] = *offset;
                    }
                    Value::Str(s) => batch.set_str(row, off, s.as_bytes()),
                    _ => unreachable!("row_parts admitted the row"),
                }
                off += part_of(v).expect("row_parts admitted the row").words();
            }
        }
        table.probe(&batch, &[], &mut gids);
        keep.extend(gids.iter().map(|&g| {
            let new = g as usize == next;
            next += usize::from(new);
            new
        }));
    }
    let mut it = keep.into_iter();
    rows.retain(|_| it.next().expect("one flag per row"));
    rows
}

/// Merges keyed aggregation partials into the final result: fold the
/// group tables, produce the empty-input row for a bare aggregate,
/// order groups by key ascending like the old BTreeMap sink, and
/// interleave keys and aggregates back into clause order.
pub(crate) fn finish_agg(
    columns: Vec<String>,
    item_agg: &[bool],
    specs: &[AggSpec],
    post: &[PostSpec],
    partials: Vec<SinkState>,
    keys_empty: bool,
) -> Result<QueryResult> {
    // A bare aggregate has one group whichever way the input went, so
    // it never needs the table: fold the per-worker state vectors and
    // emit the row even when no worker saw a row at all.
    if keys_empty {
        let mut states: Vec<Acc> = specs.iter().map(Acc::new).collect();
        for p in &partials {
            for (a, b) in states.iter_mut().zip(&p.bare) {
                a.merge(b)?;
            }
        }
        let row = states.into_iter().map(Acc::finalize).collect();
        return Ok(QueryResult::new(columns, apply_post(post, vec![row])));
    }
    // Fold every other worker into the first non-empty table rather
    // than into a fresh one, so the biggest partial is usually the one
    // nobody has to rehash.
    let mut merged: Option<GroupTable> = None;
    for p in partials {
        let Some(t) = p.groups else { continue };
        match &mut merged {
            None => merged = Some(t),
            Some(m) => m.merge_from(&t)?,
        }
    }
    let mut groups = merged.map(GroupTable::drain).unwrap_or_default();
    groups.sort_by(|a, b| key_cmp(&a.0, &b.0));
    let mut rows = Vec::with_capacity(groups.len());
    for (keyvals, states) in groups {
        let mut kit = keyvals.into_iter();
        let mut sit = states.into_iter();
        let mut row = Vec::with_capacity(item_agg.len());
        for &is_agg in item_agg {
            row.push(if is_agg {
                sit.next().expect("one state per aggregate item").finalize()
            } else {
                kit.next().expect("one value per key item")
            });
        }
        rows.push(row);
    }
    Ok(QueryResult::new(columns, apply_post(post, rows)))
}

/// Folds the workers' tables and answers with how many groups they
/// hold together, which is the whole of `count(DISTINCT ...)`: the
/// tuple went in as a group key and the group count is the number of
/// tuples that were different. Nothing accumulated per group, so the
/// merge only has to find the keys one table holds that another one
/// does not.
pub(crate) fn finish_distinct(partials: Vec<SinkState>) -> Result<i64> {
    let mut merged: Option<GroupTable> = None;
    for p in partials {
        let Some(t) = p.groups else { continue };
        match &mut merged {
            None => merged = Some(t),
            Some(m) => m.merge_from(&t)?,
        }
    }
    Ok(merged.map_or(0, |m| m.groups() as i64))
}

/// Stitches row batches back into scan order and applies the posts.
pub(crate) fn finish_rows(
    columns: Vec<String>,
    post: &[PostSpec],
    mut partials: Vec<SinkState>,
) -> QueryResult {
    // A plan with nothing above the projection filled columns instead
    // of rows, and those columns are the answer. Same reasoning as the
    // bounded case below: every worker read the same plan, so either
    // all of them kept columns or none did.
    if !partials.is_empty() && partials.iter().all(|p| p.cols.is_some()) {
        let sinks = partials.iter_mut().filter_map(|p| p.cols.take()).collect();
        let held = crate::columns::merge(&columns, sinks);
        return QueryResult::held(columns, held);
    }
    // The workers all read the same post chain, so either every one of
    // them ran bounded or none did, and the sort is already served.
    if let Some((keys, need)) = topn_of(post)
        && partials.iter().all(|p| p.top.is_some())
    {
        let tops = partials.iter_mut().filter_map(|p| p.top.take()).collect();
        return QueryResult::new(
            columns,
            apply_post(&post[1..], merge_topn(keys, need, tops)),
        );
    }
    let mut batches: Vec<(usize, Vec<Vec<Value>>)> =
        partials.into_iter().flat_map(|p| p.batches).collect();
    batches.sort_by_key(|&(ix, _)| ix);
    let mut rows = Vec::new();
    for (_, mut b) in batches {
        rows.append(&mut b);
    }
    QueryResult::new(columns, apply_post(post, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zu_query::ast::NullOrder;

    /// The sort keys a `(column, ascending)` list names, with the null
    /// ordering left at its default, which is what every test here that
    /// does not mention a null wants.
    fn keys(spec: &[(usize, bool)]) -> Vec<SortKey<usize>> {
        spec.iter()
            .map(|&(expr, ascending)| SortKey {
                expr,
                ascending,
                nulls: NullOrder::default(),
            })
            .collect()
    }

    fn rows(vals: &[(i64, &str)]) -> Vec<Vec<Value>> {
        vals.iter()
            .map(|&(n, s)| vec![Value::Int(n), Value::Str(s.into())])
            .collect()
    }

    const DATA: [(i64, &str); 7] = [
        (3, "a"),
        (1, "b"),
        (3, "c"),
        (2, "d"),
        (1, "e"),
        (2, "f"),
        (3, "g"),
    ];

    #[test]
    fn a_limited_sort_returns_the_full_sorts_prefix() {
        let full = apply_post(&[PostSpec::Sort(keys(&[(0, true)]))], rows(&DATA));
        let top = apply_post(
            &[PostSpec::Sort(keys(&[(0, true)])), PostSpec::Limit(3)],
            rows(&DATA),
        );
        assert_eq!(top, full[..3].to_vec(), "ties keep their input order");
    }

    #[test]
    fn skip_widens_what_the_selection_has_to_order() {
        let want = apply_post(
            &[
                PostSpec::Sort(keys(&[(0, false)])),
                PostSpec::Skip(2),
                PostSpec::Limit(3),
            ],
            rows(&DATA),
        );
        let mut full = apply_post(&[PostSpec::Sort(keys(&[(0, false)]))], rows(&DATA));
        full.drain(..2);
        full.truncate(3);
        assert_eq!(want, full);
    }

    #[test]
    fn later_keys_break_the_first_keys_ties() {
        let got = apply_post(
            &[PostSpec::Sort(keys(&[(0, true), (1, false)]))],
            rows(&DATA),
        );
        let names: Vec<&str> = got
            .iter()
            .map(|r| match &r[1] {
                Value::Str(s) => s.as_str(),
                other => panic!("expected a name, got {other:?}"),
            })
            .collect();
        assert_eq!(names, ["e", "b", "f", "d", "g", "c", "a"]);
    }

    /// The order the row comparator gives. Its sort is stable, so ties
    /// keep their input order, which is what the normalized key has to
    /// reproduce whatever shape the key takes.
    fn by_comparator(rows: &[Vec<Value>], keys: &[SortKey<usize>]) -> Vec<Vec<Value>> {
        let mut out = rows.to_vec();
        out.sort_by(|a, b| {
            for key in keys {
                let ord = one_key(key, &a[key.expr], &b[key.expr]);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        out
    }

    /// The bytes the key takes, or None when the set has no normalized
    /// form at all.
    fn key_width(rows: &[Vec<Value>], keys: &[SortKey<usize>]) -> Option<usize> {
        norm_plan(rows, keys).map(|f| f.iter().map(|x| x.width).sum())
    }

    /// Three integer columns with heavy repeats, so every key has to
    /// reach the column behind it and the tie break past that.
    fn wide_rows() -> Vec<Vec<Value>> {
        (0..500i64)
            .map(|i| {
                vec![
                    Value::Int(i % 5),
                    Value::Int(-(i % 7)),
                    Value::Int((i * 31) % 11),
                ]
            })
            .collect()
    }

    #[test]
    fn three_integer_columns_sort_the_way_the_comparator_does() {
        let rows = wide_rows();
        let keys = keys(&[(0usize, true), (1usize, false), (2usize, true)]);
        assert_eq!(key_width(&rows, &keys), Some(24), "past one word");
        assert_eq!(
            apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
            by_comparator(&rows, &keys)
        );
    }

    #[test]
    fn a_limited_wide_sort_returns_the_full_sorts_prefix() {
        let rows = wide_rows();
        let keys = keys(&[(2usize, false), (0usize, true), (1usize, true)]);
        let full = by_comparator(&rows, &keys);
        for need in [1, 17, 499, 500, 501] {
            let got = apply_post(
                &[PostSpec::Sort(keys.clone()), PostSpec::Limit(need as u64)],
                rows.clone(),
            );
            assert_eq!(got, full[..need.min(full.len())].to_vec(), "limit {need}");
        }
    }

    #[test]
    fn a_text_key_sorts_ahead_of_the_string_it_prefixes() {
        let names = ["ab", "", "a", "abc", "b", "a", "ab"];
        let rows: Vec<Vec<Value>> = names
            .iter()
            .zip(0i64..)
            .map(|(s, i)| vec![Value::Str((*s).into()), Value::Int(i)])
            .collect();
        for keys in [
            keys(&[(0usize, true), (1usize, true)]),
            keys(&[(0usize, false), (1usize, true)]),
        ] {
            assert_eq!(
                key_width(&rows, &keys),
                Some(12),
                "three plus one and an id"
            );
            assert_eq!(
                apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
                by_comparator(&rows, &keys)
            );
        }
    }

    #[test]
    fn floats_bools_and_nodes_all_have_a_byte_form() {
        let floats = [f64::NEG_INFINITY, -1.5, -0.0, 0.0, 1.5, f64::INFINITY];
        let rows: Vec<Vec<Value>> = (0..24i64)
            .map(|i| {
                vec![
                    Value::Float(floats[i as usize % floats.len()]),
                    Value::Bool(i % 2 == 0),
                    Value::Node {
                        table: (i % 3) as u32,
                        offset: (i % 4) as u64,
                    },
                ]
            })
            .collect();
        for keys in [
            keys(&[(0usize, true), (2usize, false)]),
            keys(&[(1usize, false), (0usize, false)]),
            keys(&[(2usize, true), (1usize, true), (0usize, true)]),
        ] {
            assert!(key_width(&rows, &keys).is_some(), "keys {keys:?}");
            assert_eq!(
                apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
                by_comparator(&rows, &keys),
                "keys {keys:?}"
            );
        }
    }

    #[test]
    fn a_shape_with_no_byte_form_falls_back_and_answers_the_same() {
        let long = "l".repeat(NORM_STR_MAX + 1);
        let sets = [
            // A column of integers and floats mixed, which compares
            // through f64 and has no fixed byte form.
            vec![
                vec![Value::Int(2), Value::Int(0)],
                vec![Value::Float(1.5), Value::Int(1)],
                vec![Value::Int(1), Value::Int(2)],
            ],
            // Text past what padding is worth.
            vec![
                vec![Value::Str(long.clone()), Value::Int(0)],
                vec![Value::Str("a".into()), Value::Int(1)],
            ],
            // A type the key has no encoding for at all.
            vec![
                vec![Value::Null, Value::Int(0)],
                vec![Value::Null, Value::Int(1)],
            ],
        ];
        let keys = keys(&[(0usize, true), (1usize, true)]);
        for rows in sets {
            assert_eq!(key_width(&rows, &keys), None, "{rows:?}");
            assert_eq!(
                apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
                by_comparator(&rows, &keys)
            );
        }
    }

    #[test]
    fn an_order_by_wider_than_the_key_falls_back_too() {
        let cols = NORM_MAX / 8 + 1;
        let rows: Vec<Vec<Value>> = (0..40i64)
            .map(|i| (0..cols as i64).map(|c| Value::Int((i * c) % 6)).collect())
            .collect();
        let spec: Vec<(usize, bool)> = (0..cols).map(|c| (c, c % 2 == 0)).collect();
        let fits = keys(&spec[..cols - 1]);
        let over = keys(&spec);
        assert_eq!(
            key_width(&rows, &fits),
            Some(NORM_MAX),
            "the last key that fits"
        );
        assert_eq!(key_width(&rows, &over), None, "one column too many");
        assert_eq!(
            apply_post(&[PostSpec::Sort(over.clone())], rows.clone()),
            by_comparator(&rows, &over)
        );
    }

    #[test]
    fn an_empty_set_has_no_key_and_sorts_to_nothing() {
        let rows: Vec<Vec<Value>> = Vec::new();
        let keys = keys(&[(0usize, true)]);
        assert_eq!(key_width(&rows, &keys), None);
        assert!(apply_post(&[PostSpec::Sort(keys)], rows).is_empty());
    }

    /// The sort keys a `(column, ascending, nulls first)` list names,
    /// for the tests that do care where the null goes.
    fn null_keys(spec: &[(usize, bool, bool)]) -> Vec<SortKey<usize>> {
        spec.iter()
            .map(|&(expr, ascending, first)| SortKey {
                expr,
                ascending,
                nulls: if first {
                    NullOrder::First
                } else {
                    NullOrder::Last
                },
            })
            .collect()
    }

    /// Nine rows whose first column holds nothing on three of them, and
    /// whose second column is the row's own position so a tie is always
    /// broken by something the comparator and the key both read.
    fn null_rows() -> Vec<Vec<Value>> {
        [
            None,
            Some(3),
            Some(1),
            None,
            Some(2),
            Some(3),
            None,
            Some(1),
            Some(2),
        ]
        .into_iter()
        .zip(0i64..)
        .map(|(v, at)| {
            let first = v.map_or(Value::Null, Value::Int);
            vec![first, Value::Int(at)]
        })
        .collect()
    }

    #[test]
    fn a_null_costs_one_byte_in_front_of_the_value() {
        let rows = null_rows();
        for first in [false, true] {
            for asc in [false, true] {
                let one = null_keys(&[(0, asc, first)]);
                assert_eq!(key_width(&rows, &one), Some(9), "the byte and the value");
                let two = null_keys(&[(0, asc, first), (1, true, false)]);
                assert_eq!(key_width(&rows, &two), Some(17), "past one word");
                // Nine bytes sort as a word and seventeen sort as bytes,
                // so this asks both encoders the same question.
                for keys in [one, two] {
                    assert_eq!(
                        apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
                        by_comparator(&rows, &keys),
                        "keys {keys:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_null_ordering_is_not_the_directions_to_reverse() {
        let rows = null_rows();
        let at = |keys: &[SortKey<usize>]| -> Vec<i64> {
            apply_post(&[PostSpec::Sort(keys.to_vec())], rows.clone())
                .iter()
                .map(|r| match r[1] {
                    Value::Int(n) => n,
                    ref other => panic!("expected a position, got {other:?}"),
                })
                .collect()
        };
        // Rows 0, 3 and 6 hold nothing. They lead a NULLS FIRST key and
        // trail a NULLS LAST one whichever way the values run, and the
        // three of them keep their input order in every case.
        assert_eq!(at(&null_keys(&[(0, true, true)]))[..3], [0, 3, 6]);
        assert_eq!(at(&null_keys(&[(0, false, true)]))[..3], [0, 3, 6]);
        assert_eq!(at(&null_keys(&[(0, true, false)]))[6..], [0, 3, 6]);
        assert_eq!(at(&null_keys(&[(0, false, false)]))[6..], [0, 3, 6]);
        // What is left is the values, upwards or downwards.
        assert_eq!(at(&null_keys(&[(0, true, true)]))[3..], [2, 7, 4, 8, 1, 5]);
        assert_eq!(
            at(&null_keys(&[(0, false, false)]))[..6],
            [1, 5, 4, 8, 2, 7]
        );
    }

    #[test]
    fn a_bounded_sort_over_a_column_with_nulls_agrees_with_the_full_one() {
        let rows = null_rows();
        for keys in [
            null_keys(&[(0, true, true)]),
            null_keys(&[(0, false, true)]),
            null_keys(&[(0, true, false), (1, false, false)]),
        ] {
            for need in 1..=rows.len() + 1 {
                let want = apply_post(
                    &[PostSpec::Sort(keys.clone()), PostSpec::Limit(need as u64)],
                    rows.clone(),
                );
                for workers in 1..=3 {
                    for chunk in [1, 2, rows.len()] {
                        assert_eq!(
                            deal(&rows, &keys, need, workers, chunk),
                            want,
                            "keys {keys:?}, need {need}, {workers} workers, chunks of {chunk}"
                        );
                    }
                }
            }
        }
    }

    /// One chunk through one buffer, the way the driver runs it: every
    /// winner staged by its key alone, the chunk settled, and only the
    /// rows the settle asked for built.
    fn chunk_through(top: &mut TopN, keys: &[SortKey<usize>], part: &[(u32, Vec<Value>)]) {
        for (pos, (at, row)) in part.iter().enumerate() {
            let key: Vec<Value> = keys.iter().map(|k| row[k.expr].clone()).collect();
            if top.wants(&key) {
                top.stage(&key, (*at, 0), pos as u32);
            }
        }
        for nth in 0..top.settle().len() {
            let row = part[top.owed_at(nth)].1.clone();
            top.owe(nth, row);
        }
    }

    /// The workers' side of a bounded run: rows are dealt to `workers`
    /// buffers one morsel each, in scan order, exactly as a worker
    /// claiming morsels sees them, and each buffer sees its share in
    /// chunks of `chunk`.
    fn deal(
        rows: &[Vec<Value>],
        keys: &[SortKey<usize>],
        need: usize,
        workers: usize,
        chunk: usize,
    ) -> Vec<Vec<Value>> {
        let mut tops: Vec<TopN> = (0..workers).map(|_| TopN::new(keys, need)).collect();
        let mut shares: Vec<Vec<(u32, Vec<Value>)>> = vec![Vec::new(); workers];
        for (at, row) in rows.iter().enumerate() {
            shares[at % workers].push((at as u32, row.clone()));
        }
        for (w, share) in shares.iter().enumerate() {
            for part in share.chunks(chunk.max(1)) {
                chunk_through(&mut tops[w], keys, part);
            }
        }
        merge_topn(keys, need, tops)
    }

    fn bounded(
        data: &[(i64, &str)],
        keys: &[SortKey<usize>],
        need: usize,
        workers: usize,
    ) -> Vec<Vec<Value>> {
        deal(&rows(data), keys, need, workers, usize::MAX)
    }

    #[test]
    fn the_bounded_buffer_answers_what_the_full_sort_answers() {
        for keys in [
            keys(&[(0usize, true)]),
            keys(&[(0usize, false)]),
            keys(&[(0usize, true), (1usize, false)]),
            keys(&[(1usize, true), (0usize, false)]),
        ] {
            for need in 1..=DATA.len() + 2 {
                let want = apply_post(
                    &[PostSpec::Sort(keys.clone()), PostSpec::Limit(need as u64)],
                    rows(&DATA),
                );
                for workers in 1..=4 {
                    assert_eq!(
                        bounded(&DATA, &keys, need, workers),
                        want,
                        "keys {keys:?}, need {need}, {workers} workers"
                    );
                }
            }
        }
    }

    #[test]
    fn a_row_that_ties_the_worst_kept_key_loses() {
        let keys = keys(&[(0, true)]);
        let mut top = TopN::new(&keys, 2);
        let part: Vec<(u32, Vec<Value>)> = rows(&[(1, "a"), (2, "b")])
            .into_iter()
            .enumerate()
            .map(|(at, row)| (at as u32, row))
            .collect();
        chunk_through(&mut top, &keys, &part);
        // The reject test is the k-th best key, which a prune records:
        // in a run that is the buffer filling, here it is by hand.
        top.prune();
        assert!(!top.wants(&[Value::Int(2)]), "later row, same key, loses");
        assert!(top.wants(&[Value::Int(1)]), "a better key still wins");
    }

    #[test]
    fn only_a_sort_a_worker_can_bound_takes_the_buffer() {
        let keys = keys(&[(0usize, true)]);
        let sort = || PostSpec::Sort(keys.clone());
        assert!(topn_of(&[]).is_none(), "no sort, nothing to bound");
        assert!(topn_of(&[sort()]).is_none(), "no limit, nothing to bound");
        assert!(
            topn_of(&[PostSpec::Distinct, sort(), PostSpec::Limit(3)]).is_none(),
            "a dedup under the sort still materializes"
        );
        assert_eq!(
            topn_of(&[sort(), PostSpec::Skip(2), PostSpec::Limit(3)]),
            Some((&keys[..], 5)),
            "a skip widens what the buffer has to hold"
        );
        assert!(
            topn_of(&[sort(), PostSpec::Limit(TOPN_MAX as u64 + 1)]).is_none(),
            "a limit past the ceiling is no cheaper bounded"
        );
    }

    #[test]
    fn nothing_above_the_sort_means_the_whole_set_is_ordered() {
        assert_eq!(needed_after(&[]), usize::MAX);
        assert_eq!(needed_after(&[PostSpec::Limit(10)]), 10);
        assert_eq!(needed_after(&[PostSpec::Skip(5), PostSpec::Limit(10)]), 15);
        assert_eq!(needed_after(&[PostSpec::Skip(5)]), usize::MAX);
        // A limit under a dedup still bounds the sort, a dedup under a
        // limit does not: it can drop rows the limit then asks for.
        assert_eq!(needed_after(&[PostSpec::Limit(10), PostSpec::Distinct]), 10);
        assert_eq!(
            needed_after(&[PostSpec::Distinct, PostSpec::Limit(10)]),
            usize::MAX
        );
    }
}
