//! Sideways information passing (perf/13 section 1).
//!
//! A join build side knows something the probe side spends most of its
//! time finding out the hard way: which keys can possibly match. Handing
//! that down to the operators that produce the probe side is what
//! perf/13 calls sideways information passing, and it is the difference
//! between filtering a blowup at the leaves and materializing the whole
//! thing before throwing nearly all of it away.
//!
//! The producer is a join build. The consumers are the scan, which
//! checks a chunk's zone before it decodes anything, the expand, which
//! masks neighbors as it emits them, and the gather, which drops a row
//! before fetching its properties. All three ask one of two questions:
//! can a range hold a match at all, and can this one key match. So a
//! filter here answers both, rather than being one of three shapes as
//! the spec's sketch has it. The range costs sixteen bytes and is worth
//! having whatever else is in the filter, and the membership test is
//! whichever of the three the build side could afford:
//!
//! A node mask is a bitmap over the span the keys cover. It is exact,
//! it needs no hash, and it is the graph-native case, since node ids
//! are dense and a build side of them covers its span thickly. It is
//! also the cheapest of the three to probe: one shift and one test
//! against a word the index falls straight out of.
//!
//! A bloom is the fallback for keys too spread out to sit in a bitmap.
//! It is register blocked, one 64-bit block a key, four bits of it
//! claimed, so a test reads one word and never straddles a cache line.
//!
//! The range alone is what is left when a build side is so large that a
//! bloom over it stops being a prefilter and becomes a second table to
//! miss in.
//!
//! On reusing the join table's own tags instead, which is what perf/13
//! suggests and what this deliberately does not do. The tags are there,
//! they cost nothing to build, and [`JoinTable::may_contain`] exposes
//! them so the choice can be measured rather than argued about. Two
//! things are wrong with them as a filter passed sideways. They are two
//! bits of a sixteen-bit tag shared by a whole bucket, so they let far
//! more strangers through than a bloom sized for the job. And they live
//! in a directory sized by the build side, which for a million keys is
//! tens of megabytes, so the test cannot stay resident while a much
//! larger probe side streams past it. Two bytes a key for a bloom, or
//! an eighth of a byte per id of span for a mask, can.
//!
//! What the bench says, a million build keys and four million probes
//! with one row in sixteen a real match, the misses sitting inside the
//! build side's own range so the range check cannot answer them.
//! Selecting the probe side runs, in M rows/s on the local M series,
//! gamingpc and server1: the mask 797, 496 to 517 and 114 to 207, the
//! bloom 401 to 406, 302 to 360 and 76 to 101, the tags 574 to 653, 270
//! to 351 and 19 to 25. server1's spread is the box and not the filter,
//! it runs a gate with no free page cache and every line on it moves
//! together.
//!
//! Read the third line carefully, because it is not the argument it
//! looks like. The tags beat the bloom on the local M series, where a
//! 28 MB directory nearly fits in the system cache and the bloom's
//! extra shifts are then the only thing either side pays. They draw on
//! gamingpc. On server1, which has neither the cache nor the memory
//! headroom, the same test costs four times as much, and that is the
//! host a filter has to be right for. The accuracy is not close
//! anywhere: the bloom let 0.48 percent of the strangers through and
//! the tags let 2.36, on every host, since that is a property of the
//! keys and not the machine. The mask let none through, being exact.
//! Memory went 0.38 MB for the mask, 2.10 for the bloom and 28.39 for
//! the table whose tags the third line measured.
//!
//! End to end, the reason any of this is here rather than in the join.
//! Every probe row costs a property gather, a random read into a
//! column, and then the join, which is the order the plan runs in.
//! Gathering all four million and joining all four million is what the
//! engine does today. Selecting first and gathering only the survivors
//! runs it in 18.74 ms against 48.75 locally, 27.60 against 85.73 on
//! gamingpc and 105.75 against 538.71 on server1, so 2.6 to 5.1x
//! depending on the host, and 1.9x in the worst round server1 turned
//! in. That factor is the hit rate more than the filter, which is the
//! point: the filter is worth having where the rows are produced, not
//! where they are joined.
//!
//! The consumer is `Op::Sip`, which the compiler puts on the level a
//! join probes from, right where that level is made. Everything between
//! there and the join then runs on the rows that can still match: the
//! predicates over that level and the probe itself today, and the walks
//! off it once the planner puts one there. The range goes further down
//! still, into the scan's zone pushdown, where it skips whole chunks
//! without decoding them.
//!
//! Only an exact test goes in as an operator. What it saves on a
//! rejected row is one probe, which is one random read of the join's
//! directory, so a test that costs a random read of its own and is
//! sometimes wrong about the row has nothing left to win. On the join
//! bench, against the same plan with ZU_SIP=0, the mask runs 1.0x on
//! the local M series, 1.1x on gamingpc and 1.2 to 1.6x on server1,
//! and the bloom ran 0.8 to 0.9x locally before it was taken back out.
//! A scan that took the filter instead of an operator over rows it has
//! already decoded is what would give the inexact one something real to
//! save, and that is not written yet.
//!
//! The range is published either way and runs 1.4x locally, 1.3x on
//! gamingpc and 1.3 to 1.4x on server1. Read the two sets together
//! rather than one at a time. They are complements: the range saves a
//! decode and the mask saves a random read, so each wins on the host
//! where the thing it removes is what that host is short of, and sits
//! near one on the host where it is not. On the local M series the
//! join's directory is in cache and the probe the mask skips was never
//! expensive, so there the mask is the wash and the range is the win;
//! on server1 it is the other way round. On every host one of the two
//! is a clear win, which is what the bench gates. Both sets are timed
//! on and off alternately, because timing them as two blocks of runs
//! lets a box that drifts over those few seconds put the drift in the
//! ratio, and that alone was most of the spread these numbers used to
//! have. Neither shape is the pass at its best. The optimizer
//! drives the side it estimated cheaper and builds the dearer one, so
//! the filter is published from the big side onto the small one, which
//! is the direction with the least to gain, and the build of the big
//! side is in the number either way. The join side choice is what
//! changes that, and it is not this file.
//!
//! A filter that turns out to reject nothing is worse than no filter,
//! so the operator watches its own rejection rate over the first few
//! vectors and stops testing when the rows are coming through anyway.
//! That is the only decision here made at runtime; everything else
//! about the shape of the filter is settled off the build side's keys.

