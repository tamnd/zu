//! Compiled expression programs (perf/02 section 3).
//!
//! A bound expression compiles once at plan time into a short register
//! program over vectorized kernels; evaluation walks the ops, never the
//! expression tree, and column loads resolve to flat indices, not name
//! probes. The optimizer integration lands with the perf/03 executor;
//! this module owns the program shape and the evaluator.

use std::sync::Arc;

use zu_common::unicode::NormalForm;
use zu_common::{Result, ZuError};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::chunk::DataChunk;
use crate::kernels::{
    BinOp, CmpOp, MathOp, MathPair, StrCut, StrFold, StrLen, StrTrim, TrimSet, binary, compare,
    cut, element_ids, fold, length, normalize, normalized, pair, trim, unary,
};
use crate::str::StrView;
use crate::vector::{Aux, PhysType, ValueVector};

pub type Reg = u8;

/// A constant at the program boundary. Becomes a Constant vector at
/// evaluation, so kernels see one encoding for literals and correlated
/// single values alike.
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Box<[u8]>),
    Node {
        table: u16,
        row: u64,
    },
    /// A temporal value: one word, and the lane that says what the
    /// count in it is a count of. The lane is on the constant rather
    /// than worked out from the word because nothing about a number of
    /// days tells you it is days.
    Lane {
        phys: PhysType,
        word: i64,
    },
}

#[derive(Clone, Debug)]
pub enum ExprOp {
    /// Read a chunk column into a register. A borrow at evaluation, not
    /// a copy; the register remembers the column index.
    LoadCol {
        col: u8,
        dst: Reg,
    },
    LoadConst {
        v: OwnedValue,
        dst: Reg,
    },
    Binary {
        op: BinOp,
        l: Reg,
        r: Reg,
        dst: Reg,
    },
    /// One of the numeric functions over a single number, whose answer
    /// is a value like any other. The op carries which function it is,
    /// so a call costs no dispatch per row.
    Math {
        op: MathOp,
        src: Reg,
        dst: Reg,
    },
    /// One of the numeric functions over two numbers: POWER, LOG and
    /// MOD under its function spelling. Two registers in and one out,
    /// and both arguments hold the one type by the time they get here.
    MathPair {
        op: MathPair,
        l: Reg,
        r: Reg,
        dst: Reg,
    },
    /// The length of a string, in characters or in bytes. The answer
    /// is a number, so this is the one string function whose output
    /// needs no room in the arena for bytes.
    StrLen {
        op: StrLen,
        src: Reg,
        dst: Reg,
    },
    /// A fold of a string, up or down. The answer is a string, so this
    /// is the first op whose output carries bytes of its own rather
    /// than only numbers.
    StrFold {
        op: StrFold,
        src: Reg,
        dst: Reg,
    },
    /// A trim of a string, off either end or both. The set of
    /// characters is whatever the statement wrote, prepared once at
    /// compile time and shared by every chunk the program runs over,
    /// since it is the same set for all of them.
    StrTrim {
        ends: StrTrim,
        set: Arc<TrimSet>,
        src: Reg,
        dst: Reg,
    },
    /// The characters at one end of a string, counted by a second
    /// register rather than by something the statement wrote, since
    /// LEFT and RIGHT take their count as an ordinary argument and a
    /// column is one of the things that can arrive in it.
    StrCut {
        end: StrCut,
        src: Reg,
        n: Reg,
        dst: Reg,
    },
    /// A string put into one of the four normal forms. The form is
    /// whatever the statement wrote, or NFC where it wrote none, and it
    /// is the same form for every chunk the program runs over.
    StrNorm {
        form: NormalForm,
        src: Reg,
        dst: Reg,
    },
    /// Whether a string is in a normal form already. This is the one
    /// string op whose answer is a predicate rather than a value, so it
    /// writes a bitmap the way a comparison does. `negated` carries the
    /// NOT, which belongs in the kernel: a null row is off in either
    /// answer, and a complement of the bitmap would have turned it on.
    StrNormalized {
        form: NormalForm,
        negated: bool,
        src: Reg,
        dst: Reg,
    },
    /// The identifier of a node, which is the one element function
    /// whose answer is a string. The table comes from the plan, a level
    /// having one for every row it will ever produce, and the register
    /// holds the rows themselves.
    ElementId {
        table: u32,
        src: Reg,
        dst: Reg,
    },
    /// Comparison produces a predicate bitmap register.
    Compare {
        op: CmpOp,
        l: Reg,
        r: Reg,
        dst: Reg,
    },
    /// A null test reads the operand's validity, which is the answer
    /// already: IS NULL is the complement of the validity bitmap, IS
    /// NOT NULL is the bitmap itself. `negated` carries the NOT.
    IsNull {
        src: Reg,
        negated: bool,
        dst: Reg,
    },
    /// A predicate the rows cannot change: every row passes, or none
    /// does. `IS TYPED` against the type a column already has is the
    /// case that puts one here, since the answer is a fact about the
    /// column and the compiler knows it before a row is read. Writing
    /// it as an op rather than dropping the filter keeps the program
    /// able to say `false`, which a dropped filter cannot.
    All {
        on: bool,
        dst: Reg,
    },
    And {
        l: Reg,
        r: Reg,
        dst: Reg,
    },
    Or {
        l: Reg,
        r: Reg,
        dst: Reg,
    },
}

