//! Epoch protection and deferred reclamation.
//!
//! Fraser's scheme (Practical Lock-Freedom, Cambridge, 2004) in the
//! engineering form FASTER uses. A thread announces the epoch it is
//! working in before it touches the log and stands down after, so
//! anything freed while it was working stays alive until it has moved
//! on. Everything in this crate that hands out a raw pointer into a log
//! page relies on it.
//!
//! Two things use it here. Page eviction defers the free of a page's
//! memory, so a reader holding a pointer into that page finishes before
//! the bytes go away. The flusher uses the same counter for a different
//! purpose: to learn when every thread that allocated below a given
//! address has finished writing its record, which is what makes a
//! partial flush of the tail page safe without a per-page byte counter.
//!
//! A slot is claimed by a [`Session`] rather than by a thread local, so
//! that a thread's participation is explicit and its cost is paid once
//! rather than on every operation. That is FASTER's session model and
//! it is why a benchmark thread holds one session for its whole run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// A slot nobody has claimed.
const FREE: u64 = 0;

/// A slot claimed by a session that is not inside an operation.
const IDLE: u64 = u64::MAX;

/// One session's announced epoch, on its own cacheline so that two
/// sessions announcing do not fight over one line.
#[repr(align(64))]
struct Slot(AtomicU64);

type Action = Box<dyn FnOnce() + Send>;

/// The epoch counter, the announced epochs, and the deferred work.
pub struct Epochs {
    current: AtomicU64,
    slots: Box<[Slot]>,
    /// One past the highest slot ever claimed.
    ///
    /// Every durable commit computes the safe epoch, and computing it
    /// over all of `slots` reads a cacheline per slot whether or not
    /// anyone is using it. A run with four workers and room for a
    /// hundred and twenty eight sessions paid for a hundred and twenty
    /// four idle cachelines on every commit. The mark only rises, so a
    /// scan bounded by it can never miss a claimed slot.
    claimed: AtomicUsize,
    /// Deferred actions with the epoch they were queued in. A mutex is
    /// right here: the queue is touched on eviction and on drain, never
    /// on the read or write path.
    deferred: Mutex<Vec<(u64, Action)>>,
}

impl Epochs {
    /// Room for `sessions` concurrent sessions.
    pub fn new(sessions: usize) -> Self {
        Self {
            // Epoch 0 is never announced, so a reclaim safe point of 0
            // means nothing has been queued yet.
            current: AtomicU64::new(1),
            slots: (0..sessions).map(|_| Slot(AtomicU64::new(FREE))).collect(),
            claimed: AtomicUsize::new(0),
            deferred: Mutex::new(Vec::new()),
        }
    }

    /// The epoch new work joins.
    #[inline]
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    /// Moves everyone forward, which is what lets deferred work retire.
    /// Returns the epoch that was current before the bump, so the
    /// caller can wait for exactly that one to drain.
    pub fn bump(&self) -> u64 {
        self.current.fetch_add(1, Ordering::AcqRel)
    }

    /// The oldest epoch any session is still working in. Everything
    /// strictly older than this is unreachable.
    pub fn safe_epoch(&self) -> u64 {
        let mut safe = self.current();
        let claimed = self.claimed.load(Ordering::Acquire);
        for slot in &self.slots[..claimed] {
            let announced = slot.0.load(Ordering::Acquire);
            if announced != FREE && announced != IDLE && announced < safe {
                safe = announced;
            }
        }
        safe
    }

    /// Runs `action` once no session can still be looking at whatever
    /// it retires.
    pub fn defer(&self, action: Action) {
        let epoch = self.current();
        self.deferred
            .lock()
            .expect("zu2 epoch queue")
            .push((epoch, action));
    }

    /// Runs the deferred actions whose epoch has passed. Cheap when the
    /// queue is empty, which is the common case.
    pub fn drain(&self) {
        if self.deferred.lock().expect("zu2 epoch queue").is_empty() {
            return;
        }
        let safe = self.safe_epoch();
        let mut ready = Vec::new();
        {
            let mut queue = self.deferred.lock().expect("zu2 epoch queue");
            let mut i = 0;
            while i < queue.len() {
                if queue[i].0 < safe {
                    ready.push(queue.swap_remove(i).1);
                } else {
                    i += 1;
                }
            }
        }
        for action in ready {
            action();
        }
    }