use zu_query::snapshot::{ColId, ZonePred};
use zu_vector::kernels::hash64;

use crate::join::prefetch;

/// How much wider than its keys a node mask may be before a bloom is
/// the cheaper filter. A mask over a span of s ids costs s/8 bytes and
/// never lies, a bloom over d keys costs two bytes a key and sometimes
/// does, and at sixteen ids of span per key those two cost the same. So
/// anything denser than one key in sixteen takes the exact filter and
/// pays nothing extra for it.
const MASK_SPREAD: u64 = 16;

/// Bits of bloom a distinct key gets, and how many of them it claims.
/// Sixteen and four put the false positive rate near a quarter of a
/// percent at two bytes a key, which is low enough that a survivor is
/// nearly always a real match and small enough to stay in cache while
/// the probe side streams by.
const BLOOM_BITS_PER_KEY: usize = 16;
const BLOOM_BITS_SET: u32 = 4;

/// Past this a bloom is no longer a prefilter, it is a second table to
/// miss in, and a build side that large gets the range on its own.
const BLOOM_CAP_BYTES: usize = 32 << 20;

/// How far ahead the vector loops warm the word they are about to test.
/// Eight, the same as the join table, for the same reason: the load is
/// a dependent random access and the point is to have several of them
/// in flight at once.
const PREFETCH: usize = 8;

/// The bits `h` claims inside its block, as one word to test or set in
/// a single go. Four six-bit fields taken off the top half of the hash,
/// so the block index taken off the bottom and the bits taken off the
/// top never read the same bits twice. Two fields landing on the same
/// bit only leaves the word with fewer bits set, which costs a little
/// accuracy and nothing else.
fn bloom_word(h: u64) -> u64 {
    let mut w = 0u64;
    for i in 0..BLOOM_BITS_SET {
        w |= 1 << ((h >> (32 + 6 * i)) & 63);
    }
    w
}

/// What a build side knows about its keys, in a form the operators
/// producing the probe side can use.
pub struct SipFilter {
    /// The smallest and largest build key, so a consumer holding a
    /// range rather than a key still has something to ask.
    lo: u64,
    hi: u64,
    /// Distinct build keys, which is what decided the shape below and
    /// what EXPLAIN ANALYZE wants to print.
    keys: usize,
    test: Membership,
}

