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

use zu_common::{Result, ZuError};
use zu_query::exec::{OrdValue, QueryResult, Value};

use crate::compile::{AggSpec, PostSpec};
use crate::group::{GroupTable, KeyBatch, PartKind};

/// Rows per DISTINCT probe vector, the pipeline's vector width.
const VECTOR: usize = 2048;

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

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
                    .ok_or_else(|| invalid("integer overflow in sum()".into()))?;
                *acc = Some(match *acc {
                    None => scaled,
                    Some(prev) => prev
                        .checked_add(scaled)
                        .ok_or_else(|| invalid("integer overflow in sum()".into()))?,
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
                *a = match (*a, b) {
                    (Some(x), Some(y)) => Some(
                        x.checked_add(y)
                            .ok_or_else(|| invalid("integer overflow in sum()".into()))?,
                    ),
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
        }
    }
    rows
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
fn needed_after(post: &[PostSpec]) -> usize {
    let mut need = usize::MAX;
    for op in post.iter().rev() {
        need = match op {
            PostSpec::Limit(k) => need.min(*k as usize),
            PostSpec::Skip(n) => need.saturating_add(*n as usize),
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

/// The smallest buffer a bounded sink prunes, so a LIMIT of one does
/// not run a selection every time a row wins.
const TOPN_FLOOR: usize = 64;

/// The ORDER BY under a LIMIT a bounded sink can serve: its keys and
/// the number of ordered rows anything above the sort can still use.
/// The sort has to be the first step above the sink, because a dedup
/// under it changes which rows the limit lands on, and the limit has
/// to be one a worker can hold.
pub(crate) fn topn_of(post: &[PostSpec]) -> Option<(&[(usize, bool)], usize)> {
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
pub(crate) struct TopN {
    keys: Vec<(usize, bool)>,
    need: usize,
    kept: Vec<Kept>,
    /// The key of the k-th best row so far, once k rows are in hand. A
    /// row that does not beat it cannot reach the answer: k rows
    /// already order ahead of it and none of them leave.
    worst: Option<Vec<Value>>,
}

impl TopN {
    pub(crate) fn new(keys: &[(usize, bool)], need: usize) -> TopN {
        TopN {
            keys: keys.to_vec(),
            need,
            kept: Vec::new(),
            worst: None,
        }
    }

    /// The columns a row has to produce before the buffer can judge it,
    /// in the order the key is built.
    pub(crate) fn keys(&self) -> &[(usize, bool)] {
        &self.keys
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

    /// Takes a row the buffer wants, with the morsel and the position
    /// inside it the row was emitted at.
    pub(crate) fn keep(&mut self, key: &[Value], at: (u32, u32), row: Vec<Value>) {
        self.kept.push(Kept {
            key: key.to_vec(),
            at,
            row,
        });
        if self.kept.len() >= self.need.saturating_mul(2).max(TOPN_FLOOR) {
            self.prune();
        }
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
            worst,
        } = self;
        let by = |x: &Kept, y: &Kept| key_order(keys, &x.key, &y.key).then(x.at.cmp(&y.at));
        kept.select_nth_unstable_by(*need - 1, by);
        kept.truncate(*need);
        *worst = Some(kept[*need - 1].key.clone());
    }
}

/// The answer out of the workers' buffers: every row they kept, put in
/// key order and then in scan order, cut to k. The tie break is the
/// order the materializing path would have stitched, so both paths
/// return the same rows the same way round.
pub(crate) fn merge_topn(keys: &[(usize, bool)], need: usize, tops: Vec<TopN>) -> Vec<Vec<Value>> {
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
/// and only the direction is left.
fn key_order(keys: &[(usize, bool)], a: &[Value], b: &[Value]) -> Ordering {
    for (at, &(_, asc)) in keys.iter().enumerate() {
        let ord = val_cmp(&a[at], &b[at]);
        let ord = if asc { ord } else { ord.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
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
fn norm_plan(rows: &[Vec<Value>], keys: &[(usize, bool)]) -> Option<Vec<NormField>> {
    if keys.is_empty() {
        return None;
    }
    let first = rows.first()?;
    let mut fields = Vec::with_capacity(keys.len());
    let mut at = 0;
    for &(col, asc) in keys {
        let kind = norm_kind(first.get(col)?)?;
        let width = match kind {
            NormKind::Int | NormKind::Float => 8,
            NormKind::Bool => 1,
            NormKind::Node => 12,
            NormKind::Str => {
                let mut longest = 0;
                for row in rows {
                    let Some(Value::Str(s)) = row.get(col) else {
                        return None;
                    };
                    longest = longest.max(s.len());
                }
                if longest > NORM_STR_MAX {
                    return None;
                }
                longest + 1
            }
        };
        if kind != NormKind::Str
            && rows
                .iter()
                .any(|r| r.get(col).and_then(norm_kind) != Some(kind))
        {
            return None;
        }
        fields.push(NormField {
            col,
            asc,
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
                    cell[f.width - 1] = s.len() as u8;
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
fn sort_rows(rows: &mut Vec<Vec<Value>>, keys: &[(usize, bool)], need: usize) {
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
        for &(col, asc) in keys {
            let ord = val_cmp(&a[col], &b[col]);
            let ord = if asc { ord } else { ord.reverse() };
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
        let full = apply_post(&[PostSpec::Sort(vec![(0, true)])], rows(&DATA));
        let top = apply_post(
            &[PostSpec::Sort(vec![(0, true)]), PostSpec::Limit(3)],
            rows(&DATA),
        );
        assert_eq!(top, full[..3].to_vec(), "ties keep their input order");
    }

    #[test]
    fn skip_widens_what_the_selection_has_to_order() {
        let want = apply_post(
            &[
                PostSpec::Sort(vec![(0, false)]),
                PostSpec::Skip(2),
                PostSpec::Limit(3),
            ],
            rows(&DATA),
        );
        let mut full = apply_post(&[PostSpec::Sort(vec![(0, false)])], rows(&DATA));
        full.drain(..2);
        full.truncate(3);
        assert_eq!(want, full);
    }

    #[test]
    fn later_keys_break_the_first_keys_ties() {
        let got = apply_post(&[PostSpec::Sort(vec![(0, true), (1, false)])], rows(&DATA));
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
    fn by_comparator(rows: &[Vec<Value>], keys: &[(usize, bool)]) -> Vec<Vec<Value>> {
        let mut out = rows.to_vec();
        out.sort_by(|a, b| {
            for &(col, asc) in keys {
                let ord = val_cmp(&a[col], &b[col]);
                let ord = if asc { ord } else { ord.reverse() };
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
    fn key_width(rows: &[Vec<Value>], keys: &[(usize, bool)]) -> Option<usize> {
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
        let keys = vec![(0usize, true), (1usize, false), (2usize, true)];
        assert_eq!(key_width(&rows, &keys), Some(24), "past one word");
        assert_eq!(
            apply_post(&[PostSpec::Sort(keys.clone())], rows.clone()),
            by_comparator(&rows, &keys)
        );
    }

    #[test]
    fn a_limited_wide_sort_returns_the_full_sorts_prefix() {
        let rows = wide_rows();
        let keys = vec![(2usize, false), (0usize, true), (1usize, true)];
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
            vec![(0usize, true), (1usize, true)],
            vec![(0usize, false), (1usize, true)],
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
            vec![(0usize, true), (2usize, false)],
            vec![(1usize, false), (0usize, false)],
            vec![(2usize, true), (1usize, true), (0usize, true)],
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
        let keys = vec![(0usize, true), (1usize, true)];
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
        let fits: Vec<(usize, bool)> = (0..cols - 1).map(|c| (c, c % 2 == 0)).collect();
        let over: Vec<(usize, bool)> = (0..cols).map(|c| (c, c % 2 == 0)).collect();
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
        let keys = vec![(0usize, true)];
        assert_eq!(key_width(&rows, &keys), None);
        assert!(apply_post(&[PostSpec::Sort(keys)], rows).is_empty());
    }

    /// The workers' side of a bounded run: rows are dealt to `workers`
    /// buffers one morsel each, in scan order, exactly as a worker
    /// claiming morsels sees them.
    fn bounded(
        data: &[(i64, &str)],
        keys: &[(usize, bool)],
        need: usize,
        workers: usize,
    ) -> Vec<Vec<Value>> {
        let mut tops: Vec<TopN> = (0..workers).map(|_| TopN::new(keys, need)).collect();
        for (at, row) in rows(data).into_iter().enumerate() {
            let top = &mut tops[at % workers];
            let key: Vec<Value> = keys.iter().map(|&(c, _)| row[c].clone()).collect();
            if top.wants(&key) {
                top.keep(&key, (at as u32, 0), row);
            }
        }
        merge_topn(keys, need, tops)
    }

    #[test]
    fn the_bounded_buffer_answers_what_the_full_sort_answers() {
        for keys in [
            vec![(0usize, true)],
            vec![(0usize, false)],
            vec![(0usize, true), (1usize, false)],
            vec![(1usize, true), (0usize, false)],
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
        let mut top = TopN::new(&[(0, true)], 2);
        for (at, row) in rows(&[(1, "a"), (2, "b")]).into_iter().enumerate() {
            let key = vec![row[0].clone()];
            assert!(top.wants(&key));
            top.keep(&key, (at as u32, 0), row);
        }
        // The reject test is the k-th best key, which a prune records:
        // in a run that is the buffer filling, here it is by hand.
        top.prune();
        assert!(!top.wants(&[Value::Int(2)]), "later row, same key, loses");
        assert!(top.wants(&[Value::Int(1)]), "a better key still wins");
    }

    #[test]
    fn only_a_sort_a_worker_can_bound_takes_the_buffer() {
        let keys = vec![(0usize, true)];
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
