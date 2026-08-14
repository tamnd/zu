//! Comparison kernels: the (op x type x encoding-pair) matrix.
//!
//! Output is a positional bitmap over the vector, bit set when the
//! predicate holds and both sides are valid, so NULL compares false and
//! null propagation costs one word-wise AND at the end. With a selection
//! present only the selected rows are evaluated; the rest stay zero.
//!
//! The dict x constant arm is the compressed-domain path from perf/07
//! section 3: the constant translates through the sorted dictionary once,
//! then every row is one integer compare against a code. The dictionary
//! order matches the value order, so range predicates translate too, via
//! the insertion point when the constant itself is absent.

use zu_common::{Result, ZuError};

use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::str::StrView;
use crate::vector::{PhysType, ValueVector, VecEncoding};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The op with its operands swapped: a < b is b > a.
    fn flip(self) -> Self {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }

    /// Whether the op holds between two values. This is the whole
    /// definition of the op, which is why it is public: a caller that
    /// rewrites a predicate before compiling it can check the rewrite
    /// against the kernel's own rule rather than against a copy of it.
    #[inline(always)]
    pub fn holds<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
}

/// Pack eight bool bytes (0 or 1 each) into eight bits. Byte i of the
/// input lands on bit i. The multiply spreads each byte's bit onto its
/// own position in the top byte; the cross terms provably never reach
/// bits 56 and up.
#[inline(always)]
fn pack8(bool_bytes: u64) -> u64 {
    bool_bytes.wrapping_mul(0x0102_0408_1020_4080) >> 56
}

/// Turn a byte-per-row predicate staging area into bitmap words.
#[inline(always)]
fn pack_stage(stage: &[u8], out: &mut Bitmap) {
    let words = out.words_mut();
    for (wi, group) in stage.chunks(64).enumerate() {
        let mut w = 0u64;
        let mut octets = group.chunks_exact(8);
        for (k, bytes) in octets.by_ref().enumerate() {
            let v = u64::from_le_bytes(bytes.try_into().expect("octet"));
            w |= pack8(v) << (8 * k);
        }
        let base = group.len() / 8 * 8;
        for (b, &s) in octets.remainder().iter().enumerate() {
            w |= u64::from(s) << (base + b);
        }
        words[wi] = w;
    }
}

/// Predicate over a full vector in two passes: compare into one bool
/// byte per row, which every backend turns into packed compares with
/// narrowing stores, then pack the bytes into mask words with one
/// multiply per eight rows. The staging array lives on the stack. The
/// op dispatch happens here, outside the loop, so each arm's inner
/// closure is branch-free and the vectorizer sees a single compare.
#[inline(always)]
fn fill_const<T: Copy + PartialOrd>(op: CmpOp, vals: &[T], c: T, out: &mut Bitmap) {
    match op {
        CmpOp::Eq => fill_const_with(vals, out, |x| x == c),
        CmpOp::Ne => fill_const_with(vals, out, |x| x != c),
        CmpOp::Lt => fill_const_with(vals, out, |x| x < c),
        CmpOp::Le => fill_const_with(vals, out, |x| x <= c),
        CmpOp::Gt => fill_const_with(vals, out, |x| x > c),
        CmpOp::Ge => fill_const_with(vals, out, |x| x >= c),
    }
}

#[inline(always)]
fn fill_const_with<T: Copy, F: Fn(T) -> bool>(vals: &[T], out: &mut Bitmap, f: F) {
    debug_assert!(vals.len() <= crate::VECTOR_SIZE);
    // Uninit staging: zeroing 2 KiB per call would cost more than the
    // whole compare pass for narrow types. Only bytes written below are
    // read back.
    let mut stage = [std::mem::MaybeUninit::<u8>::uninit(); crate::VECTOR_SIZE];
    for (s, &x) in stage.iter_mut().zip(vals) {
        s.write(u8::from(f(x)));
    }
    let stage: &[u8] = unsafe { std::slice::from_raw_parts(stage.as_ptr().cast(), vals.len()) };
    pack_stage(stage, out);
}

#[inline(always)]
fn fill_pair<T: Copy + PartialOrd>(op: CmpOp, l: &[T], r: &[T], out: &mut Bitmap) {
    match op {
        CmpOp::Eq => fill_pair_with(l, r, out, |x, y| x == y),
        CmpOp::Ne => fill_pair_with(l, r, out, |x, y| x != y),
        CmpOp::Lt => fill_pair_with(l, r, out, |x, y| x < y),
        CmpOp::Le => fill_pair_with(l, r, out, |x, y| x <= y),
        CmpOp::Gt => fill_pair_with(l, r, out, |x, y| x > y),
        CmpOp::Ge => fill_pair_with(l, r, out, |x, y| x >= y),
    }
}

