//! Kernel throughput bench and the P1 vector gates (perf/02 section 6,
//! perf/11 section 3).
//!
//! Measures the tier-1 auto-vectorized kernels on synthetic vectors at
//! the executor width. With ZU_GATE=1 the process exits nonzero when a
//! kernel misses its floor in bench/budgets.toml. Floors are set from
//! the slowest gate machine; the spec targets are printed next to each
//! measurement for the roofline picture.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-vector

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use zu_vector::{
    Bitmap, CmpOp, Dictionary, MorselArena, PhysType, SelVector, ValueVector, kernels,
};

const VECTOR: usize = 2048;

struct Budgets(Vec<(String, f64)>);

impl Budgets {
    fn load() -> Self {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
        let mut entries = Vec::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some((k, v)) = line.split_once('=')
                    && let Ok(f) = v.trim().parse::<f64>()
                {
                    entries.push((k.trim().to_string(), f));
                }
            }
        }
        Self(entries)
    }

    fn get(&self, key: &str) -> Option<f64> {
        self.0.iter().find(|(k, _)| k == key).map(|&(_, f)| f)
    }
}

/// Run `body` for at least a second of wall time after one warm-up call
/// and return iterations per second. The body must black_box its result.
fn measure<F: FnMut()>(mut body: F) -> f64 {
    body();
    let mut iters = 0u64;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 1.0 {
        body();
        iters += 1;
    }
    iters as f64 / start.elapsed().as_secs_f64()
}

/// The pre-decoded baseline for the dict gate: owned Strings compared
/// per row. inline(never) so its codegen cannot drift with the rest of
/// main between builds; the gate is a ratio and needs a stable divisor.
#[inline(never)]
fn string_eq_baseline(strings: &[String], needle: &str) -> usize {
    let mut n = 0usize;
    for s in strings {
        n += usize::from(s == needle);
    }
    n
}

/// What the read path does per row today: materialize a fresh String
/// (alloc, copy, utf8 check), then compare.
#[inline(never)]
fn today_string_path(codes: &[u16], entries: &[String], needle: &str) -> usize {
    let mut n = 0usize;
    for &code in codes {
        let s = String::from_utf8(entries[code as usize].as_bytes().to_vec()).unwrap();
        n += usize::from(s == needle);
    }
    n
}

