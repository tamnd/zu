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

/// One yielded column of a table function: a value for every node of
/// the domain, in row order, which is how the graph kernels already
/// hand their answers back. `null` marks the rows the kernel answered
/// nothing for, the unreached nodes `sssp` yields null at, and is
/// empty when every row has a value.
pub struct FuncCol {
    pub values: Vec<i64>,
    pub null: Vec<bool>,
}

impl FuncCol {
    /// Whether any row of this column is null, which decides where the
    /// value is allowed to be read: a null answers a projection and
    /// fails a comparison, but grouping and ordering on one follow
    /// rules the compiled sinks do not implement.
    pub fn nullable(&self) -> bool {
        !self.null.is_empty()
    }
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

    /// The dense row a primary key maps to in `table`, `None` when the
    /// key is absent or lands past the table. A table loaded without a
    /// key index keeps the dense contract, where the key is the row,
    /// which is the same rule property reads follow for `.id`.
    fn seek_key(&mut self, table: TableId, key: u64) -> Result<Option<u64>>;

    /// Sum of degrees over `nodes` in `dir`, offsets only; neighbor
    /// values never decode for a count.
    fn degree_batch(&mut self, rel: RelId, nodes: &[u64], dir: Dir) -> Result<u64>;

    /// Adds each node's degree in `dir` onto `out`, position for
    /// position. Adding instead of storing lets an undirected step
    /// accumulate both sides into one buffer. The default goes through
    /// pinned CSR groups; backends with a cheaper offsets-only path
    /// should override it.
    fn degrees(&mut self, rel: RelId, nodes: &[u64], dir: Dir, out: &mut [u64]) -> Result<()> {
        debug_assert_eq!(nodes.len(), out.len());
        let mut cur: Option<(GroupId, CsrPin)> = None;
        for (slot, &node) in out.iter_mut().zip(nodes) {
            let group = (node / u64::from(zu_common::GROUP_ROWS)) as GroupId;
            if cur.as_ref().map(|&(g, _)| g) != Some(group) {
                cur = Some((group, self.csr(rel, group, dir)?));
            }
            let (_, pin) = cur.as_ref().expect("a pinned group");
            *slot += pin.degree((node % u64::from(zu_common::GROUP_ROWS)) as usize);
        }
        Ok(())
    }

    /// Runs a table function kernel over `rel` and hands back the one
    /// column it yields, dense over the node domain. `None` means this
    /// backend has no vectorized answer for the call, either because it
    /// knows no kernel under that name or because the kernel yields
    /// something the compiled column cannot carry, and the query goes
    /// back to the row-at-a-time engine, errors included.
    fn table_function(&mut self, name: &str, rel: RelId, args: &[i64]) -> Result<Option<FuncCol>> {
        let _ = (name, rel, args);
        Ok(None)
    }

    /// A second handle on the same epoch for a parallel worker. Forks
    /// share warm decoded state where the backend can, so a fork is
    /// not a cold reopen: what one worker decodes, the others hit.
    /// `None` keeps execution single threaded.
    fn fork(&self) -> Option<Box<dyn Snapshot + Send>> {
        None
    }
}
