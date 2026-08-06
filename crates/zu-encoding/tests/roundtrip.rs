//! Roundtrip property tests over generated distributions.
//!
//! Each iteration draws a distribution shape and its parameters from a
//! seeded xorshift generator, builds a column, and checks three
//! properties across every encoder: encode then decode reproduces the
//! input exactly, the auto cascade does the same through its id byte,
//! and every decoder rejects a caller ceiling one below the true count
//! instead of overrunning it. Under miri the sweep shrinks so the
//! interpreter finishes in minutes while still crossing every shape.

use zu_encoding::{bool_bitpack, delta, delta_patch, dict, for_bitpack, frequency, rle, segment};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// One column of the given shape: the distributions the cascade prices
/// against each other, plus hostile mixtures none of them likes. The
/// caller walks shapes round robin so a short sweep still crosses all
/// of them.
fn generate(rng: &mut Rng, max_len: usize, shape: u64) -> Vec<u64> {
    let len = rng.below(max_len as u64) as usize;
    let mut values = Vec::with_capacity(len);
    match shape {
        // Sorted with small gaps, the CSR neighbor list shape.
        0 => {
            let mut v = rng.below(1 << 40);
            for _ in 0..len {
                v += rng.below(64);
                values.push(v);
            }
        }
        // Clustered noise in a narrow band far from zero.
        1 => {
            let base = rng.below(1 << 50);
            let width = rng.below(1 << 12) + 1;
            for _ in 0..len {
                values.push(base + rng.below(width));
            }
        }
        // Long runs.
        2 => {
            let mut v = rng.below(1 << 30);
            let mut remaining = len;
            while remaining > 0 {
                let run = (rng.below(200) as usize + 1).min(remaining);
                values.extend(std::iter::repeat_n(v, run));
                v = rng.below(1 << 30);
                remaining -= run;
            }
        }
        // Low cardinality, scattered.
        3 => {
            let pool: Vec<u64> = (0..rng.below(30) + 1).map(|_| rng.next()).collect();
            for _ in 0..len {
                values.push(pool[rng.below(pool.len() as u64) as usize]);
            }
        }
        // One dominant value with wide exceptions.
        4 => {
            let top = rng.next();
            for _ in 0..len {
                values.push(if rng.below(10) == 0 { rng.next() } else { top });
            }
        }
        // Binary.
        5 => {
            for _ in 0..len {
                values.push(rng.below(2));
            }
        }
        // Full-width randomness.
        6 => {
            for _ in 0..len {
                values.push(rng.next());
            }
        }
        // Adjacency: concatenated sorted lists with wide restarts.
        7 => {
            let mut remaining = len;
            while remaining > 0 {
                let list = (rng.below(40) as usize + 1).min(remaining);
                let mut v = rng.below(1 << 44);
                for _ in 0..list {
                    v += rng.below(32) + 1;
                    values.push(v);
                }
                remaining -= list;
            }
        }
        // Sorted-with-outliers: mostly tight gaps, rare huge jumps.
        _ => {
            let mut v = 0u64;
            for _ in 0..len {
                v += if rng.below(50) == 0 {
                    rng.below(1 << 45)
                } else {
                    rng.below(16)
                };
                values.push(v);
            }
        }
    }
    values
}

type EncodeFn = fn(&[u64], &mut Vec<u8>) -> usize;
type DecodeFn = fn(&[u8], usize, &mut Vec<u64>) -> zu_common::Result<()>;

#[test]
fn every_encoder_roundtrips_every_distribution() {
    let (iterations, max_len) = if cfg!(miri) { (9, 200) } else { (400, 4100) };
    let mut rng = Rng(0x2545F4914F6CDD1D);
    let pairs: [(&str, EncodeFn, DecodeFn); 6] = [
        ("for_bitpack", for_bitpack::encode, for_bitpack::decode),
        ("delta", delta::encode, delta::decode),
        ("delta_patch", delta_patch::encode, delta_patch::decode),
        ("rle", rle::encode, rle::decode),
        ("dict", dict::encode, dict::decode),
        ("frequency", frequency::encode, frequency::decode),
    ];
    for iter in 0..iterations {
        let values = generate(&mut rng, max_len, (iter % 9) as u64);
        for (name, encode, decode) in pairs {
            let mut buf = Vec::new();
            encode(&values, &mut buf);
            let mut out = Vec::new();
            decode(&buf, values.len(), &mut out)
                .unwrap_or_else(|e| panic!("iter {iter}: {name} rejected its own output: {e}"));
            assert_eq!(values, out, "iter {iter}: {name} roundtrip mismatch");
            if !values.is_empty() {
                let mut short = Vec::new();
                assert!(
                    decode(&buf, values.len() - 1, &mut short).is_err(),
                    "iter {iter}: {name} accepted a ceiling below its count"
                );
            }
        }
        if values.iter().all(|&v| v <= 1) {
            let mut buf = Vec::new();
            bool_bitpack::encode(&values, &mut buf);
            let mut out = Vec::new();
            bool_bitpack::decode(&buf, values.len(), &mut out).unwrap();
            assert_eq!(values, out, "iter {iter}: bool_bitpack roundtrip mismatch");
        }
        let mut buf = Vec::new();
        let id = segment::encode_auto(&values, &mut buf);
        let mut out = Vec::new();
        segment::decode_any(&buf, values.len(), &mut out)
            .unwrap_or_else(|e| panic!("iter {iter}: cascade {id:?} rejected its output: {e}"));
        assert_eq!(
            values, out,
            "iter {iter}: cascade {id:?} roundtrip mismatch"
        );
        if !values.is_empty() {
            let mut short = Vec::new();
            assert!(
                segment::decode_any(&buf, values.len() - 1, &mut short).is_err(),
                "iter {iter}: cascade {id:?} accepted a ceiling below its count"
            );
        }
    }
}