// Disassembly anchors: fixed-shape no_mangle entry points for the hot
// kernels, so bench/check_asm.sh can disassemble each one by symbol and
// assert the vectorizer did its job. The kernels inline into these
// bodies, which is exactly the codegen the executor will run. Never on
// any hot path themselves.
#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_cmp_i64_const(l: &ValueVector, r: &ValueVector, out: &mut Bitmap) {
    kernels::compare(CmpOp::Lt, l, r, None, out).unwrap();
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_cmp_i64_pair(l: &ValueVector, r: &ValueVector, out: &mut Bitmap) {
    kernels::compare(CmpOp::Le, l, r, None, out).unwrap();
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_cmp_dict_const(l: &ValueVector, r: &ValueVector, out: &mut Bitmap) {
    kernels::compare(CmpOp::Eq, l, r, None, out).unwrap();
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_sum_i64(v: &ValueVector) -> i64 {
    kernels::sum_i64(v, None)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_sum_f64(v: &ValueVector) -> f64 {
    kernels::sum_f64(v, None)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_min_i64(v: &ValueVector) -> Option<i64> {
    kernels::min_i64(v, None)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_intersect(a: &[u64], b: &[u64], out: &mut [u64]) -> usize {
    kernels::intersect_sorted(a, b, out)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_bitmap_to_sel(arena: &mut MorselArena, bits: &Bitmap) -> SelVector {
    SelVector::from_bitmap(arena, bits)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_hash(keys: &[u64], out: &mut [u64]) {
    kernels::hash_slice(keys, out);
}

#[unsafe(no_mangle)]
#[inline(never)]
pub fn zu_asm_gather(src: &[u64], idx: &[u32], out: &mut [u64]) {
    kernels::gather_u64(src, idx, out);
}

fn main() {
    let budgets = Budgets::load();
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    // Touch every anchor so the linker cannot dead-strip them out of
    // the binary check_asm.sh inspects.
    if std::env::var("ZU_ASM_ANCHOR").is_ok() {
        let mut arena = MorselArena::new();
        let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2]);
        let c = ValueVector::constant(&mut arena, PhysType::Int64, 1i64, 2);
        let mut bits = Bitmap::new_in(&mut arena, 2, false);
        zu_asm_cmp_i64_const(&v, &c, &mut bits);
        zu_asm_cmp_i64_pair(&v, &v, &mut bits);
        let dv = ValueVector::dict_str(
            &mut arena,
            &[0u16, 0],
            Arc::new(Dictionary::from_sorted(["a"].iter())),
        );
        let cs = zu_vector::const_str(&mut arena, b"a", 2);
        zu_asm_cmp_dict_const(&dv, &cs, &mut bits);
        black_box(zu_asm_sum_i64(&v));
        let f = ValueVector::flat_from(&mut arena, PhysType::Float64, &[1.0f64, 2.0]);
        black_box(zu_asm_sum_f64(&f));
        black_box(zu_asm_min_i64(&v));
        let mut out = [0u64; 2];
        black_box(zu_asm_intersect(&[1, 2], &[2, 3], &mut out));
        black_box(zu_asm_bitmap_to_sel(&mut arena, &bits).len());
        let mut hashes = [0u64; 2];
        zu_asm_hash(&[1, 2], &mut hashes);
        let mut got = [0u64; 1];
        zu_asm_gather(&[5, 6], &[1u32], &mut got);
        return;
    }

    let mut arena = MorselArena::new();
    let vals: Vec<i64> = (0..VECTOR as i64)
        .map(|i| (i * 2_654_435_761) % 100_000)
        .collect();
    let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &vals);
    let c = ValueVector::constant(&mut arena, PhysType::Int64, 50_000i64, VECTOR);

    // Predicate kernel, i64 flat vs constant. The spec target is 8 G
    // rows/s/core, roughly the load bound at 8 B/row from L1.
    let mut scratch = MorselArena::new();
    let mut bits = Bitmap::new_in(&mut scratch, VECTOR, false);
    let per_sec = measure(|| {
        kernels::compare(CmpOp::Lt, black_box(&v), black_box(&c), None, &mut bits).unwrap();
        black_box(bits.words());
    });
    let cmp_grows = per_sec * VECTOR as f64 / 1e9;
    println!("cmp_i64_const: {cmp_grows:.2} G rows/s (target 8)");
    if let Some(floor) = budgets.get("vec_cmp_grows_s")
        && cmp_grows < floor
    {
        println!("GATE FAIL vec_cmp_grows_s: {cmp_grows:.2} < floor {floor}");
        failed = true;
    }

    // Arithmetic kernel, i64 flat plus flat, with the conditions the
    // row engine raises. The kernel folds both operands into one
    // magnitude to answer whether any row could have gone over the top
    // of an integer, and the fold is the whole price of raising where
    // the row engine raises: a chunk of ordinary numbers never reaches
    // the checked walk behind it. Both numbers are printed so the price
    // is on the record rather than assumed.
    let addends: Vec<i64> = (0..VECTOR as i64)
        .map(|i| (i * 7_919) % 1_000_000)
        .collect();
    let w = ValueVector::flat_from(&mut arena, PhysType::Int64, &addends);
    let mut arith_scratch = MorselArena::new();
    let per_sec = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::binary(
                &mut arith_scratch,
                kernels::BinOp::Add,
                black_box(&v),
                black_box(&w),
                None,
            )
            .unwrap()
            .len,
        );
    });
    let arith_grows = per_sec * VECTOR as f64 / 1e9;
    println!("arith_i64_add: {arith_grows:.2} G rows/s (target 4)");
    if let Some(floor) = budgets.get("vec_arith_grows_s")
        && arith_grows < floor
    {
        println!("GATE FAIL vec_arith_grows_s: {arith_grows:.2} < floor {floor}");
        failed = true;
    }

    // The same addition over numbers wide enough that the fold cannot
    // rule out an answer that does not fit, which is what the checked
    // walk costs when it runs and finds nothing.
    let wide: Vec<i64> = (0..VECTOR as i64).map(|i| -(i64::MAX - i)).collect();
    let wv = ValueVector::flat_from(&mut arena, PhysType::Int64, &wide);
    let per_sec_wide = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::binary(
                &mut arith_scratch,
                kernels::BinOp::Add,
                black_box(&wv),
                black_box(&w),
                None,
            )
            .unwrap()
            .len,
        );
    });
    let arith_checked = per_sec_wide * VECTOR as f64 / 1e9;
    println!("arith_i64_add_checked: {arith_checked:.2} G rows/s (no target, the slow path)");

    // The numeric library, GF01, over the same width. The distance from
    // nought is the one of the five with a condition behind it on every
    // argument, so it is the one worth measuring: the fold that clears
    // a chunk is a single pass and the loop behind it is one
    // instruction per row.
    let per_sec_math = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::unary(
                &mut arith_scratch,
                kernels::MathOp::Abs,
                black_box(&v),
                None,
            )
            .unwrap()
            .len,
        );
    });
    let math_grows = per_sec_math * VECTOR as f64 / 1e9;
    println!("math_i64_abs: {math_grows:.2} G rows/s (target 4)");
    if let Some(floor) = budgets.get("vec_math_grows_s")
        && math_grows < floor
    {
        println!("GATE FAIL vec_math_grows_s: {math_grows:.2} < floor {floor}");
        failed = true;
    }

    // The approximate half of the same library, whose answer is a float
    // whatever arrived. A root costs what the hardware charges for one
    // and the fold that clears the column of negative numbers is the
    // same single pass, so the distance between this and the line above
    // is the function itself and nothing the kernel added.
    let roots: Vec<f64> = (0..VECTOR as i64).map(|i| (i % 10_000) as f64).collect();
    let rv = ValueVector::flat_from(&mut arena, PhysType::Float64, &roots);
    let per_sec_sqrt = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::unary(
                &mut arith_scratch,
                kernels::MathOp::Sqrt,
                black_box(&rv),
                None,
            )
            .unwrap()
            .len,
        );
    });
    let sqrt_grows = per_sec_sqrt * VECTOR as f64 / 1e9;
    println!("math_f64_sqrt: {sqrt_grows:.2} G rows/s (target 1)");
    if let Some(floor) = budgets.get("vec_math_real_grows_s")
        && sqrt_grows < floor
    {
        println!("GATE FAIL vec_math_real_grows_s: {sqrt_grows:.2} < floor {floor}");
        failed = true;
    }

    // The two argument half of the same library. A remainder of two
    // whole numbers is the cheap one of the three and is the one that
    // says what the kernel costs rather than what the hardware charges
    // for a power: the divisors here are all above nought, so the fold
    // clears the chunk and the loop is one instruction per row.
    let divisors: Vec<i64> = (0..VECTOR as i64).map(|i| i % 97 + 1).collect();
    let dvs = ValueVector::flat_from(&mut arena, PhysType::Int64, &divisors);
    let per_sec_mod = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::pair(
                &mut arith_scratch,
                kernels::MathPair::Mod,
                black_box(&v),
                black_box(&dvs),
                None,
            )
            .unwrap()
            .len,
        );
    });
    let mod_grows = per_sec_mod * VECTOR as f64 / 1e9;
    println!("math_i64_mod: {mod_grows:.2} G rows/s (target 1)");
    if let Some(floor) = budgets.get("vec_math_pair_grows_s")
        && mod_grows < floor
    {
        println!("GATE FAIL vec_math_pair_grows_s: {mod_grows:.2} < floor {floor}");
        failed = true;
    }

    // Dict-code equality vs the path it replaces: owned Strings compared
    // per row, which is what a Vec<Value::Str> column does today. The
    // gate is the ratio, both sides run the same logical rows.
    let entries: Vec<String> = (0..1000).map(|i| format!("category-{i:05}")).collect();
    let dict = Arc::new(Dictionary::from_sorted(entries.iter()));
    let codes: Vec<u16> = (0..VECTOR).map(|i| ((i * 7919) % 1000) as u16).collect();
    let needle = "category-00500";
    let dv = ValueVector::dict_str(&mut arena, &codes, Arc::clone(&dict));
    let cv = zu_vector::const_str(&mut arena, needle.as_bytes(), VECTOR);
    let mut dict_scratch = MorselArena::new();
    let mut dict_bits = Bitmap::new_in(&mut dict_scratch, VECTOR, false);
    let per_sec_dict = measure(|| {
        kernels::compare(
            CmpOp::Eq,
            black_box(&dv),
            black_box(&cv),
            None,
            &mut dict_bits,
        )
        .unwrap();
        black_box(dict_bits.words());
    });
    let strings: Vec<String> = codes
        .iter()
        .map(|&code| entries[code as usize].clone())
        .collect();
    let per_sec_string = measure(|| {
        black_box(string_eq_baseline(black_box(&strings), black_box(needle)));
    });
    let per_sec_today = measure(|| {
        black_box(today_string_path(
            black_box(&codes),
            black_box(&entries),
            black_box(needle),
        ));
    });
    let speedup = per_sec_dict / per_sec_string;
    let speedup_today = per_sec_dict / per_sec_today;
    println!(
        "dict_eq {:.2} G rows/s vs string_eq {:.2} G rows/s: {speedup:.1}x (target 20), vs today's per-row String path: {speedup_today:.0}x",
        per_sec_dict * VECTOR as f64 / 1e9,
        per_sec_string * VECTOR as f64 / 1e9
    );
    if let Some(floor) = budgets.get("vec_dict_eq_speedup")
        && speedup < floor
    {
        println!("GATE FAIL vec_dict_eq_speedup: {speedup:.1} < floor {floor}");
        failed = true;
    }

    // The string library's first kernel, over the same two encodings
    // the comparison above uses. A count of characters is a fold over
    // the bytes of each row when the column is flat, and a fold over
    // the table plus a gather when it is coded, so the distance
    // between these two lines is the reason the kernel reads codes
    // rather than asking the caller to materialize them.
    let flat = zu_vector::str_vector(&mut arena, &strings);
    let per_sec_len = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::length(&mut arith_scratch, kernels::StrLen::Chars, black_box(&flat))
                .unwrap()
                .len,
        );
    });
    let len_grows = per_sec_len * VECTOR as f64 / 1e9;
    println!(
        "str_char_length: {len_grows:.2} G rows/s over fourteen byte strings (no spec target)"
    );
    if let Some(floor) = budgets.get("vec_str_length_grows_s")
        && len_grows < floor
    {
        println!("GATE FAIL vec_str_length_grows_s: {len_grows:.2} < floor {floor}");
        failed = true;
    }
    let per_sec_len_dict = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::length(&mut arith_scratch, kernels::StrLen::Chars, black_box(&dv))
                .unwrap()
                .len,
        );
    });
    println!(
        "str_char_length_dict: {:.2} G rows/s ({:.1}x the flat column, no target)",
        per_sec_len_dict * VECTOR as f64 / 1e9,
        per_sec_len_dict / per_sec_len
    );

    // The fold, which is the first kernel whose answer is a string.
    // The two lines are the same two encodings again, and the reason
    // the coded one is ahead is stronger here than for a count: an
    // entry is folded once and its bytes are written once however many
    // rows point at them, so the rows themselves cost a gather of
    // views rather than a copy of a string.
    let per_sec_fold = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::fold(
                &mut arith_scratch,
                kernels::StrFold::Upper,
                black_box(&flat),
            )
            .unwrap()
            .len,
        );
    });
    let fold_grows = per_sec_fold * VECTOR as f64 / 1e9;
    println!("str_upper: {fold_grows:.2} G rows/s over fourteen byte strings (no spec target)");
    if let Some(floor) = budgets.get("vec_str_fold_grows_s")
        && fold_grows < floor
    {
        println!("GATE FAIL vec_str_fold_grows_s: {fold_grows:.2} < floor {floor}");
        failed = true;
    }
    let per_sec_fold_dict = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::fold(&mut arith_scratch, kernels::StrFold::Upper, black_box(&dv))
                .unwrap()
                .len,
        );
    });
    println!(
        "str_upper_dict: {:.2} G rows/s ({:.1}x the flat column, no target)",
        per_sec_fold_dict * VECTOR as f64 / 1e9,
        per_sec_fold_dict / per_sec_fold
    );

    // The trim, which is the string kernel that fills no buffer: the
    // set is the digits, so every entry here loses its number and what
    // is left is a part of the entry that the answer points back at.
    // The ratio against the fold is printed because the two walk the
    // same column, and it is worth reading for what it does not say.
    // The trim comes out about level with the fold rather than well
    // ahead of it, because taking six characters off a string is six
    // tests of the set, which costs about what copying fifteen bytes
    // costs. What the trim saves is the buffer, not the walk.
    let digits = kernels::TrimSet::new("0123456789");
    let per_sec_trim = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::trim(
                &mut arith_scratch,
                kernels::StrTrim::Both,
                black_box(&digits),
                black_box(&flat),
            )
            .unwrap()
            .len,
        );
    });
    let trim_grows = per_sec_trim * VECTOR as f64 / 1e9;
    println!(
        "str_trim: {trim_grows:.2} G rows/s over fifteen byte strings ({:.1}x the fold of the same column, no spec target)",
        per_sec_trim / per_sec_fold
    );
    if let Some(floor) = budgets.get("vec_str_trim_grows_s")
        && trim_grows < floor
    {
        println!("GATE FAIL vec_str_trim_grows_s: {trim_grows:.2} < floor {floor}");
        failed = true;
    }
    let per_sec_trim_dict = measure(|| {
        arith_scratch.reset();
        black_box(
            kernels::trim(
                &mut arith_scratch,
                kernels::StrTrim::Both,
                black_box(&digits),
                black_box(&dv),
            )
            .unwrap()
            .len,
        );
    });
    println!(
        "str_trim_dict: {:.2} G rows/s ({:.1}x the flat column, no target)",
        per_sec_trim_dict * VECTOR as f64 / 1e9,
        per_sec_trim_dict / per_sec_trim
    );

    // Sorted intersection, balanced inputs: the multiway join inner
    // loop. Throughput counts every element the merge consumes.
    let a: Vec<u64> = (0..4096u64).map(|i| i * 3).collect();
    let b: Vec<u64> = (0..4096u64).map(|i| i * 5).collect();
    let mut inter_out = vec![0u64; 4096];
    let per_sec = measure(|| {
        black_box(kernels::intersect_sorted(
            black_box(&a),
            black_box(&b),
            &mut inter_out,
        ));
    });
    let inter_gelems = per_sec * (a.len() + b.len()) as f64 / 1e9;
    println!("intersect_balanced: {inter_gelems:.2} G elems/s (target 2)");
    if let Some(floor) = budgets.get("vec_intersect_gelem_s")
        && inter_gelems < floor
    {
        println!("GATE FAIL vec_intersect_gelem_s: {inter_gelems:.2} < floor {floor}");
        failed = true;
    }

    // Skewed 64x: the galloping path skips most of the long side, so
    // effective elems/s should sit far above the balanced number.
    let short: Vec<u64> = (0..128u64).map(|i| i * 191).collect();
    let long: Vec<u64> = (0..8192u64).map(|i| i * 3).collect();
    let mut skew_out = vec![0u64; 128];
    let per_sec = measure(|| {
        black_box(kernels::intersect_sorted(
            black_box(&short),
            black_box(&long),
            &mut skew_out,
        ));
    });
    println!(
        "intersect_skewed_64x: {:.2} G elems/s",
        per_sec * (short.len() + long.len()) as f64 / 1e9
    );

    // Bitmap to selection at half density, the worst case for the
    // trailing_zeros loop: every word round-trips 32 pushes. The perf/11
    // budget is under 100 ns per 2048-bit vector.
    let mut half = Bitmap::new_in(&mut arena, VECTOR, false);
    for i in (0..VECTOR).step_by(2) {
        half.set(i);
    }
    let mut sel_scratch = MorselArena::new();
    let per_sec = measure(|| {
        sel_scratch.reset();
        let s = SelVector::from_bitmap(&mut sel_scratch, black_box(&half));
        black_box(s.len());
    });
    let sel_ns = 1e9 / per_sec;
    println!("bitmap_to_sel_half: {sel_ns:.0} ns per vector (budget 100)");
    if let Some(ceiling) = budgets.get("vec_bitmap_sel_ns")
        && sel_ns > ceiling
    {
        println!("GATE FAIL vec_bitmap_sel_ns: {sel_ns:.0} ns > ceiling {ceiling}");
        failed = true;
    }

    // Key hashing: the splitmix64 finalizer over a vector of keys, the
    // front half of every hash operator build and probe.
    let keys: Vec<u64> = (0..VECTOR as u64).collect();
    let mut hashes = vec![0u64; VECTOR];
    let per_sec = measure(|| {
        kernels::hash_slice(black_box(&keys), &mut hashes);
        black_box(hashes[VECTOR - 1]);
    });
    let hash_gkeys = per_sec * VECTOR as f64 / 1e9;
    println!("hash_u64: {hash_gkeys:.2} G keys/s (target 4)");
    if let Some(floor) = budgets.get("vec_hash_gkeys_s")
        && hash_gkeys < floor
    {
        println!("GATE FAIL vec_hash_gkeys_s: {hash_gkeys:.2} < floor {floor}");
        failed = true;
    }

    // Scan+sum over a flat buffer far bigger than cache: the kernel half
    // of the B3 gate, bounded by memory bandwidth.
    let big: Vec<i64> = (0..(16i64 << 20)).collect();
    let mut big_arena = MorselArena::new();
    let bigv = ValueVector::flat_from(&mut big_arena, PhysType::Int64, &big);
    let per_sec = measure(|| {
        black_box(kernels::sum_i64(black_box(&bigv), None));
    });
    let sum_gbs = per_sec * (big.len() * 8) as f64 / 1e9;
    println!("sum_i64_128MiB: {sum_gbs:.2} GB/s (B3 target 2)");
    if let Some(floor) = budgets.get("vec_sum_gbs")
        && sum_gbs < floor
    {
        println!("GATE FAIL vec_sum_gbs: {sum_gbs:.2} < floor {floor}");
        failed = true;
    }

    // Bit-unpack throughput out of the storage layout, the decode half
    // of every scan. Width 13 is what the LDBC adjacency chunks pack to.
    let raw: Vec<u64> = (0..1024u64).map(|i| (i * 2_654_435_761) & 0x1FFF).collect();
    let mut packed = Vec::new();
    zu_encoding::bitpack::pack(&raw, 13, &mut packed);
    let mut unpacked = vec![0u64; 1024];
    let per_sec = measure(|| {
        zu_encoding::bitpack::unpack(black_box(&packed), 13, &mut unpacked);
        black_box(unpacked[1023]);
    });
    let unpack_gvals = per_sec * 1024.0 / 1e9;
    println!("unpack_w13: {unpack_gvals:.2} G vals/s (target 4)");
    if let Some(floor) = budgets.get("vec_unpack_gvals_s")
        && unpack_gvals < floor
    {
        println!("GATE FAIL vec_unpack_gvals_s: {unpack_gvals:.2} < floor {floor}");
        failed = true;
    }

    // Gather through random u32 indices out of a 1 MiB source, the late
    // materialization primitive. Cache-resident source, random order.
    let src: Vec<u64> = (0..131_072u64).collect();
    let idx: Vec<u32> = (0..VECTOR)
        .map(|i| ((i * 40_503) % 131_072) as u32)
        .collect();
    let mut gathered = vec![0u64; VECTOR];
    let per_sec = measure(|| {
        kernels::gather_u64(black_box(&src), black_box(&idx), &mut gathered);
        black_box(gathered[VECTOR - 1]);
    });
    println!(
        "gather_u64_1MiB: {:.2} G rows/s",
        per_sec * VECTOR as f64 / 1e9
    );

    // Arena round trip: reset plus one vector allocation, the fixed
    // overhead every morsel pays. Not gated, printed for the record.
    let mut cycle = MorselArena::new();
    let per_sec = measure(|| {
        cycle.reset();
        let buf = cycle.alloc_of::<u64>(VECTOR);
        black_box(buf.len());
    });
    println!("arena_reset_alloc: {:.0} ns", 1e9 / per_sec);

    if gate {
        if failed {
            std::process::exit(1);
        }
        println!("gate: all vector floors met");
    }
}
