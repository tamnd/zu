//! Sorted-set intersection, the worst-case-optimal join inner loop
//! (perf/11). Neighbor lists come out of the CSR sorted, so multiway
//! intersection is repeated pairwise intersection of sorted u64 runs.
//!
//! Balanced inputs take the branchy merge, which the branch predictor
//! handles well on real adjacency data. Once one side is 32x longer the
//! merge wastes its time stepping the long side, so the short side
//! drives and the long side is galloped: doubling probe from the cursor,
//! then a binary search inside the last doubling window.

/// Length ratio at which galloping beats the merge. Measured, not
/// sacred; the crossover is flat between 16 and 64.
const GALLOP_RATIO: usize = 32;

/// Below this short-side length the split's partition search and the
/// tail compaction cost more than the overlap buys.
const SPLIT_MIN: usize = 64;

/// Intersect two sorted, duplicate-free slices into `out`, returning the
/// element count. `out` must hold at least `min(a.len(), b.len())`.
pub fn intersect_sorted(a: &[u64], b: &[u64], out: &mut [u64]) -> usize {
    // Keep the short side in `a` so the gallop test is one direction.
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if a.is_empty() {
        return 0;
    }
    if b.len() / a.len() >= GALLOP_RATIO {
        return gallop_intersect(a, b, out);
    }
    if a.len() >= 4 * SPLIT_MIN {
        return split_merge::<4>(a, b, out);
    }
    if a.len() >= SPLIT_MIN {
        return split_merge::<2>(a, b, out);
    }
    merge(a, b, out)
}

/// The plain branch-free merge. Its throughput ceiling is the
/// loop-carried load-compare-add chain on the cursors, about six cycles
/// per step, which is why `split_merge` exists.
fn merge(a: &[u64], b: &[u64], out: &mut [u64]) -> usize {
    let mut n = 0;
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out[n] = x;
        n += usize::from(x == y);
        i += usize::from(x <= y);
        j += usize::from(y <= x);
    }
    n
}

/// Cut the short side into N equal segments, partition the long side at
/// the segment pivots, and run the N independent merges interleaved in
/// one loop. The cursor chains of the segments carry no dependence on
/// each other, so they overlap in the out-of-order window and the merge
/// scales close to N-fold until the execution ports saturate.
fn split_merge<const N: usize>(a: &[u64], b: &[u64], out: &mut [u64]) -> usize {
    // Segment k of a is [ca[k], ca[k+1]); its partner in b holds exactly
    // the values in that pivot range, so segments cannot match across
    // the cuts (sorted, duplicate-free inputs).
    let mut ca = [0usize; N];
    let mut cb = [0usize; N];
    let mut ca_end = [0usize; N];
    let mut cb_end = [0usize; N];
    for k in 0..N {
        ca[k] = a.len() * k / N;
        ca_end[k] = a.len() * (k + 1) / N;
        cb[k] = if k == 0 {
            0
        } else {
            b.partition_point(|&y| y < a[ca[k]])
        };
    }
    cb_end[..N - 1].copy_from_slice(&cb[1..]);
    cb_end[N - 1] = b.len();
    // Segment k writes into out at [off[k], off[k] + its match count).
    // Match count is bounded by min of the segment lengths, and those
    // bounds sum to at most min(|a|, |b|) <= out.len(), so the segments
    // never collide; the gaps compact away at the end.
    let mut off = [0usize; N];
    for k in 1..N {
        off[k] = off[k - 1] + (ca_end[k - 1] - ca[k - 1]).min(cb_end[k - 1] - cb[k - 1]);
    }
    let mut i = ca;
    let mut j = cb;
    let mut n = off;
    loop {
        let mut all_active = true;
        for k in 0..N {
            all_active &= i[k] < ca_end[k] && j[k] < cb_end[k];
        }
        if !all_active {
            break;
        }
        for k in 0..N {
            let (x, y) = (a[i[k]], b[j[k]]);
            out[n[k]] = x;
            n[k] += usize::from(x == y);
            i[k] += usize::from(x <= y);
            j[k] += usize::from(y <= x);
        }
    }
    for k in 0..N {
        let cap = if k == N - 1 { out.len() } else { off[k + 1] };
        n[k] += merge(
            &a[i[k]..ca_end[k]],
            &b[j[k]..cb_end[k]],
            &mut out[n[k]..cap],
        );
    }
    // Compact the segment outputs together, front to back.
    let mut total = n[0];
    for k in 1..N {
        let found = n[k] - off[k];
        out.copy_within(off[k]..n[k], total);
        total += found;
    }
    total
}

fn gallop_intersect(short: &[u64], long: &[u64], out: &mut [u64]) -> usize {
    let mut n = 0;
    let mut lo = 0usize;
    for &x in short {
        // Double until the window covers x, then binary search inside.
        let mut step = 1usize;
        while lo + step < long.len() && long[lo + step] < x {
            step *= 2;
        }
        let hi = (lo + step + 1).min(long.len());
        match long[lo..hi].binary_search(&x) {
            Ok(off) => {
                out[n] = x;
                n += 1;
                lo += off + 1;
            }
            Err(off) => lo += off,
        }
        if lo >= long.len() {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(a: &[u64], b: &[u64]) -> Vec<u64> {
        a.iter().filter(|x| b.contains(x)).copied().collect()
    }

    #[test]
    fn merge_path() {
        let a = [1u64, 3, 5, 7, 9, 11];
        let b = [2u64, 3, 4, 7, 8, 11, 20];
        let mut out = [0u64; 6];
        let n = intersect_sorted(&a, &b, &mut out);
        assert_eq!(&out[..n], &[3, 7, 11]);
    }

    #[test]
    fn gallop_path_matches_naive() {
        // Long side 64x the short side forces the gallop.
        let long: Vec<u64> = (0..6400).map(|i| i * 3).collect();
        let short: Vec<u64> = (0..100).map(|i| i * 191).collect();
        let mut out = vec![0u64; short.len()];
        let n = intersect_sorted(&short, &long, &mut out);
        assert_eq!(&out[..n], naive(&short, &long).as_slice());
        // Argument order must not matter.
        let mut out2 = vec![0u64; short.len()];
        let n2 = intersect_sorted(&long, &short, &mut out2);
        assert_eq!(&out[..n], &out2[..n2]);
    }

    #[test]
    fn empty_and_disjoint() {
        let mut out = [0u64; 4];
        assert_eq!(intersect_sorted(&[], &[1, 2], &mut out), 0);
        assert_eq!(intersect_sorted(&[3, 4], &[1, 2], &mut out), 0);
    }
}
