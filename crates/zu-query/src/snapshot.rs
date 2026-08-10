//! The vectorized read surface of docs/perf 04 section 3: a snapshot
//! lends decoded storage to the executor in chunk-sized vectors and
//! pinned CSR slices instead of one value per call. The scalar
//! [`Graph`] trait stays underneath the row-at-a-time paths; operators
//! move here as the pipeline executor lands in P2.
//!
//! Methods take `&mut self` because a handle carries per-worker state:
//! the file descriptor it seeks, lazily opened readers, and decode
//! scratch. Parallel workers each hold their own handle via
//! [`Snapshot::fork`]; the sharing happens one layer down, where forks
//! point at the same block cache and decoded pools, so one worker's
//! decode warms every other worker.
//!
//! [`Graph`]: crate::exec::Graph

use std::sync::Arc;

use zu_common::Result;
use zu_vector::{MorselArena, SelVector, ValueVector};

/// Node table id from the catalog.
pub type TableId = u32;
/// Rel table id from the catalog.
pub type RelId = u32;
/// Node group index within a rel table's CSR.
pub type GroupId = u32;
/// Column position within a table's props directory.
pub type ColId = u32;

/// Rows per scan chunk, the zu1 encoding chunk: small enough that a
/// selection fits u16 indices, large enough to amortize one decode.
pub const SCAN_ROWS: usize = 1024;

/// Traversal direction over a rel table's CSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Fwd,
    Bwd,
}

/// The storage type of a property column, the two the props directory
/// holds today; the full typed catalog is M3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Str,
}

/// A pinned CSR group: shared handles on the decoded offset and
/// neighbor arrays. Cloning is two `Arc` bumps, and the slices stay
/// valid however the pools evict behind them.
#[derive(Debug, Clone)]
pub struct CsrPin {
    /// Slot offsets, `rows + 1` monotone values into `neighbors`.
    pub offsets: Arc<Vec<u64>>,
    /// Neighbor row ids, sorted within each list.
    pub neighbors: Arc<Vec<u64>>,
}

impl CsrPin {
    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }

    /// Degree of group-local row `local`, two offset reads.
    pub fn degree(&self, local: usize) -> u64 {
        self.offsets[local + 1] - self.offsets[local]
    }

    /// The sorted neighbor list of group-local row `local`.
    pub fn list(&self, local: usize) -> &[u64] {
        &self.neighbors[self.offsets[local] as usize..self.offsets[local + 1] as usize]
    }
}

/// An inclusive value range over an integer column, the pushdown a
/// scan checks against zone maps before decoding a chunk. Bounds and
/// compares are in the unsigned domain, matching the stored zones.
#[derive(Debug, Clone, Copy)]
pub struct ZonePred {
    pub col: ColId,
    pub lo: u64,
    pub hi: u64,
}

impl ZonePred {
    /// True when a zone spanning `lo..=hi` cannot hold a match.
    pub fn skips(&self, lo: u64, hi: u64) -> bool {
        self.lo > hi || self.hi < lo
    }
}

/// One scanned chunk: `columns[i]` holds `rows` values of the `i`th
/// requested column starting at table row `row_base`, and `sel` lists
/// the chunk-local rows that passed the predicate, `None` when every
/// row did.
pub struct ScanChunk {
    pub row_base: u64,
    pub rows: u32,
    pub sel: Option<SelVector>,
    pub columns: Vec<ValueVector>,
}

/// One consistent view of stored graph data, read in batches. A
/// snapshot pins one commit epoch: two readers at the same epoch see
/// the same bytes, and nothing a snapshot lends out changes under it.
pub trait Snapshot {
    /// The commit epoch this snapshot reads.
    fn epoch(&self) -> u64;

    /// Rows in a node table's dense row domain.
    fn table_rows(&mut self, table: TableId) -> Result<u64>;

    /// Resolves a property column by name, `None` when the table has
    /// no column under it.
    fn resolve_col(&mut self, table: TableId, name: &str) -> Result<Option<(ColId, ColType)>>;

    /// Reads chunk `chunk` of `table`, decoding `cols` in order into
    /// vectors backed by `arena`. `pred` filters on an integer column:
    /// chunks its zone map excludes come back `None` without touching
    /// payload bytes, and the predicate column decodes before the rest
    /// so a chunk with no surviving rows also stops early. `None` past
    /// the last chunk.
    fn scan(
        &mut self,
        table: TableId,
        chunk: u64,
        cols: &[ColId],
        pred: Option<&ZonePred>,
        arena: &mut MorselArena,
    ) -> Result<Option<ScanChunk>>;

    /// Pins one CSR group of `rel` in `dir`.
    fn csr(&mut self, rel: RelId, group: GroupId, dir: Dir) -> Result<CsrPin>;

    /// Gathers a property column for arbitrary `rows` into one vector
    /// in argument order, decoding each touched chunk once.
    fn gather(
        &mut self,
        table: TableId,
        col: ColId,
        rows: &[u64],
        arena: &mut MorselArena,
    ) -> Result<ValueVector>;

    /// The dense row a primary key maps to in `rel`'s node domain,
    /// `None` when absent.
    fn lookup_pk(&mut self, rel: RelId, key: u64) -> Result<Option<u64>>;

    /// Sum of degrees over `nodes` in `dir`, offsets only; neighbor
    /// values never decode for a count.
    fn degree_batch(&mut self, rel: RelId, nodes: &[u64], dir: Dir) -> Result<u64>;

    /// A second handle on the same epoch for a parallel worker. Forks
    /// share warm decoded state where the backend can, so a fork is
    /// not a cold reopen: what one worker decodes, the others hit.
    /// `None` keeps execution single threaded.
    fn fork(&self) -> Option<Box<dyn Snapshot + Send>> {
        None
    }
}
