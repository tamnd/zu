//! Sink partials and the final merge.
//!
//! Every worker owns one [`SinkState`] and pushes into it lock-free;
//! the merge at the end of the run folds the partials and reproduces
//! the old executor's output exactly: group rows in ascending key
//! order, distinct keeping the first occurrence, skip and limit in
//! plan order. Aggregate accumulators mirror the old `Acc` semantics
//! including the empty-input rows and the sum overflow error.

use std::collections::{BTreeSet, HashMap};

use zu_common::{Result, ZuError};
use zu_query::exec::{OrdValue, QueryResult, Value};

use crate::compile::{AggSpec, PostSpec};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// One grouping key part. Hashable, unlike Value, and cheap to order
/// through OrdValue at merge time.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum KeyVal {
    Int(i64),
    Str(String),
    Node(u32, u64),
}

impl KeyVal {
    fn value(self) -> Value {
        match self {
            KeyVal::Int(n) => Value::Int(n),
            KeyVal::Str(s) => Value::Str(s),
            KeyVal::Node(table, offset) => Value::Node { table, offset },
        }
    }
}

/// One aggregate accumulator, the integer subset of the old engine's
/// Acc with identical finalize semantics: sum of nothing is 0, avg of
/// nothing is null, min and max of nothing are null.
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

    fn merge(&mut self, other: Acc) -> Result<()> {
        match (self, other) {
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
    /// Keyed aggregation partials.
    pub groups: HashMap<Vec<KeyVal>, Vec<Acc>>,
    /// Rows of the morsel in flight.
    pub rows: Vec<Vec<Value>>,
    /// Finished morsels: (morsel index, its rows).
    pub batches: Vec<(usize, Vec<Vec<Value>>)>,
}

/// Post steps over materialized rows, exactly the old apply_post.
pub(crate) fn apply_post(post: &[PostSpec], mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    for op in post {
        match op {
            PostSpec::Distinct => {
                let mut seen = BTreeSet::new();
                rows.retain(|row| {
                    seen.insert(row.iter().cloned().map(OrdValue).collect::<Vec<_>>())
                });
            }
            PostSpec::Skip(n) => {
                let n = (*n as usize).min(rows.len());
                rows.drain(..n);
            }
            PostSpec::Limit(n) => rows.truncate(*n as usize),
        }
    }
    rows
}

/// Merges keyed aggregation partials into the final result: fold the
/// maps, add the empty-input group for a bare aggregate, order groups
/// by key ascending like the old BTreeMap sink, and interleave keys
/// and aggregates back into clause order.
pub(crate) fn finish_agg(
    columns: Vec<String>,
    item_agg: &[bool],
    specs: &[AggSpec],
    post: &[PostSpec],
    partials: Vec<SinkState>,
    keys_empty: bool,
) -> Result<QueryResult> {
    let mut merged: HashMap<Vec<KeyVal>, Vec<Acc>> = HashMap::new();
    for p in partials {
        for (k, states) in p.groups {
            match merged.entry(k) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(states);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    for (a, b) in e.get_mut().iter_mut().zip(states) {
                        a.merge(b)?;
                    }
                }
            }
        }
    }
    if merged.is_empty() && keys_empty {
        merged.insert(Vec::new(), specs.iter().map(Acc::new).collect());
    }
    let mut groups: Vec<(Vec<OrdValue>, Vec<Value>, Vec<Acc>)> = merged
        .into_iter()
        .map(|(k, states)| {
            let vals: Vec<Value> = k.into_iter().map(KeyVal::value).collect();
            let ord: Vec<OrdValue> = vals.iter().cloned().map(OrdValue).collect();
            (ord, vals, states)
        })
        .collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rows = Vec::with_capacity(groups.len());
    for (_, keyvals, states) in groups {
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
