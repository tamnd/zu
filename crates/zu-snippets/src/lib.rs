//! The snippets this repository publishes, as programs.
//!
//! A quickstart snippet is read far more often than any other code the
//! project ships, and it is the one piece of code nothing compiles: it
//! lives in a README, it is copied by hand, and it goes wrong quietly,
//! a rename at a time, until a reader's first five minutes are spent
//! on an error message. So the snippets are programs here, one per
//! `examples/`, and `tests/readme.rs` holds the README to them
//! character for character and then runs them.
//!
//! Running is the half that matters. A snippet that compiles proves
//! the names still exist; a snippet that runs proves the statements in
//! it still parse, the file it writes still opens, and the numbers it
//! prints are the numbers the page claims. Each one runs in a
//! directory of its own, because it writes a database into the working
//! directory exactly as a reader's copy would.
//!
//! The package exists at all because a crate cannot depend on itself
//! under a second name. This one is published as `zudb`, which is what
//! a reader types, and no example inside it could say `zudb` while
//! being an example of the same package.

/// The examples this package holds, in the order the README prints
/// them, and the name of the fenced block each one has to match.
pub const SNIPPETS: &[&str] = &["sixty-seconds"];
