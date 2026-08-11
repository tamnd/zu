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
//! A key is stored once however many rows it owns, next to the offset
//! where its run of rows starts, and the directory addresses those keys
//! rather than the rows. So a probe compares a key per distinct key in
//! the bucket, and a hit reads its run bounds off two offsets instead of
//! reading the rows themselves to find where its own stop. That also
//! decides how large the table is: a build side of mostly duplicates
//! gets its directory sized for the keys it ended up with rather than
//! the rows it was handed.
//!
//! That folding is the point. A probe reads one directory word, and a key
//! with no match usually dies on the tag inside that same load, so a miss
//! costs one cache line and nothing else. A hit walks a contiguous range
//! rather than chasing a pointer chain.
//!
//! The build is a counting sort, two passes over the keys with a
//! histogram in between, then one walk that orders each bucket and
//! collapses it, and it happens once. The table is meant to be wrapped
//! in an `Arc` and shared by every probe worker.
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
//! Misses run 2.0 to 4.9x a `HashSet` probe across the fleet. That is
//! the antijoin and the mark join, and it is where the folded tag earns
//! its bits, since a miss reads the directory word and stops. Hits on
//! distinct keys run 1.0 to 2.6x. Both of those hold on every host, and
//! the margin is widest on the slow ones, which is the right direction.
//!
//! The duplicate-heavy inner join, forty rows to a key, is the shape
//! perf/05 quotes the largest number on, and it used to run at a third
//! of a `HashMap<u64, Vec<u64>>` everywhere, because it read the key
//! beside every row it returned and then read the rows again to find
//! where its own run stopped. Storing the key once put it level with
//! that map and 3.5 to 5.3x its own old self: 249.03 to 47.02 ms on the
//! local M series, 240.45 to 66.58 on gamingpc, 2861 to 824 on server1.
//! Part of that is the second seating rather than the collapse alone.
//! Left in the directory the rows asked for, the same probe reads 90.34
//! on gamingpc against 66.58 seated and 1166 on server1 against 747,
//! while on the local M series, where the whole oversized directory
//! still sits in cache, the seating costs about a tenth and is kept for
//! the memory.
//!
//! The distinct probe, the miss probe and the build all come out where
//! they were, within the spread of a host. The memory moved both ways.
//! The bench's duplicate table went 24.4 MB to 8.6, since neither the
//! repeated keys nor a directory sized for them are there any more, and
//! its distinct table went 24.4 to 28.4, which is four bytes a key for a
//! run offset on a table whose runs are all one row long.
//!
//! The build is still behind on the fast hosts, 0.3x locally and 0.6x
//! on gamingpc, and ahead on the slow ones, 1.2 to 1.6x on server1: the
//! histogram and the scatter are random access over a directory the size
//! of the build side, so this is bandwidth against the standard
//! library's slower per-key hash. The write-combining radix pass perf/05
//! section 2 calls for is the named fix for that half and is not
//! written.
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

/// How far past the keys it addresses the first directory has to be
/// before the table is seated again in one sized for them. Four, so
/// that a build side with the odd duplicate in it keeps the directory
/// it already has and only a genuinely duplicate-heavy one pays for a
/// second seating.
const FIT_SLACK: usize = 4;

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
/// same bucket and therefore next to each other in `payload`, so an
/// inner join over a hot key emits its matches straight down a run of
/// the buffer and the key itself is stored once.
pub struct JoinTable {
    /// One word per bucket plus a sentinel: `tag:16 | offset:48`. The
    /// offset is where the bucket's keys start in `keys`, and the next
    /// word's offset is where they end, which is why the sentinel is
    /// there. The sentinel's tag is zero and is never read.
    dir: Vec<u64>,
    /// The distinct build keys in bucket order, one word each however
    /// many rows the key owns.
    keys: Vec<u64>,
    /// Where each key's rows start in `payload`, plus a sentinel, so
    /// key `i` owns `payload[at[i]..at[i + 1]]`.
    at: Vec<u32>,
    /// The build payloads in bucket order, a key's rows contiguous and
    /// in build order inside that.
    payload: Vec<u64>,
    /// `buckets - 1`, the mask that turns a hash into a bucket.
    mask: u64,
}

