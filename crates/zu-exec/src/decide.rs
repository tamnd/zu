//! The closed set of decisions the pipeline makes while it runs
//! (docs/07 section 7).
//!
//! Everything else about a query is settled before a row moves: which
//! order the joins go in, which side is built, which filter is pushed
//! where. What is left here is the small set of choices that cannot be
//! made off statistics, because they need a number only the data
//! itself has: how many rows a chunk really holds after the pushdown,
//! how many neighbors a particular seed really has, how much of what a
//! filter sees it really rejects.
//!
//! The set is closed on purpose. An engine that may adapt anywhere is
//! an engine whose plan does not predict its behaviour, and a slow
//! query then has no explanation short of a profiler. Eight decisions,
//! each named, each counted, each printed by EXPLAIN ANALYZE, is a
//! budget: a new one has to displace an old one or argue its way in.
//!
//! The eighth argued its way in. How a neighbor list is read, off a
//! group decoded whole or out of the chunks that one list covers, is
//! not something the plan can settle: it turns on how many lists of
//! that group this walk is about to want against how many chunks the
//! group holds, and the second number is storage's, not the
//! optimizer's. It is here rather than buried in the read path because
//! getting it wrong is worth three orders of magnitude on a point
//! query, which is exactly the case where nothing else in this listing
//! has anything to say.
//!
//! None of them persists. Nothing a run learns is written back into
//! the statistics or carried into the next query, so two runs of the
//! same query over the same data make the same decisions from the same
//! evidence. What does move between runs is how the morsels landed on
//! the workers, since a worker judges the filter it holds off the rows
//! it drew. The totals underneath are the same either way, and the
//! rendering says which of the two a line is.

use std::fmt::Write as _;

/// One sideways filter's life over a whole run: what it was asked, what
/// it let through, and how many workers gave up on it.
#[derive(Default, Clone, Copy)]
pub struct SipPass {
    /// Keys handed to the filter while it was still being asked.
    pub probes: u64,
    /// Of those, the ones it could not rule out.
    pub kept: u64,
    /// Workers that stopped asking, having found it was rejecting too
    /// little to pay for itself.
    pub dropped: u32,
    /// Workers that ran this filter at all.
    pub workers: u32,
}

/// How the driving source was cut up, which is one decision made once
/// rather than a counter every worker adds to.
#[derive(Default, Clone)]
pub struct Split {
    /// What was split: the scan's rows, one seed's frontier, or a batch
    /// of keys.
    pub of: &'static str,
    /// Morsels the work came out as.
    pub morsels: usize,
    /// Workers put on them.
    pub threads: usize,
    /// Set when a frontier was weighted by each neighbor's own degree
    /// rather than cut by position, which is the celebrity seed case.
    pub weighted: bool,
}

/// Every decision one run made. Worker-local counts add up, and
/// addition is why the total does not depend on which worker drew
/// which morsel.
#[derive(Default, Clone)]
pub struct Decisions {
    /// Decision 1, made once by the scheduler.
    pub split: Split,
    /// Decision 2: chunks the range pushdown emptied, so their payload
    /// bytes were never touched.
    pub zone_skipped: u64,
    /// Decision 3: chunks that decoded and then had every row rejected
    /// by that same range, which is the pushdown paying for itself
    /// halfway.
    pub zone_thinned: u64,
    /// Decision 4, one entry per sideways filter in the plan.
    pub sip: Vec<SipPass>,
    /// Decision 5: closes that ended before building anything because
    /// the end they close back to had no edges at all.
    pub empty_close: u64,
    /// Decision 6: morsels a bounded sink stopped early, having already
    /// been handed the rows the limit asked for.
    pub quota_stop: u64,
    /// Decision 7: morsels each worker ended up claiming. Nothing
    /// hands these out in advance; a worker takes the next one when it
    /// has finished the last, so the spread here is the only record of
    /// how evenly the work actually fell.
    pub claims: Vec<u64>,
    /// Decision 8, one side: groups decoded whole because the walk
    /// wanted enough of their lists to pay for it.
    pub group_pins: u64,
    /// Decision 8, the other side: times a walk left a group undecoded
    /// and read the chunks its lists cover instead.
    pub point_reads: u64,
}

impl Decisions {
    /// Room for one entry per sideways filter in the plan, since a
    /// worker records against a filter by its position.
    pub fn with_sips(sips: usize) -> Self {
        Self {
            sip: vec![SipPass::default(); sips],
            ..Default::default()
        }
    }

