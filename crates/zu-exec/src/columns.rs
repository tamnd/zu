//! The sink that keeps the vectors.
//!
//! Every other sink in this executor answers with rows, because a sort
//! or a dedup or a group is a step over rows and the row is what it
//! steps over. A plain projection has no step above it at all, and for
//! that one the row is a shape the answer is put into and taken back out
//! of: the executor computes in vectors, the sink used to spend an
//! allocation a row flattening them, and every columnar client then
//! walked the flattened thing twice to put it back the way it started.
//!
//! So this sink does not flatten. It appends each projected item into a
//! buffer of its own, in the layout `zu_query::column` describes and
//! Arrow and numpy and the C ABI all read, and the answer comes out as
//! [`Held`]. The rows are built only if somebody asks for them.
//!
//! Two things make it simpler than the walk it replaces. The type of
//! each column is known before the first row, because a projected item
//! is a stored column with a declared type, a node, a row id or a
//! constant, so there is no inference pass and no column that changes
//! its mind halfway. And the string bytes are copied out of the vector
//! they were decoded into straight into the column's buffer, so a
//! million string cells cost one growing buffer rather than a million
//! `String`s.

use zu_query::column::{ColumnType, Held, HeldColumn, HeldData, Offsets, StrColumn, Validity};
use zu_query::exec::Value;

/// One column of the answer, filled as the rows are found.
pub(crate) struct Fill {
    ty: ColumnType,
    buf: Buf,
    /// One byte a row saying whether the row has a value, made the
    /// first time a row has none and absent on the columns where none
    /// ever does. Bytes rather than bits while the buffer is still
    /// growing, because the pieces are concatenated in scan order at
    /// the end and bits that have to be shifted into place cost more
    /// than the one pass that packs them.
    valid: Option<Vec<u8>>,
    len: usize,
}

/// The buffer one column's values go into.
enum Buf {
    /// A column of nothing but nulls, which is what a null constant
    /// projects to. There is nothing to put in a buffer.
    Null,
    /// One byte a row, packed into bits at the end for the same reason
    /// the validity is.
    Bool(Vec<u8>),
    Int(Vec<i64>),
    Float(Vec<f64>),
    /// The bytes end to end and one offset more than there are rows.
    Str {
        bytes: Vec<u8>,
        offsets: Vec<i64>,
    },
    /// The values themselves, for a node and for anything else no
    /// buffer covers.
    Complex(Vec<Value>),
}

impl Fill {
    /// A column of this type with nothing in it yet.
    fn new(ty: ColumnType) -> Fill {
        let buf = match &ty {
            ColumnType::Null => Buf::Null,
            ColumnType::Bool => Buf::Bool(Vec::new()),
            ColumnType::Int => Buf::Int(Vec::new()),
            ColumnType::Float => Buf::Float(Vec::new()),
            ColumnType::Str => Buf::Str {
                bytes: Vec::new(),
                offsets: vec![0],
            },
            _ => Buf::Complex(Vec::new()),
        };
        Fill {
            ty,
            buf,
            valid: None,
            len: 0,
        }
    }

    /// Notes that the row about to be written has a value.
    fn there(&mut self) {
        if let Some(valid) = &mut self.valid {
            valid.push(1);
        }
        self.len += 1;
    }

    pub(crate) fn push_int(&mut self, v: i64) {
        match &mut self.buf {
            Buf::Int(values) => values.push(v),
            _ => unreachable!("an integer column takes integers"),
        }
        self.there();
    }

    pub(crate) fn push_float(&mut self, v: f64) {
        match &mut self.buf {
            Buf::Float(values) => values.push(v),
            _ => unreachable!("a real column takes reals"),
        }
        self.there();
    }

    /// One string, copied out of the vector it was decoded in without
    /// becoming a `String` on the way.
    pub(crate) fn push_bytes(&mut self, b: &[u8]) {
        match &mut self.buf {
            Buf::Str { bytes, offsets } => {
                bytes.extend_from_slice(b);
                offsets.push(bytes.len() as i64);
            }
            _ => unreachable!("a string column takes strings"),
        }
        self.there();
    }

