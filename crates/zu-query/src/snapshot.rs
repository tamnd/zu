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

use zu_common::{DurationKind, LogicalType, Result, Temporal, ZuError};
use zu_vector::{MorselArena, PhysType, SelVector, ValueVector};

use crate::column::ColumnType;

/// What a backend that resolved no edge column says if it is asked to
/// read one anyway, which is a compiler bug rather than a query error.
fn no_edge_columns() -> ZuError {
    ZuError::InvalidArgument("this snapshot reads no edge columns".into())
}

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

/// The storage type of a property column, the ones the props
/// directory hands the vector layer today; the full typed catalog is
/// M3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    /// IEEE double. It rides the same fixed width lane an integer does
    /// and is stored as its bit pattern, so the read is the integer
    /// read and only the type on the vector differs.
    Float,
    Str,
    /// A date, a time, a datetime or a duration, all of which are one
    /// count in one word and differ only in what the count is of. The
    /// tag says which, so the lane stays the integer lane and only the
    /// value the sink hands back is different.
    Temporal(TemporalLane),
}

/// Which temporal type a one word lane holds.
///
/// The zoned types are not here. A zoned value is a count and an offset
/// from UTC, which is two numbers, and a lane is one word: reading one
/// through a lane would drop the offset and answer an instant that
/// prints somewhere else. Those columns stay off the vector path until
/// there is somewhere to put the second number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalLane {
    /// Days since 1970-01-01, widened from the 32 bits they are stored
    /// in, since a lane is a word wide whatever it holds.
    Date,
    /// Nanoseconds since midnight.
    LocalTime,
    /// Nanoseconds since 1970-01-01T00:00:00.
    LocalDatetime,
    /// Months for a year-month duration, nanoseconds for a day-time
    /// one, which is what the kind says.
    Duration(DurationKind),
}

impl TemporalLane {
    /// The temporal type this lane is a lane of.
    pub fn logical_type(self) -> LogicalType {
        match self {
            TemporalLane::Date => LogicalType::Date,
            TemporalLane::LocalTime => LogicalType::LocalTime,
            TemporalLane::LocalDatetime => LogicalType::LocalDatetime,
            TemporalLane::Duration(kind) => LogicalType::Duration(kind),
        }
    }

    /// The lane a temporal type takes, `None` for the zoned types,
    /// which take none.
    pub fn of(ty: &LogicalType) -> Option<TemporalLane> {
        match ty {
            LogicalType::Date => Some(TemporalLane::Date),
            LogicalType::LocalTime => Some(TemporalLane::LocalTime),
            LogicalType::LocalDatetime => Some(TemporalLane::LocalDatetime),
            LogicalType::Duration(kind) => Some(TemporalLane::Duration(*kind)),
            _ => None,
        }
    }

    /// The physical lane the words ride in. A date counts days and the
    /// other three count one thing each, so the width is a word either
    /// way and the type is what a kernel reads to know the ordering is
    /// the integer ordering.
    pub fn phys(self) -> PhysType {
        match self {
            TemporalLane::Date => PhysType::Date,
            TemporalLane::LocalTime | TemporalLane::LocalDatetime => PhysType::Timestamp,
            TemporalLane::Duration(_) => PhysType::Interval,
        }
    }

    /// The value one word of this lane holds.
    pub fn value(self, word: i64) -> Temporal {
        match self {
            TemporalLane::Date => Temporal::Date(word as i32),
            TemporalLane::LocalTime => Temporal::LocalTime(word),
            TemporalLane::LocalDatetime => Temporal::LocalDatetime(word),
            TemporalLane::Duration(kind) => Temporal::Duration(kind, word),
        }
    }

    /// The word a value of this lane rides as, `None` for a value of
    /// some other temporal type: a date is not a datetime and a lane
    /// that took one for the other would answer the wrong instant.
    pub fn word(self, value: &Temporal) -> Option<i64> {
        match (self, value) {
            (TemporalLane::Date, Temporal::Date(days)) => Some(i64::from(*days)),
            (TemporalLane::LocalTime, Temporal::LocalTime(nanos)) => Some(*nanos),
            (TemporalLane::LocalDatetime, Temporal::LocalDatetime(nanos)) => Some(*nanos),
            (TemporalLane::Duration(kind), Temporal::Duration(had, nanos)) if kind == *had => {
                Some(*nanos)
            }
            _ => None,
        }
    }

