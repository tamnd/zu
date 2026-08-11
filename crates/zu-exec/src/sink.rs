//! Sink partials and the final merge.
//!
//! Every worker owns one [`SinkState`] and pushes into it lock-free;
//! the merge at the end of the run folds the partials and reproduces
//! the old executor's output exactly: group rows in ascending key
//! order, distinct keeping the first occurrence, skip and limit in
//! plan order. Aggregate accumulators mirror the old `Acc` semantics
//! including the empty-input rows and the sum overflow error.

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
    for op in post {
        match op {
            PostSpec::Distinct => rows = distinct(rows),
            PostSpec::Skip(n) => {
                let n = (*n as usize).min(rows.len());
                rows.drain(..n);
            }
            PostSpec::Limit(n) => rows.truncate(*n as usize),
        }
    }
    rows
}

/// Group order: the OrdValue total order over the three types a
/// grouping key can hold here, taken by reference. Sorting through
/// OrdValue itself would mean cloning every key of every group, which
/// on a hundred thousand groups costs more than the sort does.
fn key_cmp(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = match (x, y) {
            (Value::Int(p), Value::Int(q)) => p.cmp(q),
            (Value::Str(p), Value::Str(q)) => p.cmp(q),
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
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// The key layout of a row set, or None when some column holds a type
/// the group table has no part for or the columns are not the same type
/// on every row. Rows out of one sink always are, the check is cheap,
/// and it keeps the hashed path from misreading a key.
fn row_parts(rows: &[Vec<Value>]) -> Option<Vec<PartKind>> {
    let parts: Vec<PartKind> = rows.first()?.iter().map(part_of).collect::<Option<_>>()?;
    let same = rows.iter().all(|row| {
        row.len() == parts.len()
            && row
                .iter()
                .zip(&parts)
                .all(|(v, &p)| part_of(v) == Some(p))
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