    /// One value, for the columns no buffer covers and for the
    /// constants that are not a number or a string.
    pub(crate) fn push_value(&mut self, v: Value) {
        match (&mut self.buf, v) {
            (Buf::Null, Value::Null) => {}
            (Buf::Bool(values), Value::Bool(b)) => values.push(u8::from(b)),
            (Buf::Int(values), Value::Int(n)) => values.push(n),
            (Buf::Float(values), Value::Float(f)) => values.push(f),
            (Buf::Str { bytes, offsets }, Value::Str(s)) => {
                bytes.extend_from_slice(s.as_bytes());
                offsets.push(bytes.len() as i64);
            }
            (Buf::Complex(values), v) => values.push(v),
            (_, v) => unreachable!("a column's type is settled before it is filled, not by {v:?}"),
        }
        self.there();
    }

    /// A row with nothing in it, which still takes its cell so the
    /// buffer stays strided and the validity is what says the cell
    /// means nothing.
    pub(crate) fn push_null(&mut self) {
        match &mut self.buf {
            Buf::Null => {}
            Buf::Bool(values) => values.push(0),
            Buf::Int(values) => values.push(0),
            Buf::Float(values) => values.push(0.0),
            Buf::Str { bytes, offsets } => offsets.push(bytes.len() as i64),
            Buf::Complex(values) => values.push(Value::Null),
        }
        // The flags are made here and nowhere else, so a column that
        // never sees a null never allocates them. The rows already in
        // are all there, which is what the fill of ones says.
        self.valid.get_or_insert_with(|| vec![1; self.len]).push(0);
        self.len += 1;
    }
}

/// One worker's columns, and where each morsel's rows sit in them.
///
/// A worker claims morsels in order but the workers interleave, so the
/// answer is stitched by morsel index at the end exactly as the row
/// sink's batches are. The difference is that nothing here is a batch:
/// a worker appends every morsel it claims to one buffer per column and
/// records the span, so a run that ends up on one worker hands its
/// buffers over untouched.
pub(crate) struct ColumnSink {
    fills: Vec<Fill>,
    /// The morsel, where its rows start, and how many, one per morsel
    /// this worker claimed.
    spans: Vec<(usize, usize, usize)>,
    /// Rows written before the morsel in flight.
    started: usize,
    rows: usize,
}

impl ColumnSink {
    /// The columns a projection of these types fills.
    pub(crate) fn new(types: Vec<ColumnType>) -> ColumnSink {
        ColumnSink {
            fills: types.into_iter().map(Fill::new).collect(),
            spans: Vec::new(),
            started: 0,
            rows: 0,
        }
    }

    /// Column `ix`, to write one row of.
    pub(crate) fn at(&mut self, ix: usize) -> &mut Fill {
        &mut self.fills[ix]
    }

    /// More rows written, counted once for the set rather than once
    /// per column of it.
    pub(crate) fn rows(&mut self, n: usize) {
        self.rows += n;
    }

    /// The rows of one morsel are all in, and this is where they sat.
    pub(crate) fn morsel_done(&mut self, idx: usize) {
        self.spans
            .push((idx, self.started, self.rows - self.started));
        self.started = self.rows;
    }
}

/// The workers' columns stitched into one answer, in scan order.
///
/// One worker is the common case and it is free: its morsels were
/// claimed in ascending order and written end to end, so its buffers
/// already are the answer. More than one and the spans are put in
/// morsel order and copied into fresh buffers, which is a memcpy a
/// column rather than a value at a time.
pub(crate) fn merge(names: &[String], mut partials: Vec<ColumnSink>) -> Held {
    if partials.len() == 1 {
        let sink = partials.pop().expect("one partial");
        let rows = sink.rows;
        return finish(names, sink.fills, rows);
    }
    let width = partials.first().map_or(0, |p| p.fills.len());
    let rows = partials.iter().map(|p| p.rows).sum();
    let mut order: Vec<(usize, usize, usize, usize)> = partials
        .iter()
        .enumerate()
        .flat_map(|(w, p)| p.spans.iter().map(move |&(m, at, len)| (m, w, at, len)))
        .collect();
    order.sort_unstable();

    let mut out = Vec::with_capacity(width);
    for ix in 0..width {
        let mut fill = Fill::new(partials[0].fills[ix].ty.clone());
        for &(_, w, at, len) in &order {
            fill.take_from(&mut partials[w].fills[ix], at, len);
        }
        out.push(fill);
    }
    finish(names, out, rows)
}