    /// Blocks until every session that was inside an operation when
    /// this was called has left it.
    ///
    /// This is the flusher's quiescence wait. It is correct because the
    /// bump puts new work in a later epoch, so waiting for the safe
    /// epoch to pass the bumped one waits for exactly the sessions that
    /// were already running.
    pub fn wait_for_quiescence(&self) {
        let epoch = self.bump();
        let mut spins = 0u32;
        while self.safe_epoch() <= epoch {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn claim(&self) -> usize {
        for (i, slot) in self.slots.iter().enumerate() {
            if slot
                .0
                .compare_exchange(FREE, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Published after the slot is IDLE rather than before,
                // so a scan that sees the raised mark always sees a slot
                // that is at worst idle, never one still reading FREE
                // from a session about to announce.
                self.claimed.fetch_max(i + 1, Ordering::AcqRel);
                return i;
            }
        }
        panic!(
            "zu2: all {} epoch sessions are in use, raise Options::sessions",
            self.slots.len()
        );
    }
}

/// A claimed slot. Held for the life of a worker, not per operation.
pub struct Slotted<'a> {
    epochs: &'a Epochs,
    slot: usize,
}

impl<'a> Slotted<'a> {
    pub fn new(epochs: &'a Epochs) -> Self {
        let slot = epochs.claim();
        Self { epochs, slot }
    }

    /// Announces the current epoch. Every operation that dereferences
    /// an address opens with this.
    #[inline]
    pub fn protect(&self) {
        self.epochs.slots[self.slot]
            .0
            .store(self.epochs.current(), Ordering::Release);
    }

    /// Stands down, so nothing this session did holds reclamation up.
    #[inline]
    pub fn unprotect(&self) {
        self.epochs.slots[self.slot]
            .0
            .store(IDLE, Ordering::Release);
    }
}

impl Drop for Slotted<'_> {
    fn drop(&mut self) {
        self.epochs.slots[self.slot]
            .0
            .store(FREE, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn deferred_work_waits_for_the_session_that_could_see_it() {
        let epochs = Epochs::new(4);
        let session = Slotted::new(&epochs);
        session.protect();
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        epochs.defer(Box::new(move || {
            flag.fetch_add(1, Ordering::Release);
        }));
        // The session is still inside its operation, so no bump can
        // retire the action.
        for _ in 0..4 {
            epochs.bump();
            epochs.drain();
        }
        assert_eq!(ran.load(Ordering::Acquire), 0, "retired too early");
        session.unprotect();
        epochs.bump();
        epochs.drain();
        assert_eq!(ran.load(Ordering::Acquire), 1, "never retired");
    }

    #[test]
    fn an_idle_session_does_not_hold_the_safe_epoch_back() {
        let epochs = Epochs::new(4);
        let _idle = Slotted::new(&epochs);
        let before = epochs.current();
        epochs.bump();
        assert!(epochs.safe_epoch() > before, "an idle slot pinned reclaim");
    }

    #[test]
    fn a_dropped_session_frees_its_slot() {
        let epochs = Epochs::new(1);
        {
            let s = Slotted::new(&epochs);
            s.protect();
        }
        let s = Slotted::new(&epochs);
        s.protect();
        s.unprotect();
    }

    #[test]
    fn quiescence_returns_once_the_running_sessions_leave() {
        let epochs = Arc::new(Epochs::new(4));
        let started = Arc::new(AtomicUsize::new(0));
        let worker = {
            let epochs = Arc::clone(&epochs);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                let s = Slotted::new(&epochs);
                s.protect();
                started.fetch_add(1, Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(20));
                s.unprotect();
            })
        };
        while started.load(Ordering::Acquire) == 0 {
            std::hint::spin_loop();
        }
        epochs.wait_for_quiescence();
        worker.join().expect("worker");
    }
}