    /// What a result column of this lane is called.
    pub fn column_type(self) -> ColumnType {
        match self {
            TemporalLane::Date => ColumnType::Date,
            TemporalLane::LocalTime => ColumnType::LocalTime,
            TemporalLane::LocalDatetime => ColumnType::LocalDatetime,
            TemporalLane::Duration(DurationKind::YearMonth) => ColumnType::YearMonth,
            TemporalLane::Duration(DurationKind::DayTime) => ColumnType::DayTime,
        }
    }
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

    /// The same for a rel table's edge columns, `None` when the rel
    /// stores nothing under that name and `None` from a backend with
    /// no edge columns at all.
    ///
    /// Answering `None` is what keeps the other two edge methods
    /// honest: nothing compiles a plan that reads an edge column
    /// unless this resolved one, so a backend that does not implement
    /// them is never asked.
    fn resolve_rel_col(&mut self, rel: RelId, name: &str) -> Result<Option<(ColId, ColType)>> {
        let _ = (rel, name);
        Ok(None)
    }

    /// Appends the load-order ordinal of every edge of `node`'s list
    /// in `dir` to `out`, position for position with
    /// [`Snapshot::list_into`].
    ///
    /// An edge's ordinal is the row its properties sit in, and it is
    /// what a pair with several edges between it needs: the pair alone
    /// names the first of the run for all of them, and counting the
    /// list out instead gives every copy its own value.
    fn list_ords_into(
        &mut self,
        rel: RelId,
        node: u64,
        dir: Dir,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let _ = (rel, node, dir, out);
        Err(no_edge_columns())
    }

    /// Gathers an edge column for arbitrary `ords` into one vector in
    /// argument order, the edge-side [`Snapshot::gather`].
    fn gather_rel(
        &mut self,
        rel: RelId,
        col: ColId,
        ords: &[u64],
        arena: &mut MorselArena,
    ) -> Result<ValueVector> {
        let _ = (rel, col, ords, arena);
        Err(no_edge_columns())
    }

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

    /// How many lists of one group a caller has to want before pinning
    /// the group beats reading each list on its own. A pin decodes the
    /// group's whole neighbor array, so it is the right read for a scan
    /// and the wrong one for a point lookup: a seed with sixteen
    /// friends does not need the two million edges its group holds
    /// decoded, and on a working set larger than the decoded pool it
    /// does not even get to keep them. Backends that decode nothing
    /// lazily answer 0, which asks for the pin every time.
    ///
    /// `None` is not a very high threshold, it is a refusal: this group
    /// must be read a list at a time whatever the caller wants. A
    /// backend answers that when a pin would be wrong rather than
    /// merely expensive, which is what an unfolded edge over the group
    /// makes it, since a pin hands out the arrays the file holds and
    /// those are the ones the edge is not in.
    fn list_threshold(&mut self, _rel: RelId, _group: GroupId, _dir: Dir) -> Result<Option<usize>> {
        Ok(Some(0))
    }

    /// Appends one node's sorted neighbor list in `dir` to `out`,
    /// reading the range that list occupies rather than the group
    /// around it. Worth it below [`Snapshot::list_threshold`] lists per
    /// group and a loss above it, since a range read pays its own way
    /// in per list where a pin pays once for all of them.
    fn list_into(&mut self, rel: RelId, node: u64, dir: Dir, out: &mut Vec<u64>) -> Result<()> {
        let group = (node / u64::from(zu_common::GROUP_ROWS)) as GroupId;
        let pin = self.csr(rel, group, dir)?;
        out.extend_from_slice(pin.list((node % u64::from(zu_common::GROUP_ROWS)) as usize));
        Ok(())
    }

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
