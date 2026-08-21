//! The lock that makes pinning an executor safe.
//!
//! Several suites here run one statement twice, once through the
//! pipeline executor and once through the row-at-a-time one, because
//! the second is the oracle the first has to agree with. The only lever
//! the public API offers for that is `ZU_EXEC2` in the environment.
//!
//! The environment belongs to the process, and the tests in one file
//! share one. A test that sets `ZU_EXEC2=0` sets it for whatever its
//! siblings happen to be running at that instant, so those quietly
//! answer from the old executor for as long as the window is open, and
//! the ones asserting that a plain projection kept its vectors fail on
//! an assertion nowhere near the thing that broke them. It is rare
//! enough to look like the machine and frequent enough to make a green
//! workspace run mean less than it should: the failure that started
//! this was one test in one file in one run on one server, and reading
//! it as disk pressure would have been the easy mistake.
//!
//! So every statement runs under [`reading`] and every pin runs under
//! [`pinned`]. A pin waits for the statements already in flight and the
//! statements that start during it wait for the pin. The discipline is
//! the file's own helper's, not each test's: a test calls `answer` or
//! `row` or `buffered` the way it always did and gets this for free.
//!
//! Not every file needs it. A file holding one test has no siblings to
//! race, which is why `numeric.rs` and `zuql_parity.rs` are left alone.

use std::cell::Cell;
use std::sync::{RwLock, RwLockReadGuard};

/// Held for reading by a statement and for writing by a pin. It guards
/// the environment, which is why it guards nothing: the data is the
/// process's own and there is nothing to put inside.
static ENV: RwLock<()> = RwLock::new(());

thread_local! {
    /// Whether this thread is inside a pin. The lock is not reentrant,
    /// so a pin whose body ran a statement through [`reading`] would
    /// wait on itself forever; the body is the one caller that already
    /// holds the exclusive side and must not ask again.
    static PINNING: Cell<bool> = const { Cell::new(false) };
}

/// The guard a statement runs under. `None` inside a pin, where the
/// exclusive side is already held by this thread.
pub fn reading() -> Option<RwLockReadGuard<'static, ()>> {
    if PINNING.with(Cell::get) {
        return None;
    }
    // A test that panicked while holding this poisoned nothing, because
    // there is nothing behind the lock to leave half written. Taking the
    // inner guard keeps one real failure from becoming a file of
    // poisoned ones that say nothing about why.
    Some(ENV.read().unwrap_or_else(|e| e.into_inner()))
}

/// Runs `body` with `var` set to `value` and no other statement in this
/// process in flight, then puts the environment back.
///
/// Back on the way out of a panic as well, which is what the guard is
/// for. A pin that left the variable set would turn one failing test
/// into every test after it in the file.
pub fn pinned<T>(var: &str, value: &str, body: impl FnOnce() -> T) -> T {
    let _exclusive = ENV.write().unwrap_or_else(|e| e.into_inner());
    let _restore = Restore(var.to_string());
    PINNING.with(|inside| inside.set(true));
    // Safe here and only here: the write side is held, so no other
    // thread in this process is inside a statement that reads it.
    unsafe { std::env::set_var(var, value) };
    body()
}

/// Puts one variable back and says the thread is out of its pin.
struct Restore(String);

impl Drop for Restore {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(&self.0) };
        PINNING.with(|inside| inside.set(false));
    }
}
