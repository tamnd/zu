//! The storage engine contract every engine implements.
//!
//! `docs/02-architecture.md` §3 defines the full trait surface. The types
//! here are skeletons: enough for engines and the query layer to compile
//! against, fleshed out as M1 (zu1), M3 (sqlite), and M5 (s3) land.

use std::sync::Arc;

use zu_common::{Epoch, NodeGroupId, NodeOffset, Result, TableId};

/// Traversal direction for adjacency access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Fwd,
    Bwd,
}

/// How much work a checkpoint is allowed to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointMode {
    /// Fold deltas and flip the root header.
    Normal,
    /// Also compact tombstoned groups and rewrite fragmented CSR.
    Full,
}

/// Schema catalog. Placeholder until the catalog lands in M1.
#[derive(Default, Debug)]
pub struct Catalog {}

/// A batch of writes to commit atomically. Placeholder until M3.
#[derive(Default, Debug)]
pub struct CommitBatch {}

/// A sealed, encoded node group produced by bulk load. Placeholder until M1.
#[derive(Debug)]
pub struct SealedNodeGroup {}

/// Reference to one encoded column segment.
#[derive(Debug)]
pub struct SegmentRef {}

/// Reference to one CSR adjacency group.
#[derive(Debug)]
pub struct CsrRef {}

/// A consistent read view at a fixed epoch.
pub trait Snapshot: Send + Sync {
    fn scan_column(&self, t: TableId, g: NodeGroupId, c: u32) -> Result<SegmentRef>;
    fn csr(&self, t: TableId, g: NodeGroupId, dir: Direction) -> Result<CsrRef>;
    fn lookup_pk(&self, t: TableId, key: &[u8]) -> Result<Option<NodeOffset>>;
    fn epoch(&self) -> Epoch;
}

/// A storage engine: zu1, sqlite, or s3.
pub trait GraphStore: Send + Sync {
    fn catalog(&self) -> Arc<Catalog>;
    fn snapshot(&self) -> Arc<dyn Snapshot>;
    fn commit(&self, batch: CommitBatch) -> Result<Epoch>;
    fn checkpoint(&self, mode: CheckpointMode) -> Result<()>;
    fn ingest(&self, groups: Vec<SealedNodeGroup>) -> Result<Epoch>;
}