/// The per-key half of a filter, tightest first.
enum Membership {
    /// The build side is empty, so nothing matches and every consumer
    /// can stop.
    Nothing,
    /// A bitmap over `lo..=hi`, exact and hash-free.
    Mask(NodeMask),
    /// A register-blocked bloom over the keys.
    Bloom(Bloom),
    /// The range and nothing else, for a build side too large to filter
    /// any harder without becoming the problem.
    Range,
}

impl SipFilter {
    /// The tightest filter `keys` supports. The keys need not be
    /// distinct, but a build publishing this should pass the distinct
    /// ones, since a duplicate only makes the sizing decision worse.
    pub fn over(keys: &[u64]) -> Self {
        let Some(&first) = keys.first() else {
            return Self {
                lo: 0,
                hi: 0,
                keys: 0,
                test: Membership::Nothing,
            };
        };
        let mut lo = first;
        let mut hi = first;
        for &k in keys {
            lo = lo.min(k);
            hi = hi.max(k);
        }
        let d = keys.len();
        // One less than the span, because a build side holding both 0
        // and u64::MAX has a span that does not fit in a u64.
        let spread = hi - lo;

        let test = if spread < MASK_SPREAD.saturating_mul(d as u64) {
            Membership::Mask(NodeMask::over(keys, lo, spread))
        } else {
            let blocks = (d * BLOOM_BITS_PER_KEY / 64).next_power_of_two().max(1);
            if blocks * 8 > BLOOM_CAP_BYTES {
                Membership::Range
            } else {
                Membership::Bloom(Bloom::over(keys, blocks))
            }
        };
        Self {
            lo,
            hi,
            keys: d,
            test,
        }
    }

    /// Whether the build side had no keys at all, which is a stronger
    /// fact than any of the tests below can carry: the join produces
    /// nothing and the pipeline under it need not run.
    pub fn is_empty(&self) -> bool {
        self.keys == 0
    }

    /// Build keys the filter was made over.
    pub fn keys(&self) -> usize {
        self.keys
    }

    /// What the filter holds, in bytes, over and above the range.
    pub fn bytes(&self) -> usize {
        match &self.test {
            Membership::Nothing | Membership::Range => 0,
            Membership::Mask(m) => size_of_val(m.bits.as_slice()),
            Membership::Bloom(b) => size_of_val(b.blocks.as_slice()),
        }
    }

    /// Whether every value between the smallest key and the largest is
    /// itself a key. A filter like that says nothing the range does not
    /// already say, so a consumer that has the range pushed down has no
    /// reason to test rows against it one at a time.
    ///
    /// Only the mask can answer this, since it is the only shape that
    /// knows which values inside its range are missing.
    pub fn gapless(&self) -> bool {
        match &self.test {
            Membership::Mask(m) => m.ones() == self.hi - self.lo + 1,
            Membership::Nothing | Membership::Bloom(_) | Membership::Range => false,
        }
    }

    /// Whether a survivor is certainly a match. True for the exact
    /// shapes, false for the bloom and for the bare range.
    pub fn exact(&self) -> bool {
        matches!(self.test, Membership::Nothing | Membership::Mask(_))
    }

