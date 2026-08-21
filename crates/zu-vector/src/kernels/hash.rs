//! Key hashing for joins and grouping. One multiply-xor finalizer per
//! key (the splitmix64 tail): two multiplies and three shifts, no data
//! dependence between rows, so the slice form streams at memory speed.

/// Mix a 64-bit key. Full-avalanche: every input bit flips ~half the
/// output bits, which is what a radix-partitioned join table needs from
/// both ends of the hash.
#[inline]
pub fn hash64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Hash a slice of keys into `out`. Unrolled by four so the multiply
/// chains of neighboring keys overlap instead of waiting on the loop
/// bookkeeping.
pub fn hash_slice(keys: &[u64], out: &mut [u64]) {
    debug_assert_eq!(keys.len(), out.len());
    // Fours as arrays rather than as slices, which is the same unroll
    // with the length in the type, so the four stores need no bounds
    // check to prove.
    let (fours, tail) = keys.as_chunks::<4>();
    let (outs, out_tail) = out.as_chunks_mut::<4>();
    for (k4, o4) in fours.iter().zip(outs) {
        o4[0] = hash64(k4[0]);
        o4[1] = hash64(k4[1]);
        o4[2] = hash64(k4[2]);
        o4[3] = hash64(k4[3]);
    }
    for (o, &k) in out_tail.iter_mut().zip(tail) {
        *o = hash64(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_and_deterministic() {
        let keys: Vec<u64> = (0..1000).collect();
        let mut out = vec![0u64; 1000];
        hash_slice(&keys, &mut out);
        let mut seen = out.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 1000, "no collisions on a small dense domain");
        assert_eq!(hash64(42), hash64(42));
        assert_ne!(hash64(42), hash64(43));
    }

    #[test]
    fn low_bits_avalanche() {
        // Sequential keys must not produce sequential low bits, or a
        // power-of-two partition count degenerates.
        let buckets: std::collections::BTreeSet<u64> = (0..64u64).map(|k| hash64(k) & 7).collect();
        assert!(buckets.len() > 4);
    }
}
