//! A directory a test or a bench can write into, which goes away
//! afterwards.
//!
//! Ten places in this crate had written their own, and every one of them
//! had written the same half. They each picked a name nothing else was
//! using, and then either never removed it or removed it on the last
//! line of the test, which is the line a failing test does not reach. So
//! the directories that survived were the ones belonging to the runs
//! worth looking at.
//!
//! That is invisible on a laptop, where the operating system clears the
//! temporary directory, and it is not invisible on a build machine that
//! stays up for a month. Clearing a hundred of them off one by hand is
//! how this module came to exist.
//!
//! `tempfile` would do this and is not a dependency of this crate. The
//! four it has are deliberate, since this is the crate that has to build
//! before anything else can be checked, and forty lines is a smaller
//! price than a fifth.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory that removes itself.
///
/// It is a guard and not a path, so a test holds it for as long as it
/// needs the files and drops it when it is done. A test that only needs
/// the directory for one call can pass a temporary and it is gone at the
/// end of the statement.
///
/// The name is a process id and a counter, which is enough to keep two
/// runs on one machine and two threads in one run apart, plus a label
/// that says which test wrote it, so one that does somehow survive says
/// who to ask.
///
/// ```
/// use xtask::scratch::Scratch;
///
/// let dir = Scratch::new("doc");
/// std::fs::write(dir.join("a.txt"), "hello").unwrap();
/// assert!(dir.join("a.txt").exists());
///
/// let path = dir.to_path_buf();
/// drop(dir);
/// assert!(!path.exists());
/// ```
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    /// A new empty directory under the system temporary one, labelled
    /// with `what`.
    ///
    /// Anything already at that name is removed first. Two runs on one
    /// machine can collide only if a process id has come round again,
    /// and picking up whatever the last one left would be a worse start
    /// than an empty directory.
    ///
    /// # Panics
    ///
    /// If the directory cannot be created. A test that cannot write to
    /// the temporary directory has nothing to say about the code it was
    /// going to exercise, and failing here names the real problem.
    pub fn new(what: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("zu-{what}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory is writable");
        Self(path)
    }

    /// The directory itself.
    ///
    /// Rarely needed, since this derefs to [`Path`], and there for the
    /// places where inference wants to be told.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Whether it went is not reported, and that is the right way
        // round. A test that has already failed should be read for what
        // it found rather than for a second complaint about tidying up,
        // and a test that passed has nothing to say about a directory
        // that is gone either way.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_there_while_it_is_held_and_gone_once_it_is_not() {
        let path;
        {
            let dir = Scratch::new("held");
            path = dir.to_path_buf();
            assert!(path.is_dir(), "it was not created");
            std::fs::write(dir.join("a.txt"), "contents").expect("writes");
        }
        assert!(!path.exists(), "it outlived the guard that owned it");
    }

    /// The case the removal has to handle and a plain `remove_dir` would
    /// not: a test leaves files behind, which is the whole point of it.
    #[test]
    fn what_was_written_into_it_goes_with_it() {
        let path;
        {
            let dir = Scratch::new("full");
            path = dir.to_path_buf();
            std::fs::create_dir_all(dir.join("one/two")).expect("makes");
            std::fs::write(dir.join("one/two/deep.txt"), "deep").expect("writes");
            std::fs::write(dir.join("shallow.txt"), "shallow").expect("writes");
        }
        assert!(!path.exists(), "a directory with files in it stayed");
    }

    #[test]
    fn two_of_them_are_two_directories() {
        let a = Scratch::new("same");
        let b = Scratch::new("same");
        assert_ne!(a.path(), b.path(), "the same label named the same place");
        std::fs::write(a.join("mine.txt"), "a").expect("writes");
        assert!(
            !b.join("mine.txt").exists(),
            "one of them saw the other's files"
        );
    }

    /// Dropping the first must not take the second with it, which is
    /// what would happen if the name were the label alone.
    #[test]
    fn one_going_away_leaves_the_other_alone() {
        let b = Scratch::new("outlives");
        {
            let _a = Scratch::new("outlives");
        }
        assert!(b.is_dir(), "it went when a sibling did");
    }

    #[test]
    fn the_label_is_in_the_name_so_a_stray_one_says_who_made_it() {
        let dir = Scratch::new("labelled");
        let name = dir
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .to_string();
        assert!(name.starts_with("zu-labelled-"), "{name}");
    }
}