impl JoinTable {
    /// Build over `keys`, carrying `payload[i]` for `keys[i]`.
    ///
    /// # Panics
    /// If the two slices are different lengths, or if the build side is
    /// longer than `u32::MAX` rows, which a run offset cannot address.
    pub fn build(keys: &[u64], payload: &[u64]) -> Self {
        assert_eq!(keys.len(), payload.len(), "a payload per key");
        let n = keys.len();
        assert!(n <= u32::MAX as usize, "a build side under 4 billion rows");
        if n == 0 {
            // One empty bucket and its sentinel, so probe needs no
            // special case for a build side that produced nothing.
            return Self {
                dir: vec![0, 0],
                keys: Vec::new(),
                at: vec![0],
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
        // A tag is 16 bits, and this is written at random, so it is held
        // at its own width: a word apiece would put four times the pages
        // under the scatter's random writes for no more information.
        let mut tags = vec![0u16; buckets];
        let mut sorted_keys = vec![0u64; n];
        let mut sorted_payload = vec![0u64; n];
        for i in 0..n {
            let h = hashes[i];
            let b = (h & mask) as usize;
            let at = cursor[b] as usize;
            cursor[b] += 1;
            sorted_keys[at] = keys[i];
            sorted_payload[at] = payload[i];
            tags[b] |= tag_bits(h) as u16;
        }

        // Pass three, one walk over the buckets that orders each one and
        // then collapses it, since both halves want the same bucket in
        // cache at the same moment.
        //
        // Ordering first: the scatter kept build order inside a bucket,
        // so two distinct keys that hashed to the same bucket are
        // interleaved there, and a lookup walking one contiguous run
        // would stop at the first foreign key and miss the rest of its
        // own. Sorting the bucket by key makes a key's rows one range
        // again. Buckets hold about a row each and a bucket that is
        // already ordered, which is nearly all of them and every bucket
        // holding one key however many times, costs the scan and nothing
        // more.
        //
        // Collapsing second: a key's rows are one run of the payload
        // now, and that run can be described once instead of being spelt
        // out again beside every row it holds. Keep the key once and
        // where its run starts, and point the directory at those rather
        // than at the rows, so that a probe compares one key per distinct
        // key in the bucket, which is usually one key, and takes the run
        // bounds off two offsets rather than reading the rows to find
        // where its own stop. The runs tile the payload in order, so the
        // next key's offset is this key's end and one sentinel closes the
        // last of them.
        //
        // The keys collapse in place, over the buffer the scatter filled.
        // The write stays behind the read, because a bucket never keeps
        // more keys than it holds rows, so the walk cannot reach a slot
        // it has already overwritten. The tags fold in on the way past,
        // and the directory sentinel keeps its bare offset because the
        // last bucket's range has to end somewhere.
        let mut scratch: Vec<(u64, u64)> = Vec::new();
        let mut keys = sorted_keys;
        let mut at = Vec::with_capacity(n + 1);
        let mut kept = 0usize;
        let mut start = 0usize;
        for b in 0..buckets {
            let end = dir[b + 1] as usize;
            if end - start >= 2 && !keys[start..end].is_sorted() {
                scratch.clear();
                scratch.extend(
                    keys[start..end]
                        .iter()
                        .copied()
                        .zip(sorted_payload[start..end].iter().copied()),
                );
                // Stable, so a key's payloads stay in build order.
                scratch.sort_by_key(|(k, _)| *k);
                for (i, (k, p)) in scratch.iter().enumerate() {
                    keys[start + i] = *k;
                    sorted_payload[start + i] = *p;
                }
            }
            dir[b] = kept as u64 | ((tags[b] as u64) << PTR_BITS);
            let mut row = start;
            while row < end {
                let key = keys[row];
                keys[kept] = key;
                at.push(row as u32);
                kept += 1;
                row += 1;
                while row < end && keys[row] == key {
                    row += 1;
                }
            }
            start = end;
        }
        debug_assert_eq!(start, n);
        keys.truncate(kept);
        at.push(n as u32);
        dir[buckets] = keys.len() as u64;

        // This directory was sized for the rows, and every row of a key
        // beyond the first turned out to share one slot with it, so a
        // build side of mostly duplicates has left it several times
        // larger than the keys it addresses. A probe pays that size
        // twice, once in the cache and TLB misses of a random walk over
        // a mostly empty array, and once in the memory it holds for the
        // whole life of the table. Seat the keys again in a directory
        // sized for them.
        if kept * FIT_SLACK <= buckets {
            return seat(&keys, &at, &sorted_payload);
        }
        // Short of that the buffers still hold what the rows asked for
        // rather than what the keys did.
        keys.shrink_to_fit();
        at.shrink_to_fit();

        Self {
            dir,
            keys,
            at,
            payload: sorted_payload,
            mask,
        }
    }

    /// Rows on the build side.
    pub fn rows(&self) -> usize {
        self.payload.len()
    }

    /// Distinct keys on the build side, which is how many keys a probe
    /// can walk past in total.
    pub fn distinct(&self) -> usize {
        self.keys.len()
    }

    /// What the table holds, in bytes. Every probe worker shares the one
    /// table, so this is paid once, but it is paid for as long as the
    /// join runs.
    pub fn bytes(&self) -> usize {
        size_of_val(self.dir.as_slice())
            + size_of_val(self.keys.as_slice())
            + size_of_val(self.at.as_slice())
            + size_of_val(self.payload.as_slice())
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
        // A bucket holds about one distinct key, so this is a short walk
        // over keys in the line the directory word pulled in, and the
        // key that matches says where its rows are without the walk
        // having to read a single one of them.
        for i in start..end {
            if self.keys[i] == key {
                let lo = self.at[i] as usize;
                let hi = self.at[i + 1] as usize;
                return &self.payload[lo..hi];
            }
        }
        &[]
    }

    /// Whether `key` is on the build side. This is the semijoin and
    /// antijoin question, and unlike [`lookup`](Self::lookup) it never
    /// touches the payload at all.
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

/// Seat already collapsed keys in a directory sized for the keys
/// themselves, moving each key's run of rows with it.
///
/// This is the same counting sort the build runs, one bucket per key
/// rather than one per row, and it needs no per-bucket ordering: the
/// keys are distinct by the time they arrive here, so a probe that
/// walks a bucket comparing keys does not care what order they sit in.
/// The rows follow their key so that the runs still tile the payload in
/// order, which is what lets a run's end be read off the next key's
/// offset.
fn seat(keys: &[u64], at: &[u32], payload: &[u64]) -> JoinTable {
    let d = keys.len();
    let buckets = d.next_power_of_two();
    let mask = buckets as u64 - 1;

    let mut hashes = vec![0u64; d];
    hash_slice(keys, &mut hashes);
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
    debug_assert_eq!(acc, d as u64);

    // The keys move now, and each one takes a note of where its rows
    // were, which the copy below turns into where they are.
    let mut cursor: Vec<u64> = dir[..buckets].to_vec();
    let mut tags = vec![0u16; buckets];
    let mut seated = vec![0u64; d];
    let mut was = vec![0u32; d];
    let mut runs = vec![0u32; d];
    for i in 0..d {
        let h = hashes[i];
        let b = (h & mask) as usize;
        let slot = cursor[b] as usize;
        cursor[b] += 1;
        seated[slot] = keys[i];
        was[slot] = at[i];
        runs[slot] = at[i + 1] - at[i];
        tags[b] |= tag_bits(h) as u16;
    }
    for (b, tag) in tags.into_iter().enumerate() {
        dir[b] |= (tag as u64) << PTR_BITS;
    }

    let mut rows = Vec::with_capacity(payload.len());
    let mut starts = Vec::with_capacity(d + 1);
    for i in 0..d {
        starts.push(rows.len() as u32);
        let from = was[i] as usize;
        rows.extend_from_slice(&payload[from..from + runs[i] as usize]);
    }
    starts.push(rows.len() as u32);
    debug_assert_eq!(rows.len(), payload.len());

    JoinTable {
        dir,
        keys: seated,
        at: starts,
        payload: rows,
        mask,
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
    fn runs_of_different_lengths_keep_their_own_bounds() {
        // A key's rows end where the next key's begin, and the next key
        // is usually in another bucket, so a run whose length is read
        // off the following offset is only right if the runs tile the
        // payload in order. Uneven lengths are what would catch that:
        // key k owns k rows, so a bound that ran one long or one short
        // lands in a neighbour that looks nothing like it.
        let mut keys = Vec::new();
        let mut payload = Vec::new();
        for k in 1..200u64 {
            for r in 0..k {
                keys.push(k);
                payload.push(k * 1000 + r);
            }
        }
        let t = JoinTable::build(&keys, &payload);
        assert_eq!(t.rows(), keys.len());
        assert_eq!(t.distinct(), 199);
        for k in 1..200u64 {
            assert_eq!(t.lookup(k), reference(&keys, &payload, k), "key {k}");
            assert_eq!(t.lookup(k).len(), k as usize, "key {k}");
        }
        assert!(t.lookup(0).is_empty());
        assert!(t.lookup(200).is_empty());
    }

    #[test]
    fn a_duplicate_heavy_table_is_sized_by_its_keys() {
        // A hundred rows a key, so the directory the rows asked for
        // addresses a hundred times the keys it needs to and the table
        // is seated a second time over those keys. The answers have to
        // survive that move, payload runs and all, and the table has to
        // come out smaller than the first directory alone would have
        // been.
        let mut keys = Vec::new();
        let mut payload = Vec::new();
        for k in 0..100u64 {
            for r in 0..100u64 {
                keys.push(k * 13 + 1);
                payload.push(k * 1000 + r);
            }
        }
        let t = JoinTable::build(&keys, &payload);
        assert_eq!(t.rows(), 10_000);
        assert_eq!(t.distinct(), 100);
        for k in 0..100u64 {
            let k = k * 13 + 1;
            assert_eq!(t.lookup(k), reference(&keys, &payload, k), "key {k}");
        }
        assert!(t.lookup(2).is_empty());
        let rowsized = keys.len().next_power_of_two() * 8;
        assert!(
            t.bytes() < rowsized + keys.len() * 8,
            "table is {} bytes, a row sized directory and its keys is {}",
            t.bytes(),
            rowsized + keys.len() * 8
        );
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
