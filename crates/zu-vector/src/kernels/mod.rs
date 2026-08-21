//! Vectorized kernels (perf/02 section 3, perf/11).
//!
//! Tier 1 policy throughout: plain scalar Rust in fixed-shape loops the
//! compiler auto-vectorizes, measured before anything fancier. No
//! per-ISA intrinsics; portable std::simd goes behind a feature only
//! where a bench proves tier 1 short. Comparison kernels build the
//! predicate bitmap a word at a time, one u64 per 64 rows, so the inner
//! loop has no per-row store or branch.

mod agg;
mod arith;
mod cmp;
mod gather;
mod hash;
mod math;
mod setops;
mod strings;

pub use agg::{count_valid, max_i64, min_i64, sum_f64, sum_i64};
pub use arith::{BinOp, binary};
pub use cmp::{CmpOp, compare};
pub use gather::{compact, gather_u64};
pub use hash::{hash_slice, hash64};
pub use math::{MathOp, MathPair, pair, unary};
pub use setops::intersect_sorted;
pub use strings::{
    StrCut, StrFold, StrLen, StrNorm, StrTrim, TrimSet, cut, element_ids, fold, length, normalize,
    normalized, trim,
};