pub struct Program {
    pub ops: Vec<ExprOp>,
    pub regs: u8,
}

enum Slot {
    Empty,
    /// Borrowed chunk column, resolved on use.
    Col(u8),
    Vec(ValueVector),
    Bits(Bitmap),
}

impl Program {
    /// Evaluate against a chunk, using the chunk's own selection.
    /// Returns the final op's destination register.
    fn run(&self, chunk: &DataChunk, arena: &mut MorselArena) -> Result<Slot> {
        let mut regs: Vec<Slot> = (0..self.regs).map(|_| Slot::Empty).collect();
        let sel = chunk.sel.as_ref();
        let count = chunk.count as usize;
        let mut last = 0;
        for op in &self.ops {
            match op {
                ExprOp::LoadCol { col, dst } => {
                    regs[*dst as usize] = Slot::Col(*col);
                    last = *dst;
                }
                ExprOp::LoadConst { v, dst } => {
                    regs[*dst as usize] = Slot::Vec(const_vector(arena, v, count)?);
                    last = *dst;
                }
                ExprOp::Binary { op, l, r, dst } => {
                    let out = {
                        let lv = resolve(&regs, *l, chunk)?;
                        let rv = resolve(&regs, *r, chunk)?;
                        binary(arena, *op, lv, rv, sel)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::Math { op, src, dst } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        unary(arena, *op, v, sel)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::MathPair { op, l, r, dst } => {
                    let out = {
                        let lv = resolve(&regs, *l, chunk)?;
                        let rv = resolve(&regs, *r, chunk)?;
                        pair(arena, *op, lv, rv, sel)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrLen { op, src, dst } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        length(arena, *op, v)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrFold { op, src, dst } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        fold(arena, *op, v)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrTrim {
                    ends,
                    set,
                    src,
                    dst,
                } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        trim(arena, *ends, set, v)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrCut { end, src, n, dst } => {
                    let out = {
                        let s = resolve(&regs, *src, chunk)?;
                        let count = resolve(&regs, *n, chunk)?;
                        cut(arena, *end, s, count, sel)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrNorm { form, src, dst } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        normalize(arena, *form, v)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::StrNormalized {
                    form,
                    negated,
                    src,
                    dst,
                } => {
                    let mut bits = Bitmap::new_in(arena, count, false);
                    {
                        let v = resolve(&regs, *src, chunk)?;
                        normalized(*form, *negated, v, &mut bits)?;
                    }
                    regs[*dst as usize] = Slot::Bits(bits);
                    last = *dst;
                }
                ExprOp::ElementId { table, src, dst } => {
                    let out = {
                        let v = resolve(&regs, *src, chunk)?;
                        element_ids(arena, *table, v)?
                    };
                    regs[*dst as usize] = Slot::Vec(out);
                    last = *dst;
                }
                ExprOp::Compare { op, l, r, dst } => {
                    let mut bits = Bitmap::new_in(arena, count, false);
                    {
                        let lv = resolve(&regs, *l, chunk)?;
                        let rv = resolve(&regs, *r, chunk)?;
                        compare(*op, lv, rv, sel, &mut bits)?;
                    }
                    regs[*dst as usize] = Slot::Bits(bits);
                    last = *dst;
                }
                ExprOp::IsNull { src, negated, dst } => {
                    let mut bits = Bitmap::new_in(arena, count, false);
                    {
                        let v = resolve(&regs, *src, chunk)?;
                        validity_bits(v, *negated, &mut bits);
                    }
                    regs[*dst as usize] = Slot::Bits(bits);
                    last = *dst;
                }
                ExprOp::All { on, dst } => {
                    let bits = Bitmap::new_in(arena, count, *on);
                    regs[*dst as usize] = Slot::Bits(bits);
                    last = *dst;
                }
                ExprOp::And { l, r, dst } | ExprOp::Or { l, r, dst } => {
                    let is_and = matches!(op, ExprOp::And { .. });
                    let rbits = match std::mem::replace(&mut regs[*r as usize], Slot::Empty) {
                        Slot::Bits(b) => b,
                        _ => return Err(bad_reg("boolean op on a non-predicate register")),
                    };
                    let mut lbits = match std::mem::replace(&mut regs[*l as usize], Slot::Empty) {
                        Slot::Bits(b) => b,
                        _ => return Err(bad_reg("boolean op on a non-predicate register")),
                    };
                    if is_and {
                        lbits.and_with(&rbits);
                    } else {
                        lbits.or_with(&rbits);
                    }
                    regs[*dst as usize] = Slot::Bits(lbits);
                    last = *dst;
                }
            }
        }
        Ok(std::mem::replace(&mut regs[last as usize], Slot::Empty))
    }

    /// Evaluate a predicate program to a bitmap over the chunk.
    pub fn eval_filter(&self, chunk: &DataChunk, arena: &mut MorselArena) -> Result<Bitmap> {
        match self.run(chunk, arena)? {
            Slot::Bits(b) => Ok(b),
            _ => Err(bad_reg("filter program did not end in a predicate")),
        }
    }

    /// Evaluate a value program to a vector.
    pub fn eval(&self, chunk: &DataChunk, arena: &mut MorselArena) -> Result<ValueVector> {
        match self.run(chunk, arena)? {
            Slot::Vec(v) => Ok(v),
            Slot::Col(c) => Err(ZuError::InvalidArgument(format!(
                "bare column program (col {c}); the caller should read the chunk directly"
            ))),
            _ => Err(bad_reg("value program did not end in a vector")),
        }
    }
}

/// Write a null test into `out`, which arrives cleared. Rows outside
/// the chunk's selection may land either way: the filter refines the
/// selection it already has, so a bit off the selection never reaches a
/// row. Absent validity means every row is valid, the convention the
/// vector reader keeps, so IS NULL over such a column is empty.
fn validity_bits(v: &ValueVector, negated: bool, out: &mut Bitmap) {
    debug_assert_eq!(out.len(), v.len as usize);
    match (&v.validity, negated) {
        (None, false) => {}
        (None, true) => {
            out.words_mut().fill(!0u64);
        }
        (Some(valid), false) => {
            for (w, o) in out.words_mut().iter_mut().zip(valid.words()) {
                *w = !o;
            }
        }
        (Some(valid), true) => {
            out.words_mut().copy_from_slice(valid.words());
        }
    }
    out.mask_tail();
}

fn bad_reg(what: &str) -> ZuError {
    ZuError::InvalidArgument(what.to_string())
}

fn resolve<'a>(regs: &'a [Slot], r: Reg, chunk: &'a DataChunk) -> Result<&'a ValueVector> {
    match &regs[r as usize] {
        Slot::Col(c) => Ok(&chunk.vecs[*c as usize]),
        Slot::Vec(v) => Ok(v),
        _ => Err(bad_reg("operand register holds no vector")),
    }
}

fn const_vector(arena: &mut MorselArena, v: &OwnedValue, len: usize) -> Result<ValueVector> {
    Ok(match v {
        OwnedValue::Int(i) => ValueVector::constant(arena, PhysType::Int64, *i, len),
        OwnedValue::Float(f) => ValueVector::constant(arena, PhysType::Float64, *f, len),
        OwnedValue::Node { table, row } => ValueVector::constant(
            arena,
            PhysType::NodeRef,
            crate::vector::node_ref(*table, *row),
            len,
        ),
        OwnedValue::Str(bytes) => const_str(arena, bytes, len),
        OwnedValue::Lane { phys, word } => ValueVector::constant(arena, *phys, *word, len),
        OwnedValue::Bool(_) | OwnedValue::Null => {
            return Err(ZuError::InvalidArgument(
                "bool and null constants land with the executor port".into(),
            ));
        }
    })
}

/// A constant string vector: inline view when short, otherwise one
/// shared buffer holding the payload.
pub fn const_str(arena: &mut MorselArena, bytes: &[u8], len: usize) -> ValueVector {
    if bytes.len() <= crate::str::INLINE_LEN {
        let mut v = ValueVector::constant(arena, PhysType::Str, StrView::inline(bytes), len);
        v.aux = Aux::None;
        v
    } else {
        let mut bufs = crate::str::StrBuffers::new();
        let id = bufs.push(std::sync::Arc::from(bytes.to_vec().into_boxed_slice()));
        let view = StrView::long(bytes, id, 0);
        let mut v = ValueVector::constant(arena, PhysType::Str, view, len);
        v.aux = Aux::Str(std::sync::Arc::new(bufs));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sel::SelVector;

    fn chunk_i64(arena: &mut MorselArena, vals: &[i64]) -> DataChunk {
        DataChunk::new(
            vec![ValueVector::flat_from(arena, PhysType::Int64, vals)],
            vals.len() as u32,
        )
    }

    #[test]
    fn filter_program() {
        let mut arena = MorselArena::new();
        let chunk = chunk_i64(&mut arena, &[5i64, 15, 25, 35]);
        // col0 > 10 AND col0 < 30
        let p = Program {
            ops: vec![
                ExprOp::LoadCol { col: 0, dst: 0 },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(10),
                    dst: 1,
                },
                ExprOp::Compare {
                    op: CmpOp::Gt,
                    l: 0,
                    r: 1,
                    dst: 2,
                },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(30),
                    dst: 3,
                },
                ExprOp::Compare {
                    op: CmpOp::Lt,
                    l: 0,
                    r: 3,
                    dst: 4,
                },
                ExprOp::And { l: 2, r: 4, dst: 2 },
            ],
            regs: 5,
        };
        let bits = p.eval_filter(&chunk, &mut arena).unwrap();
        let sel = SelVector::from_bitmap(&mut arena, &bits);
        assert_eq!(sel.as_slice(), &[1, 2]);
    }

    #[test]
    fn value_program() {
        let mut arena = MorselArena::new();
        let chunk = chunk_i64(&mut arena, &[1i64, 2, 3]);
        // col0 * 10 + 5
        let p = Program {
            ops: vec![
                ExprOp::LoadCol { col: 0, dst: 0 },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(10),
                    dst: 1,
                },
                ExprOp::Binary {
                    op: BinOp::Mul,
                    l: 0,
                    r: 1,
                    dst: 2,
                },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(5),
                    dst: 1,
                },
                ExprOp::Binary {
                    op: BinOp::Add,
                    l: 2,
                    r: 1,
                    dst: 3,
                },
            ],
            regs: 4,
        };
        let out = p.eval(&chunk, &mut arena).unwrap();
        assert_eq!(out.values::<i64>(), &[15, 25, 35]);
    }

    /// A kernel that missed a row leaves the value slot alone and
    /// clears the validity bit, which is what the two null tests read.
    #[test]
    fn null_tests_read_the_validity() {
        let mut arena = MorselArena::new();
        let mut chunk = chunk_i64(&mut arena, &[7i64, 0, 9, 0, 11]);
        let mut valid = Bitmap::new_in(&mut arena, 5, true);
        valid.clear(1);
        valid.clear(3);
        chunk.vecs[0].validity = Some(valid);

        let program = |negated| Program {
            ops: vec![
                ExprOp::LoadCol { col: 0, dst: 0 },
                ExprOp::IsNull {
                    src: 0,
                    negated,
                    dst: 1,
                },
            ],
            regs: 2,
        };

        let bits = program(true).eval_filter(&chunk, &mut arena).unwrap();
        let sel = SelVector::from_bitmap(&mut arena, &bits);
        assert_eq!(sel.as_slice(), &[0, 2, 4]);

        let bits = program(false).eval_filter(&chunk, &mut arena).unwrap();
        let sel = SelVector::from_bitmap(&mut arena, &bits);
        assert_eq!(sel.as_slice(), &[1, 3]);
    }

    /// A column with no validity is every row valid, so the tests
    /// answer all or nothing without reading a bitmap that is not
    /// there. The tail matters: a length that is not a multiple of the
    /// word width must not report the padding as rows.
    #[test]
    fn a_dense_column_is_null_nowhere() {
        let mut arena = MorselArena::new();
        let vals: Vec<i64> = (0..70).collect();
        let chunk = chunk_i64(&mut arena, &vals);

        let program = |negated| Program {
            ops: vec![
                ExprOp::LoadCol { col: 0, dst: 0 },
                ExprOp::IsNull {
                    src: 0,
                    negated,
                    dst: 1,
                },
            ],
            regs: 2,
        };

        let bits = program(true).eval_filter(&chunk, &mut arena).unwrap();
        assert_eq!(bits.count_ones(), 70);
        let bits = program(false).eval_filter(&chunk, &mut arena).unwrap();
        assert_eq!(bits.count_ones(), 0);
    }
}