    /// Folds one worker's counts into the run's. The split is the
    /// scheduler's and is not a worker's to report, so it is left
    /// alone here.
    pub fn merge(&mut self, other: &Self) {
        self.zone_skipped += other.zone_skipped;
        self.zone_thinned += other.zone_thinned;
        self.empty_close += other.empty_close;
        self.quota_stop += other.quota_stop;
        self.group_pins += other.group_pins;
        self.point_reads += other.point_reads;
        self.claims.extend_from_slice(&other.claims);
        for (mine, theirs) in self.sip.iter_mut().zip(&other.sip) {
            mine.probes += theirs.probes;
            mine.kept += theirs.kept;
            mine.dropped += theirs.dropped;
            mine.workers += theirs.workers;
        }
    }

    /// One line per decision that had something to say. A decision the
    /// query never reached prints nothing: a listing where every line
    /// is a zero teaches the reader to skip the block.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let weighted = match self.split.weighted {
            true => ", weighted by degree",
            false => "",
        };
        let _ = writeln!(
            out,
            "  split {} into {} morsel(s) over {} worker(s){weighted}",
            self.split.of, self.split.morsels, self.split.threads
        );
        if self.zone_skipped > 0 || self.zone_thinned > 0 {
            let _ = writeln!(
                out,
                "  zone pushdown skipped {} chunk(s) undecoded, emptied {} more after decoding",
                self.zone_skipped, self.zone_thinned
            );
        }
        for (i, pass) in self.sip.iter().enumerate() {
            if pass.probes == 0 && pass.dropped == 0 {
                continue;
            }
            let rejected = pass.probes.saturating_sub(pass.kept);
            let share = match pass.probes {
                0 => 0.0,
                n => rejected as f64 * 100.0 / n as f64,
            };
            let _ = writeln!(
                out,
                "  sideways filter {i} rejected {rejected} of {} probe(s), {share:.1}%, \
                 dropped by {} of {} worker(s)",
                pass.probes, pass.dropped, pass.workers
            );
        }
        if self.empty_close > 0 {
            let _ = writeln!(
                out,
                "  close ended early on {} vector(s), the far end had no edges",
                self.empty_close
            );
        }
        if self.group_pins > 0 || self.point_reads > 0 {
            let _ = writeln!(
                out,
                "  decoded {} whole group(s) and read around {} more, \
                 list by list",
                self.group_pins, self.point_reads
            );
        }
        if self.claims.len() > 1 {
            let busiest = self.claims.iter().max().copied().unwrap_or(0);
            let idlest = self.claims.iter().min().copied().unwrap_or(0);
            let _ = writeln!(
                out,
                "  workers claimed {busiest} morsel(s) at most and {idlest} at least",
            );
        }
        if self.quota_stop > 0 {
            let _ = writeln!(
                out,
                "  bounded sink stopped {} morsel(s) with the limit already met",
                self.quota_stop
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_workers_adds_their_counts_and_leaves_the_split_alone() {
        let mut a = Decisions::with_sips(1);
        a.split = Split {
            of: "scan",
            morsels: 8,
            threads: 4,
            weighted: false,
        };
        a.zone_skipped = 3;
        a.sip[0] = SipPass {
            probes: 100,
            kept: 10,
            dropped: 0,
            workers: 1,
        };
        let mut b = Decisions::with_sips(1);
        // A worker reports no split of its own, and merging must not
        // take this default for an answer.
        b.zone_skipped = 4;
        b.sip[0] = SipPass {
            probes: 50,
            kept: 40,
            dropped: 1,
            workers: 1,
        };
        a.merge(&b);
        assert_eq!(a.zone_skipped, 7);
        assert_eq!(a.sip[0].probes, 150);
        assert_eq!(a.sip[0].kept, 50);
        assert_eq!(a.sip[0].dropped, 1);
        assert_eq!(a.sip[0].workers, 2);
        assert_eq!(
            a.split.morsels, 8,
            "the scheduler's split is not a worker's"
        );
    }

    #[test]
    fn a_decision_nothing_reached_prints_nothing() {
        let mut d = Decisions::with_sips(1);
        d.split = Split {
            of: "scan",
            morsels: 1,
            threads: 1,
            weighted: false,
        };
        let text = d.render();
        assert_eq!(
            text.trim(),
            "split scan into 1 morsel(s) over 1 worker(s)",
            "got:\n{text}"
        );

        d.sip[0].probes = 8;
        d.sip[0].kept = 2;
        d.sip[0].workers = 1;
        d.empty_close = 2;
        d.point_reads = 3;
        let text = d.render();
        assert!(
            text.contains("decoded 0 whole group(s) and read around 3 more"),
            "got:\n{text}"
        );
        assert!(
            text.contains("rejected 6 of 8 probe(s), 75.0%"),
            "got:\n{text}"
        );
        assert!(
            text.contains("close ended early on 2 vector(s)"),
            "got:\n{text}"
        );
        assert!(!text.contains("bounded sink"), "got:\n{text}");
    }
}
