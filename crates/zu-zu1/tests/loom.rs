//! loom exhaustion of the snapshot epoch ledger. The fault harness
//! covers what a crash can tear; these tests cover what a scheduler
//! can reorder, exhausting every interleaving of the writer publishing
//! commits, readers pinning snapshots, and the checkpoint computing
//! its fold horizon. Run with
//! `RUSTFLAGS="--cfg loom" cargo test -q -p zu-zu1 --test loom --release`.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering::SeqCst};
use loom::thread;

use zu_zu1::epoch::EpochLedger;

/// The reclamation safety property: a completed pin is never below an
/// epoch the writer already reclaimed. The writer thread commits then
/// checkpoints, reclaiming up to the horizon, exactly the program
/// order the engine's `&mut` APIs enforce; the reader pins and checks
/// it did not land in reclaimed territory. The pin re-read loop is
/// what makes this hold: drop it and loom finds the schedule where
/// the scan runs before the pin store lands and the fold seals past
/// the epoch the reader snapshots. Moving the horizon onto its own
/// thread also fails, which is why the ledger contract keeps advance
/// and horizon on the single writer.
#[test]
fn a_pin_never_lands_below_a_reclaimed_epoch() {
    loom::model(|| {
        let ledger = Arc::new(EpochLedger::new(0));
        let reclaimed = Arc::new(AtomicU64::new(0));

        let writer = {
            let ledger = Arc::clone(&ledger);
            let reclaimed = Arc::clone(&reclaimed);
            thread::spawn(move || {
                ledger.advance(1);
                let h = ledger.horizon();
                reclaimed.fetch_max(h, SeqCst);
                ledger.advance(2);
                let h = ledger.horizon();
                reclaimed.fetch_max(h, SeqCst);
            })
        };
        let reader = {
            let ledger = Arc::clone(&ledger);
            let reclaimed = Arc::clone(&reclaimed);
            thread::spawn(move || {
                let e = ledger.pin(0);
                let r = reclaimed.load(SeqCst);
                assert!(
                    e >= r,
                    "reader pinned epoch {e} but the checkpoint reclaimed through {r}"
                );
                ledger.unpin(0);
            })
        };

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// A held pin bounds every horizon computed while it is held, across a
/// concurrent commit: the reader observes its own pin respected, and
/// the checkpoint never reports a horizon above an epoch some
/// completed pin still holds.
#[test]
fn a_held_pin_bounds_the_horizon() {
    loom::model(|| {
        let ledger = Arc::new(EpochLedger::new(0));

        let writer = {
            let ledger = Arc::clone(&ledger);
            thread::spawn(move || {
                ledger.advance(1);
                ledger.advance(2);
            })
        };
        let reader = {
            let ledger = Arc::clone(&ledger);
            thread::spawn(move || {
                let e = ledger.pin(0);
                let h = ledger.horizon();
                assert!(h <= e, "horizon {h} passed a held pin at {e}");
                ledger.unpin(0);
            })
        };

        writer.join().unwrap();
        reader.join().unwrap();
    });
}

/// Two readers racing for different slots each get a committed epoch,
/// and the horizon never exceeds either while both hold their pins.
#[test]
fn concurrent_readers_pin_independently() {
    loom::model(|| {
        let ledger = Arc::new(EpochLedger::new(3));

        let handles: Vec<_> = (0..2)
            .map(|slot| {
                let ledger = Arc::clone(&ledger);
                thread::spawn(move || {
                    let e = ledger.pin(slot);
                    assert_eq!(e, 3, "no writer ran, every pin sees the committed epoch");
                    let h = ledger.horizon();
                    assert!(h <= e);
                    ledger.unpin(slot);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(ledger.horizon(), 3, "released pins free the horizon");
    });
}
