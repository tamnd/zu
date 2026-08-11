//! The join table behind hash joins (perf/05 section 2).
//!
//! The old engine's join is a membership `HashSet<(u64, u64)>` that every
//! worker sweeps the whole rel table to fill, so the build is duplicated
//! per worker and carries no payload at all. This is the replacement, an
//! unchained table in the shape Birler et al. describe (DaMoN 2024): the
//! rows are laid out contiguously in bucket order, so a bucket's rows are
//! a dense range of one packed buffer rather than a pointer list, and the
//! directory holds one word per bucket with a 16-bit Bloom tag folded
//! into the pointer's unused high bits.
//!
//! That folding is the point. A probe reads one directory word, and a key
//! with no match usually dies on the tag inside that same load, so a miss
//! costs one cache line and nothing else. A hit walks a contiguous range
//! rather than chasing a pointer chain.
//!
//! The build is a counting sort, two passes over the keys with a
//! histogram in between, and it happens once. The table is meant to be
//! wrapped in an `Arc` and shared by every probe worker.
//!
//! Probing is a vector at a time for the same reason the group table
//! probes a vector at a time: the directory load is a dependent random
//! access, so the loop warms the line several rows ahead and keeps that
//! many loads in flight instead of one.
//!
//! What the bench actually says, so the next person does not have to
//! rediscover it. The baseline is the standard library on the same keys,
//! which is not a strawman: it is the shape `exec.rs` uses today.
//!
//! Misses run 2.0 to 3.5x a `HashSet` probe across the fleet. That is
//! the antijoin and the mark join, and it is where the folded tag earns
//! its bits, since a miss reads the directory word and stops. Hits on
//! distinct keys run 1.1 to 2.1x. Both of those hold on every host, and
//! the margin is widest on the slow ones, which is the right direction.
//!
//! Two things are behind. The duplicate-heavy inner join, which is the
//! shape perf/05 quotes the largest number on, runs at a third of a
//! `HashMap<u64, Vec<u64>>` everywhere. That layout checks a key once
//! per probe and then reads a compact payload run, while this one
//! carries the key beside every build row and re-checks it per row to
//! find where the run ends, so it moves twice the memory. Storing a run
//! length per distinct key rather than repeating the key is the obvious
//! answer and is not written. The build is behind on the fast hosts,
//! 0.3x locally and 0.7x on gamingpc, and ahead on the slow ones, 1.8
//! to 2.6x on server1: the histogram and the scatter are random access
//! over a directory the size of the build side, so this is bandwidth
//! against the standard library's slower per-key hash. The
//! write-combining radix pass perf/05 section 2 calls for is the named
//! fix for that half and is not written either.
//!
//! None of it is load bearing yet. Nothing compiles to a join, so this
//! table is reachable only from its own tests and its own bench.

use zu_vector::kernels::{hash_slice, hash64};

/// Rows per bucket the table is sized for. One keeps the average
/// bucket inside a single cache line, which is what makes the
/// contiguous scan cheaper than a chain walk.
const ROWS_PER_BUCKET: usize = 1;

/// How far ahead the probe loop warms directory lines. Eight is what
/// the group table settled on for the same dependent-load problem.
const PREFETCH: usize = 8;

/// Bits of a directory word given to the offset. The rest is the tag.
const PTR_BITS: u32 = 48;
const PTR_MASK: u64 = (1 << PTR_BITS) - 1;

/// The two Bloom bits a hash claims in its bucket's 16-bit tag. Two
/// bits rather than one: at about one row per bucket a single bit
/// leaves a 1 in 16 false positive rate, and the second bit takes that
/// to roughly 1 in 64 for the price of one more shift.
fn tag_bits(h: u64) -> u64 {
    (1 << ((h >> 48) & 15)) | (1 << ((h >> 52) & 15))
}

