//! Aggregate input kernels: straight-line reductions over a vector,
//! validity- and selection-aware. These feed the aggregate operators;
//! the grouping machinery lives with the executor.

use crate::sel::SelVector;
use crate::vector::{ValueVector, VecEncoding};

/// Wrapping sum of an Int64-domain vector. The no-null no-selection flat
/// case is the B3 scan+sum inner loop and must stay a bare reduction the
/// auto-vectorizer folds.
pub fn sum_i64(v: &ValueVector, sel: Option<&SelVector>) -> i64 {
    match (v.encoding, sel, &v.validity) {
        (VecEncoding::Constant, sel, _) => {
            let n = match sel {
                Some(s) => s.len() as i64,
                None => i64::from(v.len),
            };
            v.constant_value::<i64>().wrapping_mul(n)
        }
        (VecEncoding::Flat, None, None) => {
            let mut acc = 0i64;
            for &x in v.values::<i64>() {
                acc = acc.wrapping_add(x);
            }
            acc
        }
        (VecEncoding::Flat, None, Some(valid)) => {
            // Branch-free: multiply each value by its validity bit.
            let mut acc = 0i64;
            for (i, &x) in v.values::<i64>().iter().enumerate() {
                acc = acc.wrapping_add(x.wrapping_mul(i64::from(valid.get(i))));
            }
            acc
        }
        (VecEncoding::Flat, Some(s), _) => {
            let vals = v.values::<i64>();
            let mut acc = 0i64;
            for &row in s.as_slice() {
                let i = row as usize;
                if v.is_valid(i) {
                    acc = acc.wrapping_add(vals[i]);
                }
            }
            acc
        }
        (VecEncoding::Dict { .. }, _, _) => unreachable!("no i64 dict vectors"),
    }
}

pub fn sum_f64(v: &ValueVector, sel: Option<&SelVector>) -> f64 {
    match (v.encoding, sel) {
        (VecEncoding::Constant, sel) => {
            let n = match sel {
                Some(s) => s.len() as f64,
                None => f64::from(v.len),
            };
            v.constant_value::<f64>() * n
        }
        (VecEncoding::Flat, None) => {
            let mut acc = 0f64;
            for (i, &x) in v.values::<f64>().iter().enumerate() {
                if v.is_valid(i) {
                    acc += x;
                }
            }
            acc
        }
        (VecEncoding::Flat, Some(s)) => {
            let vals = v.values::<f64>();
            let mut acc = 0f64;
            for &row in s.as_slice() {
                let i = row as usize;
                if v.is_valid(i) {
                    acc += vals[i];
                }
            }
            acc
        }
        (VecEncoding::Dict { .. }, _) => unreachable!("no f64 dict vectors"),
    }
}

/// Non-null row count under the selection.
pub fn count_valid(v: &ValueVector, sel: Option<&SelVector>) -> usize {
    match (&v.validity, sel) {
        (None, None) => v.len(),
        (None, Some(s)) => s.len(),
        (Some(valid), None) => valid.count_ones(),
        (Some(valid), Some(s)) => s
            .as_slice()
            .iter()
            .filter(|&&row| valid.get(row as usize))
            .count(),
    }
}

macro_rules! minmax {
    ($name:ident, $better:tt) => {
        /// Extreme of the valid rows, None when every row is null.
        pub fn $name(v: &ValueVector, sel: Option<&SelVector>) -> Option<i64> {
            let mut best: Option<i64> = None;
            let mut consider = |x: i64| match best {
                Some(b) if !(x $better b) => {}
                _ => best = Some(x),
            };
            match (v.encoding, sel) {
                (VecEncoding::Constant, _) => {
                    if count_valid(v, sel) > 0 {
                        consider(v.constant_value::<i64>());
                    }
                }
                // A column with nothing missing in it, which is the
                // scan this reduction is on the hot path of, and the
                // one shape here written for the vectorizer rather than
                // for the reader. The accumulator is an i64 and not an
                // Option of one, and the row is a select rather than a
                // branch, which is the pair of things that turn into a
                // lane wide minimum. The loop below it, folding an
                // Option that is empty until the first row over a
                // validity nobody here has to ask about, takes a row at
                // a time and measured 0 vector instructions on both
                // architectures; this one takes 31 on aarch64.
                //
                // Where it lands is the ISA's to say. NEON compares two
                // 64 bit lanes with cmgt and selects with bsl, so the
                // reduction folds. A generic x86-64 build has neither:
                // the packed 64 bit compare is SSE4.2 and the packed 64
                // bit minimum is AVX-512, and with only SSE2 to hand
                // LLVM leaves the loop scalar. The disassembly gate
                // knows that and asks for lanes on the one where they
                // are reachable.
                (VecEncoding::Flat, None) if v.validity.is_none() => {
                    let vals = v.values::<i64>();
                    if let Some((&first, rest)) = vals.split_first() {
                        let mut acc = first;
                        for &x in rest {
                            acc = if x $better acc { x } else { acc };
                        }
                        consider(acc);
                    }
                }
                (VecEncoding::Flat, None) => {
                    for (i, &x) in v.values::<i64>().iter().enumerate() {
                        if v.is_valid(i) {
                            consider(x);
                        }
                    }
                }
                (VecEncoding::Flat, Some(s)) => {
                    let vals = v.values::<i64>();
                    for &row in s.as_slice() {
                        if v.is_valid(row as usize) {
                            consider(vals[row as usize]);
                        }
                    }
                }
                (VecEncoding::Dict { .. }, _) => unreachable!("no i64 dict vectors"),
            }
            best
        }
    };
}

minmax!(min_i64, <);
minmax!(max_i64, >);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::MorselArena;
    use crate::bitmap::Bitmap;
    use crate::vector::PhysType;

    #[test]
    fn sums_and_extremes() {
        let mut arena = MorselArena::new();
        let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[3i64, -1, 7, 0]);
        assert_eq!(sum_i64(&v, None), 9);
        assert_eq!(min_i64(&v, None), Some(-1));
        assert_eq!(max_i64(&v, None), Some(7));
        assert_eq!(count_valid(&v, None), 4);
    }

    #[test]
    fn nulls_are_skipped() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[5i64, 100, 2]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        assert_eq!(sum_i64(&v, None), 7);
        assert_eq!(max_i64(&v, None), Some(5));
        assert_eq!(count_valid(&v, None), 2);
    }

    #[test]
    fn constant_scales_by_count() {
        let mut arena = MorselArena::new();
        let v = ValueVector::constant(&mut arena, PhysType::Int64, 4i64, 10);
        assert_eq!(sum_i64(&v, None), 40);
    }
}