#[inline(always)]
fn fill_pair_with<T: Copy, F: Fn(T, T) -> bool>(l: &[T], r: &[T], out: &mut Bitmap, f: F) {
    debug_assert!(l.len() <= crate::VECTOR_SIZE);
    let mut stage = [std::mem::MaybeUninit::<u8>::uninit(); crate::VECTOR_SIZE];
    for ((s, &x), &y) in stage.iter_mut().zip(l).zip(r) {
        s.write(u8::from(f(x, y)));
    }
    let n = l.len().min(r.len());
    let stage: &[u8] = unsafe { std::slice::from_raw_parts(stage.as_ptr().cast(), n) };
    pack_stage(stage, out);
}

#[inline(always)]
fn fill_sel_const<T: Copy, F: Fn(T) -> bool>(vals: &[T], sel: &SelVector, out: &mut Bitmap, f: F) {
    for &row in sel.as_slice() {
        if f(vals[row as usize]) {
            out.set(row as usize);
        }
    }
}

#[inline(always)]
fn fill_sel_pair<T: Copy, F: Fn(T, T) -> bool>(
    l: &[T],
    r: &[T],
    sel: &SelVector,
    out: &mut Bitmap,
    f: F,
) {
    for &row in sel.as_slice() {
        if f(l[row as usize], r[row as usize]) {
            out.set(row as usize);
        }
    }
}

/// The typed arms share this shape; the macro stamps one per physical
/// representation so each loop monomorphizes on its element type.
macro_rules! cmp_typed {
    ($op:expr, $l:expr, $r:expr, $sel:expr, $out:expr, $ty:ty) => {{
        let op = $op;
        match ($l.encoding, $r.encoding) {
            (VecEncoding::Flat, VecEncoding::Constant) => {
                let c = $r.constant_value::<$ty>();
                match $sel {
                    None => fill_const(op, $l.values::<$ty>(), c, $out),
                    Some(s) => fill_sel_const($l.values::<$ty>(), s, $out, |x| op.holds(x, c)),
                }
            }
            (VecEncoding::Constant, VecEncoding::Flat) => {
                let op = op.flip();
                let c = $l.constant_value::<$ty>();
                match $sel {
                    None => fill_const(op, $r.values::<$ty>(), c, $out),
                    Some(s) => fill_sel_const($r.values::<$ty>(), s, $out, |x| op.holds(x, c)),
                }
            }
            (VecEncoding::Flat, VecEncoding::Flat) => match $sel {
                None => fill_pair(op, $l.values::<$ty>(), $r.values::<$ty>(), $out),
                Some(s) => {
                    fill_sel_pair($l.values::<$ty>(), $r.values::<$ty>(), s, $out, |x, y| {
                        op.holds(x, y)
                    })
                }
            },
            (VecEncoding::Constant, VecEncoding::Constant) => {
                if op.holds($l.constant_value::<$ty>(), $r.constant_value::<$ty>()) {
                    set_all($sel, $out);
                }
            }
            _ => return unsupported($l, $r),
        }
    }};
}

fn set_all(sel: Option<&SelVector>, out: &mut Bitmap) {
    match sel {
        None => {
            out.words_mut().fill(!0u64);
            out.mask_tail();
        }
        Some(s) => {
            for &row in s.as_slice() {
                out.set(row as usize);
            }
        }
    }
}

fn unsupported(l: &ValueVector, r: &ValueVector) -> Result<()> {
    Err(ZuError::InvalidArgument(format!(
        "no compare kernel for {:?} {:?} vs {:?} {:?}",
        l.phys, l.encoding, r.phys, r.encoding
    )))
}

/// Compare two vectors of the same physical type into `out`, which must
/// be sized to the vector length and cleared (or freshly allocated).
pub fn compare(
    op: CmpOp,
    l: &ValueVector,
    r: &ValueVector,
    sel: Option<&SelVector>,
    out: &mut Bitmap,
) -> Result<()> {
    debug_assert_eq!(l.len, r.len);
    debug_assert_eq!(out.len(), l.len as usize);
    if l.phys != r.phys {
        return unsupported(l, r);
    }
    match l.phys {
        PhysType::Int64 | PhysType::Date | PhysType::Timestamp | PhysType::Interval => {
            cmp_typed!(op, l, r, sel, out, i64)
        }
        PhysType::Float64 => cmp_typed!(op, l, r, sel, out, f64),
        PhysType::NodeRef | PhysType::RelRef => cmp_typed!(op, l, r, sel, out, u64),
        PhysType::Str => cmp_str(op, l, r, sel, out)?,
        PhysType::Bool | PhysType::List => return unsupported(l, r),
    }
    if let Some(v) = &l.validity {
        out.and_with(v);
    }
    if let Some(v) = &r.validity {
        out.and_with(v);
    }
    out.mask_tail();
    Ok(())
}

