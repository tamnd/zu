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
    /// The absence a `REMOVE` leaves behind. A column holds it the way
    /// storage does, as a row whose validity bit is clear, so what the
    /// fold writes for one of these is a bit rather than a value.
    Null,
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

/// Committed overlay edges of one rel table, in commit order, and the
/// edges a `DETACH DELETE` took away.
#[derive(Debug, Default)]
struct RelOverlay {
    edges: Vec<OverlayEdge>,
    /// The rows a removed edge ran between, stamped with the removing
    /// epoch. An edge has no offset of its own, so the pair is its name,
    /// which means an entry here takes away every edge over that pair.
    /// Nothing that has a name of its own is lost by that: a table that
    /// stores edge properties holds a pair once, and one that stores
    /// none has nothing to tell two edges over a pair apart with.
    dead: BTreeMap<(u64, u64), Epoch>,
    /// What a statement wrote onto an edge that was already there,
    /// keyed by the rows it runs between and the column, newest last.
    /// The pair names the edge here for the same reason it does in
    /// `dead`, and the column has to be part of the key because two
    /// statements can change two properties of one edge.
    updates: BTreeMap<(u64, u64, u32), Vec<(Epoch, Cell)>>,
}

/// One committed edge the base does not hold yet: the rows it runs
/// between and what it carries.
#[derive(Debug)]
struct OverlayEdge {
    epoch: Epoch,
    src: u64,
    dst: u64,
    /// Column position in the rel table's props directory and the cell
    /// this edge holds there, one entry per column the table stores.
    cols: Vec<(u32, Cell)>,
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
        cols: Vec<(u32, Cell)>,
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
    DeleteRel {
        rel: u32,
        src: u64,
        dst: u64,
    },
    UpdateRel {
        rel: u32,
        src: u64,
        dst: u64,
        col: u32,
        value: Cell,
    },
}

/// An open write transaction: stages mutations locally and publishes
/// them on [`WriteTxn::commit`]. Dropping it without committing
/// abandons everything staged, on disk and in memory alike.
pub struct WriteTxn<'a> {
    mvcc: &'a mut Mvcc,
    ops: Vec<Op>,
}

/// The kind of one staged cell, which is what a logged column is made
/// of one of: a record carries words, byte strings, or absences, and
/// never two of the three, because a column of the store is one of the
/// three as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Str,
    Null,
}

fn kind_of(cell: &Cell) -> Kind {
    match cell {
        Cell::Int(_) => Kind::Int,
        Cell::Str(_) => Kind::Str,
        Cell::Null => Kind::Null,
    }
}