impl Fill {
    /// Appends `len` rows of `src` starting at `at`, taking the values
    /// rather than copying them: the partial they came from is spent.
    fn take_from(&mut self, src: &mut Fill, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        match (&mut self.buf, &mut src.buf) {
            (Buf::Null, Buf::Null) => {}
            (Buf::Bool(into), Buf::Bool(from)) => into.extend_from_slice(&from[at..at + len]),
            (Buf::Int(into), Buf::Int(from)) => into.extend_from_slice(&from[at..at + len]),
            (Buf::Float(into), Buf::Float(from)) => into.extend_from_slice(&from[at..at + len]),
            (
                Buf::Str {
                    bytes: into,
                    offsets: at_into,
                },
                Buf::Str {
                    bytes: from,
                    offsets: at_from,
                },
            ) => {
                let (start, end) = (at_from[at] as usize, at_from[at + len] as usize);
                let base = at_into.last().copied().expect("an offset per column");
                into.extend_from_slice(&from[start..end]);
                at_into.extend(
                    at_from[at + 1..=at + len]
                        .iter()
                        .map(|off| base + off - start as i64),
                );
            }
            (Buf::Complex(into), Buf::Complex(from)) => {
                let taken = from[at..at + len]
                    .iter_mut()
                    .map(|v| std::mem::replace(v, Value::Null));
                into.extend(taken);
            }
            _ => unreachable!("the partials came from one projection"),
        }
        match &src.valid {
            // A partial with no flags had a value on every row, and the
            // one being built may still be waiting for its first null.
            None => {
                if let Some(valid) = &mut self.valid {
                    valid.resize(valid.len() + len, 1);
                }
            }
            Some(from) => {
                self.valid
                    .get_or_insert_with(|| vec![1; self.len])
                    .extend_from_slice(&from[at..at + len]);
            }
        }
        self.len += len;
    }

    /// This column as a client reads it: the bits packed, the offsets
    /// narrowed, and the validity dropped where nothing is null.
    fn finish(self, name: &str) -> HeldColumn {
        let Fill {
            ty,
            buf,
            valid,
            len,
        } = self;
        let nulls = valid
            .as_ref()
            .map_or(0, |v| v.iter().filter(|&&b| b == 0).count());
        let data = match buf {
            Buf::Null => HeldData::Null,
            Buf::Bool(values) => HeldData::Bool {
                bits: pack(&values),
            },
            Buf::Int(values) => HeldData::Int(values),
            Buf::Float(values) => HeldData::Float(values),
            Buf::Str { bytes, offsets } => {
                // Narrow if the bytes fit a 32 bit offset, which they do
                // for every answer short of two gigabytes of text. The
                // check is the last offset and the walk is over offsets,
                // never over the bytes.
                let offsets = if bytes.len() <= i32::MAX as usize {
                    Offsets::I32(offsets.into_iter().map(|off| off as i32).collect())
                } else {
                    Offsets::I64(offsets)
                };
                HeldData::Str(StrColumn { bytes, offsets })
            }
            Buf::Complex(values) => HeldData::Complex(values),
        };
        HeldColumn {
            name: name.to_string(),
            // A column that turned out to hold nothing but nulls is a
            // column of nulls, which is the type the walk over rows
            // gives it and the one every columnar format has for it.
            ty: if nulls == len && len > 0 {
                ColumnType::Null
            } else {
                ty
            },
            data: if nulls == len && len > 0 {
                HeldData::Null
            } else {
                data
            },
            // A bitmap with nothing null in it is a buffer every reader
            // would AND against for no reason.
            validity: (nulls > 0 && nulls < len).then(|| Validity {
                bits: pack(valid.as_deref().unwrap_or_default()),
                len,
                nulls,
            }),
        }
    }
}

/// One bit a row out of one byte a row, least significant bit first,
/// which is the packing every columnar format keeps validity in.
fn pack(flags: &[u8]) -> Vec<u8> {
    let mut bits = vec![0u8; flags.len().div_ceil(8)];
    for (at, &set) in flags.iter().enumerate() {
        if set != 0 {
            bits[at / 8] |= 1u8 << (at % 8);
        }
    }
    bits
}