fn cmp_str(
    op: CmpOp,
    l: &ValueVector,
    r: &ValueVector,
    sel: Option<&SelVector>,
    out: &mut Bitmap,
) -> Result<()> {
    match (l.encoding, r.encoding) {
        // The compressed-domain arm: constant through the dictionary
        // once, then a u16 compare per row.
        (VecEncoding::Dict { codes_width: 2 }, VecEncoding::Constant) => {
            let dict = l.dictionary();
            let needle = const_str_bytes(r);
            let codes = l.codes_u16();
            // Translate the predicate into the code domain. The
            // dictionary is sorted, so when the needle is absent the
            // insertion point carries the range semantics.
            let (op, threshold) = match (op, dict.code_of(needle)) {
                (CmpOp::Eq, Ok(c)) => (CmpOp::Eq, c),
                (CmpOp::Eq, Err(_)) => return Ok(()),
                (CmpOp::Ne, Ok(c)) => (CmpOp::Ne, c),
                (CmpOp::Ne, Err(_)) => {
                    set_all(sel, out);
                    return Ok(());
                }
                (CmpOp::Lt, Ok(c)) | (CmpOp::Lt | CmpOp::Le, Err(c)) => (CmpOp::Lt, c),
                (CmpOp::Le, Ok(c)) => (CmpOp::Le, c),
                (CmpOp::Gt, Ok(c)) => (CmpOp::Gt, c),
                (CmpOp::Ge, Ok(c)) | (CmpOp::Gt | CmpOp::Ge, Err(c)) => (CmpOp::Ge, c),
            };
            let t = threshold as u16;
            match sel {
                None => fill_const(op, codes, t, out),
                Some(s) => fill_sel_const(codes, s, out, |c| op.holds(c, t)),
            }
            Ok(())
        }
        (VecEncoding::Constant, VecEncoding::Dict { .. }) => cmp_str(op.flip(), r, l, sel, out),
        // Two vectors over the same dictionary compare as codes.
        (VecEncoding::Dict { codes_width: 2 }, VecEncoding::Dict { codes_width: 2 })
            if std::ptr::eq(l.dictionary(), r.dictionary()) =>
        {
            let (a, b) = (l.codes_u16(), r.codes_u16());
            match sel {
                None => fill_pair(op, a, b, out),
                Some(s) => fill_sel_pair(a, b, s, out, |x, y| op.holds(x, y)),
            }
            Ok(())
        }
        (VecEncoding::Flat, VecEncoding::Constant) => {
            let needle = const_str_bytes(r);
            let bufs = l.str_buffers();
            let views = l.values::<StrView>();
            // A vector without buffers holds only inline views.
            let eval = |i: usize| -> bool {
                let bytes = match bufs {
                    Some(b) => views[i].bytes(b),
                    None => views[i].inline_bytes(),
                };
                op.holds(bytes, needle)
            };
            match sel {
                None => {
                    for i in 0..views.len() {
                        if eval(i) {
                            out.set(i);
                        }
                    }
                }
                Some(s) => {
                    for &row in s.as_slice() {
                        if eval(row as usize) {
                            out.set(row as usize);
                        }
                    }
                }
            }
            Ok(())
        }
        (VecEncoding::Constant, VecEncoding::Flat) => cmp_str(op.flip(), r, l, sel, out),
        _ => unsupported(l, r),
    }
}

/// Bytes of a constant string vector. Inline constants read from the
/// view, which lives in the vector's data buffer; long ones resolve
/// through the vector's own buffers.
fn const_str_bytes(v: &ValueVector) -> &[u8] {
    debug_assert_eq!(v.encoding, VecEncoding::Constant);
    let view: &StrView = &v.data.as_slice::<StrView>()[0];
    if view.is_inline() {
        view.inline_bytes()
    } else {
        let bufs = v.str_buffers().expect("long constant needs buffers");
        view.bytes(bufs)
    }
}