/// A build-once probe-many hash table over `u64` keys.
///
/// Keys need not be distinct. Duplicates of the same key land in the
/// same bucket and therefore next to each other in `keys`, so an inner
/// join over a hot key emits its matches straight down a run of the
/// buffer.
pub struct JoinTable {
    /// One word per bucket plus a sentinel: `tag:16 | offset:48`. The
    /// offset is where the bucket's rows start in `keys`, and the next
    /// entry's offset is where they end, which is why the sentinel is
    /// there. The sentinel's tag is zero and is never read.
    dir: Vec<u64>,
    /// The build keys in bucket order.
    keys: Vec<u64>,
    /// The build payloads, in the same order as `keys`.
    payload: Vec<u64>,
    /// `buckets - 1`, the mask that turns a hash into a bucket.
    mask: u64,
}

impl JoinTable {
    /// Build over `keys`, carrying `payload[i]` for `keys[i]`.
    ///
    /// # Panics
    /// If the two slices are different lengths.
    pub fn build(keys: &[u64], payload: &[u64]) -> Self {
        assert_eq!(keys.len(), payload.len(), "a payload per key");
        let n = keys.len();
        if n == 0 {
            // One empty bucket and its sentinel, so probe needs no
            // special case for a build side that produced nothing.
            return Self {
                dir: vec![0, 0],
                keys: Vec::new(),
                payload: Vec::new(),
                mask: 0,
            };
        }
        let buckets = (n / ROWS_PER_BUCKET).max(1).next_power_of_two();
        let mask = buckets as u64 - 1;

        let mut hashes = vec![0u64; n];
        hash_slice(keys, &mut hashes);

        // Pass one, count the rows per bucket. `dir` holds the running
        // counts first, then the offsets, then the packed words, so the
        // build allocates it once.
        let mut dir = vec![0u64; buckets + 1];
        for &h in &hashes {
            dir[(h & mask) as usize] += 1;
        }
        let mut acc = 0u64;
        for slot in &mut dir {
            let count = *slot;
            *slot = acc;
            acc += count;
        }
        // The sentinel counted nothing, so it took the running total
        // and left it alone, which is exactly the end of the last
        // bucket's range.
        debug_assert_eq!(acc, n as u64);

        // Pass two, scatter into bucket order and collect the tags. The
        // cursor walks a copy of the offsets so `dir` keeps the starts.
        let mut cursor: Vec<u64> = dir[..buckets].to_vec();
        let mut tags = vec![0u64; buckets];
        let mut sorted_keys = vec![0u64; n];
        let mut sorted_payload = vec![0u64; n];
        for i in 0..n {
            let h = hashes[i];
            let b = (h & mask) as usize;
            let at = cursor[b] as usize;
            cursor[b] += 1;
            sorted_keys[at] = keys[i];
            sorted_payload[at] = payload[i];
            tags[b] |= tag_bits(h);
        }

        // The scatter kept build order inside a bucket, so two distinct
        // keys that hashed to the same bucket are interleaved there, and
        // a lookup walking one contiguous run would stop at the first
        // foreign key and miss the rest of its own. Order each bucket by
        // key so that a key's rows are one range again. Buckets hold
        // about a row each and a bucket that is already ordered, which
        // is nearly all of them and every bucket holding one key however
        // many times, costs the scan and nothing more.
        let mut scratch: Vec<(u64, u64)> = Vec::new();
        for b in 0..buckets {
            let start = dir[b] as usize;
            let end = dir[b + 1] as usize;
            if end - start < 2 || sorted_keys[start..end].is_sorted() {
                continue;
            }
            scratch.clear();
            scratch.extend(
                sorted_keys[start..end]
                    .iter()
                    .copied()
                    .zip(sorted_payload[start..end].iter().copied()),
            );
            // Stable, so a key's payloads stay in build order.
            scratch.sort_by_key(|(k, _)| *k);
            for (i, (k, p)) in scratch.iter().enumerate() {
                sorted_keys[start + i] = *k;
                sorted_payload[start + i] = *p;
            }
        }

        // Fold the tags in. The sentinel keeps its bare offset, which is
        // `n`, because the last bucket's range has to end somewhere.
        for (b, tag) in tags.into_iter().enumerate() {
            dir[b] |= tag << PTR_BITS;
        }
        debug_assert_eq!(dir[buckets], n as u64);

        Self {
            dir,
            keys: sorted_keys,
            payload: sorted_payload,
            mask,
        }
    }

