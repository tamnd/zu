//! Epoch protection and deferred reclamation.
//!
//! Fraser's scheme (Practical Lock-Freedom, Cambridge, 2004) in the
//! engineering form FASTER uses. A thread announces the epoch it is
//! working in before it touches the log and stands down after, so
//! anything freed while it was working stays alive until it has moved
//! on. Everything in this crate that hands out a raw pointer into a log
//! page relies on it.
//!
//! Page eviction is what uses it: the free of a page's memory is
//! deferred, so a reader holding a pointer into that page finishes
//! before the bytes go away.
//!
//! The flusher needs a different answer, and it used to take it from the
//! same counter. It has to know that every record below the address it
//! is about to write is complete, and waiting for the epoch to turn over
//! answers that by waiting for every session to leave whatever it was
//! doing, readers included. That is far more than the question asked, and
//! on a busy database it made a commit cost what the other threads were
//! doing rather than what the device charges. Each session now publishes
//! the lowest address it may be writing, so the flusher can ask its own
//! question directly and a reader never appears in the answer.
//!
//! A slot is claimed by a [`Session`] rather than by a thread local, so
//! that a thread's participation is explicit and its cost is paid once
//! rather than on every operation. That is FASTER's session model and
//! it is why a benchmark thread holds one session for its whole run.
//!
//! Quiescence needed the same separation the flusher got, and for the
//! same reason. A borrowing scan stands its epoch up, hands the host
//! pointers into log pages, and returns, and the epoch stays announced
//! for as long as the host is reading them: that is exactly right for
//! reclamation, which is the question `safe_epoch` answers. It is wrong
//! for [`Epochs::wait_for_quiescence`], whose four callers all want to
//! know whether a session is *inside* an operation, because a session
//! that is parked between calls is not, and a wait that counts it never
//! returns at all. So a parked slot announces that too, and quiescence
//! skips it while reclamation does not. See [`Slotted::park`].

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// A slot nobody has claimed.
const FREE: u64 = 0;

/// A slot claimed by a session that is not inside an operation.
const IDLE: u64 = u64::MAX;

/// The frontier of a session that is not writing anything. Above every
/// real address, so a session at rest never holds the floor down.
pub const NOWHERE: u64 = u64::MAX;

/// Slots the engine keeps for itself, at the front of the table.
///
/// Flushing and compaction each want a session, and a host that sized
/// `Options::sessions` for its workers has none to give: that is the
/// sizing the option invites, and before this the background compactor
/// panicked in a thread nobody was watching when it hit it. Two, because
/// the background maintainer and a foreground `Db::compact` can both be
/// running.
///
/// At the front rather than the end because [`Epochs::claimed`] is a
/// high water mark. A reserved slot at the end would raise it past every
/// idle worker slot, and every durable commit would then read a
/// cacheline for each of them.
const RESERVED: usize = 2;

/// One session's announced epoch and write frontier, on their own
/// cacheline so that two sessions announcing do not fight over one line.
/// The two live together because a write announces both and pays for one
/// line rather than two.
#[repr(align(64))]
struct Slot {
    epoch: AtomicU64,
    writing: AtomicU64,
    /// Whether this session announced its epoch and then left the
    /// engine, which is what a borrowing scan does. See
    /// [`Slotted::park`]. On the same line as the other two: it is read
    /// by quiescence, which already reads the epoch beside it.
    parked: AtomicBool,
}

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
    /// Whether the gate is shut, read once by every operation.
    ///
    /// An atomic beside the mutex rather than the mutex alone, because
    /// the answer is no on every operation of every run that never takes
    /// a checkpoint, and the whole cost of asking should be one load of
    /// a line nobody writes. See [`Epochs::shut`].
    barred: AtomicBool,
    /// The gate itself, and who is waiting at it.
    gate: Mutex<bool>,
    lifted: Condvar,
}

