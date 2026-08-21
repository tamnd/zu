//! Randomized differential tests: every kernel against a naive per-row
//! reference over generated inputs, nulls, selections, and encodings.
//! Deterministic xorshift seeds, so a failure reproduces by rerunning.

use std::sync::Arc;

use zu_vector::{
    Bitmap, CmpOp, DataChunk, Dictionary, ExprOp, MorselArena, OwnedValue, PhysType, Program,
    SelVector, ValueVector, kernels, str_vector,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const OPS: [CmpOp; 6] = [
    CmpOp::Eq,
    CmpOp::Ne,
    CmpOp::Lt,
    CmpOp::Le,
    CmpOp::Gt,
    CmpOp::Ge,
];

fn holds<T: PartialOrd>(op: CmpOp, a: T, b: T) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    }
}

/// Rows with ~1/8 nulls out of a narrow value domain so equality hits.
fn gen_rows(rng: &mut Rng, len: usize) -> Vec<Option<i64>> {
    (0..len)
        .map(|_| {
            if rng.below(8) == 0 {
                None
            } else {
                Some(rng.below(64) as i64 - 32)
            }
        })
        .collect()
}

fn to_vector(arena: &mut MorselArena, rows: &[Option<i64>]) -> ValueVector {
    let vals: Vec<i64> = rows.iter().map(|r| r.unwrap_or(0)).collect();
    let mut v = ValueVector::flat_from(arena, PhysType::Int64, &vals);
    if rows.iter().any(|r| r.is_none()) {
        let mut valid = Bitmap::new_in(arena, rows.len(), true);
        for (i, r) in rows.iter().enumerate() {
            if r.is_none() {
                valid.clear(i);
            }
        }
        v.validity = Some(valid);
    }
    v
}

fn bitmap_rows(bits: &Bitmap) -> Vec<bool> {
    (0..bits.len()).map(|i| bits.get(i)).collect()
}

#[test]
fn compare_i64_matches_reference() {
    let mut rng = Rng(0x1D872B41C3F0AA57);
    for round in 0..50 {
        let len = 1 + rng.below(2048) as usize;
        let a = gen_rows(&mut rng, len);
        let b = gen_rows(&mut rng, len);
        let use_sel = round % 3 == 0;
        let mut arena = MorselArena::new();
        let av = to_vector(&mut arena, &a);
        let bv = to_vector(&mut arena, &b);
        let sel = if use_sel {
            let mut s = SelVector::with_capacity(&mut arena, len);
            for i in 0..len {
                if rng.below(2) == 0 {
                    s.push(i as u16);
                }
            }
            Some(s)
        } else {
            None
        };
        let mut in_sel = vec![sel.is_none(); len];
        if let Some(s) = &sel {
            for &row in s.as_slice() {
                in_sel[row as usize] = true;
            }
        }
        for op in OPS {
            let mut bits = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &av, &bv, sel.as_ref(), &mut bits).unwrap();
            let got = bitmap_rows(&bits);
            for i in 0..len {
                let want = in_sel[i]
                    && match (a[i], b[i]) {
                        (Some(x), Some(y)) => holds(op, x, y),
                        _ => false,
                    };
                assert_eq!(got[i], want, "round {round} op {op:?} row {i}");
            }
        }
    }
}

#[test]
fn compare_i64_const_matches_reference() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for round in 0..50 {
        let len = 1 + rng.below(2048) as usize;
        let a = gen_rows(&mut rng, len);
        let c = rng.below(64) as i64 - 32;
        let mut arena = MorselArena::new();
        let av = to_vector(&mut arena, &a);
        let cv = ValueVector::constant(&mut arena, PhysType::Int64, c, len);
        for op in OPS {
            // Flat vs constant, then constant vs flat through the flip.
            let mut bits = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &av, &cv, None, &mut bits).unwrap();
            let mut flipped = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &cv, &av, None, &mut flipped).unwrap();
            for (i, ai) in a.iter().enumerate() {
                let want = ai.is_some_and(|x| holds(op, x, c));
                let want_flip = ai.is_some_and(|x| holds(op, c, x));
                assert_eq!(bits.get(i), want, "round {round} op {op:?} row {i}");
                assert_eq!(
                    flipped.get(i),
                    want_flip,
                    "flip round {round} op {op:?} row {i}"
                );
            }
        }
    }
}

#[test]
fn dict_code_compare_matches_string_compare() {
    let mut rng = Rng(0xA0761D6478BD642F);
    let words: Vec<String> = (0..200)
        .map(|i| format!("entry-{i:04}-{}", "x".repeat(i % 20)))
        .collect();
    let mut sorted = words.clone();
    sorted.sort();
    sorted.dedup();
    let dict = Arc::new(Dictionary::from_sorted(sorted.iter()));
    for round in 0..30 {
        let len = 1 + rng.below(1024) as usize;
        let codes: Vec<u16> = (0..len)
            .map(|_| rng.below(sorted.len() as u64) as u16)
            .collect();
        // Present needles and absent ones both matter: the absent case
        // exercises the insertion-point range mapping.
        let needle = if round % 2 == 0 {
            sorted[rng.below(sorted.len() as u64) as usize].clone()
        } else {
            format!("entry-{:04}-absent", rng.below(300))
        };
        let mut arena = MorselArena::new();
        let dv = ValueVector::dict_str(&mut arena, &codes, Arc::clone(&dict));
        let cv = zu_vector::const_str(&mut arena, needle.as_bytes(), len);
        for op in OPS {
            let mut bits = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &dv, &cv, None, &mut bits).unwrap();
            for (i, &code) in codes.iter().enumerate() {
                let value = &sorted[code as usize];
                let want = holds(op, value.as_bytes(), needle.as_bytes());
                assert_eq!(
                    bits.get(i),
                    want,
                    "round {round} op {op:?} row {i} value {value}"
                );
            }
        }
    }
}

