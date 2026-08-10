//! Kernels against a naive reference on fuzzer-shaped inputs: compare,
//! selection building, and sorted intersection must agree with the
//! per-row versions and never panic or overrun the arena buffers.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zu_vector::{Bitmap, CmpOp, MorselArena, PhysType, SelVector, ValueVector, kernels};

const OPS: [CmpOp; 6] = [
    CmpOp::Eq,
    CmpOp::Ne,
    CmpOp::Lt,
    CmpOp::Le,
    CmpOp::Gt,
    CmpOp::Ge,
];

fn holds(op: CmpOp, a: i64, b: i64) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let op = OPS[(data[0] % 6) as usize];
    let c = data[1] as i64 - 128;
    let vals: Vec<i64> = data[2..]
        .chunks(2)
        .take(zu_vector::VECTOR_SIZE)
        .map(|w| i16::from_le_bytes([w[0], *w.get(1).unwrap_or(&0)]) as i64)
        .collect();
    if vals.is_empty() {
        return;
    }

    let mut arena = MorselArena::new();
    let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &vals);
    let cv = ValueVector::constant(&mut arena, PhysType::Int64, c, vals.len());
    let mut bits = Bitmap::new_in(&mut arena, vals.len(), false);
    kernels::compare(op, &v, &cv, None, &mut bits).unwrap();
    let sel = SelVector::from_bitmap(&mut arena, &bits);

    let expect: Vec<u16> = vals
        .iter()
        .enumerate()
        .filter(|&(_, &x)| holds(op, x, c))
        .map(|(i, _)| i as u16)
        .collect();
    assert_eq!(sel.as_slice(), expect.as_slice());

    // Intersection of the matching rows with an arbitrary second sorted
    // set, exercising the merge, split, and gallop paths.
    let mut a: Vec<u64> = vals.iter().map(|&x| (x + 40_000) as u64).collect();
    a.sort_unstable();
    a.dedup();
    let mut b: Vec<u64> = vals.iter().map(|&x| (x as u64) & 0xFFF).collect();
    b.sort_unstable();
    b.dedup();
    let mut out = vec![0u64; a.len().min(b.len())];
    let n = kernels::intersect_sorted(&a, &b, &mut out);
    let naive: Vec<u64> = a
        .iter()
        .filter(|x| b.binary_search(x).is_ok())
        .copied()
        .collect();
    assert_eq!(&out[..n], naive.as_slice());
});