    /// Rows on the build side.
    pub fn rows(&self) -> usize {
        self.keys.len()
    }

    /// The payloads `key` matched, empty when it matched nothing.
    ///
    /// The common miss returns here off the tag alone, having read one
    /// word of `dir` and nothing else.
    pub fn lookup(&self, key: u64) -> &[u64] {
        let h = hash64(key);
        let b = (h & self.mask) as usize;
        let entry = self.dir[b];
        let want = tag_bits(h);
        if entry >> PTR_BITS & want != want {
            return &[];
        }
        let start = (entry & PTR_MASK) as usize;
        let end = (self.dir[b + 1] & PTR_MASK) as usize;
        // A bucket holds about one row, so this is a short forward walk
        // over keys already in the line the directory word pulled in.
        let mut lo = start;
        while lo < end && self.keys[lo] != key {
            lo += 1;
        }
        if lo == end {
            return &[];
        }
        let mut hi = lo + 1;
        while hi < end && self.keys[hi] == key {
            hi += 1;
        }
        &self.payload[lo..hi]
    }

    /// Whether `key` is on the build side. This is the semijoin and
    /// antijoin question, and unlike [`lookup`](Self::lookup) it stops
    /// at the first equal key rather than measuring the run.
    pub fn contains(&self, key: u64) -> bool {
        let h = hash64(key);
        let b = (h & self.mask) as usize;
        let entry = self.dir[b];
        let want = tag_bits(h);
        if entry >> PTR_BITS & want != want {
            return false;
        }
        let start = (entry & PTR_MASK) as usize;
        let end = (self.dir[b + 1] & PTR_MASK) as usize;
        self.keys[start..end].contains(&key)
    }

    /// Mark each of `keys` with whether it is on the build side, a
    /// vector at a time.
    ///
    /// This is the mark join, which is what `WHERE EXISTS` and a
    /// pattern predicate want: the probe side keeps every row and gains
    /// a boolean, so nothing is flattened and nothing is dropped. A
    /// semijoin is this followed by a retain, and an antijoin is this
    /// followed by the opposite retain.
    ///
    /// # Panics
    /// If `out` is not as long as `keys`.
    pub fn mark(&self, keys: &[u64], out: &mut [bool]) {
        assert_eq!(keys.len(), out.len(), "a mark per key");
        let n = keys.len();
        for i in 0..n {
            // Warm the directory line for a row several ahead, so that
            // many dependent loads are in flight at once. The hash is
            // cheap enough to pay for twice.
            if i + PREFETCH < n {
                let b = (hash64(keys[i + PREFETCH]) & self.mask) as usize;
                prefetch(&self.dir[b]);
            }
            out[i] = self.contains(keys[i]);
        }
    }

    /// Emit the inner join of `keys` against the build side.
    ///
    /// For every probe row that matched, `emit` is called once per
    /// matching build row with the probe row's index into `keys` and
    /// the build row's payload. A probe row that matched nothing is not
    /// mentioned.
    pub fn join(&self, keys: &[u64], mut emit: impl FnMut(usize, u64)) {
        let n = keys.len();
        for i in 0..n {
            if i + PREFETCH < n {
                let b = (hash64(keys[i + PREFETCH]) & self.mask) as usize;
                prefetch(&self.dir[b]);
            }
            for &row in self.lookup(keys[i]) {
                emit(i, row);
            }
        }
    }
}

