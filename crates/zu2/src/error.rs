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

    /// The live span of the log reached `max_pages`. This is a size:
    /// everything from the compaction floor to the tail, so it is the
    /// bytes the database is actually holding and not the bytes it has
    /// written over its life. Compacting or a larger `max_pages` is the
    /// fix.
    ///
    /// It used to be neither of those things. The page table was flat
    /// and indexed by absolute page, so a page index once used was
    /// never used again and `max_pages` bounded every byte the database
    /// would ever append. One megabyte of live data died permanently
    /// after eighty three megabytes of writes (#470).
    #[error("log is full: {span} pages live, which is the {max} it was sized for")]
    LogFull { span: usize, max: usize },

    /// The tail reached [`crate::addr::MAX_PAGES`], which is what the
    /// 48 address bits of an index entry can name. There is no option
    /// that raises this one and no compaction that helps: addresses
    /// only go up. Dumping and reloading is the answer, and at 256 TiB
    /// of appends it is a long way off.
    #[error("log reached the end of the address space at {pages} pages")]
    AddressSpaceFull { pages: usize },

    /// The file holds a log longer than the options left room for,
    /// which is what reopening with a smaller `max_pages` than the run
    /// that wrote it looks like. Nothing is wrong with the file, so
    /// this names the number that would open it.
    #[error("file needs max_pages of at least {needs}, the options gave room for {max}")]
    NeedsPages { needs: usize, max: usize },

    /// The scan stopped at `at` and found a record that parses at
    /// `above`, so the file has a hole in it rather than a torn tail.
    /// Everything from `at` up is unreachable: the log is read in order
    /// and the order is broken there.
    ///
    /// A torn tail is the ordinary thing and says nothing, because the
    /// records that went missing were never acknowledged. This is the
    /// other thing, and the records above the hole were. Opening at the
    /// prefix and saying nothing would be a database quietly missing
    /// keys it promised, so this is an error, and `Options::salvage`
    /// is the way to take the prefix anyway (#472).
    #[error("log has a hole: the scan stopped at {at} and a record parses at {above}")]
    LogHole { at: u64, above: u64 },

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

    /// The host has as many sessions open as it asked for. A session is
    /// held for the life of a worker rather than per operation, so this
    /// is a sizing mistake and not back pressure: raise
    /// `Options::sessions` to the number of threads that will hold one.
    #[error("all {max} sessions are open, raise Options::sessions")]
    NoSessions { max: usize },

    /// A capture met state it cannot describe. Every reason is a broken
    /// precondition rather than damage, so this fails the checkpoint and
    /// never the database: the log is still the write ahead log and a
    /// reopen without a checkpoint is a slower reopen and not a lost
    /// one.
    #[error("cannot take a checkpoint: {why}")]
    Checkpoint { why: &'static str },

    /// The scan plane's arena has as many chunks as it can name. A
    /// node is never freed, so this is the key set the database has
    /// ever held rather than the one it holds, and at a megabyte a
    /// chunk it is sixty four gigabytes of keys. Nothing raises it
    /// short of a rebuild, which a reopen does.
    #[error("the scan plane's arena reached its ceiling of {max} chunks")]
    ArenaFull { max: usize },

    /// A key longer than the scan plane's node header can name. The
    /// header packs the length beside the height, so twenty seven bits
    /// of it are the length and this is 128 MiB. A record has to fit in
    /// a page to be written at all, so a key that reaches this cannot
    /// have come from a commit, and the check is here because packing
    /// the two together would otherwise truncate rather than fail.
    #[error("key of {len} bytes is longer than the scan plane's limit of {max}")]
    KeyTooLong { len: usize, max: usize },

    /// A transaction committed on the log and then could not finish
    /// putting its records into the index, so what is in memory is part
    /// of a transaction and what is on the log is all of it. Nothing
    /// keyed runs after that. The log is the database and it is intact,
    /// so closing and reopening builds the index from it and comes back
    /// with the whole transaction applied.
    #[error("database is wedged by a transaction memory could not finish applying: {why}")]
    Wedged { why: &'static str },
}

pub type Result<T> = std::result::Result<T, Error>;