impl Epochs {
    /// Room for `sessions` of the host's sessions, plus the engine's own.
    pub fn new(sessions: usize) -> Self {
        Self {
            // Epoch 0 is never announced, so a reclaim safe point of 0
            // means nothing has been queued yet.
            current: AtomicU64::new(1),
            slots: (0..sessions + RESERVED)
                .map(|_| Slot {
                    epoch: AtomicU64::new(FREE),
                    writing: AtomicU64::new(NOWHERE),
                    parked: AtomicBool::new(false),
                })
                .collect(),
            claimed: AtomicUsize::new(0),
            deferred: Mutex::new(Vec::new()),
            barred: AtomicBool::new(false),
            gate: Mutex::new(false),
            lifted: Condvar::new(),
        }
    }

    /// Shuts the gate, so that a session which is not inside an
    /// operation cannot start one.
    ///
    /// This is half of a barrier and it is useless alone. The other half
    /// is [`Epochs::wait_for_quiescence`], which waits out the sessions
    /// that were already inside one when this was called. After both,
    /// and until [`Epochs::lift`], nothing in the engine is reading or
    /// writing either plane, which is what a checkpoint needs to write
    /// down a state that a log address can be named for.
    ///
    /// The engine's own sessions go through the gate rather than wait at
    /// it. The flusher is why: a checkpoint makes its boundary durable
    /// before it captures anything, and the thread that would carry that
    /// write to the device is the one the gate would have stopped, so a
    /// gate that held it would be waiting for a flush that is waiting
    /// for the gate. What keeps compaction out is not this but the
    /// maintenance lock, which is the right tool for it: compaction is
    /// off the hot path and can afford a mutex, and a checkpoint has to
    /// exclude it for a whole pass rather than for an operation.
    pub fn shut(&self) {
        let mut gate = self.gate.lock().expect("zu2 gate");
        *gate = true;
        self.barred.store(true, Ordering::SeqCst);
    }

    /// Opens the gate and wakes everyone waiting at it.
    pub fn lift(&self) {
        let mut gate = self.gate.lock().expect("zu2 gate");
        *gate = false;
        self.barred.store(false, Ordering::SeqCst);
        drop(gate);
        self.lifted.notify_all();
    }

    /// Whether the gate is shut. One load of a read-mostly line.
    #[inline]
    fn barred(&self) -> bool {
        self.barred.load(Ordering::Acquire)
    }

