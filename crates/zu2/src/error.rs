//! What the record plane can fail with.
//!
//! Deliberately its own type rather than `zu_common::ZuError`, because
//! the crate depends on nothing in the workspace and the two engines
//! have to be buildable apart. The bridge in the scan plane is where a
//! conversion belongs, once there is a bridge.

use std::io;

/// The record plane's error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),

    /// A record has to fit a page, because the log never lets one
    /// straddle a page boundary and pages are the unit of flush and
    /// eviction.
    #[error("record of {size} bytes does not fit a {page} byte page")]
    RecordTooLarge { size: usize, page: usize },

    /// The address space ran past the page table. Raising
    /// [`crate::addr::MAX_PAGES`] is the fix, and it is a constant
    /// rather than a growable table so that page lookup stays one load.
    #[error("log is full: {pages} pages allocated")]
    LogFull { pages: usize },

    /// A record header on the log did not parse, which during recovery
    /// bounds the durable prefix and at any other time is corruption.
    #[error("record at {address} is malformed: {why}")]
    Malformed { address: u64, why: &'static str },

    /// A node id landed past the end of the node table. The table is
    /// chunked and the chunks are allocated on demand, but the array of
    /// chunk pointers is sized once so that a node lookup stays two
    /// loads, so the ceiling is a configuration decision.
    #[error("node {node} is past the {max} the graph was sized for")]
    NodeOutOfRange { node: u32, max: usize },

    /// The file holds an edge between nodes the options have no room
    /// for, which is what reopening with a smaller `max_nodes` than the
    /// run that wrote it looks like. Nothing is wrong with the file, so
    /// this names the number that would open it rather than reporting
    /// the same thing the write path would have.
    #[error("file needs max_nodes of at least {needs}, the options gave room for {max}")]
    GraphTooSmall { needs: usize, max: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
