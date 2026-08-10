//! Snapshot epoch accounting: the ledger concurrent readers pin and
//! the checkpoint consults for its fold horizon.
//!
//! The single-threaded facade gets snapshot isolation for free from
//! the borrow checker; the shared-reader facade in docs/09 does not,
//! and this ledger is its accounting. The writer publishes each commit
//! with [`EpochLedger::advance`], a reader claims a slot and pins the
//! committed epoch with [`EpochLedger::pin`], and the checkpoint asks
//! [`EpochLedger::horizon`] how far it may fold: never past an epoch a
//! reader still sees.
//!
//! Three rules carry the whole safety argument, and the loom suite in
//! `tests/loom.rs` exhausts the interleavings behind them. A reader
//! must re-read the committed epoch after storing its pin, because a
//! horizon scan may have missed the store. The pin's store then
//! re-read and the horizon's publish then scan are each split by a
//! SeqCst fence, the Dekker shape: either the reader's fence comes
//! first in the fence order and the scan sees the pin, or the
//! writer's comes first and the re-read sees the advanced epoch and
//! retries; plain acquire and release orderings allow the store-load
//! reordering that breaks both arms, and loom finds it immediately.
//! And [`EpochLedger::advance`] and [`EpochLedger::horizon`] belong
//! to the single writer thread, which the engine's `&mut` commit and
//! checkpoint APIs already serialize. loom rejected the alternative:
//! a checkpoint thread computing the horizon independently can
//! observe a concurrent commit that a racing reader's re-read does
//! not, scan before that reader's pin lands, and reclaim past the
//! epoch the reader snapshots, because no fence pairing orders a
//! third thread's scan against an advance it did not perform.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering::SeqCst, fence};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering::SeqCst, fence};

use zu_common::Epoch;

/// Fixed reader capacity; the docs/09 facade hands each reader thread
/// one slot for its lifetime.
pub const READER_SLOTS: usize = 8;

const EMPTY: u64 = u64::MAX;

/// The committed epoch and one pin slot per reader. `EMPTY` marks an
/// unpinned slot.
#[derive(Debug)]
pub struct EpochLedger {
    committed: AtomicU64,
    slots: [AtomicU64; READER_SLOTS],
}

impl EpochLedger {
    /// A ledger with no pinned readers at the given committed epoch.
    pub fn new(epoch: Epoch) -> Self {
        EpochLedger {
            committed: AtomicU64::new(epoch),
            slots: std::array::from_fn(|_| AtomicU64::new(EMPTY)),
        }
    }

    /// The newest committed epoch.
    pub fn committed(&self) -> Epoch {
        self.committed.load(SeqCst)
    }

    /// Publishes `epoch` as committed. The writer calls this after its
    /// WAL frame syncs and its overlays publish; epochs only advance,
    /// and only the single writer thread calls this.
    pub fn advance(&self, epoch: Epoch) {
        let prev = self.committed.swap(epoch, SeqCst);
        debug_assert!(prev <= epoch, "committed epoch moved backwards");
    }

    /// Pins the committed epoch into `slot` and returns it. The store,
    /// fence, then re-read loop closes the race with a concurrent
    /// [`Self::horizon`]: a scan that missed this pin fenced before
    /// this pin's fence, so its advance is visible to the re-read,
    /// which then retries at the newer epoch. The returned epoch is
    /// never below a horizon the writer computes.
    pub fn pin(&self, slot: usize) -> Epoch {
        loop {
            let epoch = self.committed.load(SeqCst);
            self.slots[slot].store(epoch, SeqCst);
            fence(SeqCst);
            if self.committed.load(SeqCst) == epoch {
                return epoch;
            }
        }
    }

    /// Releases `slot`; the reader's snapshot no longer holds the
    /// horizon back.
    pub fn unpin(&self, slot: usize) {
        self.slots[slot].store(EMPTY, SeqCst);
    }

    /// The oldest epoch any current reader may still see, the upper
    /// bound for a fold. Must run on the writer thread: the fence
    /// pairs against each pinning reader's, so a pin the scan misses
    /// re-read this thread's last advance and retried at or above it,
    /// while a pin the re-read beat is seen by the scan and bounds the
    /// result. Computed on any other thread that pairing says nothing
    /// about a concurrent advance, and loom finds the
    /// reclaim-past-a-pin schedule immediately.
    pub fn horizon(&self) -> Epoch {
        fence(SeqCst);
        let mut horizon = self.committed.load(SeqCst);
        for slot in &self.slots {
            let pinned = slot.load(SeqCst);
            if pinned != EMPTY {
                horizon = horizon.min(pinned);
            }
        }
        horizon
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// With no readers the horizon is the committed epoch itself: a
    /// fold may seal everything.
    #[test]
    fn unpinned_horizon_is_committed() {
        let ledger = EpochLedger::new(3);
        assert_eq!(ledger.committed(), 3);
        assert_eq!(ledger.horizon(), 3);
        ledger.advance(5);
        assert_eq!(ledger.horizon(), 5);
    }

    /// A pin holds the horizon at its epoch across later commits, and
    /// releasing it lets the horizon catch up.
    #[test]
    fn pins_hold_the_horizon_back() {
        let ledger = EpochLedger::new(2);
        let e = ledger.pin(0);
        assert_eq!(e, 2);
        ledger.advance(7);
        assert_eq!(ledger.horizon(), 2);
        ledger.unpin(0);
        assert_eq!(ledger.horizon(), 7);
    }

    /// The horizon is the minimum over every pinned reader.
    #[test]
    fn horizon_is_the_oldest_pin() {
        let ledger = EpochLedger::new(1);
        ledger.pin(0);
        ledger.advance(4);
        ledger.pin(1);
        ledger.advance(9);
        assert_eq!(ledger.horizon(), 1);
        ledger.unpin(0);
        assert_eq!(ledger.horizon(), 4);
        ledger.unpin(1);
        assert_eq!(ledger.horizon(), 9);
    }
}