    /// The shape, for EXPLAIN ANALYZE. Which filter a build published
    /// is a plan property a reader should be able to see without
    /// working it out from the key distribution.
    pub fn kind(&self) -> &'static str {
        match self.test {
            Membership::Nothing => "nothing",
            Membership::Mask(_) => "mask",
            Membership::Bloom(_) => "bloom",
            Membership::Range => "range",
        }
    }

    /// True when a zone spanning `lo..=hi` cannot hold a match, which
    /// is the question a scan asks of a chunk before decoding it.
    pub fn skips(&self, lo: u64, hi: u64) -> bool {
        match self.test {
            Membership::Nothing => true,
            _ => self.lo > hi || self.hi < lo,
        }
    }

    /// The range as a scan pushdown over `col`, `None` for an empty
    /// build side, which [`is_empty`](Self::is_empty) covers and no
    /// pushdown can express.
    pub fn zone(&self, col: ColId) -> Option<ZonePred> {
        match self.test {
            Membership::Nothing => None,
            _ => Some(ZonePred {
                col,
                lo: self.lo,
                hi: self.hi,
            }),
        }
    }

    /// Whether `key` could match. False is certain, true is certain
    /// only when [`exact`](Self::exact) holds.
    pub fn may_contain(&self, key: u64) -> bool {
        match &self.test {
            Membership::Nothing => false,
            Membership::Mask(m) => m.may_contain(key),
            Membership::Bloom(b) => key >= self.lo && key <= self.hi && b.may_contain(key),
            Membership::Range => key >= self.lo && key <= self.hi,
        }
    }

    /// Warm the word `key`'s test is about to read.
    fn warm(&self, key: u64) {
        match &self.test {
            Membership::Mask(m) => m.warm(key),
            Membership::Bloom(b) => b.warm(key),
            Membership::Nothing | Membership::Range => {}
        }
    }

    /// Write the indices of the keys that survive into `out` and return
    /// how many there were. This is what an expand calls on a neighbor
    /// list it has just read.
    ///
    /// The store is unconditional and only the cursor moves, so the
    /// loop has no branch on the filter's answer and does not care how
    /// selective the filter turned out to be.
    ///
    /// # Panics
    /// If `out` has no room for every key to survive.
    pub fn select(&self, keys: &[u64], out: &mut [u32]) -> usize {
        assert!(out.len() >= keys.len(), "room for every key to survive");
        let len = keys.len();
        let mut n = 0;
        for i in 0..len {
            if i + PREFETCH < len {
                self.warm(keys[i + PREFETCH]);
            }
            out[n] = i as u32;
            n += usize::from(self.may_contain(keys[i]));
        }
        n
    }

    /// Narrow a selection that already exists, in place, and return how
    /// much of it is left. `sel` holds indices into `keys`. This is what
    /// a gather calls, since by then the rows have been selected once
    /// already and only the properties are still unread.
    pub fn refine(&self, keys: &[u64], sel: &mut [u32]) -> usize {
        let len = sel.len();
        let mut n = 0;
        for i in 0..len {
            if i + PREFETCH < len {
                self.warm(keys[sel[i + PREFETCH] as usize]);
            }
            let at = sel[i];
            // The write is at or behind the read, so this compacts in
            // place without a second buffer.
            sel[n] = at;
            n += usize::from(self.may_contain(keys[at as usize]));
        }
        n
    }
}

/// An exact bitmap over the span the build keys cover.
struct NodeMask {
    lo: u64,
    bits: Vec<u64>,
}

impl NodeMask {
    fn over(keys: &[u64], lo: u64, spread: u64) -> Self {
        let mut bits = vec![0u64; (spread >> 6) as usize + 1];
        for &k in keys {
            let off = k - lo;
            bits[(off >> 6) as usize] |= 1 << (off & 63);
        }
        Self { lo, bits }
    }

    /// Keys in the mask, counted off the bitmap itself, so a build side
    /// that handed the same key over twice is still counted once.
    fn ones(&self) -> u64 {
        self.bits.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// The word and bit `key` would sit at, `None` when it sits outside
    /// the span the mask covers at all.
    fn slot(&self, key: u64) -> Option<(usize, u32)> {
        let off = key.checked_sub(self.lo)?;
        let word = (off >> 6) as usize;
        (word < self.bits.len()).then_some((word, (off & 63) as u32))
    }

    fn may_contain(&self, key: u64) -> bool {
        self.slot(key)
            .is_some_and(|(word, bit)| self.bits[word] >> bit & 1 == 1)
    }

    fn warm(&self, key: u64) {
        if let Some((word, _)) = self.slot(key) {
            prefetch(&self.bits[word]);
        }
    }
}

/// A register-blocked bloom: one 64-bit block a key, four bits claimed
/// inside it, so a test reads one word and never straddles a line.
struct Bloom {
    blocks: Vec<u64>,
    mask: u64,
}

impl Bloom {
    fn over(keys: &[u64], blocks: usize) -> Self {
        debug_assert!(blocks.is_power_of_two());
        let mut b = Self {
            blocks: vec![0u64; blocks],
            mask: blocks as u64 - 1,
        };
        for &k in keys {
            let h = hash64(k);
            let at = (h & b.mask) as usize;
            b.blocks[at] |= bloom_word(h);
        }
        b
    }

    fn may_contain(&self, key: u64) -> bool {
        let h = hash64(key);
        let w = bloom_word(h);
        self.blocks[(h & self.mask) as usize] & w == w
    }