/// Pull a word's cache line toward the core without stalling on it.
/// On anything else this is a no-op and the loop just runs one load at
/// a time, which is correct, only slower.
#[inline(always)]
fn prefetch(at: &u64) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the pointer comes from a live reference, and the
    // instruction only warms a cache line. It cannot fault and it
    // cannot be observed.
    unsafe {
        std::arch::x86_64::_mm_prefetch(
            std::ptr::from_ref(at).cast::<i8>(),
            std::arch::x86_64::_MM_HINT_T0,
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above, prfm only warms a line.
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) std::ptr::from_ref(at), options(nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = at;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference the table is checked against: a plain scan of the
    /// build side, which is slow and obviously right.
    fn reference(keys: &[u64], payload: &[u64], key: u64) -> Vec<u64> {
        keys.iter()
            .zip(payload)
            .filter(|(k, _)| **k == key)
            .map(|(_, p)| *p)
            .collect()
    }

    #[test]
    fn empty_build_side_matches_nothing() {
        let t = JoinTable::build(&[], &[]);
        assert_eq!(t.rows(), 0);
        assert!(t.lookup(7).is_empty());
        assert!(!t.contains(7));
        let mut marks = [true; 3];
        t.mark(&[1, 2, 3], &mut marks);
        assert_eq!(marks, [false; 3]);
    }

    #[test]
    fn distinct_keys_find_their_own_payload() {
        let keys: Vec<u64> = (0..1000).map(|i| i * 7 + 3).collect();
        let payload: Vec<u64> = (0..1000).collect();
        let t = JoinTable::build(&keys, &payload);
        assert_eq!(t.rows(), 1000);
        for (i, &k) in keys.iter().enumerate() {
            assert_eq!(t.lookup(k), &[i as u64], "key {k}");
            assert!(t.contains(k));
        }
    }

    #[test]
    fn absent_keys_miss() {
        let keys: Vec<u64> = (0..1000).map(|i| i * 7 + 3).collect();
        let payload: Vec<u64> = (0..1000).collect();
        let t = JoinTable::build(&keys, &payload);
        // Nothing here is 3 mod 7, so none of it was built.
        for k in (0..1000u64).map(|i| i * 7) {
            assert!(t.lookup(k).is_empty(), "key {k}");
            assert!(!t.contains(k));
        }
    }

    #[test]
    fn duplicate_keys_come_back_together() {
        // Forty rows a key is the duplicate-heavy shape a graph join
        // produces, and the run has to come back whole and in build
        // order.
        let mut keys = Vec::new();
        let mut payload = Vec::new();
        for k in 0..50u64 {
            for r in 0..40u64 {
                keys.push(k);
                payload.push(k * 100 + r);
            }
        }
        let t = JoinTable::build(&keys, &payload);
        for k in 0..50u64 {
            assert_eq!(t.lookup(k), reference(&keys, &payload, k), "key {k}");
        }
    }

    #[test]
    fn one_key_for_the_whole_build_side() {
        // Every row in one bucket, which is the worst the layout can be
        // asked to do and the case the contiguous range has to survive.
        let keys = vec![42u64; 5000];
        let payload: Vec<u64> = (0..5000).collect();
        let t = JoinTable::build(&keys, &payload);
        assert_eq!(t.lookup(42).len(), 5000);
        assert_eq!(t.lookup(42), &payload[..]);
        assert!(t.lookup(43).is_empty());
    }

    #[test]
    fn mark_matches_contains_row_for_row() {
        let keys: Vec<u64> = (0..500).map(|i| i * 3).collect();
        let payload: Vec<u64> = (0..500).collect();
        let t = JoinTable::build(&keys, &payload);
        let probe: Vec<u64> = (0..2000).collect();
        let mut marks = vec![false; probe.len()];
        t.mark(&probe, &mut marks);
        for (i, &k) in probe.iter().enumerate() {
            assert_eq!(marks[i], t.contains(k), "key {k}");
            assert_eq!(marks[i], k % 3 == 0 && k < 1500, "key {k}");
        }
    }

    #[test]
    fn join_emits_every_pair_once() {
        let build_keys: Vec<u64> = (0..200).flat_map(|k| [k, k]).collect();
        let build_payload: Vec<u64> = (0..400).collect();
        let t = JoinTable::build(&build_keys, &build_payload);
        let probe: Vec<u64> = (0..300).collect();

        let mut got: Vec<(usize, u64)> = Vec::new();
        t.join(&probe, |i, row| got.push((i, row)));

        let mut want: Vec<(usize, u64)> = Vec::new();
        for (i, &k) in probe.iter().enumerate() {
            for row in reference(&build_keys, &build_payload, k) {
                want.push((i, row));
            }
        }
        assert_eq!(got, want);
        // Two build rows a key, and only the first 200 keys exist.
        assert_eq!(got.len(), 400);
    }

    #[test]
    fn two_keys_sharing_a_bucket_keep_their_own_rows() {
        // The case the bucket scatter alone gets wrong. Rows land in a
        // bucket in build order, so two keys in one bucket interleave,
        // and a lookup walking a contiguous run stops at the first
        // foreign key. Force it: search for a pair of keys that share a
        // bucket at the size this build side produces, then interleave
        // their rows on purpose.
        let rows = 4096usize;
        let buckets = rows.next_power_of_two() as u64;
        let mask = buckets - 1;
        let mut pair = None;
        'outer: for a in 1u64..100_000 {
            for b in a + 1..a + 4000 {
                if hash64(a) & mask == hash64(b) & mask {
                    pair = Some((a, b));
                    break 'outer;
                }
            }
        }
        let (a, b) = pair.expect("two keys must share a bucket at this size");

        let mut keys = Vec::new();
        let mut payload = Vec::new();
        for i in 0..(rows as u64 / 2) {
            keys.push(a);
            payload.push(i);
            keys.push(b);
            payload.push(1_000_000 + i);
        }
        let t = JoinTable::build(&keys, &payload);
        assert_eq!(t.lookup(a), reference(&keys, &payload, a), "key {a}");
        assert_eq!(t.lookup(b), reference(&keys, &payload, b), "key {b}");
        assert_eq!(t.lookup(a).len(), rows / 2);
        assert!(t.contains(a) && t.contains(b));
    }

    #[test]
    fn many_keys_in_one_bucket_all_come_back() {
        // The same hazard widened: a lot of distinct keys, every one of
        // them landing in the same bucket, arriving interleaved.
        let rows = 1024usize;
        let buckets = rows.next_power_of_two() as u64;
        let mask = buckets - 1;
        let want = hash64(1) & mask;
        let sharing: Vec<u64> = (1u64..)
            .filter(|k| hash64(*k) & mask == want)
            .take(16)
            .collect();
        assert_eq!(sharing.len(), 16);

        let mut keys = Vec::new();
        let mut payload = Vec::new();
        for round in 0..(rows as u64 / 16) {
            for (j, k) in sharing.iter().enumerate() {
                keys.push(*k);
                payload.push(round * 100 + j as u64);
            }
        }
        let t = JoinTable::build(&keys, &payload);
        for k in &sharing {
            assert_eq!(t.lookup(*k), reference(&keys, &payload, *k), "key {k}");
            assert_eq!(t.lookup(*k).len(), rows / 16);
        }
    }

    #[test]
    fn keys_that_collide_on_the_bucket_still_separate() {
        // Keys a power of two apart land in the same bucket at any
        // table size, so this is where a tag that lied or a range that
        // ran long would show up.
        let buckets = 1024u64;
        let keys: Vec<u64> = (0..64).map(|i| i * buckets).collect();
        let payload: Vec<u64> = (0..64).collect();
        let t = JoinTable::build(&keys, &payload);
        for (i, &k) in keys.iter().enumerate() {
            assert_eq!(t.lookup(k), &[i as u64], "key {k}");
        }
        for i in 0..64u64 {
            assert!(t.lookup(i * buckets + 1).is_empty());
        }
    }

    #[test]
    fn a_single_row_build_side_works() {
        let t = JoinTable::build(&[9], &[123]);
        assert_eq!(t.lookup(9), &[123]);
        assert!(t.lookup(8).is_empty());
        assert!(t.lookup(10).is_empty());
    }
}
