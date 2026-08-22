//! A sampled latency histogram, shared by the native latency examples.
//!
//! This lives in its own file rather than in each example because
//! `readlatency` exists to compare zu2 against sqlite, and a comparison
//! where the two sides quantise, calibrate or rank differently is not a
//! comparison. Same code, same process, same clock, same calibration.

use std::time::Instant;

/// One in this many reads is timed. See `Hist`.
pub const SAMPLE: u64 = 64;

/// Sub-buckets per octave, five bits of mantissa, so a bucket is at most
/// 1/32 wider than its own lower bound and a reported percentile is
/// within about three percent of the truth. That is finer than the run
/// to run spread on any host here, so the quantisation is not what
/// limits these numbers.
const SUB_BITS: u32 = 5;
const SUB: usize = 1 << SUB_BITS;

/// A log-bucketed latency histogram, sampled.
///
/// The problem this shape solves is that the thing being measured is
/// smaller than the instrument. A zu2 point read is around 34 ns on
/// these hosts and `Instant::now()` is 20 to 25 ns, so timing every read
/// with a pair of clock calls would report something like 75 ns and call
/// it a 34 ns operation. Two defences, and both are needed.
///
/// One in `SAMPLE` reads is timed rather than all of them, with each
/// thread offset to a different phase so the threads are not sampling
/// the same position in their loops. At one in sixty four the amortised
/// cost is under half a nanosecond a read, which does not move the rate
/// column, and four million ops still leaves about sixty two thousand
/// samples, which is enough to place a p99 and marginal for a p999.
///
/// The clock's own cost is calibrated once at startup and subtracted
/// from every sample. That leaves the samples honest to within the
/// spread of the calibration rather than biased high by a fixed 20 ns,
/// which at this scale is the difference between a real number and a
/// meaningless one. The calibration is printed so a reader who does not
/// trust the subtraction can add it back.
///
/// What sampling cannot do is see a tail rarer than one in sixty four
/// times the sample count. A p999 here is roughly sixty samples and
/// should be read as an order of magnitude, not a measurement.
pub struct Hist {
    counts: Vec<u64>,
}

impl Hist {
    pub fn new() -> Self {
        Hist {
            counts: vec![0; 64 * SUB],
        }
    }

    /// Bucket index for `n` nanoseconds. Below `SUB` every value is its
    /// own bucket and there is no error at all, which matters because
    /// that is where the p50 of a hit sits.
    fn bucket(n: u64) -> usize {
        if n < SUB as u64 {
            return n as usize;
        }
        let oct = 63 - n.leading_zeros();
        oct as usize * SUB + ((n >> (oct - SUB_BITS)) as usize & (SUB - 1))
    }

    /// The lower bound of bucket `i`, which is what a percentile
    /// reports. Reporting the bound rather than the midpoint keeps the
    /// error one-sided and understating, which is the safe direction for
    /// a number that is going into a comparison against a rival.
    fn value(i: usize) -> u64 {
        if i < SUB {
            return i as u64;
        }
        let oct = (i / SUB) as u32;
        let sub = (i % SUB) as u64;
        (SUB as u64 + sub) << (oct - SUB_BITS)
    }

    pub fn add(&mut self, ns: u64) {
        self.counts[Self::bucket(ns)] += 1;
    }

    pub fn merge(&mut self, other: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
    }

    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    pub fn quantile(&self, q: f64) -> u64 {
        let total = self.total();
        if total == 0 {
            return 0;
        }
        // Ceiling rank, so p50 of a hundred samples is the fiftieth and
        // not the fifty first, and a q of 1.0 lands on the last sample
        // rather than one past it.
        let want = ((total as f64 * q).ceil() as u64).clamp(1, total);
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return Self::value(i);
            }
        }
        Self::value(self.counts.len() - 1)
    }
}

/// Per-call cost of `Instant::now()`, which is what gets subtracted
/// from every sample.
///
/// The obvious version of this, the median of many back to back
/// `now()` deltas, is wrong on a host with a coarse clock and wrong in
/// the direction that flatters the engine. Apple silicon ticks at
/// 125/3 ns, about 41.67, so two adjacent `now()` calls usually land
/// inside the same tick, the median delta is zero, and nothing gets
/// subtracted even though each call really costs something. It was
/// measured at exactly zero on the first host tried.
///
/// So the cost is taken over a batch instead. A batch of `BATCH` calls
/// spans many ticks whatever the granularity, and dividing gives a per
/// call figure the clock is able to express. The median across batches
/// then throws out the ones that caught a scheduler hit, which a mean
/// would smear into every sample for the rest of the run.
pub fn clock_overhead() -> u64 {
    const BATCH: u64 = 512;
    let mut per = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = Instant::now();
        for _ in 0..BATCH {
            std::hint::black_box(Instant::now());
        }
        per.push(started.elapsed().as_nanos() as u64 / BATCH);
    }
    per.sort_unstable();
    per[per.len() / 2]
}