    fn warm(&self, key: u64) {
        prefetch(&self.blocks[(hash64(key) & self.mask) as usize]);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Dense ids, the graph case, so this is the mask.
    fn dense() -> Vec<u64> {
        (0..10_000u64).map(|i| i * 3 + 7).collect()
    }

    /// Keys scattered over the whole word, so no bitmap fits them.
    fn scattered() -> Vec<u64> {
        (0..10_000u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect()
    }

    #[test]
    fn an_empty_build_side_rejects_everything() {
        let f = SipFilter::over(&[]);
        assert!(f.is_empty());
        assert_eq!(f.kind(), "nothing");
        assert!(f.exact());
        assert!(!f.may_contain(0));
        assert!(!f.may_contain(u64::MAX));
        assert!(f.skips(0, u64::MAX), "no zone can hold a match");
        assert!(f.zone(0).is_none());
    }

    #[test]
    fn dense_keys_take_the_exact_mask() {
        let keys = dense();
        let f = SipFilter::over(&keys);
        assert_eq!(f.kind(), "mask");
        assert!(f.exact());
        assert_eq!(f.keys(), keys.len());
        for &k in &keys {
            assert!(f.may_contain(k), "key {k}");
        }
        // Nothing else in the span, and nothing outside it either. An
        // exact filter has no room to be generous.
        let want: HashSet<u64> = keys.iter().copied().collect();
        for k in 0..40_000u64 {
            assert_eq!(f.may_contain(k), want.contains(&k), "key {k}");
        }
    }

    #[test]
    fn scattered_keys_take_the_bloom_and_never_lose_one() {
        let keys = scattered();
        let f = SipFilter::over(&keys);
        assert_eq!(f.kind(), "bloom");
        assert!(!f.exact());
        for &k in &keys {
            assert!(f.may_contain(k), "key {k} was on the build side");
        }
    }

    #[test]
    fn the_bloom_lets_few_strangers_through() {
        let keys = scattered();
        let f = SipFilter::over(&keys);
        // Probes drawn the same way but past the end of the build side,
        // so none of them is a match and every survivor is a false one.
        let probe: Vec<u64> = (10_000..110_000u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect();
        let through = probe.iter().filter(|k| f.may_contain(**k)).count();
        // A quarter of a percent is what the sizing is meant to give.
        // One percent is loose enough that a bad hash split or a sizing
        // slip shows up here and ordinary variance does not.
        assert!(
            through * 100 < probe.len(),
            "{through} of {} strangers got through",
            probe.len()
        );
    }

    #[test]
    fn the_mask_costs_less_than_the_bloom_it_replaced() {
        // The sizing rule is only worth having if the filter it picks
        // is the smaller one, so check that on the shape it was tuned
        // for rather than trusting the arithmetic in the constant.
        let keys = dense();
        let f = SipFilter::over(&keys);
        assert_eq!(f.kind(), "mask");
        assert!(
            f.bytes() < keys.len() * BLOOM_BITS_PER_KEY / 8,
            "mask is {} bytes, a bloom would have been {}",
            f.bytes(),
            keys.len() * BLOOM_BITS_PER_KEY / 8
        );
    }

    #[test]
    fn a_zone_outside_the_range_is_skipped() {
        let keys: Vec<u64> = (100..200).collect();
        let f = SipFilter::over(&keys);
        assert!(f.skips(0, 99), "wholly below");
        assert!(f.skips(200, 1000), "wholly above");
        assert!(!f.skips(0, 100), "touches the bottom");
        assert!(!f.skips(199, 1000), "touches the top");
        assert!(!f.skips(120, 130), "inside");
        let z = f.zone(3).expect("a range to push down");
        assert_eq!((z.col, z.lo, z.hi), (3, 100, 199));
    }

    #[test]
    fn select_keeps_the_survivors_in_order() {
        let build = dense();
        let f = SipFilter::over(&build);
        let probe: Vec<u64> = (0..5000).collect();
        let mut out = vec![0u32; probe.len()];
        let n = f.select(&probe, &mut out);

        let on_build: HashSet<u64> = build.iter().copied().collect();
        let want: Vec<u32> = probe
            .iter()
            .enumerate()
            .filter(|(_, k)| on_build.contains(k))
            .map(|(i, _)| i as u32)
            .collect();
        assert_eq!(&out[..n], &want[..]);
        assert!(n > 0 && n < probe.len(), "a filter that did nothing");
    }

    #[test]
    fn select_survives_a_filter_that_keeps_everything() {
        // The compaction writes into the buffer it is reading indices
        // out of, so the case where nothing is dropped is the one where
        // the write catches up with the read.
        let build: Vec<u64> = (0..1000).collect();
        let f = SipFilter::over(&build);
        let mut out = vec![0u32; build.len()];
        let n = f.select(&build, &mut out);
        assert_eq!(n, build.len());
        assert!(out.iter().enumerate().all(|(i, at)| i as u32 == *at));
    }

    #[test]
    fn refine_narrows_a_selection_in_place() {
        let build: Vec<u64> = (0..1000u64).map(|i| i * 4).collect();
        let f = SipFilter::over(&build);
        let probe: Vec<u64> = (0..2000u64).collect();
        // Start from the even rows, so the answer is the rows that are
        // both already selected and on the build side.
        let mut sel: Vec<u32> = (0..probe.len() as u32).filter(|i| i % 2 == 0).collect();
        let was = sel.len();
        let n = f.refine(&probe, &mut sel);

        let on_build: HashSet<u64> = build.iter().copied().collect();
        let want: Vec<u32> = (0..probe.len() as u32)
            .filter(|i| i % 2 == 0 && on_build.contains(&probe[*i as usize]))
            .collect();
        assert_eq!(&sel[..n], &want[..]);
        assert!(
            n > 0 && n < was,
            "a refine that dropped nothing or everything"
        );
    }

    #[test]
    fn one_key_is_still_a_filter() {
        let f = SipFilter::over(&[42]);
        assert_eq!(f.keys(), 1);
        assert!(f.may_contain(42));
        assert!(!f.may_contain(41));
        assert!(!f.may_contain(43));
        assert!(f.skips(0, 41));
        assert!(!f.skips(42, 42));
    }

    #[test]
    fn the_widest_possible_span_does_not_overflow() {
        // Both ends of the word at once, which is the span that does
        // not fit in a u64 and would have to fall through to the bloom.
        let f = SipFilter::over(&[0, u64::MAX]);
        assert_eq!(f.kind(), "bloom");
        assert!(f.may_contain(0));
        assert!(f.may_contain(u64::MAX));
        assert!(!f.skips(0, 0));
        assert!(!f.skips(u64::MAX, u64::MAX));
    }

    #[test]
    fn a_run_with_no_holes_in_it_is_gapless() {
        let f = SipFilter::over(&(1000..2000u64).collect::<Vec<_>>());
        assert_eq!(f.kind(), "mask");
        assert!(f.gapless(), "every value between the ends is a key");
    }

    #[test]
    fn one_missing_value_is_enough_of_a_gap() {
        let mut keys: Vec<u64> = (1000..2000).collect();
        keys.remove(500);
        let f = SipFilter::over(&keys);
        assert_eq!(f.kind(), "mask");
        assert!(!f.gapless(), "1500 is inside the range and not a key");
    }

    #[test]
    fn the_shapes_that_cannot_answer_gapless_say_no() {
        // Neither of these knows which values inside its range are
        // missing, so neither is allowed to claim there are none.
        assert!(!SipFilter::over(&[]).gapless());
        assert!(!SipFilter::over(&scattered()).gapless());
        assert!(!SipFilter::over(&dense()).gapless(), "strided, so holes");
    }

    #[test]
    fn a_single_key_is_gapless() {
        // The range is one value wide and that value is the key, so a
        // consumer with the range pushed down has nothing left to test.
        let f = SipFilter::over(&[42]);
        assert!(f.gapless());
    }

    #[test]
    fn repeats_do_not_make_a_run_look_gapped() {
        // The count comes off the bitmap rather than off the build
        // side, so a key handed over twice is still one key and the
        // run still covers its range.
        let mut keys: Vec<u64> = (0..100).collect();
        keys.extend(0..100);
        assert!(SipFilter::over(&keys).gapless());
    }

    #[test]
    fn duplicate_build_keys_do_not_change_the_answers() {
        let distinct: Vec<u64> = (0..500u64).map(|i| i * 7).collect();
        let mut with_dups = Vec::new();
        for &k in &distinct {
            for _ in 0..10 {
                with_dups.push(k);
            }
        }
        let a = SipFilter::over(&distinct);
        let b = SipFilter::over(&with_dups);
        for k in 0..4000u64 {
            assert_eq!(a.may_contain(k), b.may_contain(k), "key {k}");
        }
    }
}
