//! Compiled expression programs (perf/02 section 3).
//!
//! A bound expression compiles once at plan time into a short register
//! program over vectorized kernels; evaluation walks the ops, never the
//! expression tree, and column loads resolve to flat indices, not name
//! probes. The optimizer integration lands with the perf/03 executor;
//! this module owns the program shape and the evaluator.

use zu_common::{Result, ZuError};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::chunk::DataChunk;
use crate::kernels::{BinOp, CmpOp, binary, compare};
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
    Node { table: u16, row: u64 },
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
    /// Comparison produces a predicate bitmap register.
    Compare {
        op: CmpOp,
        l: Reg,
        r: Reg,
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
                        binary(arena, *op, lv, rv)?
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
}