fn cell_values(cells: &[Cell]) -> Result<WalValues> {
    let first = cells.first().map_or(Kind::Int, kind_of);
    if cells.iter().any(|c| kind_of(c) != first) {
        return Err(ZuError::InvalidArgument(
            "a logged column cannot mix values of different kinds".into(),
        ));
    }
    Ok(match first {
        Kind::Int => WalValues::Int(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Int(x) => *x,
                    _ => unreachable!("checked above"),
                })
                .collect(),
        ),
        Kind::Str => WalValues::Str(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Str(s) => s.clone(),
                    _ => unreachable!("checked above"),
                })
                .collect(),
        ),
        Kind::Null => WalValues::Null(cells.len() as u32),
    })
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
                WalRecord::RelInsert {
                    rel,
                    src,
                    dst,
                    cols,
                } => {
                    let edges = &mut mvcc.rels.entry(*rel).or_default().edges;
                    // A logged column runs across the record's edges, and
                    // an overlay edge carries its own cells, so the read
                    // back turns the columns on their side: edge `i`
                    // takes value `i` of every column.
                    for (i, (s, d)) in src.iter().zip(dst).enumerate() {
                        edges.push(OverlayEdge {
                            epoch,
                            src: *s,
                            dst: *d,
                            cols: cols
                                .iter()
                                .map(|c| {
                                    let cell = match &c.values {
                                        WalValues::Int(v) => Cell::Int(v[i]),
                                        WalValues::Str(v) => Cell::Str(v[i].clone()),
                                        WalValues::Null(_) => Cell::Null,
                                    };
                                    (c.col, cell)
                                })
                                .collect(),
                        });
                    }
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
                            WalValues::Null(_) => Cell::Null,
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
                WalRecord::RelDelete { rel, src, dst } => {
                    let overlay = mvcc.rels.entry(*rel).or_default();
                    for (s, d) in src.iter().zip(dst) {
                        overlay.dead.entry((*s, *d)).or_insert(epoch);
                    }
                    Ok(())
                }
                WalRecord::RelUpdate {
                    rel,
                    col,
                    src,
                    dst,
                    values,
                } => {
                    let overlay = mvcc.rels.entry(*rel).or_default();
                    for (i, (s, d)) in src.iter().zip(dst).enumerate() {
                        let cell = match values {
                            WalValues::Int(v) => Cell::Int(v[i]),
                            WalValues::Str(v) => Cell::Str(v[i].clone()),
                            WalValues::Null(_) => Cell::Null,
                        };
                        overlay
                            .updates
                            .entry((*s, *d, *col))
                            .or_default()
                            .push((epoch, cell));
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
                    WalValues::Null(_) => Cell::Null,
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
            for edge in &overlay.edges {
                if edge.epoch > epoch || self.edge_gone(rel, edge.src, edge.dst, epoch) {
                    continue;
                }
                if !reversed && edge.src == node {
                    out.push(edge.dst);
                } else if reversed && edge.dst == node {
                    out.push(edge.src);
                }
            }
        }
    }

    /// Whether the edges between two rows are gone for a reader at
    /// `epoch`. The base holds edges the overlay never saw, so this is
    /// asked about a pair rather than about an overlay entry, and the
    /// facade asks it of every base edge it reads.
    pub fn edge_gone(&self, rel: u32, src: u64, dst: u64, epoch: Epoch) -> bool {
        self.rels
            .get(&rel)
            .and_then(|o| o.dead.get(&(src, dst)))
            .is_some_and(|&e| e <= epoch)
    }

    /// The pairs `rel` has lost by `epoch`, ascending, which is what a
    /// fold drops out of the CSR it rebuilds.
    pub fn dead_edges(&self, rel: u32, epoch: Epoch) -> Vec<(u64, u64)> {
        self.rels.get(&rel).map_or_else(Vec::new, |o| {
            o.dead
                .iter()
                .filter(|&(_, &e)| e <= epoch)
                .map(|(&pair, _)| pair)
                .collect()
        })
    }

    /// What `rel` has had written onto its edges by `epoch`, keyed by
    /// the rows an edge runs between and the column, one cell each.
    ///
    /// A cell's chain holds every value written to it in order, and the
    /// one a reader at `epoch` sees is the last one written at or before
    /// it, which is what makes two assignments to one property in one
    /// statement end with the second.
    pub fn edge_updates(&self, rel: u32, epoch: Epoch) -> BTreeMap<(u64, u64, u32), Cell> {
        self.rels.get(&rel).map_or_else(BTreeMap::new, |overlay| {
            overlay
                .updates
                .iter()
                .filter_map(|(key, chain)| {
                    let (_, cell) = chain.iter().rev().find(|(e, _)| *e <= epoch)?;
                    Some((*key, cell.clone()))
                })
                .collect()
        })
    }

    /// Whether `rel` holds any edge update visible at `epoch`, which is
    /// what says a fold has work to do on a rel table nothing else
    /// touched.
    pub fn has_edge_updates(&self, rel: u32, epoch: Epoch) -> bool {
        self.rels.get(&rel).is_some_and(|overlay| {
            overlay
                .updates
                .values()
                .any(|chain| chain.iter().any(|&(e, _)| e <= epoch))
        })
    }

    /// Overlay edges of `rel` visible at `epoch`, for checkpoint folds.
    pub fn edges(&self, rel: u32, epoch: Epoch) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.edge_cells(rel, epoch).map(|(src, dst, _)| (src, dst))
    }

    /// The same edges with what they carry, which is what a fold of a
    /// rel table that stores edge properties has to write.
    pub fn edge_cells(
        &self,
        rel: u32,
        epoch: Epoch,
    ) -> impl Iterator<Item = (u64, u64, &[(u32, Cell)])> + '_ {
        self.rels
            .get(&rel)
            .into_iter()
            .flat_map(|o| &o.edges)
            .filter(move |edge| {
                edge.epoch <= epoch && !self.edge_gone(rel, edge.src, edge.dst, epoch)
            })
            .map(|edge| (edge.src, edge.dst, edge.cols.as_slice()))
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
                edges.extend(src.into_iter().zip(dst).map(|(src, dst)| OverlayEdge {
                    epoch,
                    src,
                    dst,
                    cols: Vec::new(),
                }));
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
                Op::InsertRel {
                    rel,
                    src,
                    dst,
                    cols,
                } => {
                    self.rels.entry(rel).or_default().edges.push(OverlayEdge {
                        epoch,
                        src,
                        dst,
                        cols,
                    });
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
                Op::DeleteRel { rel, src, dst } => {
                    self.rels
                        .entry(rel)
                        .or_default()
                        .dead
                        .entry((src, dst))
                        .or_insert(epoch);
                }
                Op::UpdateRel {
                    rel,
                    src,
                    dst,
                    col,
                    value,
                } => {
                    self.rels
                        .entry(rel)
                        .or_default()
                        .updates
                        .entry((src, dst, col))
                        .or_default()
                        .push((epoch, value));
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

    /// Stages one new edge carrying nothing, which is every edge in a
    /// table that stores no properties on its edges.
    pub fn insert_rel(&mut self, rel: u32, src: u64, dst: u64) {
        self.insert_rel_carrying(rel, src, dst, Vec::new());
    }

    /// Stages one new edge and the cells it holds: a column's position
    /// in the rel table's props directory and the value this edge takes
    /// there, one entry per column the table stores.
    pub fn insert_rel_carrying(&mut self, rel: u32, src: u64, dst: u64, cols: Vec<(u32, Cell)>) {
        self.ops.push(Op::InsertRel {
            rel,
            src,
            dst,
            cols,
        });
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

    /// Stages the removal of the edges between two rows. An edge is
    /// named by the rows it runs between, so a table holding a pair
    /// twice loses both, which is what a `DETACH DELETE` of either end
    /// wants anyway.
    pub fn delete_rel(&mut self, rel: u32, src: u64, dst: u64) {
        self.ops.push(Op::DeleteRel { rel, src, dst });
    }

    /// Stages one cell update on an edge that is already there. The
    /// edge is named by the rows it runs between, the way a removed one
    /// is, and `col` is the column's position in the rel table's props
    /// directory.
    pub fn update_rel(&mut self, rel: u32, src: u64, dst: u64, col: u32, value: Cell) {
        self.ops.push(Op::UpdateRel {
            rel,
            src,
            dst,
            col,
            value,
        });
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
/// Edge update batches per (rel table, column) while lowering. An edge
/// is named by a pair rather than an offset, so there is no group to
/// batch by: a pair says nothing about where in the file the edge is.
type EdgeUpdateBatches = BTreeMap<(u32, u32), (Vec<u64>, Vec<u64>, Vec<Cell>)>;

fn build_records(ops: &[Op]) -> Vec<WalRecord> {
    let mut out = Vec::new();
    let mut updates = UpdateBatches::new();
    let mut edge_updates = EdgeUpdateBatches::new();
    let mut deletes: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    let mut detaches: BTreeMap<u32, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
    for op in ops {
        match op {
            Op::InsertNodes { table, cols, .. } => out.push(WalRecord::NodeInsert {
                table: *table,
                cols: cols.clone(),
            }),
            Op::InsertRel {
                rel,
                src,
                dst,
                cols,
            } => out.push(WalRecord::RelInsert {
                rel: *rel,
                src: vec![*src],
                dst: vec![*dst],
                // One cell is one value of one type, so the lowering
                // that can disagree with itself over a batch cannot
                // here.
                cols: cols
                    .iter()
                    .map(|(col, cell)| WalColumn {
                        col: *col,
                        values: cell_values(std::slice::from_ref(cell))
                            .expect("one staged cell is one type"),
                    })
                    .collect(),
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
            Op::DeleteRel { rel, src, dst } => {
                let (srcs, dsts) = detaches.entry(*rel).or_default();
                srcs.push(*src);
                dsts.push(*dst);
            }
            Op::UpdateRel {
                rel,
                src,
                dst,
                col,
                value,
            } => {
                let (srcs, dsts, cells) = edge_updates.entry((*rel, *col)).or_default();
                srcs.push(*src);
                dsts.push(*dst);
                cells.push(value.clone());
            }
        }
    }
    for ((table, group, col), (offsets, cells)) in updates {
        // One record carries one kind of value, so a batch that changes
        // kind partway is logged as one record per run of a kind. The
        // runs go out in the order they were staged, which is the order
        // a cell's chain has to end up in: a statement that writes a
        // value over an absence and one that writes an absence over a
        // value differ only in that order.
        let mut at = 0usize;
        while at < cells.len() {
            let kind = kind_of(&cells[at]);
            let mut end = at + 1;
            while end < cells.len() && kind_of(&cells[end]) == kind {
                end += 1;
            }
            out.push(WalRecord::Update {
                table,
                group,
                col,
                offsets: offsets[at..end].to_vec(),
                values: cell_values(&cells[at..end]).expect("one run is one kind"),
            });
            at = end;
        }
    }
    // An edge's cells are logged in runs of a kind for the reason a
    // row's are: one record carries one kind of value, and the order
    // the runs go out in is the order the cell's chain ends up in.
    for ((rel, col), (src, dst, cells)) in edge_updates {
        let mut at = 0usize;
        while at < cells.len() {
            let kind = kind_of(&cells[at]);
            let mut end = at + 1;
            while end < cells.len() && kind_of(&cells[end]) == kind {
                end += 1;
            }
            out.push(WalRecord::RelUpdate {
                rel,
                col,
                src: src[at..end].to_vec(),
                dst: dst[at..end].to_vec(),
                values: cell_values(&cells[at..end]).expect("one run is one kind"),
            });
            at = end;
        }
    }
    // The edges go before the rows, so a replay that stops partway
    // never leaves a row that is gone with an edge that still names it.
    for (rel, (src, dst)) in detaches {
        out.push(WalRecord::RelDelete { rel, src, dst });
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

    /// An edge the overlay took away stops being a neighbor at the
    /// epoch that took it, and recovery reads the same thing back out
    /// of the log.
    #[test]
    fn a_removed_edge_leaves_the_overlay_at_its_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel(9, 0, 1);
        txn.insert_rel(9, 0, 2);
        let e1 = txn.commit(&mut wal).unwrap();
        let mut txn = mvcc.begin();
        txn.delete_rel(9, 0, 1);
        let e2 = txn.commit(&mut wal).unwrap();

        let recovered = Mvcc::recover(&wal, 0).unwrap();
        for m in [&mvcc, &recovered] {
            let mut nbrs = Vec::new();
            m.neighbors(9, 0, false, e1, &mut nbrs);
            assert_eq!(nbrs, vec![1, 2], "both edges stand one epoch earlier");
            nbrs.clear();
            m.neighbors(9, 0, false, e2, &mut nbrs);
            assert_eq!(nbrs, vec![2]);
            nbrs.clear();
            m.neighbors(9, 1, true, e2, &mut nbrs);
            assert!(nbrs.is_empty(), "the end it arrived at loses it too");
            assert!(m.edge_gone(9, 0, 1, e2));
            assert!(!m.edge_gone(9, 0, 1, e1));
            assert_eq!(m.dead_edges(9, e2), vec![(0, 1)]);
            assert!(m.dead_edges(9, e1).is_empty());
            // What a fold would seal: the edge that was taken away is
            // not among the edges it would write.
            assert_eq!(m.edges(9, e2).collect::<Vec<_>>(), vec![(0, 2)]);
        }
    }

    /// A value written onto an edge is in the overlay at the epoch that
    /// wrote it and not one epoch earlier, and the log says the same
    /// thing, which is what a fold after a crash reads.
    #[test]
    fn a_value_written_onto_an_edge_leaves_the_overlay_at_its_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update_rel(9, 0, 1, 0, Cell::Int(1990));
        txn.update_rel(9, 0, 2, 1, Cell::Str(b"note".to_vec()));
        let e1 = txn.commit(&mut wal).unwrap();
        // A second value over the first one, which the chain keeps in
        // order so the reader takes the later of the two.
        let mut txn = mvcc.begin();
        txn.update_rel(9, 0, 1, 0, Cell::Null);
        let e2 = txn.commit(&mut wal).unwrap();

        let recovered = Mvcc::recover(&wal, 0).unwrap();
        for m in [&mvcc, &recovered] {
            assert!(!m.has_edge_updates(9, 0));
            assert!(m.has_edge_updates(9, e1));
            let first = m.edge_updates(9, e1);
            assert_eq!(first.get(&(0, 1, 0)), Some(&Cell::Int(1990)));
            assert_eq!(
                first.get(&(0, 2, 1)),
                Some(&Cell::Str(b"note".to_vec())),
                "the other edge and the other column"
            );
            let second = m.edge_updates(9, e2);
            assert_eq!(second.get(&(0, 1, 0)), Some(&Cell::Null));
            assert!(m.edge_updates(9, 0).is_empty());
        }
    }

    /// Two values written onto one property of one edge in one txn end
    /// with the second, because the chain is read back in the order it
    /// was staged.
    #[test]
    fn the_last_value_written_onto_an_edge_is_the_one_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = wal_in(&dir);
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update_rel(9, 0, 1, 0, Cell::Int(1));
        txn.update_rel(9, 0, 1, 0, Cell::Int(2));
        let epoch = txn.commit(&mut wal).unwrap();

        let recovered = Mvcc::recover(&wal, 0).unwrap();
        for m in [&mvcc, &recovered] {
            assert_eq!(
                m.edge_updates(9, epoch).get(&(0, 1, 0)),
                Some(&Cell::Int(2))
            );
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
