//! The single-writer commit path and its in-memory MVCC overlays.
//!
//! Committed state per docs/08 section 2 is the base epoch data in the
//! file plus per-table overlays: appended rows, tombstones, property
//! update chains newest-first, and overlay edges, every entry stamped
//! with its commit epoch. A reader pinned at epoch E sees base data
//! plus overlay entries with commit epoch at or below E, so snapshots
//! stay consistent while the writer commits past them. Version data
//! never reaches the columnar file; a checkpoint folds overlays into
//! new sealed segments and recovery rebuilds them from the WAL alone.
//!
//! Single writer is enforced by the borrow checker: [`Mvcc::begin`]
//! hands out a [`WriteTxn`] holding the store's only mutable borrow,
//! so a second writer cannot exist while one is open. Commit appends
//! the txn's records to the WAL, syncs through the `TxnCommit` frame,
//! and only then publishes the staged changes to the overlays, so a
//! crash on either side of the sync leaves the pre-txn or post-txn
//! state and nothing else.

use std::collections::{BTreeMap, HashMap};

use zu_common::{Epoch, GROUP_ROWS, Result, ZuError};

use crate::file::BlockPtr;
use crate::wal::{Wal, WalColumn, WalRecord, WalValues};

/// One overlay cell value, the unit of an update chain entry and of an
/// appended row's column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Int(u64),
    Str(Vec<u8>),
}

/// One committed batch of appended rows, columnar like the props store.
#[derive(Debug, Clone)]
struct AppendBatch {
    epoch: Epoch,
    cols: Vec<WalColumn>,
    rows: u64,
}

/// Committed but not yet checkpointed state of one node table.
#[derive(Debug, Default)]
struct TableOverlay {
    appended: Vec<AppendBatch>,
    /// Offset of the deleted row, stamped with the deleting epoch.
    tombstones: BTreeMap<u64, Epoch>,
    /// Update chains keyed by cell, newest entry last.
    updates: HashMap<(u64, u32), Vec<(Epoch, Cell)>>,
}

/// Committed overlay edges of one rel table, in commit order.
#[derive(Debug, Default)]
struct RelOverlay {
    edges: Vec<(Epoch, u64, u64)>,
}

/// The payload of one resolved or freshly written ingest: the sealed
/// data as the overlay will serve it until a fold seals it into the
/// base. The blocks in the file hold the durable copy; this is the
/// in-memory image recovery reads back from them.
#[derive(Debug)]
pub enum IngestPayload {
    Nodes { cols: Vec<WalColumn>, rows: u64 },
    Edges { src: Vec<u64>, dst: Vec<u64> },
}

/// The committed overlay store and the epoch counter, owned by
/// whichever handle owns the write side of the database.
#[derive(Debug, Default)]
pub struct Mvcc {
    epoch: Epoch,
    tables: HashMap<u32, TableOverlay>,
    rels: HashMap<u32, RelOverlay>,
    /// Manifest roots of ingests not yet folded, with their commit
    /// epochs; the fold frees these blocks once the data is sealed.
    ingests: Vec<(Epoch, BlockPtr)>,
}

/// One staged mutation, in statement order.
#[derive(Debug)]
enum Op {
    InsertNodes {
        table: u32,
        cols: Vec<WalColumn>,
        rows: u64,
    },
    InsertRel {
        rel: u32,
        src: u64,
        dst: u64,
    },
    Update {
        table: u32,
        offset: u64,
        col: u32,
        value: Cell,
    },
    Delete {
        table: u32,
        offset: u64,
    },
}

/// An open write transaction: stages mutations locally and publishes
/// them on [`WriteTxn::commit`]. Dropping it without committing
/// abandons everything staged, on disk and in memory alike.
pub struct WriteTxn<'a> {
    mvcc: &'a mut Mvcc,
    ops: Vec<Op>,
}

fn cell_values(cells: &[Cell]) -> Result<WalValues> {
    let all_int = cells.iter().all(|c| matches!(c, Cell::Int(_)));
    if all_int {
        return Ok(WalValues::Int(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Int(x) => *x,
                    Cell::Str(_) => unreachable!(),
                })
                .collect(),
        ));
    }
    let all_str = cells.iter().all(|c| matches!(c, Cell::Str(_)));
    if !all_str {
        return Err(ZuError::InvalidArgument(
            "a column cannot mix int and string values".into(),
        ));
    }
    Ok(WalValues::Str(
        cells
            .iter()
            .map(|c| match c {
                Cell::Str(s) => s.clone(),
                Cell::Int(_) => unreachable!(),
            })
            .collect(),
    ))
}

