//! The handle a caller keeps on a statement that is running: whether
//! it has been asked to stop, and how far it has got.
//!
//! A statement runs on the thread that asked for it, so the only way to
//! stop one is to leave something behind that the run reads as it goes.
//! That something is one word. The executors check it where they
//! already have a natural boundary, which is a chunk in the pull engine
//! and a morsel in the pipeline one, so a cancellation is answered
//! within a vector of rows rather than at the end of the query, and the
//! check costs a relaxed load in a place that has just done thousands
//! of them.
//!
//! Nothing here is a timeout, and nothing here unwinds anything. The
//! run notices, returns [`ZuError::Interrupted`], and the session it
//! ran on is untouched: no plan is dropped, no reader is closed, and
//! the next statement on it starts warm. That is the difference between
//! cancelling a statement and killing a process, and it is the whole
//! reason a shell wants this rather than a signal.
//!
//! A handle nobody armed costs one branch on a null pointer per check
//! and never says stop, which is what every caller who did not ask for
//! cancellation gets by default.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{Result, ZuError};

/// What the running statement and whoever is waiting for it share.
///
/// The two words sit together because they are read together: a caller
/// waiting on a statement asks whether to stop it and how far it has
/// got in the same breath, and the run touches both at the same
/// boundary.
#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    rows: AtomicU64,
}

/// A handle on a running statement.
///
/// Cloning is cloning the handle, not the state: every clone reads and
/// writes the same word, which is what lets one thread hand a copy to
/// the executor and keep one to raise. [`Interrupt::default`] is the
/// handle nobody is holding, and it is the one that costs nothing.
#[derive(Clone, Debug, Default)]
pub struct Interrupt(Option<Arc<Shared>>);

impl Interrupt {
    /// A handle a caller can raise, with the word allocated for it.
    ///
    /// Spelled apart from [`Default`] because the two are different
    /// things: this one can stop a statement and the default one
    /// cannot, and a constructor that quietly returned the second when
    /// asked for the first would be a cancellation that never arrives.
    pub fn armed() -> Interrupt {
        Interrupt(Some(Arc::new(Shared::default())))
    }

    /// Asks the statement to stop. Does nothing on a handle nobody
    /// armed, which is the honest answer: there is no statement on the
    /// other end of it.
    pub fn stop(&self) {
        if let Some(shared) = &self.0 {
            shared.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Whether somebody has asked.
    pub fn stopped(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|shared| shared.stop.load(Ordering::Relaxed))
    }

    /// The check an executor makes at a boundary: `Ok` while nobody has
    /// asked, and the condition that ends the statement once somebody
    /// has.
    pub fn check(&self) -> Result<()> {
        if self.stopped() {
            return Err(ZuError::Interrupted);
        }
        Ok(())
    }

    /// Puts the flag down and the count back to zero, which is what
    /// makes one handle serve a session rather than one statement.
    ///
    /// Called before a statement rather than after it, so that a stop
    /// which arrived while nothing was running cannot end the next
    /// thing the user types.
    pub fn clear(&self) {
        if let Some(shared) = &self.0 {
            shared.stop.store(false, Ordering::Relaxed);
            shared.rows.store(0, Ordering::Relaxed);
        }
    }

    /// The executor reporting rows it has read out of storage.
    ///
    /// Rows read rather than rows answered, because the number is there
    /// to show a person that something is happening, and a statement
    /// that reads a hundred million rows to answer one row is exactly
    /// the statement they are waiting on.
    pub fn read(&self, rows: u64) {
        if let Some(shared) = &self.0 {
            shared.rows.fetch_add(rows, Ordering::Relaxed);
        }
    }

    /// How many rows it has read so far.
    pub fn rows(&self) -> u64 {
        self.0
            .as_ref()
            .map_or(0, |shared| shared.rows.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_armed_handle_stops_and_a_default_one_never_does() {
        let armed = Interrupt::armed();
        assert!(!armed.stopped());
        assert!(armed.check().is_ok());
        armed.stop();
        assert!(armed.stopped());
        assert!(matches!(armed.check(), Err(ZuError::Interrupted)));

        let never = Interrupt::default();
        never.stop();
        assert!(!never.stopped());
        assert!(never.check().is_ok());
    }

    #[test]
    fn every_clone_is_the_same_word() {
        let held = Interrupt::armed();
        let handed = held.clone();
        handed.read(2048);
        held.read(2048);
        assert_eq!(held.rows(), 4096);
        assert_eq!(handed.rows(), 4096);
        held.stop();
        assert!(handed.stopped(), "the executor's copy sees the ask");

        // Clearing readies the same handle for the next statement, which
        // is what a session does with it between two of them.
        handed.clear();
        assert!(!held.stopped());
        assert_eq!(held.rows(), 0);
    }

    #[test]
    fn a_default_handle_counts_nothing_and_answers_nothing() {
        let never = Interrupt::default();
        never.read(4096);
        assert_eq!(never.rows(), 0);
    }
}
