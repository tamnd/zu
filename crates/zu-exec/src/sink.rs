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

/// One order-preserving 128 bit key per row, when every sort column is
/// an integer and there are no more than two of them. An i64 keeps its
/// order as a u64 once the sign bit is flipped, and a descending column
/// is that key inverted, so the tuple of them sorts as plain numbers.
/// Sorting those beats sorting through the rows: the compare reads one
/// contiguous array instead of chasing a Vec per row, and the pair
/// carries the row position so equal keys keep their input order. This
/// is the IS and IC shape, ORDER BY a date or a score with an id to
/// break the ties.
fn norm_keys(rows: &[Vec<Value>], keys: &[(usize, bool)]) -> Option<Vec<u128>> {
    if keys.is_empty() || keys.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut key = 0u128;
        for (at, &(col, asc)) in keys.iter().enumerate() {
            let Some(Value::Int(n)) = row.get(col) else {
                return None;
            };
            let part = (*n as u64) ^ (1 << 63);
            key |= u128::from(if asc { part } else { !part }) << (64 * (1 - at));
        }
        out.push(key);
    }
    Some(out)
}

/// ORDER BY over materialized rows, ordering the whole set only when
/// something above wants the whole set. Under a LIMIT it partitions
/// once around the k-th row and orders the surviving prefix, which is
/// O(n) plus O(k log k) instead of O(n log n). The comparator breaks
/// ties on the row's position so the answer is the stable sort's, the
/// order the old engine's sort_by produces.
fn sort_rows(rows: &mut Vec<Vec<Value>>, keys: &[(usize, bool)], need: usize) {
    if let Some(norm) = norm_keys(rows, keys) {
        let mut order: Vec<(u128, u32)> = norm.into_iter().zip(0u32..).collect();
        if need < order.len() {
            order.select_nth_unstable(need);
            order.truncate(need);
        }
        order.sort_unstable();
        let mut out = Vec::with_capacity(order.len());
        for (_, i) in order {
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
        return Ok(QueryResult {
            columns,
            rows: apply_post(post, vec![row]),
        });
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
    Ok(QueryResult {
        columns,
        rows: apply_post(post, rows),
    })
}

/// Stitches row batches back into scan order and applies the posts.
pub(crate) fn finish_rows(
    columns: Vec<String>,
    post: &[PostSpec],
    partials: Vec<SinkState>,
) -> QueryResult {
    let mut batches: Vec<(usize, Vec<Vec<Value>>)> =
        partials.into_iter().flat_map(|p| p.batches).collect();
    batches.sort_by_key(|&(ix, _)| ix);
    let mut rows = Vec::new();
    for (_, mut b) in batches {
        rows.append(&mut b);
    }
    QueryResult {
        columns,
        rows: apply_post(post, rows),
    }
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