impl Mvcc {
    /// A store with no overlays at the given committed epoch, the
    /// state right after a checkpoint or a fresh create.
    pub fn new(epoch: Epoch) -> Self {
        Mvcc {
            epoch,
            ..Mvcc::default()
        }
    }

    /// The newest committed epoch; a new reader pins this.
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Opens the write transaction. The mutable borrow is the writer
    /// lock: a second call cannot compile while one txn is open.
    pub fn begin(&mut self) -> WriteTxn<'_> {
        WriteTxn {
            mvcc: self,
            ops: Vec::new(),
        }
    }

    /// Rebuilds the overlay store from the log: every committed txn
    /// above `floor` replays into overlays exactly as its commit
    /// published it, and the epoch counter resumes past the last one.
    /// An `IngestRef` in the log is an error here; recovery with a
    /// database file at hand goes through [`Self::recover_with`], whose
    /// resolver reads the referenced sealed segments back.
    pub fn recover(wal: &Wal, floor: Epoch) -> Result<Self> {
        Self::recover_with(wal, floor, |_, _| {
            Err(ZuError::Unsupported {
                what: "resolving an IngestRef without the database file",
                id: 0,
            })
        })
    }

    /// [`Self::recover`] with a resolver that turns each committed
    /// `IngestRef` back into its payload by reading the sealed
    /// segments its manifest names, returning the payload and the
    /// manifest root for the next fold to free.
    pub fn recover_with(
        wal: &Wal,
        floor: Epoch,
        mut resolve: impl FnMut(u32, &[u64]) -> Result<(IngestPayload, BlockPtr)>,
    ) -> Result<Self> {
        let mut mvcc = Mvcc::new(floor);
        wal.replay(floor, |epoch, rec| {
            mvcc.epoch = mvcc.epoch.max(epoch);
            match rec {
                WalRecord::TxnBegin | WalRecord::TxnCommit => Ok(()),
                WalRecord::NodeInsert { table, cols } => {
                    let rows = cols.first().map_or(0, |c| c.values.len() as u64);
                    if cols.iter().any(|c| c.values.len() as u64 != rows) {
                        return Err(ZuError::Corrupt {
                            what: "wal record",
                            detail: format!("ragged node insert into table {table}"),
                        });
                    }
                    mvcc.tables
                        .entry(*table)
                        .or_default()
                        .appended
                        .push(AppendBatch {
                            epoch,
                            cols: cols.clone(),
                            rows,
                        });
                    Ok(())
                }
                WalRecord::RelInsert { rel, src, dst } => {
                    let edges = &mut mvcc.rels.entry(*rel).or_default().edges;
                    edges.extend(src.iter().zip(dst).map(|(s, d)| (epoch, *s, *d)));
                    Ok(())
                }
                WalRecord::Update {
                    table,
                    col,
                    offsets,
                    values,
                    ..
                } => {
                    let overlay = mvcc.tables.entry(*table).or_default();
                    for (i, offset) in offsets.iter().enumerate() {
                        let cell = match values {
                            WalValues::Int(v) => Cell::Int(v[i]),
                            WalValues::Str(v) => Cell::Str(v[i].clone()),
                        };
                        overlay
                            .updates
                            .entry((*offset, *col))
                            .or_default()
                            .push((epoch, cell));
                    }
                    Ok(())
                }
                WalRecord::Delete { table, ids } => {
                    let overlay = mvcc.tables.entry(*table).or_default();
                    for id in ids {
                        overlay.tombstones.entry(*id).or_insert(epoch);
                    }
                    Ok(())
                }
                WalRecord::IngestRef { table, ptrs } => {
                    let (payload, root) = resolve(*table, ptrs)?;
                    mvcc.publish_ingest(epoch, *table, payload, root);
                    Ok(())
                }
                WalRecord::DdlCatalog { .. } => Err(ZuError::Unsupported {
                    what: "wal replay kind",
                    id: 0,
                }),
                WalRecord::CheckpointNote => Ok(()),
            }
        })?;
        Ok(mvcc)
    }

    /// Rows appended to `table` and visible at `epoch`.
    pub fn appended_rows(&self, table: u32, epoch: Epoch) -> u64 {
        self.tables.get(&table).map_or(0, |t| {
            t.appended
                .iter()
                .filter(|b| b.epoch <= epoch)
                .map(|b| b.rows)
                .sum()
        })
    }

    /// Whether the row at `offset` is deleted for a reader at `epoch`.
    pub fn is_deleted(&self, table: u32, offset: u64, epoch: Epoch) -> bool {
        self.tables
            .get(&table)
            .and_then(|t| t.tombstones.get(&offset))
            .is_some_and(|&e| e <= epoch)
    }

    /// The newest overlay value of one cell at `epoch`: an update chain
    /// entry first, then an appended row's stored column. `base_count`
    /// is the row count of the sealed file, where appended offsets
    /// start. Returns `None` when the base file holds the answer.
    pub fn cell(
        &self,
        table: u32,
        base_count: u64,
        offset: u64,
        col: u32,
        epoch: Epoch,
    ) -> Option<Cell> {
        let overlay = self.tables.get(&table)?;
        if let Some(chain) = overlay.updates.get(&(offset, col))
            && let Some((_, cell)) = chain.iter().rev().find(|(e, _)| *e <= epoch)
        {
            return Some(cell.clone());
        }
        if offset < base_count {
            return None;
        }
        let mut at = base_count;
        for batch in &overlay.appended {
            if batch.epoch <= epoch && offset < at + batch.rows {
                let row = (offset - at) as usize;
                let column = batch.cols.iter().find(|c| c.col == col)?;
                return Some(match &column.values {
                    WalValues::Int(v) => Cell::Int(v[row]),
                    WalValues::Str(v) => Cell::Str(v[row].clone()),
                });
            }
            if batch.epoch <= epoch {
                at += batch.rows;
            }
        }
        None
    }

    /// Appends the overlay neighbors of `node` visible at `epoch` to
    /// `out`: destinations of overlay edges leaving it, or sources of
    /// edges entering it when `reversed`. Tombstoned endpoints are the
    /// facade's concern; the overlay stores edges as committed.
    pub fn neighbors(&self, rel: u32, node: u64, reversed: bool, epoch: Epoch, out: &mut Vec<u64>) {
        if let Some(overlay) = self.rels.get(&rel) {
            for &(e, src, dst) in &overlay.edges {
                if e > epoch {
                    continue;
                }
                if !reversed && src == node {
                    out.push(dst);
                } else if reversed && dst == node {
                    out.push(src);
                }
            }
        }
    }

    /// Overlay edges of `rel` visible at `epoch`, for checkpoint folds.
    pub fn edges(&self, rel: u32, epoch: Epoch) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.rels
            .get(&rel)
            .into_iter()
            .flat_map(|o| &o.edges)
            .filter(move |(e, _, _)| *e <= epoch)
            .map(|&(_, s, d)| (s, d))
    }

    /// Node tables holding any overlay state, sorted so a fold walks
    /// them deterministically.
    pub fn tables_touched(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.tables.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Rel tables holding overlay edges, sorted like the node tables.
    pub fn rels_touched(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.rels.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Tombstoned offsets of `table` visible at `epoch`, ascending.
    pub fn tombstones(&self, table: u32, epoch: Epoch) -> Vec<u64> {
        self.tables.get(&table).map_or_else(Vec::new, |t| {
            t.tombstones
                .iter()
                .filter(|&(_, &e)| e <= epoch)
                .map(|(&offset, _)| offset)
                .collect()
        })
    }

    /// Whether any append batch of `table` visible at `epoch` carries
    /// column values, which a fold must merge into stored props.
    pub fn appends_carry_columns(&self, table: u32, epoch: Epoch) -> bool {
        self.tables.get(&table).is_some_and(|t| {
            t.appended
                .iter()
                .any(|b| b.epoch <= epoch && b.rows > 0 && !b.cols.is_empty())
        })
    }

    /// Whether `table` holds any update chain entry visible at `epoch`.
    pub fn has_updates(&self, table: u32, epoch: Epoch) -> bool {
        self.tables.get(&table).is_some_and(|t| {
            t.updates
                .values()
                .any(|chain| chain.iter().any(|&(e, _)| e <= epoch))
        })
    }

    /// Seeds tombstones a checkpoint persisted into the base file. They
    /// enter at epoch 0 so every reader sees them, matching their state
    /// as folded rather than freshly deleted.
    pub fn seed_tombstones(&mut self, table: u32, offsets: &[u64]) {
        let overlay = self.tables.entry(table).or_default();
        for &offset in offsets {
            overlay.tombstones.insert(offset, 0);
        }
    }

    /// Publishes one committed ingest at `epoch`: the payload enters
    /// the overlays exactly as a plain committed txn would have put it
    /// there, and the manifest root is remembered for the fold to
    /// free. Called by the write path after its WAL frame syncs and by
    /// recovery when it resolves the frame back.
    pub(crate) fn publish_ingest(
        &mut self,
        epoch: Epoch,
        table: u32,
        payload: IngestPayload,
        root: BlockPtr,
    ) {
        match payload {
            IngestPayload::Nodes { cols, rows } => {
                self.tables
                    .entry(table)
                    .or_default()
                    .appended
                    .push(AppendBatch { epoch, cols, rows });
            }
            IngestPayload::Edges { src, dst } => {
                let edges = &mut self.rels.entry(table).or_default().edges;
                edges.extend(src.into_iter().zip(dst).map(|(s, d)| (epoch, s, d)));
            }
        }
        self.ingests.push((epoch, root));
        self.epoch = self.epoch.max(epoch);
    }

    /// Manifest roots of ingests committed at or below `epoch`, for
    /// the fold to free once their data is sealed into the base.
    pub fn ingest_roots(&self, epoch: Epoch) -> Vec<BlockPtr> {
        self.ingests
            .iter()
            .filter(|&&(e, _)| e <= epoch)
            .map(|&(_, root)| root)
            .collect()
    }

    /// Publishes a committed txn's staged ops at `epoch`.
    fn apply(&mut self, epoch: Epoch, ops: Vec<Op>) {
        for op in ops {
            match op {
                Op::InsertNodes { table, cols, rows } => {
                    self.tables
                        .entry(table)
                        .or_default()
                        .appended
                        .push(AppendBatch { epoch, cols, rows });
                }
                Op::InsertRel { rel, src, dst } => {
                    self.rels
                        .entry(rel)
                        .or_default()
                        .edges
                        .push((epoch, src, dst));
                }
                Op::Update {
                    table,
                    offset,
                    col,
                    value,
                } => {
                    self.tables
                        .entry(table)
                        .or_default()
                        .updates
                        .entry((offset, col))
                        .or_default()
                        .push((epoch, value));
                }
                Op::Delete { table, offset } => {
                    self.tables
                        .entry(table)
                        .or_default()
                        .tombstones
                        .entry(offset)
                        .or_insert(epoch);
                }
            }
        }
        self.epoch = epoch;
    }
}

impl WriteTxn<'_> {
    /// Stages a columnar batch of new rows for `table`. Every column
    /// must carry the same row count.
    pub fn insert_nodes(&mut self, table: u32, cols: Vec<(u32, Vec<Cell>)>) -> Result<u64> {
        let rows = cols.first().map_or(0, |(_, v)| v.len() as u64);
        if cols.iter().any(|(_, v)| v.len() as u64 != rows) {
            return Err(ZuError::InvalidArgument(
                "ragged node insert: columns disagree on row count".into(),
            ));
        }
        let cols = cols
            .into_iter()
            .map(|(col, cells)| {
                Ok(WalColumn {
                    col,
                    values: cell_values(&cells)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.ops.push(Op::InsertNodes { table, cols, rows });
        Ok(rows)
    }

    /// Stages one new edge.
    pub fn insert_rel(&mut self, rel: u32, src: u64, dst: u64) {
        self.ops.push(Op::InsertRel { rel, src, dst });
    }

    /// Stages one cell update.
    pub fn update(&mut self, table: u32, offset: u64, col: u32, value: Cell) {
        self.ops.push(Op::Update {
            table,
            offset,
            col,
            value,
        });
    }

    /// Stages one row deletion.
    pub fn delete(&mut self, table: u32, offset: u64) {
        self.ops.push(Op::Delete { table, offset });
    }

    /// Whether nothing has been staged.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Commits: appends every staged record and the `TxnCommit` frame,
    /// syncs, then publishes the overlays. The fdatasync inside
    /// [`Wal::commit`] is the commit point; failure before it leaves
    /// the store untouched and the tail torn, which replay drops.
    pub fn commit(self, wal: &mut Wal) -> Result<Epoch> {
        if self.ops.is_empty() {
            return Ok(self.mvcc.epoch);
        }
        let epoch = self.mvcc.epoch + 1;
        wal.append(epoch, &WalRecord::TxnBegin)?;
        for rec in build_records(&self.ops) {
            wal.append(epoch, &rec)?;
        }
        wal.commit(epoch)?;
        self.mvcc.apply(epoch, self.ops);
        Ok(epoch)
    }
}

/// Lowers staged ops to log records: inserts and deletes batch as they
/// come, updates batch per (table, group, column) preserving statement
/// order within each cell's chain.
/// Update batches per (table, group, column) while lowering.
type UpdateBatches = BTreeMap<(u32, u64, u32), (Vec<u64>, Vec<Cell>)>;

fn build_records(ops: &[Op]) -> Vec<WalRecord> {
    let mut out = Vec::new();
    let mut updates = UpdateBatches::new();
    let mut deletes: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for op in ops {
        match op {
            Op::InsertNodes { table, cols, .. } => out.push(WalRecord::NodeInsert {
                table: *table,
                cols: cols.clone(),
            }),
            Op::InsertRel { rel, src, dst } => out.push(WalRecord::RelInsert {
                rel: *rel,
                src: vec![*src],
                dst: vec![*dst],
            }),
            Op::Update {
                table,
                offset,
                col,
                value,
            } => {
                let key = (*table, offset / GROUP_ROWS as u64, *col);
                let (offsets, cells) = updates.entry(key).or_default();
                offsets.push(*offset);
                cells.push(value.clone());
            }
            Op::Delete { table, offset } => deletes.entry(*table).or_default().push(*offset),
        }
    }
    for ((table, group, col), (offsets, cells)) in updates {
        out.push(WalRecord::Update {
            table,
            group,
            col,
            offsets,
            values: cell_values(&cells).expect("staged cells are column-typed"),
        });
    }
    for (table, ids) in deletes {
        out.push(WalRecord::Delete { table, ids });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_in(dir: &tempfile::TempDir) -> Wal {
        Wal::open(&dir.path().join("db.wal")).unwrap()
    }

    /// A committed txn is visible at its epoch and invisible one epoch
    /// earlier, the snapshot isolation contract.
    #[test]
    fn snapshots_see_only_their_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(1, vec![(0, vec![Cell::Int(7), Cell::Int(8)])])
            .unwrap();
        txn.insert_rel(9, 0, 1);
        let e1 = txn.commit(&mut wal).unwrap();
        assert_eq!(e1, 1);

        let mut txn = mvcc.begin();
        txn.update(1, 0, 0, Cell::Int(70));
        txn.delete(1, 1);
        let e2 = txn.commit(&mut wal).unwrap();
        assert_eq!(e2, 2);

        // At epoch 1 the insert is visible, the update and delete are not.
        assert_eq!(mvcc.appended_rows(1, e1), 2);
        assert_eq!(mvcc.cell(1, 0, 0, 0, e1), Some(Cell::Int(7)));
        assert!(!mvcc.is_deleted(1, 1, e1));
        let mut nbrs = Vec::new();
        mvcc.neighbors(9, 0, false, e1, &mut nbrs);
        assert_eq!(nbrs, vec![1]);

        // At epoch 2 the update chain wins and the tombstone shows.
        assert_eq!(mvcc.cell(1, 0, 0, 0, e2), Some(Cell::Int(70)));
        assert!(mvcc.is_deleted(1, 1, e2));

        // At epoch 0 nothing exists.
        assert_eq!(mvcc.appended_rows(1, 0), 0);
        assert_eq!(mvcc.cell(1, 0, 0, 0, 0), None);
    }

    /// Recovery rebuilds exactly what commit published: the same cells,
    /// tombstones, edges, and epoch, straight from the log.
    #[test]
    fn recovery_rebuilds_committed_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(
            1,
            vec![
                (0, vec![Cell::Int(10), Cell::Int(20)]),
                (
                    1,
                    vec![Cell::Str(b"ada".to_vec()), Cell::Str(b"kay".to_vec())],
                ),
            ],
        )
        .unwrap();
        txn.insert_rel(9, 5, 6);
        let e1 = txn.commit(&mut wal).unwrap();
        let mut txn = mvcc.begin();
        txn.update(1, 5, 1, Cell::Str(b"grace".to_vec()));
        txn.delete(1, 6);
        let e2 = txn.commit(&mut wal).unwrap();

        let recovered = Mvcc::recover(&wal, 0).unwrap();
        assert_eq!(recovered.epoch(), e2);
        // base_count 5: offsets 5 and 6 are the appended rows.
        for m in [&mvcc, &recovered] {
            assert_eq!(m.appended_rows(1, e2), 2);
            assert_eq!(m.cell(1, 5, 5, 0, e2), Some(Cell::Int(10)));
            assert_eq!(m.cell(1, 5, 5, 1, e1), Some(Cell::Str(b"ada".to_vec())));
            assert_eq!(m.cell(1, 5, 5, 1, e2), Some(Cell::Str(b"grace".to_vec())));
            assert!(m.is_deleted(1, 6, e2));
            assert!(!m.is_deleted(1, 6, e1));
            let mut nbrs = Vec::new();
            m.neighbors(9, 6, true, e2, &mut nbrs);
            assert_eq!(nbrs, vec![5]);
        }
    }

    /// A txn abandoned before commit leaves no trace: not in the
    /// store, not in the log, not in recovery.
    #[test]
    fn abandoned_txn_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(1, vec![(0, vec![Cell::Int(1)])]).unwrap();
        drop(txn);
        assert_eq!(mvcc.epoch(), 0);
        assert_eq!(mvcc.appended_rows(1, u64::MAX), 0);
        assert!(wal.is_empty());
        let recovered = Mvcc::recover(&wal, 0).unwrap();
        assert_eq!(recovered.epoch(), 0);
    }

    /// The recovery floor skips folded epochs, modeling a checkpoint
    /// that already sealed them into the base file.
    #[test]
    fn recovery_floor_skips_folded_epochs() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        for i in 0..3u64 {
            let mut txn = mvcc.begin();
            txn.insert_rel(9, i, i + 1);
            txn.commit(&mut wal).unwrap();
        }
        let recovered = Mvcc::recover(&wal, 2).unwrap();
        assert_eq!(recovered.epoch(), 3);
        let all: Vec<_> = recovered.edges(9, 3).collect();
        assert_eq!(all, vec![(2, 3)], "epochs 1 and 2 are folded");
    }

    /// Updates to one cell across txns read newest-first per epoch,
    /// and statement order within a txn is preserved.
    #[test]
    fn update_chains_resolve_per_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update(1, 0, 0, Cell::Int(1));
        txn.update(1, 0, 0, Cell::Int(2));
        let e1 = txn.commit(&mut wal).unwrap();
        let mut txn = mvcc.begin();
        txn.update(1, 0, 0, Cell::Int(3));
        let e2 = txn.commit(&mut wal).unwrap();
        assert_eq!(mvcc.cell(1, 9, 0, 0, e1), Some(Cell::Int(2)));
        assert_eq!(mvcc.cell(1, 9, 0, 0, e2), Some(Cell::Int(3)));
        let recovered = Mvcc::recover(&wal, 0).unwrap();
        assert_eq!(recovered.cell(1, 9, 0, 0, e1), Some(Cell::Int(2)));
        assert_eq!(recovered.cell(1, 9, 0, 0, e2), Some(Cell::Int(3)));
    }

    /// A mixed-type column is rejected at staging, before anything
    /// reaches the log.
    #[test]
    fn mixed_type_column_is_rejected() {
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        let err = txn
            .insert_nodes(1, vec![(0, vec![Cell::Int(1), Cell::Str(b"x".to_vec())])])
            .unwrap_err();
        assert!(format!("{err}").contains("mix"));
    }
}