    /// Waits until the gate opens. Called by a session that has already
    /// stood its epoch down, which is what stops the wait from holding
    /// up the quiescence the gate closer is waiting for.
    fn wait_at_gate(&self) {
        let mut gate = self.gate.lock().expect("zu2 gate");
        while *gate {
            gate = self.lifted.wait(gate).expect("zu2 gate");
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
    ///
    /// The fence is half of a pair with the one in [`Slotted::protect`],
    /// and it is what makes the answer mean anything. A caller gets here
    /// after publishing whatever it wants the sessions to see, and a
    /// session announces before it reads. Without the fences those are a
    /// store and a load on each side with nothing between them, so both
    /// sides may miss the other: the scan reads a slot that has not
    /// announced yet and calls the epoch drained, while the session that
    /// was about to announce goes on reading what the caller has already
    /// replaced. With them the two orders cannot both come out that way,
    /// so either the scan sees the announcement and waits, or the
    /// session sees the publication and takes the new one (#465).
    pub fn safe_epoch(&self) -> u64 {
        std::sync::atomic::fence(Ordering::SeqCst);
        let mut safe = self.current();
        let claimed = self.claimed.load(Ordering::Acquire);
        for slot in &self.slots[..claimed] {
            let announced = slot.epoch.load(Ordering::Acquire);
            if announced != FREE && announced != IDLE && announced < safe {
                safe = announced;
            }
        }
        safe
    }

    /// The oldest epoch any session that is *inside an operation* is
    /// still working in.
    ///
    /// The same scan as [`Epochs::safe_epoch`] with the parked slots
    /// left out, and it answers a different question. Reclamation asks
    /// who might still dereference an address, and a parked session
    /// might, so it counts there. Quiescence asks who is still running,
    /// and a parked session is not, so it does not count here.
    ///
    /// Nothing may free memory on the strength of this. It is only for
    /// the four callers of [`Epochs::wait_for_quiescence`], every one of
    /// which is waiting out an in-flight operation rather than a held
    /// pointer: two of them a `pread` that could land in a punched hole
    /// (#563), one an append the capture would otherwise miss, one a
    /// walk of the index table that is about to be rewritten.
    fn running_epoch(&self) -> u64 {
        std::sync::atomic::fence(Ordering::SeqCst);
        let mut safe = self.current();
        let claimed = self.claimed.load(Ordering::Acquire);
        for slot in &self.slots[..claimed] {
            let announced = slot.epoch.load(Ordering::Acquire);
            if announced == FREE || announced == IDLE || announced >= safe {
                continue;
            }
            // The park is read after the epoch. A session parks after it
            // has announced and unparks before it does anything, so a
            // reader that saw the announcement and then sees the park
            // flag set is looking at a session that has genuinely left.
            if slot.parked.load(Ordering::Acquire) {
                continue;
            }
            safe = announced;
        }
        safe
    }

    /// The lowest address any session may still be writing, or `ceiling`
    /// when none of them is writing below it.
    ///
    /// Everything below the answer is a complete record, which is what
    /// the flusher needs to know before it hands bytes to the device.
    ///
    /// Two orderings carry this. The caller reads the tail before it
    /// calls, and an appending session publishes its frontier before it
    /// claims tail space, so a caller whose tail read saw the claim also
    /// sees the frontier that went with it. A session updating a record
    /// in place has no tail claim to carry it, so that one is a sequenced
    /// store followed by a sequenced load of the flush target on its
    /// side and the mirror image on the flusher's, which leaves no
    /// interleaving where both of them think they have the bytes.
    pub fn write_floor(&self, ceiling: u64) -> u64 {
        let mut floor = ceiling;
        let claimed = self.claimed.load(Ordering::Acquire);
        for slot in &self.slots[..claimed] {
            let at = slot.writing.load(Ordering::SeqCst);
            if at < floor {
                floor = at;
            }
        }
        floor
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

    /// Runs every deferred action whatever epoch it was queued in.
    ///
    /// Only for a caller that knows nothing is running, which in
    /// practice is a log being dropped. [`Epochs::drain`] cannot do this
    /// job: an action queued in the current epoch is never older than
    /// the safe epoch, so a queue that was filled after the last bump
    /// would go to the allocator unfreed.
    pub fn retire_all(&self) {
        let queued: Vec<_> = self
            .deferred
            .lock()
            .expect("zu2 epoch queue")
            .drain(..)
            .collect();
        for (_, action) in queued {
            action();
        }
    }

    /// Deferred actions that have not run yet.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.deferred.lock().expect("zu2 epoch queue").len()
    }

    /// Blocks until every session that was inside an operation when
    /// this was called has left it.
    ///
    /// This is the flusher's quiescence wait. It is correct because the
    /// bump puts new work in a later epoch, so waiting for the safe
    /// epoch to pass the bumped one waits for exactly the sessions that
    /// were already running.
    ///
    /// A session parked on a borrow is not one of them, and this used to
    /// wait for it regardless, which is a wait with no bound: the park
    /// ends when the host comes back and the host may be doing anything.
    /// A compaction pass beside a borrowing scan spun here forever
    /// (#738). It reads [`Epochs::running_epoch`] for that reason.
    pub fn wait_for_quiescence(&self) {
        let epoch = self.bump();
        let mut spins = 0u32;
        while self.running_epoch() <= epoch {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    /// How many sessions the host was given room for.
    pub fn sessions(&self) -> usize {
        self.slots.len() - RESERVED
    }

    fn take(&self, from: usize, upto: usize) -> Option<usize> {
        for (i, slot) in self.slots[from..upto].iter().enumerate() {
            if slot
                .epoch
                .compare_exchange(FREE, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Published after the slot is IDLE rather than before,
                // so a scan that sees the raised mark always sees a slot
                // that is at worst idle, never one still reading FREE
                // from a session about to announce.
                self.claimed.fetch_max(from + i + 1, Ordering::AcqRel);
                return Some(from + i);
            }
        }
        None
    }

    /// A slot for one of the host's sessions, or `None` when it has
    /// opened as many as it asked for.
    fn claim(&self) -> Option<usize> {
        self.take(RESERVED, self.slots.len())
    }

    /// A slot for the engine's own work. The reserved ones first, and
    /// the host's range after that, because a spare worker slot is
    /// better spent on a compaction pass than left idle.
    fn claim_reserved(&self) -> Option<usize> {
        self.take(0, RESERVED)
            .or_else(|| self.take(RESERVED, self.slots.len()))
    }
}

/// A claimed slot. Held for the life of a worker, not per operation.
pub struct Slotted<'a> {
    epochs: &'a Epochs,
    slot: usize,
    /// Whether this is one of the engine's own sessions, which pass
    /// through a shut gate rather than wait at it. See [`Epochs::shut`].
    engine: bool,
}

impl<'a> Slotted<'a> {
    /// A slot for the host, or `None` when its sessions are all open.
    pub fn claim(epochs: &'a Epochs) -> Option<Self> {
        epochs.claim().map(|slot| Self {
            epochs,
            slot,
            engine: false,
        })
    }

    /// Whether this slot belongs to the engine rather than to the host.
    /// The log lets these spend the compaction reserve, since a pass has
    /// to copy a record forward before it can free the space behind it.
    /// See [`crate::log::Log::allocate`].
    pub fn is_engine(&self) -> bool {
        self.engine
    }

    /// A slot for the engine's own flushing and compaction, which the
    /// host cannot take.
    pub fn reserved(epochs: &'a Epochs) -> Option<Self> {
        epochs.claim_reserved().map(|slot| Self {
            epochs,
            slot,
            engine: true,
        })
    }

    /// Announces the current epoch. Every operation that dereferences
    /// an address opens with this.
    ///
    /// The announcement is a swap and not a store, and the ordering is
    /// sequential and not release, because everything this session is
    /// about to read is something another thread may be in the middle of
    /// replacing and the announcement is the only thing telling that
    /// thread to wait. A plain store may still be sitting in this core's
    /// store buffer while the reads that follow it are already running,
    /// which is the one reordering x86 allows and the one that matters
    /// here: the scan in [`Epochs::safe_epoch`] would see a slot that
    /// has not announced, call the epoch drained, and free a table this
    /// session is about to walk into (#465). A swap is a locked
    /// instruction, so it drains the buffer as part of doing the store,
    /// which costs less than a store and a separate fence.
    ///
    /// The epoch it announces can be one behind by the time it lands,
    /// and that is fine: an epoch older than the truth holds reclamation
    /// back rather than letting it run early.
    /// The gate is read after the announcement and not before, which is
    /// the order that makes the barrier hold. A session that read an
    /// open gate and then announced could announce after the closer had
    /// already seen every slot idle, and would then be inside an
    /// operation the closer believes nobody is inside. Announcing first
    /// means the closer either sees the announcement, and waits for it,
    /// or has already shut the gate, and this sees that and stands down.
    ///
    /// The park flag is cleared first and unconditionally. An operation
    /// that starts is by definition running, and clearing before the
    /// announcement rather than after means there is no window in which
    /// a slot is announced in the new epoch and still reads as parked,
    /// which is the one interleaving that would let a quiescence wait
    /// skip a session that is inside an operation.
    #[inline]
    pub fn protect(&self) {
        loop {
            self.epochs.slots[self.slot]
                .parked
                .store(false, Ordering::Release);
            self.epochs.slots[self.slot]
                .epoch
                .swap(self.epochs.current(), Ordering::SeqCst);
            if self.engine || !self.epochs.barred() {
                return;
            }
            self.unprotect();
            self.epochs.wait_at_gate();
        }
    }

    /// Stands down, so nothing this session did holds reclamation up.
    #[inline]
    pub fn unprotect(&self) {
        self.epochs.slots[self.slot]
            .parked
            .store(false, Ordering::Release);
        self.epochs.slots[self.slot]
            .epoch
            .store(IDLE, Ordering::Release);
    }

    /// Keeps the epoch announced while this session leaves the engine.
    ///
    /// A borrowing scan is the caller: it hands the host pointers into
    /// log pages and returns, and the announcement is the only thing
    /// keeping those pages from being freed under it. What the park adds
    /// is the other half of the truth, that nobody is running on this
    /// slot, so [`Epochs::wait_for_quiescence`] does not sit waiting for
    /// an operation that has already finished.
    ///
    /// The cost is real and it is paid in space, not in a stall:
    /// reclamation genuinely is held at this epoch until the host comes
    /// back, so deferred frees queue up behind a park the same way they
    /// queue behind a long operation. A host that parks and never
    /// returns pins the log. That is a lease and it is the host's to
    /// end, which is what [`crate::Session::scan_release`] is for, and
    /// every entry point calls it before it does anything else.
    #[inline]
    pub fn park(&self) {
        self.epochs.slots[self.slot]
            .parked
            .store(true, Ordering::Release);
    }

    /// Announces that this session is about to claim tail space at or
    /// above `address` and write a record there.
    ///
    /// A release store is enough because the claim itself follows, and a
    /// flusher that saw the claim in the tail therefore sees this. A
    /// flusher that did not see the claim has a tail below the record
    /// and is bounded by that instead.
    #[inline]
    pub fn appending_at(&self, address: u64) {
        self.epochs.slots[self.slot]
            .writing
            .store(address, Ordering::Release);
    }

    /// Announces that this session is about to rewrite the record at
    /// `address` where it lies.
    ///
    /// Nothing follows this to carry it, so it is sequenced, and the
    /// caller reads the flush target sequenced straight after. See
    /// [`Epochs::write_floor`].
    #[inline]
    pub fn updating_at(&self, address: u64) {
        self.epochs.slots[self.slot]
            .writing
            .store(address, Ordering::SeqCst);
    }

    /// Announces that the record is complete, so the flusher may take
    /// it. Release, so the bytes are visible to whoever sees this.
    #[inline]
    pub fn wrote(&self) {
        self.epochs.slots[self.slot]
            .writing
            .store(NOWHERE, Ordering::Release);
    }
}

impl Drop for Slotted<'_> {
    fn drop(&mut self) {
        self.epochs.slots[self.slot]
            .writing
            .store(NOWHERE, Ordering::Release);
        self.epochs.slots[self.slot]
            .parked
            .store(false, Ordering::Release);
        self.epochs.slots[self.slot]
            .epoch
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
        let session = Slotted::claim(&epochs).expect("slot");
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
    fn retire_all_runs_what_a_drain_cannot() {
        let epochs = Epochs::new(4);
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        epochs.defer(Box::new(move || {
            flag.fetch_add(1, Ordering::Release);
        }));
        // Queued in the current epoch, so it is not older than the safe
        // epoch and no drain will ever take it.
        epochs.drain();
        assert_eq!(ran.load(Ordering::Acquire), 0, "a drain took it");
        epochs.retire_all();
        assert_eq!(
            ran.load(Ordering::Acquire),
            1,
            "it was dropped on the floor"
        );
        assert_eq!(epochs.pending(), 0);
    }

    #[test]
    fn an_idle_session_does_not_hold_the_safe_epoch_back() {
        let epochs = Epochs::new(4);
        let _idle = Slotted::claim(&epochs).expect("slot");
        let before = epochs.current();
        epochs.bump();
        assert!(epochs.safe_epoch() > before, "an idle slot pinned reclaim");
    }

    #[test]
    fn a_dropped_session_frees_its_slot() {
        let epochs = Epochs::new(1);
        {
            let s = Slotted::claim(&epochs).expect("slot");
            s.protect();
        }
        let s = Slotted::claim(&epochs).expect("slot");
        s.protect();
        s.unprotect();
    }

    #[test]
    fn a_writing_session_holds_the_write_floor_and_a_reader_does_not() {
        let epochs = Epochs::new(4);
        let reader = Slotted::claim(&epochs).expect("slot");
        reader.protect();
        assert_eq!(epochs.write_floor(100), 100, "a reader held the floor");
        let writer = Slotted::claim(&epochs).expect("slot");
        writer.appending_at(40);
        assert_eq!(epochs.write_floor(100), 40, "the writer did not hold it");
        writer.wrote();
        assert_eq!(epochs.write_floor(100), 100, "the floor did not come back");
        writer.updating_at(60);
        assert_eq!(epochs.write_floor(100), 60, "an in-place write is a write");
        writer.wrote();
        assert_eq!(epochs.write_floor(100), 100);
    }

    #[test]
    fn a_dropped_session_does_not_hold_the_write_floor() {
        let epochs = Epochs::new(2);
        {
            let writer = Slotted::claim(&epochs).expect("slot");
            writer.appending_at(40);
        }
        assert_eq!(epochs.write_floor(100), 100, "a gone session pinned it");
    }

    #[test]
    fn a_parked_session_still_holds_reclamation_back() {
        // The whole point of a park. The bytes the host is reading are
        // in a page whose free is sitting on this queue, and it must not
        // run until the park ends.
        let epochs = Epochs::new(4);
        let session = Slotted::claim(&epochs).expect("slot");
        session.protect();
        session.park();
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&ran);
        epochs.defer(Box::new(move || {
            flag.fetch_add(1, Ordering::Release);
        }));
        for _ in 0..4 {
            epochs.bump();
            epochs.drain();
        }
        assert_eq!(
            ran.load(Ordering::Acquire),
            0,
            "a page was freed under a parked borrow"
        );
        session.unprotect();
        epochs.bump();
        epochs.drain();
        assert_eq!(ran.load(Ordering::Acquire), 1, "the park never ended");
    }

    #[test]
    fn a_parked_session_does_not_hold_quiescence_up() {
        // #738. This is the deadlock: without the park the wait below
        // never returns, because the session it is waiting for is not
        // running and will only stand down when the host says so.
        let epochs = Epochs::new(4);
        let session = Slotted::claim(&epochs).expect("slot");
        session.protect();
        session.park();
        epochs.wait_for_quiescence();
        // And it is not a one way door: the slot is still announced, so
        // the session can pick up where it left off.
        assert!(
            epochs.safe_epoch() <= epochs.current(),
            "the park gave up the epoch"
        );
        session.unprotect();
    }

    #[test]
    fn starting_an_operation_ends_a_park() {
        // A stale park would be the dangerous direction: a session
        // inside a real operation that quiescence walks straight past.
        let epochs = Epochs::new(4);
        let session = Slotted::claim(&epochs).expect("slot");
        session.protect();
        session.park();
        session.protect();
        assert_eq!(
            epochs.running_epoch(),
            epochs.safe_epoch(),
            "a running session was still counted as parked"
        );
        session.unprotect();
    }

    #[test]
    fn quiescence_returns_once_the_running_sessions_leave() {
        let epochs = Arc::new(Epochs::new(4));
        let started = Arc::new(AtomicUsize::new(0));
        let worker = {
            let epochs = Arc::clone(&epochs);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                let s = Slotted::claim(&epochs).expect("slot");
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