#[test]
fn flat_str_compare_matches_reference() {
    let mut rng = Rng(0xE7037ED1A0B428DB);
    for round in 0..20 {
        let len = 1 + rng.below(512) as usize;
        let vals: Vec<String> = (0..len)
            .map(|_| {
                let n = rng.below(30) as usize;
                (0..n)
                    .map(|_| (b'a' + rng.below(4) as u8) as char)
                    .collect()
            })
            .collect();
        let needle: String = {
            let n = rng.below(30) as usize;
            (0..n)
                .map(|_| (b'a' + rng.below(4) as u8) as char)
                .collect()
        };
        let mut arena = MorselArena::new();
        let sv = str_vector(&mut arena, &vals);
        let cv = zu_vector::const_str(&mut arena, needle.as_bytes(), len);
        for op in OPS {
            let mut bits = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &sv, &cv, None, &mut bits).unwrap();
            for (i, val) in vals.iter().enumerate() {
                let want = holds(op, val.as_bytes(), needle.as_bytes());
                assert_eq!(bits.get(i), want, "round {round} op {op:?} row {i} {val:?}");
            }
        }
    }
}

/// Two string columns compared row against row, which is the arm with
/// no constant to translate against and the one an engine reaches when
/// a query compares two properties or a property with what a function
/// made of it. The strings run either side of the twelve bytes a view
/// holds, so equality is settled on the prefix for some pairs and in a
/// buffer for others, and the answer has to be the same either way.
#[test]
fn flat_str_pair_compare_matches_reference() {
    let mut rng = Rng(0xB5026F5AA96619E9);
    let word = |rng: &mut Rng| -> String {
        let n = rng.below(30) as usize;
        (0..n)
            .map(|_| (b'a' + rng.below(4) as u8) as char)
            .collect()
    };
    for round in 0..20 {
        let len = 1 + rng.below(512) as usize;
        let left: Vec<String> = (0..len).map(|_| word(&mut rng)).collect();
        // Half the rows repeat the left side, so equality is not a
        // question the generator answers no to every time.
        let right: Vec<String> = left
            .iter()
            .map(|l| {
                if rng.below(2) == 0 {
                    l.clone()
                } else {
                    word(&mut rng)
                }
            })
            .collect();
        let mut arena = MorselArena::new();
        let lv = str_vector(&mut arena, &left);
        let rv = str_vector(&mut arena, &right);
        for op in OPS {
            let mut bits = Bitmap::new_in(&mut arena, len, false);
            kernels::compare(op, &lv, &rv, None, &mut bits).unwrap();
            for i in 0..len {
                let want = holds(op, left[i].as_bytes(), right[i].as_bytes());
                assert_eq!(bits.get(i), want, "round {round} op {op:?} row {i}");
            }
        }
    }
}

#[test]
fn selection_algebra_composes() {
    let mut rng = Rng(0x243F6A8885A308D3);
    for _ in 0..30 {
        let len = 1 + rng.below(2048) as usize;
        let mut arena = MorselArena::new();
        let mut first = Bitmap::new_in(&mut arena, len, false);
        let mut second = Bitmap::new_in(&mut arena, len, false);
        for i in 0..len {
            if rng.below(3) == 0 {
                first.set(i);
            }
            if rng.below(3) == 0 {
                second.set(i);
            }
        }
        // Refining the first selection by the second bitmap must equal
        // building the selection from the AND of the bitmaps.
        let sel = SelVector::from_bitmap(&mut arena, &first);
        let refined = SelVector::refine(&mut arena, &sel, &second);
        first.and_with(&second);
        let anded = SelVector::from_bitmap(&mut arena, &first);
        assert_eq!(refined.as_slice(), anded.as_slice());
    }
}

#[test]
fn sum_and_intersect_match_reference() {
    let mut rng = Rng(0x452821E638D01377);
    for _ in 0..30 {
        let len = 1 + rng.below(2048) as usize;
        let rows = gen_rows(&mut rng, len);
        let mut arena = MorselArena::new();
        let v = to_vector(&mut arena, &rows);
        let want: i64 = rows.iter().flatten().sum();
        assert_eq!(kernels::sum_i64(&v, None), want);

        // Sizes span the plain merge, the 2-way and the 4-way split
        // paths, and the gallop.
        let mut a: Vec<u64> = (0..rng.below(3000)).map(|_| rng.below(40_000)).collect();
        let mut b: Vec<u64> = (0..rng.below(3000)).map(|_| rng.below(40_000)).collect();
        a.sort_unstable();
        a.dedup();
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
    }
}

#[test]
fn program_matches_hand_evaluation() {
    let mut rng = Rng(0x13198A2E03707344);
    for _ in 0..20 {
        let len = 1 + rng.below(2048) as usize;
        let vals: Vec<i64> = (0..len).map(|_| rng.below(1000) as i64).collect();
        let lo = rng.below(1000) as i64;
        let hi = lo + rng.below(500) as i64;
        let mut arena = MorselArena::new();
        let chunk = DataChunk::new(
            vec![ValueVector::flat_from(&mut arena, PhysType::Int64, &vals)],
            len as u32,
        );
        let p = Program {
            ops: vec![
                ExprOp::LoadCol { col: 0, dst: 0 },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(lo),
                    dst: 1,
                },
                ExprOp::Compare {
                    op: CmpOp::Ge,
                    l: 0,
                    r: 1,
                    dst: 2,
                },
                ExprOp::LoadConst {
                    v: OwnedValue::Int(hi),
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
        for (i, &x) in vals.iter().enumerate() {
            assert_eq!(bits.get(i), x >= lo && x < hi, "row {i}");
        }
    }
}