fn finish(names: &[String], fills: Vec<Fill>, rows: usize) -> Held {
    Held {
        columns: fills
            .into_iter()
            .zip(names)
            .map(|(fill, name)| fill.finish(name))
            .collect(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["n".into(), "s".into()]
    }

    fn types() -> Vec<ColumnType> {
        vec![ColumnType::Int, ColumnType::Str]
    }

    /// One worker's morsel: the integers given, each with a string of
    /// its own, and a `None` for a row that holds nothing at all.
    fn morsel(sink: &mut ColumnSink, idx: usize, rows: &[Option<i64>]) {
        for &row in rows {
            match row {
                Some(n) => {
                    sink.at(0).push_int(n);
                    sink.at(1).push_bytes(format!("s{n}").as_bytes());
                }
                None => {
                    sink.at(0).push_null();
                    sink.at(1).push_null();
                }
            }
            sink.rows(1);
        }
        sink.morsel_done(idx);
    }

    fn ints(held: &Held, ix: usize) -> Vec<Option<i64>> {
        (0..held.rows)
            .map(|at| match &held.columns[ix].data {
                HeldData::Int(values) => held.columns[ix]
                    .validity
                    .as_ref()
                    .is_none_or(|v| v.is_valid(at))
                    .then(|| values[at]),
                _ => panic!("an integer column"),
            })
            .collect()
    }

    #[test]
    fn one_worker_hands_its_buffers_over_as_they_are() {
        let mut sink = ColumnSink::new(types());
        morsel(&mut sink, 0, &[Some(1), Some(2)]);
        morsel(&mut sink, 1, &[Some(3)]);
        let held = merge(&names(), vec![sink]);
        assert_eq!(held.rows, 3);
        assert_eq!(ints(&held, 0), [Some(1), Some(2), Some(3)]);
        assert!(held.columns[0].validity.is_none(), "nothing is null");
    }

    #[test]
    fn the_morsels_of_two_workers_stitch_back_into_scan_order() {
        let mut one = ColumnSink::new(types());
        let mut two = ColumnSink::new(types());
        // Worker one took morsels 0 and 2, worker two took 1 and 3,
        // which is what interleaved claims look like.
        morsel(&mut one, 0, &[Some(1), Some(2)]);
        morsel(&mut two, 1, &[Some(3)]);
        morsel(&mut one, 2, &[Some(4), Some(5)]);
        morsel(&mut two, 3, &[Some(6)]);
        let held = merge(&names(), vec![one, two]);
        assert_eq!(held.rows, 6);
        assert_eq!(
            ints(&held, 0),
            [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
        );
        let rows = held.rows();
        let names: Vec<&Value> = rows.iter().map(|r| &r[1]).collect();
        assert_eq!(
            names.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>(),
            (1..=6)
                .map(|n| format!("{:?}", Value::Str(format!("s{n}"))))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_null_takes_its_cell_and_loses_its_bit() {
        let mut one = ColumnSink::new(types());
        let mut two = ColumnSink::new(types());
        morsel(&mut one, 0, &[Some(1), None]);
        morsel(&mut two, 1, &[None, Some(4)]);
        let held = merge(&names(), vec![one, two]);
        assert_eq!(ints(&held, 0), [Some(1), None, None, Some(4)]);
        let validity = held.columns[0].validity.as_ref().expect("two nulls");
        assert_eq!(validity.nulls, 2);
        assert_eq!(validity.len, 4);
        // The string column's null is an empty span, which is how a
        // variable width buffer spells a row with nothing in it.
        let HeldData::Str(strings) = &held.columns[1].data else {
            panic!("a string column");
        };
        assert_eq!(strings.bytes, b"s1s4");
        assert_eq!(strings.span(1), (2, 2));
        assert_eq!(strings.span(3), (2, 4));
    }

    #[test]
    fn a_column_of_nothing_but_nulls_is_a_column_of_nulls() {
        let mut sink = ColumnSink::new(types());
        morsel(&mut sink, 0, &[None, None]);
        let held = merge(&names(), vec![sink]);
        assert_eq!(held.columns[0].ty, ColumnType::Null);
        assert_eq!(held.columns[0].data, HeldData::Null);
        // Everything is null, so a bitmap would say what the type says.
        assert!(held.columns[0].validity.is_none());
        assert_eq!(held.rows()[0][0], Value::Null);
    }

    #[test]
    fn a_worker_that_claimed_nothing_adds_nothing() {
        let mut one = ColumnSink::new(types());
        morsel(&mut one, 1, &[Some(7)]);
        let two = ColumnSink::new(types());
        let held = merge(&names(), vec![one, two]);
        assert_eq!(held.rows, 1);
        assert_eq!(ints(&held, 0), [Some(7)]);
    }
}