/// The smallest gap the clock can express, which bounds what any of
/// this can resolve.
///
/// Worth printing beside the percentiles rather than keeping quiet
/// about it. A 34 ns read measured on a clock that ticks every 41.67 ns
/// does not have a p50 in any useful sense: every sample is zero ticks
/// or one, and the histogram is a picture of the clock. On a host where
/// this comes back near 41 the single-operation percentiles should be
/// read as a bound and the comparison rests on the mean instead, which
/// is taken over millions of operations and does not care about
/// granularity.
pub fn clock_granularity() -> u64 {
    let mut best = u64::MAX;
    for _ in 0..100_000 {
        let a = Instant::now();
        let b = Instant::now();
        let d = (b - a).as_nanos() as u64;
        if d > 0 && d < best {
            best = d;
        }
    }
    if best == u64::MAX { 0 } else { best }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the percentiles rest on: a value never lands in a
    /// bucket whose reported lower bound is above it, and never more
    /// than one part in `SUB` below it. If this slips, every percentile
    /// in the table is wrong by an amount nobody can see.
    #[test]
    fn every_bucket_bounds_its_own_value() {
        let mut n = 0u64;
        while n < 1 << 22 {
            let i = Hist::bucket(n);
            let lo = Hist::value(i);
            assert!(lo <= n, "bucket for {n} reports {lo}, which is above it");
            assert!(
                n - lo <= (n / SUB as u64) + 1,
                "bucket for {n} reports {lo}, further below than one part in {SUB}"
            );
            // Dense at the bottom where the reads are, sparse above it,
            // so this covers the interesting range without four million
            // iterations of the rest.
            n += if n < 4096 { 1 } else { n / 512 };
        }
    }

    /// Buckets have to be monotonic in the value, otherwise the walk in
    /// `quantile` returns whatever the ordering happened to be.
    #[test]
    fn buckets_rise_with_the_value() {
        let mut last = 0;
        for n in 0..100_000u64 {
            let i = Hist::bucket(n);
            assert!(i >= last, "bucket for {n} went backwards");
            last = i;
        }
    }

    #[test]
    fn quantiles_of_a_known_distribution() {
        let mut h = Hist::new();
        // One to a hundred, each once, so the answers are known and
        // small enough to be exact: everything under SUB is its own
        // bucket and above it the bound is within a thirty second.
        for n in 1..=100u64 {
            h.add(n);
        }
        assert_eq!(h.total(), 100);
        assert_eq!(h.quantile(0.50), 50);
        // 95 and 99 are above SUB so they round down to a bucket bound.
        for (q, want) in [(0.95, 95u64), (0.99, 99)] {
            let got = h.quantile(q);
            assert!(
                got <= want && want - got <= want / SUB as u64 + 1,
                "q{q} gave {got}, wanted about {want}"
            );
        }
    }

    #[test]
    fn an_empty_histogram_reports_zero_rather_than_panicking() {
        let h = Hist::new();
        assert_eq!(h.total(), 0);
        assert_eq!(h.quantile(0.50), 0);
        assert_eq!(h.quantile(0.999), 0);
    }

    #[test]
    fn merge_adds_the_counts() {
        let mut a = Hist::new();
        let mut b = Hist::new();
        for _ in 0..10 {
            a.add(7);
        }
        for _ in 0..30 {
            b.add(7);
        }
        a.merge(&b);
        assert_eq!(a.total(), 40);
        assert_eq!(a.quantile(0.50), 7);
    }

    /// A tail of one sample in a thousand has to survive to the p999,
    /// since that is the column it exists to fill.
    #[test]
    fn a_lone_outlier_shows_up_at_p999_and_not_at_p99() {
        let mut h = Hist::new();
        for _ in 0..999 {
            h.add(30);
        }
        h.add(500_000);
        assert_eq!(h.quantile(0.99), 30);
        assert!(h.quantile(0.999) >= 30);
        assert!(h.quantile(1.0) >= 480_000);
    }
}
