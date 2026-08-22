//! zu2, the record plane: a hash index over a hybrid log.
//!
//! This is a second storage engine, not a replacement for zu1. zu1 is a
//! columnar single-file store and it is good at what it was built for,
//! which is scanning. What it has no plane for is the point operation:
//! a read of one key by value costs a label scan, and a commit costs a
//! fold plus an fsync. Those are shape defects, not constants, and the
//! measurements are in Spec/2064g/zu2/00-why.md.
//!
//! The design is FASTER (Chandramouli et al., SIGMOD 2018) as continued
//! by F2 (Kanellis, Chandramouli, Hart, Venkataraman, PVLDB 18(12),
//! 2025): records live in an append-only log whose tail is in memory,
//! an in-memory hash index maps a key hash to the address of the newest
//! record, and older versions chain backwards through the records
//! themselves. A point read is a cacheline probe and one dereference. A
//! write is an append. Durability is a group commit, so one fsync
//! serves every commit waiting behind it rather than one per commit.
//!
//! ```no_run
//! # fn main() -> zu2::Result<()> {
//! use zu2::{Db, Durability, Options};
//!
//! let db = Db::create(
//!     std::path::Path::new("/tmp/example.zu2"),
//!     Options {
//!         durability: Durability::Durable,
//!         ..Options::default()
//!     },
//! )?;
//! let mut s = db.session();
//! s.upsert(b"user1", b"field0=a")?;
//! let mut out = Vec::new();
//! assert!(s.read(b"user1", &mut out)?);
//! # Ok(())
//! # }
//! ```
//!
//! The crate depends on nothing else in the workspace and nothing else
//! in the workspace depends on it, so both engines build and run in the
//! same binary and a benchmark can put them side by side.

pub mod addr;
pub mod checkpoint;
pub mod cold;
pub mod compact;
pub mod db;
pub mod epoch;
pub mod error;
pub mod file;
pub mod graph;
pub mod index;
pub mod log;
pub mod record;
pub mod recover;
pub mod scan;

pub use checkpoint::Checkpointed;
pub use compact::Compacted;
pub use db::{Compaction, Db, Options, Session, Transaction};
pub use error::{Error, Result};
pub use graph::{Direction, Graph};
pub use log::{Durability, PROVISION_CHUNK};
pub use scan::{Cursor, Ordered};
