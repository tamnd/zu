//! The checkpoint fold: overlays into new sealed segments, published
//! by one header flip, per docs/08 section 4.
//!
//! A fold rebuilds every table the overlay store touched into fresh
//! blocks without publishing anything, then updates the catalog root,
//! the table index root, and `wal_seq` in a single [`Zu1File::checkpoint`]
//! call. That one flip is the atomicity point: a crash anywhere before
//! it leaves the old roots, the old `wal_seq`, and the intact WAL, so
//! recovery replays the same txns onto the same base and the fold's
//! half-written blocks are unreferenced garbage. A crash after it finds
//! `wal_seq` at the folded epoch, so replay skips everything the fold
//! sealed even when the WAL truncation never ran. Double apply is
//! impossible on either side.
//!
//! Deletes fold as tombstones, not compaction: offsets stay stable and
//! rows recycle only under VACUUM per docs/03. The database header has
//! no room for another root (its seven words and crc fill the body
//! exactly), so each table's tombstone set persists as a meta chain
//! under the reserved table index key `id | TOMBSTONE_KEY`, and
//! [`recover`] seeds it back into the overlay store at epoch 0.
//!
//! Column ids in overlay data are positions in the table's props
//! directory, the convention the WAL records established. Fold
//! granularity is the whole table; group-local rebuilds with slack
//! arrive with the updatable CSR work.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use zu_common::gqlstatus::codes;
use zu_common::{Epoch, Result, ZuError};

use crate::catalog::{Catalog, ElementKind, GraphType, RelTable, TableIndex};
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::graph::{
    Direction, Directory, GraphReader, GroupMeta, build_direction, build_direction_over,
    free_chain, free_directory_keeping_edges, free_directory_keeping_props, group_bases,
    group_rows, pad_direction,
};
use crate::keys::write_key_index_live;
use crate::meta;
use crate::props::{
    PropsDirectory, PropsReader, free_props_keeping_labels, free_props_reusing, load_props_at,
};
use crate::rows::{read_rows, rewrite_rows, rewrite_rows_reordered};
use crate::segment::{CHUNK_ROWS, read_segment, rewrite_segment, write_segment};
use crate::txn::{Cell, Mvcc};
use crate::wal::Wal;

/// Table index entries with this bit carry a tombstone chain for the
/// node table `id & !TOMBSTONE_KEY`. Catalog ids stay under the 14-bit
/// `NodeId` field, so the bit can never collide with a real table.
pub const TOMBSTONE_KEY: u32 = 1 << 31;

const TOMBSTONES_VERSION: u16 = 1;

fn corrupt(detail: String) -> ZuError {
    ZuError::Corrupt {
        what: "tombstone chain",
        detail,
    }
}

/// Encodes a sorted tombstone set: `version: u16`, `count: u64`, then
/// the offsets ascending.
pub fn encode_tombstones(offsets: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + offsets.len() * 8);
    out.extend_from_slice(&TOMBSTONES_VERSION.to_le_bytes());
    out.extend_from_slice(&(offsets.len() as u64).to_le_bytes());
    for &offset in offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out
}

/// Decodes a tombstone chain, rejecting unsorted or duplicate offsets.
pub fn decode_tombstones(bytes: &[u8]) -> Result<Vec<u64>> {
    let head = bytes
        .get(..10)
        .ok_or_else(|| corrupt("truncated header".into()))?;
    let version = u16::from_le_bytes(head[..2].try_into().unwrap());
    if version != TOMBSTONES_VERSION {
        return Err(ZuError::Unsupported {
            what: "tombstone chain version",
            id: u32::from(version),
        });
    }
    let count = u64::from_le_bytes(head[2..10].try_into().unwrap());
    if bytes.len() as u64 != 10 + count * 8 {
        return Err(corrupt(format!(
            "{} bytes hold {count} offsets",
            bytes.len()
        )));
    }
    let mut offsets = Vec::with_capacity(count as usize);
    for raw in bytes[10..].as_chunks::<8>().0 {
        let offset = u64::from_le_bytes(*raw);
        if let Some(&last) = offsets.last()
            && offset <= last
        {
            return Err(corrupt(format!("offset {offset} after {last}")));
        }
        offsets.push(offset);
    }
    Ok(offsets)
}

/// Rebuilds the overlay store after a crash or reopen: committed txns
/// above the header's `wal_seq` replay from the log, an `IngestRef`
/// resolves back into its payload by reading the sealed segments it
/// names, then every persisted tombstone chain seeds back in at epoch
/// 0.
///
/// A file with a transaction still open on it is put back first. The
/// log is cut to what the state going back in had folded, because a
/// frame above that is a statement of the transaction being taken back
/// and replaying it would put the transaction back on as an overlay,
/// and then the kept state is published. Nothing here is a special case
/// afterwards: the file reads as one whose last statement was the one
/// before the transaction started.
pub fn recover(db: &mut Zu1File, wal: &mut Wal) -> Result<Mvcc> {
    if let Some(open) = db.interrupted() {
        wal.rollback_above(open.log_floor)?;
        db.finish_rollback()?;
    }
    let floor = db.db_header().wal_seq;
    let mut mvcc = Mvcc::recover_with(wal, floor, |table, ptrs| {
        crate::ingest::resolve(db, table, ptrs)
    })?;
    seed_persisted_tombstones(db, &mut mvcc)?;
    Ok(mvcc)
}

fn seed_persisted_tombstones(db: &mut Zu1File, mvcc: &mut Mvcc) -> Result<()> {
    let entries: Vec<(u32, u64)> = TableIndex::load(db)?.entries().to_vec();
    for (id, root) in entries {
        if id & TOMBSTONE_KEY == 0 {
            continue;
        }
        let offsets = decode_tombstones(&meta::read_chain(db, root)?)?;
        mvcc.seed_tombstones(id & !TOMBSTONE_KEY, &offsets);
    }
    Ok(())
}

/// Folds every overlay at the store's current epoch into new sealed
/// segments, publishes them, and truncates the WAL. On return the
/// overlay store is empty at the same epoch with persisted tombstones
/// reseeded, and the file reads identically to the pre-fold snapshot
/// at that epoch. On error nothing published: the header, the WAL, and
/// the store are exactly as they were, and any blocks written are
/// unreferenced.
pub fn checkpoint_fold(db: &mut Zu1File, mvcc: &mut Mvcc, wal: &mut Wal) -> Result<()> {
    fold(db, mvcc, wal, true)
}

/// Folds the same way, and stops before the header flip.
///
/// This is the shape a write statement wants. The fold is what makes a
/// committed change something the next `MATCH` reads, so it has to
/// happen per commit; the flip is what makes it something the next
/// process reads, and the log already says that. Two full syncs of the
/// three a one cell write pays are the flip, and the frame the commit
/// synced covers exactly the window skipping them opens: a crash finds
/// the older header and replays the log back to where the fold had
/// got to.
///
/// What it costs is file growth. Nothing freed can be handed out again
/// until a checkpoint publishes, so every fold in between takes fresh
/// blocks for what it rewrites, and the caller watches
/// [`Zu1File::unpublished_blocks`] and publishes on a threshold.
pub fn staged_fold(db: &mut Zu1File, mvcc: &mut Mvcc, wal: &mut Wal) -> Result<()> {
    fold(db, mvcc, wal, false)
}

fn fold(db: &mut Zu1File, mvcc: &mut Mvcc, wal: &mut Wal, publish: bool) -> Result<()> {
    // A fold that failed partway may have left a packing scope open,
    // and a block half filled by work that was thrown away is not one
    // this fold may write into.
    db.pack_reset();
    let epoch = mvcc.epoch();
    let mut catalog = Catalog::load(db)?;
    let mut index = TableIndex::load(db)?;
    let mut changed = false;
    // Node tables first, so rel rebuilds see the grown row domains.
    let mut grown: HashSet<u32> = HashSet::new();
    // And which of them lost a row, which a keyed rel table over the
    // table has to hear about even when no edge of it moved.
    let mut retired: HashSet<u32> = HashSet::new();
    for table in mvcc.tables_touched() {
        let node = catalog.node_by_id(table).ok_or_else(|| {
            ZuError::InvalidArgument(format!("overlay names unknown node table {table}"))
        })?;
        let (base, primary) = (node.node_count, node.primary_label());
        // What the table has declared it may hold, which is what bounds
        // a label change: a bit outside it would leave a file that says
        // a row carries a label its table never declared.
        let declared = node.label_mask();
        let (name, graph) = (node.name.clone(), node.graph);
        let appended = mvcc.appended_rows(table, epoch);
        if appended > 0 || mvcc.has_updates(table, epoch) || mvcc.has_label_changes(table, epoch) {
            let labels_of = Labels {
                primary,
                declared,
                name: &name,
                graph_type: catalog.closed_type_of(graph),
                names: catalog.labels(),
            };
            // Everything this table's properties are about to be
            // written as packs into blocks of its own, because the
            // next fold of the table frees exactly those blocks.
            let held = db.pack_open();
            let folded = fold_props(db, mvcc, &mut index, table, &labels_of, base, epoch);
            db.pack_close(held);
            changed |= folded?;
        }
        if appended > 0 {
            catalog.grow_node(table, base + appended)?;
            grown.insert(table);
            changed = true;
        }
        if fold_tombstones(db, mvcc, &mut index, table, epoch)? {
            retired.insert(table);
            changed = true;
        }
    }
    let rels: Vec<RelTable> = catalog.rel_tables().to_vec();
    let dirty: HashSet<u32> = mvcc.rels_touched().into_iter().collect();
    for id in &dirty {
        if catalog.rel_by_id(*id).is_none() {
            return Err(ZuError::InvalidArgument(format!(
                "overlay names unknown rel table {id}"
            )));
        }
    }
    for rel in rels {
        let moved = dirty.contains(&rel.id) || grown.contains(&rel.from) || grown.contains(&rel.to);
        // A row that went away takes its key with it, and the key index
        // is the one thing a rel table holds about the rows rather than
        // about the edges. So a table that keys the node table the
        // delete hit is rebuilt too, and one that keys nothing is left
        // where it is: the edges of both are exactly as they were.
        if !moved && !(retired.contains(&rel.from) && is_keyed(db, &index, rel.id)?) {
            continue;
        }
        let held = db.pack_open();
        let folded = fold_rel(db, mvcc, &catalog, &mut index, &rel, epoch);
        db.pack_close(held);
        let edge_count = folded?;
        catalog.set_edge_count(rel.id, edge_count)?;
        changed = true;
    }
    // Ingested segments are sealed into the rebuilt tables above, so
    // their manifest and data blocks free here and publish with the
    // same flip; until it lands they stay untouched for replay.
    for root in mvcc.ingest_roots(epoch) {
        crate::ingest::free_ingest(db, root)?;
        changed = true;
    }
    if !changed && wal.is_empty() {
        return Ok(());
    }
    // Both roots and the WAL floor move together, so whichever way
    // this ends the header names segments that hold everything folded
    // through the epoch it names and nothing above it.
    free_chain(db, db.db_header().catalog_root)?;
    free_chain(db, db.db_header().table_index_root)?;
    let catalog_root = meta::write_chain(db, &catalog.encode())?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().catalog_root = catalog_root;
    db.db_header_mut().table_index_root = index_root;
    db.db_header_mut().wal_seq = epoch;
    match publish {
        true => {
            db.checkpoint()?;
            wal.truncate()?;
        }
        false => db.stage_fold(),
    }
    *mvcc = Mvcc::new(epoch);
    seed_persisted_tombstones(db, mvcc)
}

/// The validity words of one column while a fold rewrites it: what the
/// base said about the rows it had, widened over the rows the appends
/// added, and then whatever the overlay says about each row it names.
///
/// A column stores a mask only when some row of it holds nothing, so
/// this starts as every bit set on a column that had no mask, and it
/// writes nothing back when every bit is still set at the end. That is
/// what keeps a store that never removed anything the same size and the
/// same shape as one that could not.
struct Validity {
    words: Vec<u64>,
    rows: u64,
}

impl Validity {
    /// Reads the column's mask over `base` rows and widens it to
    /// `rows`, where the rows the appends added start out holding a
    /// value because an appended row carries one for every column.
    fn read(
        db: &mut Zu1File,
        col: &crate::props::PropColumn,
        base: u64,
        rows: u64,
    ) -> Result<Self> {
        let mut words = vec![u64::MAX; rows.div_ceil(64) as usize];
        if let Some(meta) = &col.validity {
            let mut stored = Vec::new();
            read_segment(db, meta, &mut stored)?;
            for (at, word) in stored.iter().enumerate() {
                let Some(slot) = words.get_mut(at) else { break };
                // The last stored word covers rows past `base` only if
                // the base did not end on a word boundary, and those
                // rows are appended ones, which hold a value. Keeping
                // their bits set is what the OR does.
                let covered = base.saturating_sub(at as u64 * 64).min(64);
                let mask = match covered {
                    64 => u64::MAX,
                    n => (1u64 << n) - 1,
                };
                *slot = (word & mask) | !mask;
            }
        }
        Ok(Self { words, rows })
    }

    /// A mask over `rows` rows that all hold a value, which is where a
    /// rebuilt rel table's column starts: an edge order the fold just
    /// built has no old mask to widen, so every bit is put there by what
    /// the fold reads or writes into each slot.
    fn full(rows: u64) -> Self {
        Self {
            words: vec![u64::MAX; rows.div_ceil(64) as usize],
            rows,
        }
    }

    fn set(&mut self, offset: u64) {
        self.words[(offset / 64) as usize] |= 1u64 << (offset % 64);
    }

    fn clear(&mut self, offset: u64) {
        self.words[(offset / 64) as usize] &= !(1u64 << (offset % 64));
    }

    /// Whether every row of the column holds a value, which is asked of
    /// the rows the column has rather than of the words, since the last
    /// word runs past the last row.
    fn all_set(&self) -> bool {
        (0..self.rows)
            .all(|offset| self.words[(offset / 64) as usize] & (1u64 << (offset % 64)) != 0)
    }

    /// The segment the rebuilt column points at, or `None` when every
    /// row holds a value and the column needs no mask.
    fn write(mut self, db: &mut Zu1File) -> Result<Option<crate::segment::SegmentMeta>> {
        if self.all_set() {
            return Ok(None);
        }
        // The bits past the last row belong to no row, and a mask is
        // compared word for word by everything that reads it, so they
        // are cleared rather than left as whatever the widening put
        // there.
        if let Some(last) = self.words.last_mut()
            && !self.rows.is_multiple_of(64)
        {
            *last &= (1u64 << (self.rows % 64)) - 1;
        }
        Ok(Some(write_segment(db, &self.words)?))
    }
}

/// Merges one table's appended rows and update chains into a rebuilt
/// props directory. A table without stored props cannot absorb column
/// data, and its row count still grows through the catalog alone.
/// What one table's rows may be called: the label its own name is,
/// which every row carries, the mask of every label the table has
/// declared, which bounds what a change may put on a row, and what the
/// graph's type says a row of it may look like.
#[derive(Debug, Clone)]
struct Labels<'a> {
    primary: u16,
    declared: u64,
    /// The table's own name, which is what a refusal calls it: an id
    /// is what the file addresses a table by and not what anybody
    /// wrote.
    name: &'a str,
    /// The closed graph type the table's graph is of, `None` when the
    /// graph has none or has an open one. A closed type is a promise
    /// about every element, so a row that a change would take out of
    /// every element type is a change that cannot land.
    graph_type: Option<&'a GraphType>,
    /// The graph's label dictionary, which is how a refusal says which
    /// labels a row would have carried rather than printing the word.
    names: &'a [String],
}

/// The labels a word holds, written out, which is what a message about
/// one says: a bitmask names nothing to the person reading it.
fn spell(word: u64, names: &[String]) -> String {
    let mut out = String::new();
    for id in 0..64u16 {
        if word & 1 << id == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        match names.get(usize::from(id)) {
            Some(name) => out.push_str(name),
            None => out.push_str(&format!("label {id}")),
        }
    }
    out
}

/// Refuses a value the column's declared width does not admit.
///
/// A fold is the last place between a statement and the file, so this
/// is where a column declared `BINARY(16)` stops holding fifteen
/// octets, whatever wrote them. The check is by the column and not by
/// the statement on purpose: a column whose own type does not describe
/// its rows is worse than a refused write, because every reader after
/// it takes the type at its word. A list column written at its bound is
/// the same case: its rows carry no count, so a row of another length
/// is a row nothing can read back.
fn at_width(col: &crate::props::PropColumn, value: &[u8]) -> Result<()> {
    match col.row_width() {
        Some(width) if value.len() != width => Err(ZuError::InvalidArgument(format!(
            "column '{}' is {} and was given {} octets",
            col.name,
            col.ty,
            value.len()
        ))),
        _ => Ok(()),
    }
}

fn fold_props(
    db: &mut Zu1File,
    mvcc: &Mvcc,
    index: &mut TableIndex,
    table: u32,
    labels_of: &Labels,
    base: u64,
    epoch: Epoch,
) -> Result<bool> {
    let primary = labels_of.primary;
    let Some(root) = index.get(table) else {
        if mvcc.appends_carry_columns(table, epoch) || mvcc.has_updates(table, epoch) {
            return Err(ZuError::Unsupported {
                what: "folding column data into a table without stored props",
                id: table,
            });
        }
        if !mvcc.has_label_changes(table, epoch) {
            return Ok(false);
        }
        // A table that stores nothing on its rows still has labels to
        // hold, and the bitset hangs off a props directory, so the
        // first label change is what makes the directory exist. Its
        // columns stay empty: what the change carries is a word per
        // row, not a value.
        let new_count = base + mvcc.appended_rows(table, epoch);
        let mut words = vec![1u64 << primary; new_count as usize];
        apply_label_changes(
            &mvcc.label_changes(table, epoch),
            &mut words,
            table,
            labels_of,
        )?;
        let dir = PropsDirectory {
            node_count: new_count,
            columns: Vec::new(),
            labels: Some(write_segment(db, &words)?),
        };
        index.set(table, meta::write_chain(db, &dir.encode())?);
        return Ok(true);
    };
    let dir = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    if dir.node_count != base {
        return Err(ZuError::Corrupt {
            what: "props directory",
            detail: format!(
                "table {table} props span {} rows, catalog holds {base}",
                dir.node_count
            ),
        });
    }
    let new_count = base + mvcc.appended_rows(table, epoch);
    let mismatch = |name: &str, offset: u64| {
        ZuError::InvalidArgument(format!(
            "row {offset} of column '{name}' does not match its stored type"
        ))
    };

    let missing = |name: &str, offset: u64| {
        ZuError::InvalidArgument(format!(
            "appended row {offset} carries no value for column '{name}'"
        ))
    };
    // A column the transaction wrote nothing into comes out of the fold
    // holding what it went in holding, so the fold neither reads it nor
    // writes it: the new directory names the segments the old one named
    // and their blocks stay where they are. That is the difference
    // between a one cell write costing one column and costing every
    // column of the table, and it holds only while the row domain does,
    // because a column that has to grow has to be rewritten to grow.
    let touched = mvcc.touched_cols(table, epoch);
    let grew = new_count != base;
    let mut reused = vec![false; dir.columns.len()];
    let mut columns = Vec::with_capacity(dir.columns.len());
    for (ci, col) in dir.columns.iter().enumerate() {
        if !grew && !touched.contains(&(ci as u32)) {
            reused[ci] = true;
            columns.push(col.clone());
            continue;
        }
        // A column holds a null the way storage does, as a row whose
        // validity bit is clear, so the fold carries a mask beside the
        // values: what the base said, widened over the rows the appends
        // added, and then whatever the overlay says about each row it
        // names. A column nothing removed from ends with every bit set
        // and stores no mask at all, so a graph with no null in it pays
        // nothing for the ones that could have been.
        let mut valid = Validity::read(db, col, base, new_count)?;
        // The fold splits the way storage does, on the lane against
        // the blob, because that is what it rewrites. Cells the overlay
        // holds are words or byte strings and the column's type says
        // which of the two it must be.
        let meta = if col.is_lane() {
            let mut values = Vec::with_capacity(new_count as usize);
            read_segment(db, &col.meta, &mut values)?;
            // Which chunks of the segment the fold is about to change,
            // so that the rest keep the bytes they already encode to
            // rather than going back through the cascade selector.
            let mut dirty = BTreeSet::new();
            let mut touch = |offset: u64| {
                dirty.insert(offset as usize / CHUNK_ROWS);
            };
            // The overlay says which rows it holds rather than being
            // asked about each one in turn. Asking row by row is a
            // probe per row of the table to find the handful a
            // statement wrote, and that is the whole of what a one
            // cell write into a wide table used to cost.
            for (offset, cell) in mvcc.col_updates(table, ci as u32, base, epoch) {
                match cell {
                    Cell::Int(x) => {
                        values[offset as usize] = x;
                        valid.set(offset);
                    }
                    // A removed row keeps a word where its value was,
                    // because the lane is fixed width and a reader that
                    // has been told the bit is clear never looks at it.
                    Cell::Null => {
                        values[offset as usize] = 0;
                        valid.clear(offset);
                    }
                    Cell::Str(_) => return Err(mismatch(&col.name, offset)),
                }
                touch(offset);
            }
            // The rows the appends added, which are new and so are all
            // of them dirty. `cell` rather than the batch directly,
            // because a statement can write over a row another one in
            // the same fold window appended.
            for offset in base..new_count {
                match mvcc.cell(table, base, offset, ci as u32, epoch) {
                    Some(Cell::Int(x)) => {
                        values.push(x);
                        valid.set(offset);
                    }
                    Some(Cell::Null) => {
                        values.push(0);
                        valid.clear(offset);
                    }
                    Some(Cell::Str(_)) => return Err(mismatch(&col.name, offset)),
                    None => return Err(missing(&col.name, offset)),
                }
                touch(offset);
            }
            rewrite_segment(db, &col.meta, &values, &dirty)?
        } else {
            // The blob side carries the rows a statement wrote rather
            // than the column they are in, because reading the column
            // to change one cell of it is the cost the rewrite exists
            // to avoid: the segment reads back only the chunks these
            // rows land in.
            // What a column declared `BINARY(n)`, or a list column
            // written at its bound, holds where it holds nothing. The
            // absence below is the empty string, which in a column of
            // equal length rows is a row of another length: it would
            // cost the column its layout for bytes no reader looks at,
            // so it is written at the declared width instead.
            let absent = || col.row_width().map_or_else(Vec::new, |n| vec![0u8; n]);
            let mut updates = BTreeMap::new();
            for (offset, cell) in mvcc.col_updates(table, ci as u32, base, epoch) {
                match cell {
                    Cell::Str(s) => {
                        at_width(col, &s)?;
                        updates.insert(offset, s);
                        valid.set(offset);
                    }
                    // The blob side stores the absence as the empty
                    // string, which costs the row nothing: a blob is
                    // addressed by its ends and two equal ends are no
                    // bytes at all.
                    Cell::Null => {
                        updates.insert(offset, absent());
                        valid.clear(offset);
                    }
                    Cell::Int(_) => return Err(mismatch(&col.name, offset)),
                }
            }
            let mut appended = Vec::with_capacity((new_count - base) as usize);
            for offset in base..new_count {
                match mvcc.cell(table, base, offset, ci as u32, epoch) {
                    Some(Cell::Str(s)) => {
                        at_width(col, &s)?;
                        appended.push(s);
                        valid.set(offset);
                    }
                    Some(Cell::Null) => {
                        appended.push(absent());
                        valid.clear(offset);
                    }
                    Some(Cell::Int(_)) => return Err(mismatch(&col.name, offset)),
                    None => return Err(missing(&col.name, offset)),
                }
            }
            rewrite_rows(db, &col.meta, &updates, &appended)?
        };
        // A zoned column's second plane grows with the rows the way the
        // first one does, and it is dirty in the same chunks. What a
        // commit left carries no zone, because a cell is one word, so a
        // row a statement wrote is UTC, which is the offset a value
        // written without one already has.
        let zones = match col.zones.as_ref() {
            None => None,
            Some(plane) => {
                let mut offsets = Vec::with_capacity(new_count as usize);
                read_segment(db, plane, &mut offsets)?;
                let mut dirty = BTreeSet::new();
                for (offset, _) in mvcc.col_updates(table, ci as u32, base, epoch) {
                    offsets[offset as usize] = 0;
                    dirty.insert(offset as usize / CHUNK_ROWS);
                }
                for offset in base..new_count {
                    offsets.push(0);
                    dirty.insert(offset as usize / CHUNK_ROWS);
                }
                Some(rewrite_segment(db, plane, &offsets, &dirty)?)
            }
        };
        columns.push(crate::props::PropColumn {
            name: col.name.clone(),
            ty: col.ty.clone(),
            meta,
            validity: valid.write(db)?,
            // Every row that went past `at_width` is the width the
            // column was already written at, so the count the rows do
            // not carry is the count they still do not carry.
            fixed_len: col.fixed_len,
            // And the same sentence for the width the rows that do carry
            // a count carried it in. This directory is being written at
            // the current version over row blobs that were written at
            // whatever version they were written at, so the width has to
            // come off the old entry rather than off the declared bound:
            // a version 11 column said four, and its rows still say four
            // however narrow the bound would let a fresh write be.
            count_width: col.count_width,
            zones,
        });
    }
    // The label bitset grows with the row domain, and an appended row
    // carries the table's own label and nothing else until something
    // says otherwise: a row of a table is what that table is called.
    // What says otherwise is a label change, which is a pair of masks
    // per row rather than a word, so a row nothing named keeps what it
    // had and the ones named take their bits on and off.
    let changes = mvcc.label_changes(table, epoch);
    // The bitset comes through on the same terms a column does: the
    // same rows, and nothing renamed one of them.
    let keep_labels = !grew && changes.is_empty();
    let labels = match (&dir.labels, changes.is_empty()) {
        (None, true) => None,
        (Some(meta), _) if keep_labels => Some(meta.clone()),
        (base_labels, _) => {
            let mut words = Vec::with_capacity(new_count as usize);
            if let Some(meta) = base_labels {
                read_segment(db, meta, &mut words)?;
            }
            words.resize(new_count as usize, 1u64 << primary);
            apply_label_changes(&changes, &mut words, table, labels_of)?;
            Some(write_segment(db, &words)?)
        }
    };
    free_props_reusing(db, root, keep_labels, &reused)?;
    let new_dir = PropsDirectory {
        node_count: new_count,
        columns,
        labels,
    };
    index.set(table, meta::write_chain(db, &new_dir.encode())?);
    Ok(true)
}

/// Puts the labels one txn added onto the rows it named and takes the
/// ones it removed off them. The two masks of a change are disjoint, so
/// the order of the two halves does not matter and the row ends with
/// exactly what the last statement to name it said.
fn apply_label_changes(
    changes: &BTreeMap<u64, (u64, u64)>,
    words: &mut [u64],
    table: u32,
    labels_of: &Labels,
) -> Result<()> {
    let rows = words.len();
    let primary = 1u64 << labels_of.primary;
    for (&offset, &(add, remove)) in changes {
        let undeclared = add & !labels_of.declared;
        if undeclared != 0 {
            return Err(ZuError::InvalidArgument(format!(
                "a label change puts {undeclared:#x} on a row of table {table}, \
                 which has not declared it"
            )));
        }
        if remove & primary != 0 {
            return Err(ZuError::InvalidArgument(format!(
                "a label change takes the name of table {table} off one of its own rows"
            )));
        }
        let word = words.get_mut(offset as usize).ok_or_else(|| {
            ZuError::InvalidArgument(format!(
                "a label change names row {offset} of table {table}, which holds {rows} rows"
            ))
        })?;
        let after = (*word | add) & !remove;
        // A closed graph type is a promise about every element the
        // graph holds, so it is checked here, where the label set the
        // row ends with is known. The type an element belongs to is
        // allowed to change, which is what a key label set change is;
        // what is not allowed is ending up in none of them.
        if let Some(ty) = labels_of.graph_type
            && ty.holder(ElementKind::Node, after).is_none()
        {
            return Err(ZuError::gql(
                codes::CG2000,
                format!(
                    "the element at row {offset} of '{}' would carry {} after this change, and no element type of graph type '{}' describes a node carrying that",
                    labels_of.name,
                    spell(after, labels_of.names),
                    ty.name
                ),
            ));
        }
        *word = after;
    }
    Ok(())
}

/// Merges one table's overlay tombstones with its persisted chain and
/// rewrites the chain when the set changed.
fn fold_tombstones(
    db: &mut Zu1File,
    mvcc: &Mvcc,
    index: &mut TableIndex,
    table: u32,
    epoch: Epoch,
) -> Result<bool> {
    let key = table | TOMBSTONE_KEY;
    let mut offsets = mvcc.tombstones(table, epoch);
    let previous = match index.get(key) {
        Some(root) => decode_tombstones(&meta::read_chain(db, root)?)?,
        None => Vec::new(),
    };
    offsets.extend_from_slice(&previous);
    offsets.sort_unstable();
    offsets.dedup();
    if offsets == previous {
        return Ok(false);
    }
    if let Some(root) = index.get(key) {
        free_chain(db, root)?;
        index.remove(key);
    }
    if !offsets.is_empty() {
        index.set(key, meta::write_chain(db, &encode_tombstones(&offsets))?);
    }
    Ok(true)
}

/// Rebuilds one rel table over its possibly grown row domain, merging
/// overlay edges into the base CSR. Returns the new edge count.
fn fold_rel(
    db: &mut Zu1File,
    mvcc: &Mvcc,
    catalog: &Catalog,
    index: &mut TableIndex,
    rel: &RelTable,
    epoch: Epoch,
) -> Result<u64> {
    let root = index.get(rel.id).ok_or_else(|| ZuError::Corrupt {
        what: "table index",
        detail: format!("rel table '{}' has no directory entry", rel.name),
    })?;
    // Both ends can have grown, and they are different tables when the
    // rel table runs between two labels, so each end is asked for its
    // own row count rather than one standing for the pair.
    let end_rows = |id: u32| {
        catalog
            .node_by_id(id)
            .expect("validated on decode")
            .node_count
    };
    let (new_from, new_to) = (end_rows(rel.from), end_rows(rel.to));
    if new_from.max(new_to) > u64::from(u32::MAX) {
        return Err(ZuError::Unsupported {
            what: "folding a rel table past the u32 row domain",
            id: rel.id,
        });
    }
    let mut reader = GraphReader::load_table(db, &rel.name)?;
    let old = reader.directory().clone();
    let overlay: Vec<AddedEdge> = mvcc
        .edge_cells(rel.id, epoch)
        .map(|(src, dst, cols)| (src, dst, cols.to_vec()))
        .collect();
    // An edge carries a value only where there is a column to put it
    // in, and what columns a rel table has is the table's own answer.
    if old.props == NULL_BLOCK
        && (overlay.iter().any(|(_, _, cols)| !cols.is_empty())
            || mvcc.has_edge_updates(rel.id, epoch))
    {
        return Err(ZuError::Unsupported {
            what: "writing a property onto an edge of a table that stores none on its edges",
            id: rel.id,
        });
    }
    // An edge property column is in load order, so the fold has to say
    // where every edge of the new order came from: an edge the base
    // already held keeps the value at its old place, and one the
    // overlay adds takes the value it was written with.
    let mut edges: Vec<(u32, u32, Came)> =
        Vec::with_capacity(old.edge_count as usize + overlay.len());
    // An edge the statement took away is dropped here rather than
    // tombstoned, because the CSR is being rebuilt anyway and an edge
    // has no offset a reader could filter by. The ordinal still counts
    // it: the property columns it is being cut out of are in the old
    // load order, and dropping an edge does not move the ones behind it
    // in that order.
    let dead: BTreeSet<(u64, u64)> = mvcc.dead_edges(rel.id, epoch).into_iter().collect();
    let mut ordinal = 0usize;
    for node in 0..old.from_count {
        for &dst in reader.neighbors_dir(db, node, Direction::Fwd)? {
            if !dead.contains(&(node, dst)) {
                edges.push((node as u32, dst as u32, Came::Base(ordinal)));
            }
            ordinal += 1;
        }
    }
    for (at, (src, dst, _)) in overlay.iter().enumerate() {
        let (src, dst) = (*src, *dst);
        if src >= new_from || dst >= new_to {
            return Err(ZuError::InvalidArgument(format!(
                "overlay edge ({src}, {dst}) references a row outside 0..{new_from} and 0..{new_to}"
            )));
        }
        edges.push((src as u32, dst as u32, Came::Overlay(at)));
    }
    // A pair can run more than once: a bulk load holds every copy the
    // file gave it, and the reader answers a pair with the whole run.
    // What the copies need is an order, since the property column is
    // dense over the edges and the permutation below is what says which
    // value belongs to which. This sort is stable and the edges went in
    // base first, in the order the base holds them, so a copy keeps its
    // place among the copies and an overlay edge lands behind the ones
    // that were already there, which is where a newly written edge
    // belongs.
    edges.sort_by_key(|&(src, dst, _)| (src, dst));
    let order: Vec<Came> = edges.iter().map(|&(_, _, came)| came).collect();
    let edges: Vec<(u32, u32)> = edges.into_iter().map(|(src, dst, _)| (src, dst)).collect();
    // A keyed table's index is rebuilt row for row, which is what lets
    // the row domain grow underneath it: the rows the base had keep the
    // keys the index already holds, and the rows the appends added take
    // theirs from the `id` column of the node table the index is over.
    // Node tables fold ahead of rel tables, so that column already
    // holds the appended values by the time this reads it.
    let key_by_row = match &old.keys {
        None => None,
        Some(keys) => {
            let mut key_list = Vec::with_capacity(keys.keys.value_count as usize);
            read_segment(db, &keys.keys, &mut key_list)?;
            let mut rows = Vec::with_capacity(keys.rows.value_count as usize);
            read_segment(db, &keys.rows, &mut rows)?;
            let mut by_row = vec![0u64; new_from as usize];
            for (i, &row) in rows.iter().enumerate() {
                // A row the index names that the table no longer has is
                // a file that disagrees with itself, and reading past
                // the end of the vector would panic rather than say so.
                let slot = by_row.get_mut(row as usize).ok_or_else(|| {
                    corrupt(format!(
                        "the key index of '{}' names row {row} of a table holding {new_from}",
                        rel.name
                    ))
                })?;
                *slot = key_list[i];
            }
            read_appended_keys(db, index, rel, old.from_count, &mut by_row)?;
            Some(by_row)
        }
    };
    // Which of those rows a DELETE took away, which the rebuilt index
    // leaves out. Node tables fold first, so the fold's own table index
    // already holds the merged set rather than the published one.
    let dead_rows = tombstones_of(db, index, rel.from)?;
    let changed = mvcc.edge_updates(rel.id, epoch);
    // Whether anything wrote an edge, which is not the same question as
    // whether this fold has work to do. A rel table is rebuilt whenever
    // either end table grows, because the row domain its CSR is keyed by
    // has moved, and a row appended to that domain adds an offset and no
    // neighbour. So the lists are the lists the base holds and the
    // columns over them still name the same edges in the same order.
    let stood = overlay.is_empty() && dead.is_empty() && changed.is_empty();
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let (mut fwd, mut bwd) = match stood {
        true => {
            free_directory_keeping_edges(db, root)?;
            (
                build_direction_over(db, "source", new_from, &edges, &old.groups, Direction::Fwd)?,
                build_direction_over(db, "destination", new_to, &rev, &old.groups, Direction::Bwd)?,
            )
        }
        false => {
            free_directory_keeping_props(db, root)?;
            (
                build_direction(db, "source", new_from, &edges)?,
                build_direction(db, "destination", new_to, &rev)?,
            )
        }
    };
    drop(rev);
    let group_count = fwd.len().max(bwd.len());
    pad_direction(db, &mut fwd, group_count)?;
    pad_direction(db, &mut bwd, group_count)?;
    let bases = group_bases(new_from, &edges);
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: group_rows(new_from, g as u64),
            edge_base: bases.get(g).copied().unwrap_or(edges.len() as u64),
            fwd,
            bwd,
        })
        .collect();
    // The columns go the same way the neighbour lists did, and for the
    // same reason. A column is dense over the edge ordinal, the edges
    // are where they were, and the values are in the order they were
    // laid out in, so the old chain is already the answer. Writing it
    // again would cost the whole of it: on the LinkBench shape that is
    // two and a half megabytes of identical bytes handed back to the
    // allocator per fold, and folds land often.
    let props = match old.props {
        NULL_BLOCK => NULL_BLOCK,
        root if stood => root,
        root => {
            // Inside the rel table's own scope but not sharing with
            // it: a fold that keeps the adjacency still rewrites the
            // properties, and the two are freed by different calls.
            let held = db.pack_open();
            let folded = fold_rel_props(db, rel, root, &order, &edges, &overlay, &changed);
            db.pack_close(held);
            folded?
        }
    };
    let directory = Directory {
        from_count: new_from,
        to_count: new_to,
        edge_count: edges.len() as u64,
        keys: key_by_row
            .map(|keys| write_key_index_live(db, &keys, &dead_rows))
            .transpose()?,
        props,
        groups,
    };
    index.set(rel.id, meta::write_chain(db, &directory.encode())?);
    Ok(directory.edge_count)
}

/// Whether a rel table carries a key index over the rows it leaves,
/// read from its directory rather than from a rebuild of it.
fn is_keyed(db: &mut Zu1File, index: &TableIndex, rel: u32) -> Result<bool> {
    let Some(root) = index.get(rel) else {
        return Ok(false);
    };
    Ok(Directory::decode(&meta::read_chain(db, root)?)?
        .keys
        .is_some())
}

/// The rows of a node table that a DELETE took away, sorted, as the
/// fold's own table index holds them.
fn tombstones_of(db: &mut Zu1File, index: &TableIndex, table: u32) -> Result<Vec<u64>> {
    match index.get(table | TOMBSTONE_KEY) {
        Some(root) => decode_tombstones(&meta::read_chain(db, root)?),
        None => Ok(Vec::new()),
    }
}

/// Fills in the keys of the rows a fold appended to the node table a
/// rel table's key index is built over.
///
/// `by_row` is the whole index laid out row by row, already holding the
/// keys of the rows `base` covered, and the rows from `base` on are the
/// ones this answers. Their keys are the values of the node table's
/// `id` column, which is where the loader put them and where a reader
/// asking for `n.id` looks: a table whose ids are its row offsets is
/// dense and carries no key index at all, so a table that has one has
/// the column too.
///
/// Doing nothing when the domain did not grow is what makes this free
/// on the fold that only rewrote a property.
///
/// The directory is reached through the fold's own table index rather
/// than the file's, because the file's is a checkpoint behind: the node
/// props this reads were rebuilt earlier in this same fold and nothing
/// is published until it ends.
fn read_appended_keys(
    db: &mut Zu1File,
    index: &TableIndex,
    rel: &RelTable,
    base: u64,
    by_row: &mut [u64],
) -> Result<()> {
    if by_row.len() as u64 <= base {
        return Ok(());
    }
    let root = index.get(rel.from).ok_or(ZuError::Unsupported {
        what: "growing a keyed table whose node table stores no properties",
        id: rel.id,
    })?;
    let dir = load_props_at(db, root)?;
    let mut reader = PropsReader::new(dir);
    let col = reader.col("id").ok_or(ZuError::Unsupported {
        what: "growing a keyed table whose node table has no 'id' column to take a key from",
        id: rel.id,
    })?;
    for row in base..by_row.len() as u64 {
        by_row[row as usize] = reader.read_int(db, col, row)?;
    }
    Ok(())
}

/// Where one edge of a rebuilt rel table came from, which is what says
/// where its property values are: an edge the base held keeps the value
/// at its place in the old load order, and one the overlay adds takes
/// the value it was written with.
#[derive(Debug, Clone, Copy)]
enum Came {
    Base(usize),
    Overlay(usize),
}

/// One edge a fold is adding to a rel table: the row it leaves, the row
/// it arrives at, and the cell it holds in each column of the table.
type AddedEdge = (u64, u64, Vec<(u32, Cell)>);

/// Rewrites a rel table's property columns into the edge order the fold
/// just built, and answers the new props root.
///
/// A column is dense over the edges in load order, and an added edge
/// moves every edge behind it, so this is a rewrite of the whole column
/// rather than an append to it. The old blocks are freed once their
/// values are read.
///
/// `order` says where each edge of the new order came from and `pairs`
/// says which rows it runs between, in the same order, which is what
/// lets a statement that wrote onto an edge be found: an edge is named
/// by its pair, so `changed` is keyed by the pair and the column rather
/// than by a place in a column that the fold is in the middle of
/// deciding.
fn fold_rel_props(
    db: &mut Zu1File,
    rel: &RelTable,
    props: BlockPtr,
    order: &[Came],
    pairs: &[(u32, u32)],
    overlay: &[AddedEdge],
    changed: &BTreeMap<(u64, u64, u32), Cell>,
) -> Result<BlockPtr> {
    let dir = load_props_at(db, props)?;
    let missing = |name: &str| {
        ZuError::InvalidArgument(format!(
            "the new edge carries no value for column '{name}' of '{}', and every edge of a table that stores properties holds one",
            rel.name
        ))
    };
    let mismatch = |name: &str| {
        ZuError::InvalidArgument(format!(
            "the new edge's value for column '{name}' of '{}' does not match its stored type",
            rel.name
        ))
    };
    let mut columns = Vec::with_capacity(dir.columns.len());
    for (ci, col) in dir.columns.iter().enumerate() {
        let cell = |at: usize| {
            overlay[at]
                .2
                .iter()
                .find(|(c, _)| *c == ci as u32)
                .map(|(_, cell)| cell)
                .ok_or_else(|| missing(&col.name))
        };
        // What a statement wrote onto the edge in this slot, which
        // stands in for whatever the slot would otherwise have held:
        // an edge the base held keeps its old value only where nothing
        // has been written over it.
        let written = |slot: usize| {
            let (src, dst) = pairs[slot];
            changed.get(&(u64::from(src), u64::from(dst), ci as u32))
        };
        // A column holds a null as a clear bit beside a placeholder, the
        // way a node column does. The mask starts full because the edge
        // order is new: a base edge brings its old bit along and every
        // other slot is a value until a `REMOVE` says otherwise.
        let mut valid = Validity::full(order.len() as u64);
        let mut was = Vec::new();
        if let Some(meta) = &col.validity {
            read_segment(db, meta, &mut was)?;
        }
        let held = |at: usize| was.is_empty() || was[at / 64] & (1u64 << (at % 64)) != 0;
        // The fold splits on the lane against the blob, because that is
        // what it rewrites, and the column's type says which of the two
        // a cell going into it must be.
        let meta = if col.is_lane() {
            let mut old = Vec::with_capacity(order.len());
            read_segment(db, &col.meta, &mut old)?;
            let mut values = Vec::with_capacity(order.len());
            for (slot, came) in order.iter().enumerate() {
                values.push(match written(slot) {
                    Some(Cell::Int(x)) => *x,
                    // The lane keeps a word where the value was, since
                    // it is fixed width and a reader told the bit is
                    // clear never looks at what is under it.
                    Some(Cell::Null) => {
                        valid.clear(slot as u64);
                        0
                    }
                    Some(Cell::Str(_)) => return Err(mismatch(&col.name)),
                    None => match came {
                        Came::Base(at) => {
                            if !held(*at) {
                                valid.clear(slot as u64);
                            }
                            old[*at]
                        }
                        // An added edge says the absence the same way a
                        // written one does, and for the same reason:
                        // the lane is fixed width, so the bit says
                        // there is nothing there and the word under it
                        // is never read.
                        Came::Overlay(at) => match cell(*at)? {
                            Cell::Int(x) => *x,
                            Cell::Null => {
                                valid.clear(slot as u64);
                                0
                            }
                            Cell::Str(_) => return Err(mismatch(&col.name)),
                        },
                    },
                });
            }
            write_segment(db, &values)?
        } else {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_rows(db, &col.meta, &mut bytes, &mut ends)?;
            let mut old: Vec<&[u8]> = Vec::with_capacity(ends.len());
            let mut start = 0usize;
            for &end in &ends {
                old.push(&bytes[start..end as usize]);
                start = end as usize;
            }
            // An edge column takes the same rule a node column does: in
            // a column whose declaration fixes the width, the absence is
            // a row of that width rather than no bytes at all, and a
            // value of another length is refused.
            let pad = vec![0u8; col.row_width().unwrap_or(0)];
            let mut values: Vec<&[u8]> = Vec::with_capacity(order.len());
            for (slot, came) in order.iter().enumerate() {
                values.push(match written(slot) {
                    Some(Cell::Str(s)) => {
                        at_width(col, s)?;
                        s.as_slice()
                    }
                    // The blob side says the absence with no bytes at
                    // all, which costs the edge nothing: a blob is
                    // addressed by its ends and two equal ends are
                    // empty.
                    Some(Cell::Null) => {
                        valid.clear(slot as u64);
                        &pad
                    }
                    Some(Cell::Int(_)) => return Err(mismatch(&col.name)),
                    None => match came {
                        Came::Base(at) => {
                            if !held(*at) {
                                valid.clear(slot as u64);
                            }
                            old[*at]
                        }
                        Came::Overlay(at) => match cell(*at)? {
                            Cell::Str(s) => {
                                at_width(col, s)?;
                                s.as_slice()
                            }
                            Cell::Null => {
                                valid.clear(slot as u64);
                                &pad
                            }
                            Cell::Int(_) => return Err(mismatch(&col.name)),
                        },
                    },
                });
            }
            rewrite_rows_reordered(db, &col.meta, &values)?
        };
        // The zone plane follows its edges the way the values above do.
        // A plane left in the old order would pair every edge the sort
        // moved with somebody else's zone, which is a wrong answer and
        // not a missing one, so it is permuted here and not later.
        let zones = match col.zones.as_ref() {
            None => None,
            Some(plane) => {
                let mut old = Vec::with_capacity(order.len());
                read_segment(db, plane, &mut old)?;
                let moved: Vec<u64> = order
                    .iter()
                    .map(|came| match came {
                        Came::Base(at) => old[*at],
                        // An added edge has no zone to bring, for the
                        // reason a written one has none: a cell is one
                        // word and the zone is not in it.
                        Came::Overlay(_) => 0,
                    })
                    .collect();
                Some(write_segment(db, &moved)?)
            }
        };
        columns.push(crate::props::PropColumn {
            name: col.name.clone(),
            ty: col.ty.clone(),
            meta,
            validity: valid.write(db)?,
            fixed_len: col.fixed_len,
            count_width: col.count_width,
            zones,
        });
    }
    free_props_keeping_labels(db, props)?;
    let new = PropsDirectory {
        node_count: order.len() as u64,
        columns,
        labels: dir.labels,
    };
    meta::write_chain(db, &new.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{bulk_load_as, bulk_load_keyed};
    use crate::props::{ListRows, PropValues, PropsReader, load_props, store_props};
    use zu_common::LogicalType;

    struct Fixture {
        db: Zu1File,
        wal: Wal,
        mvcc: Mvcc,
        person: u32,
        knows: u32,
    }

    /// Four people with age and name props, three edges, and one
    /// committed txn: two appended rows, edges both ways between old
    /// and new, an update on row 1, a delete of row 2.
    fn seeded(dir: &std::path::Path) -> Fixture {
        let mut db = Zu1File::create(&dir.join("fold.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let mut wal = Wal::open(&dir.join("fold.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(
            person,
            vec![
                (0, vec![Cell::Int(50), Cell::Int(60)]),
                (
                    1,
                    vec![Cell::Str(b"eva".to_vec()), Cell::Str(b"raj".to_vec())],
                ),
            ],
        )
        .unwrap();
        txn.insert_rel(knows, 4, 0);
        txn.insert_rel(knows, 0, 4);
        txn.update(person, 1, 0, Cell::Int(21));
        txn.delete(person, 2);
        assert_eq!(txn.commit(&mut wal).unwrap(), 1);
        Fixture {
            db,
            wal,
            mvcc,
            person,
            knows,
        }
    }

    /// Declares a label for a table and publishes the catalog, without
    /// storing a bitset. That is the state a txn will leave once it can
    /// stage a catalog change of its own, and it is what a fold has to
    /// be able to write the first bitset from.
    fn declare(db: &mut Zu1File, table: u32, label: &str) -> u16 {
        let mut catalog = Catalog::load(db).unwrap();
        let id = catalog.declare_label(table, label).unwrap();
        free_chain(db, db.db_header().catalog_root).unwrap();
        let root = meta::write_chain(db, &catalog.encode()).unwrap();
        db.db_header_mut().catalog_root = root;
        db.checkpoint().unwrap();
        id
    }

    fn read_age(db: &mut Zu1File, person: u32, row: u64) -> u64 {
        let dir = load_props(db, person).unwrap().unwrap();
        let mut reader = PropsReader::new(dir);
        let col = reader.col("age").unwrap();
        reader.read_int(db, col, row).unwrap()
    }

    fn read_name(db: &mut Zu1File, person: u32, row: u64) -> Vec<u8> {
        let dir = load_props(db, person).unwrap().unwrap();
        let mut reader = PropsReader::new(dir);
        let col = reader.col("name").unwrap();
        let mut out = Vec::new();
        reader.read_str(db, col, row, &mut out).unwrap();
        out
    }

    /// A file with one `BINARY(4)` column over four rows, and a log to
    /// write into it.
    fn hashed(dir: &std::path::Path) -> (Fixture, LogicalType) {
        let mut db = Zu1File::create(&dir.join("fold.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let binary4 = LogicalType::Bytes {
            min: Some(4),
            max: Some(4),
            fixed: true,
        };
        let rows: Vec<&[u8]> = vec![b"aaaa", b"bbbb", b"cccc", b"dddd"];
        crate::props::store_props_nullable(
            &mut db,
            "person",
            &[crate::props::PropInput::typed(
                "hash",
                PropValues::Bytes(&rows),
                &binary4,
            )],
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let wal = Wal::open(&dir.join("fold.wal")).unwrap();
        let fixture = Fixture {
            db,
            wal,
            mvcc: Mvcc::new(0),
            person,
            knows,
        };
        (fixture, binary4)
    }

    /// A fold over a column whose declaration fixes the width leaves it
    /// at that width: the row a statement wrote is the new value, and
    /// the row it cleared is the declared width of nothing rather than
    /// no bytes at all, so one null does not cost the column its layout.
    #[test]
    fn a_fold_keeps_a_fixed_width_column_at_its_width() {
        let dir = tempfile::tempdir().unwrap();
        let (mut f, binary4) = hashed(dir.path());
        let mut txn = f.mvcc.begin();
        txn.update(f.person, 1, 0, Cell::Str(b"zzzz".to_vec()));
        txn.update(f.person, 2, 0, Cell::Null);
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();

        let directory = load_props(&mut f.db, f.person).unwrap().unwrap();
        assert_eq!(directory.columns[0].ty, binary4);
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.payload_len, 4 + 4 * 4);
        let mut reader = PropsReader::new(directory);
        let mut out = Vec::new();
        reader.read_str(&mut f.db, 0, 1, &mut out).unwrap();
        assert_eq!(out, b"zzzz");
        assert!(!reader.is_valid(&mut f.db, 0, 2).unwrap());
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A value the column's width does not admit is refused by the fold
    /// whatever wrote it, because the fold is the last place between a
    /// statement and the file.
    #[test]
    fn a_fold_refuses_a_value_the_column_width_does_not_admit() {
        let dir = tempfile::tempdir().unwrap();
        let (mut f, _) = hashed(dir.path());
        let mut txn = f.mvcc.begin();
        txn.update(f.person, 1, 0, Cell::Str(b"zzz".to_vec()));
        txn.commit(&mut f.wal).unwrap();
        let err = checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap_err();
        assert!(err.to_string().contains("was given 3 octets"), "{err}");
    }

    /// A list column written at its bound is the same case: its rows
    /// carry no count of their own, so a fold that put a row of another
    /// length in would leave rows nothing can read back. The absence is
    /// the bound in zero elements, so the fold keeps the column at the
    /// count the directory claims for it.
    #[test]
    fn a_fold_keeps_a_fixed_count_list_column_at_its_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fold.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let elem = LogicalType::Int {
            signed: true,
            bits: zu_common::IntBits::B32,
            precision: None,
        };
        let declared = LogicalType::List {
            elem: Box::new(elem.clone()),
            max: Some(2),
        };
        let pair: Vec<crate::props::ListElement> = (1..=2)
            .map(crate::props::ListElement::Word)
            .collect::<Vec<_>>();
        let rows: Vec<&[crate::props::ListElement]> = vec![&pair, &pair, &pair, &pair];
        crate::props::store_props_nullable(
            &mut db,
            "person",
            &[crate::props::PropInput::typed(
                "xs",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let wal = Wal::open(&dir.path().join("fold.wal")).unwrap();
        let mut f = Fixture {
            db,
            wal,
            mvcc: Mvcc::new(0),
            person,
            knows,
        };

        // A row of the right width goes in, a row of the wrong one is
        // refused, and a null becomes the width of nothing.
        let mut txn = f.mvcc.begin();
        let mut wrote = Vec::new();
        wrote.extend_from_slice(&9i32.to_le_bytes());
        wrote.extend_from_slice(&8i32.to_le_bytes());
        txn.update(f.person, 1, 0, Cell::Str(wrote));
        txn.update(f.person, 2, 0, Cell::Null);
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();

        let directory = load_props(&mut f.db, f.person).unwrap().unwrap();
        assert_eq!(directory.columns[0].fixed_len, Some(2));
        let meta = &directory.columns[0].meta;
        assert_eq!(meta.structural, crate::segment::Structural::Stride);
        assert_eq!(meta.payload_len, 4 + 4 * 2 * 4);
        let mut reader = PropsReader::new(directory);
        let mut out = Vec::new();
        reader.read_str(&mut f.db, 0, 1, &mut out).unwrap();
        assert_eq!(
            crate::props::list_elements(&elem, &out, ListRows::Fixed(2)).unwrap(),
            vec![
                crate::props::ListElement::Word(9),
                crate::props::ListElement::Word(8)
            ]
        );
        assert!(!reader.is_valid(&mut f.db, 0, 2).unwrap());
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();

        // And the wrong width is refused, on the same file shape.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("fold.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        crate::props::store_props_nullable(
            &mut db,
            "person",
            &[crate::props::PropInput::typed(
                "xs",
                PropValues::List {
                    elem: &elem,
                    rows: &rows,
                },
                &declared,
            )],
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let wal = Wal::open(&dir.path().join("fold.wal")).unwrap();
        let mut f = Fixture {
            db,
            wal,
            mvcc: Mvcc::new(0),
            person,
            knows,
        };
        let mut txn = f.mvcc.begin();
        txn.update(f.person, 1, 0, Cell::Str(9i32.to_le_bytes().to_vec()));
        txn.commit(&mut f.wal).unwrap();
        let err = checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap_err();
        assert!(err.to_string().contains("was given 4 octets"), "{err}");
    }

    /// After the fold the base file alone answers what the overlay
    /// answered before it: appended rows, updated cells, merged edges,
    /// grown counts, and an empty log.
    #[test]
    fn fold_seals_overlays_into_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        assert!(f.wal.is_empty());
        assert_eq!(f.mvcc.epoch(), 1);
        assert_eq!(f.mvcc.appended_rows(f.person, 1), 0);
        assert!(f.mvcc.is_deleted(f.person, 2, 0), "tombstone reseeded");
        let catalog = Catalog::load(&mut f.db).unwrap();
        assert_eq!(catalog.node_by_id(f.person).unwrap().node_count, 6);
        assert_eq!(catalog.rel_by_id(f.knows).unwrap().edge_count, 5);
        assert_eq!(read_age(&mut f.db, f.person, 1), 21);
        assert_eq!(read_age(&mut f.db, f.person, 4), 50);
        assert_eq!(read_age(&mut f.db, f.person, 5), 60);
        assert_eq!(read_name(&mut f.db, f.person, 4), b"eva");
        assert_eq!(read_name(&mut f.db, f.person, 0), b"ada");
        // The tombstoned row keeps its cells until VACUUM.
        assert_eq!(read_name(&mut f.db, f.person, 2), b"joe");
        let mut g = GraphReader::load_table(&mut f.db, "knows").unwrap();
        assert_eq!(
            g.neighbors_dir(&mut f.db, 0, Direction::Fwd).unwrap(),
            &[1, 4]
        );
        assert_eq!(g.neighbors_dir(&mut f.db, 4, Direction::Fwd).unwrap(), &[0]);
        assert_eq!(g.neighbors_dir(&mut f.db, 0, Direction::Bwd).unwrap(), &[4]);
        assert!(
            g.neighbors_dir(&mut f.db, 5, Direction::Fwd)
                .unwrap()
                .is_empty()
        );
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A fold that touched one column leaves the others alone: the new
    /// directory names the same blocks the old one did for them, and it
    /// names new blocks for the one that was written. That is what makes
    /// a one cell write cost one column instead of the whole table, and
    /// `verify` at the end is what says the reused blocks were not also
    /// handed to the free list.
    #[test]
    fn an_untouched_column_keeps_its_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cols.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1)]).unwrap();
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe", b"amy"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30, 40])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .unwrap();
        let person = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let before = load_props(&mut db, person).unwrap().unwrap();
        let age = before.columns[0].meta.blocks.clone();
        let name = before.columns[1].meta.blocks.clone();
        let labels = before.labels.clone();
        let mut wal = Wal::open(&dir.path().join("cols.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update(person, 1, 0, Cell::Int(21));
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let after = load_props(&mut db, person).unwrap().unwrap();
        assert_ne!(after.columns[0].meta.blocks, age, "age was written");
        assert_eq!(after.columns[1].meta.blocks, name, "name came through");
        assert_eq!(after.labels, labels, "the bitset came through");
        assert_eq!(read_age(&mut db, person, 1), 21);
        assert_eq!(read_age(&mut db, person, 3), 40);
        assert_eq!(read_name(&mut db, person, 2), b"joe");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// An appended row carries the table's own label and nothing else,
    /// which is what the insert said, and the rows that were already
    /// there keep the labels they had.
    #[test]
    fn labels_grow_with_the_row_domain() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        crate::props::store_labels(
            &mut f.db,
            "person",
            &[vec!["Bot"], vec![], vec!["Bot", "Admin"], vec![]],
        )
        .unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let directory = load_props(&mut f.db, f.person).unwrap().unwrap();
        assert_eq!(directory.node_count, 6);
        let mut reader = PropsReader::new(directory);
        let words: Vec<u64> = (0..6)
            .map(|row| reader.label_word(&mut f.db, row).unwrap().unwrap())
            .collect();
        assert_eq!(words, [0b011, 0b001, 0b111, 0b001, 0b001, 0b001]);
        assert_eq!(read_age(&mut f.db, f.person, 5), 60);
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A label the overlay put on a row and one it took off land in the
    /// bitset the fold writes, and a row nothing named keeps the word it
    /// had.
    #[test]
    fn a_label_change_lands_in_the_bitset() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        crate::props::store_labels(
            &mut f.db,
            "person",
            &[vec!["Bot"], vec![], vec!["Bot", "Admin"], vec![]],
        )
        .unwrap();
        let mut txn = f.mvcc.begin();
        // Admin onto kay, Bot off ada: bit 0 is the table's own label,
        // bit 1 is Bot and bit 2 is Admin, in the order they were first
        // declared.
        txn.update_labels(f.person, 1, 0b100, 0).unwrap();
        txn.update_labels(f.person, 0, 0, 0b010).unwrap();
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let directory = load_props(&mut f.db, f.person).unwrap().unwrap();
        let mut reader = PropsReader::new(directory);
        let words: Vec<u64> = (0..6)
            .map(|row| reader.label_word(&mut f.db, row).unwrap().unwrap())
            .collect();
        assert_eq!(words, [0b001, 0b101, 0b111, 0b001, 0b001, 0b001]);
        assert_eq!(read_name(&mut f.db, f.person, 0), b"ada", "props untouched");
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A table whose rows carry no labels yet gets the bitset from the
    /// first change: the rows nothing named take the table's own label,
    /// which is what they had all along without a word to say it in.
    #[test]
    fn a_first_label_change_writes_the_bitset_a_table_lacked() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        assert_eq!(declare(&mut f.db, f.person, "Bot"), 1);
        let mut txn = f.mvcc.begin();
        txn.update_labels(f.person, 3, 0b010, 0).unwrap();
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let directory = load_props(&mut f.db, f.person).unwrap().unwrap();
        let mut reader = PropsReader::new(directory);
        let words: Vec<u64> = (0..6)
            .map(|row| reader.label_word(&mut f.db, row).unwrap().unwrap())
            .collect();
        assert_eq!(words, [1, 1, 1, 0b011, 1, 1]);
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A table that stores nothing on its rows has no props directory
    /// for a bitset to hang off, so the first label change is what makes
    /// one, holding the labels and no columns.
    #[test]
    fn a_label_change_makes_the_directory_a_bare_table_lacked() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("bare.zu1")).unwrap();
        bulk_load_as(&mut db, "thing", "links", 3, &[(0, 1)]).unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let thing = catalog.node_by_name("thing").unwrap().id;
        assert_eq!(declare(&mut db, thing, "Bot"), 1);
        let mut wal = Wal::open(&dir.path().join("bare.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        assert!(
            load_props(&mut db, thing).unwrap().is_none(),
            "nothing stored on the rows to begin with"
        );
        let mut txn = mvcc.begin();
        txn.update_labels(thing, 2, 0b010, 0).unwrap();
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let directory = load_props(&mut db, thing).unwrap().unwrap();
        assert_eq!(directory.node_count, 3);
        assert!(directory.columns.is_empty());
        let mut reader = PropsReader::new(directory);
        let words: Vec<u64> = (0..3)
            .map(|row| reader.label_word(&mut db, row).unwrap().unwrap())
            .collect();
        assert_eq!(words, [1, 1, 0b011]);
        let path = dir.path().join("bare.zu1");
        drop(db);
        drop(wal);
        crate::verify(&path).unwrap();
    }

    /// A label the table has not declared is refused at the fold, and
    /// so is taking the table's own name off one of its rows: either one
    /// would leave a file saying something about a row that the catalog
    /// says cannot be true.
    #[test]
    fn a_label_change_stays_inside_what_the_table_declares() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let mut txn = f.mvcc.begin();
        txn.update_labels(f.person, 0, 0b100, 0).unwrap();
        txn.commit(&mut f.wal).unwrap();
        let err = checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap_err();
        assert!(format!("{err}").contains("has not declared it"), "{err:?}");

        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let mut txn = f.mvcc.begin();
        txn.update_labels(f.person, 0, 0, 0b001).unwrap();
        txn.commit(&mut f.wal).unwrap();
        let err = checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap_err();
        assert!(
            format!("{err}").contains("off one of its own rows"),
            "{err:?}"
        );
    }

    /// A change naming a row past the end of the table is refused
    /// rather than written past the words the table has.
    #[test]
    fn a_label_change_past_the_last_row_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let mut txn = f.mvcc.begin();
        txn.update_labels(f.person, 99, 0b010, 0).unwrap();
        txn.commit(&mut f.wal).unwrap();
        let err = checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap_err();
        assert!(matches!(err, ZuError::InvalidArgument(_)), "{err:?}");
    }

    /// A reopened file recovers to the folded state: nothing replays,
    /// tombstones seed from their chain, reads match the pre-reopen
    /// answers.
    #[test]
    fn folded_state_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        drop(f.db);
        drop(f.wal);
        let mut db = Zu1File::open(&dir.path().join("fold.zu1")).unwrap();
        let mut wal = Wal::open(&dir.path().join("fold.wal")).unwrap();
        let mvcc = recover(&mut db, &mut wal).unwrap();
        assert_eq!(mvcc.epoch(), 1);
        assert_eq!(mvcc.appended_rows(f.person, 1), 0);
        assert!(mvcc.is_deleted(f.person, 2, 0));
        assert_eq!(read_age(&mut db, f.person, 4), 50);
        assert_eq!(read_name(&mut db, f.person, 5), b"raj");
    }

    /// A fold that stops before the flip answers through the handle
    /// that made it and leaves the file alone, and the log is what
    /// covers the difference: a reopen replays it and lands on the same
    /// answers a published fold would have left.
    #[test]
    fn a_staged_fold_reads_through_and_is_not_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let published = f.db.db_header().epoch;
        staged_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        assert_eq!(read_age(&mut f.db, f.person, 4), 50);
        assert_eq!(read_name(&mut f.db, f.person, 5), b"raj");
        assert!(f.db.db_header().epoch > published, "the readers above it");
        assert!(!f.wal.is_empty(), "the staged fold cut the log");

        let path = dir.path().join("fold.zu1");
        {
            let mut cold = Zu1File::open(&path).unwrap();
            assert_eq!(cold.db_header().epoch, published);
            let rows = Catalog::load(&mut cold)
                .unwrap()
                .node_by_id(f.person)
                .unwrap()
                .node_count;
            assert_eq!(rows, 4, "the staged fold reached the file");
        }

        drop(f.db);
        drop(f.wal);
        let mut db = Zu1File::open(&path).unwrap();
        let mut wal = Wal::open(&dir.path().join("fold.wal")).unwrap();
        let mut mvcc = recover(&mut db, &mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert_eq!(read_age(&mut db, f.person, 4), 50);
        assert_eq!(read_name(&mut db, f.person, 5), b"raj");
        assert!(mvcc.is_deleted(f.person, 2, 0));
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A crash at any point before the header flip is the pre-fold
    /// state plus garbage blocks: recovery replays the WAL and answers
    /// exactly what the folded file answers.
    #[test]
    fn crash_before_the_flip_folds_to_the_same_answers() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        // The copy is the crash image: taken before the fold ran, so
        // its header and WAL are exactly what an interrupted fold
        // leaves behind.
        std::fs::copy(dir.path().join("fold.zu1"), dir.path().join("crash.zu1")).unwrap();
        std::fs::copy(dir.path().join("fold.wal"), dir.path().join("crash.wal")).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let mut db = Zu1File::open(&dir.path().join("crash.zu1")).unwrap();
        let mut wal = Wal::open(&dir.path().join("crash.wal")).unwrap();
        let mut mvcc = recover(&mut db, &mut wal).unwrap();
        assert_eq!(mvcc.epoch(), 1);
        assert_eq!(mvcc.appended_rows(f.person, 1), 2);
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        for row in [0, 1, 4, 5] {
            assert_eq!(
                read_age(&mut db, f.person, row),
                read_age(&mut f.db, f.person, row)
            );
            assert_eq!(
                read_name(&mut db, f.person, row),
                read_name(&mut f.db, f.person, row)
            );
        }
        let mut a = GraphReader::load_table(&mut db, "knows").unwrap();
        let mut b = GraphReader::load_table(&mut f.db, "knows").unwrap();
        for node in 0..6 {
            assert_eq!(
                a.neighbors_dir(&mut db, node, Direction::Fwd).unwrap(),
                b.neighbors_dir(&mut f.db, node, Direction::Fwd).unwrap()
            );
        }
    }

    /// Tombstones accumulate across folds through the persisted chain.
    #[test]
    fn second_fold_merges_tombstone_chains() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let mut txn = f.mvcc.begin();
        txn.delete(f.person, 0);
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        let index = TableIndex::load(&mut f.db).unwrap();
        let root = index.get(f.person | TOMBSTONE_KEY).unwrap();
        let chain = decode_tombstones(&meta::read_chain(&mut f.db, root).unwrap()).unwrap();
        assert_eq!(chain, vec![0, 2]);
        assert!(f.mvcc.is_deleted(f.person, 0, 0));
        assert!(f.mvcc.is_deleted(f.person, 2, 0));
        let path = dir.path().join("fold.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A keyed table folds updates in place and keeps its key index
    /// while the row domain holds still, and grows it when the domain
    /// does: the rows the appends added take their keys from the `id`
    /// column, and the index answers for them afterwards.
    #[test]
    fn keyed_tables_keep_their_index_and_grow_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("keyed.zu1")).unwrap();
        bulk_load_keyed(
            &mut db,
            "person",
            "knows",
            4,
            &[(0, 1), (2, 3)],
            Some(&[100, 200, 300, 400]),
        )
        .unwrap();
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[1, 2, 3, 4])),
                ("id", PropValues::Int(&[100, 200, 300, 400])),
            ],
        )
        .unwrap();
        let person = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("keyed.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update(person, 3, 0, Cell::Int(44));
        txn.insert_rel(knows, 1, 3);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut g = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(g.lookup_key(&mut db, 200).unwrap(), Some(1));
        assert_eq!(g.lookup_key(&mut db, 400).unwrap(), Some(3));
        assert_eq!(g.neighbors_dir(&mut db, 1, Direction::Fwd).unwrap(), &[3]);
        assert_eq!(read_age(&mut db, person, 3), 44);
        // Growth: the appended row brings its own key in the `id`
        // column, and the index it lands in answers for it and for
        // every key that was already there.
        let mut txn = mvcc.begin();
        txn.insert_nodes(
            person,
            vec![(0, vec![Cell::Int(5)]), (1, vec![Cell::Int(500)])],
        )
        .unwrap();
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert_eq!(
            Catalog::load(&mut db)
                .unwrap()
                .node_by_id(person)
                .unwrap()
                .node_count,
            5
        );
        let mut g = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(g.lookup_key(&mut db, 500).unwrap(), Some(4));
        assert_eq!(g.lookup_key(&mut db, 100).unwrap(), Some(0));
        assert_eq!(g.lookup_key(&mut db, 400).unwrap(), Some(3));
        assert_eq!(g.lookup_key(&mut db, 250).unwrap(), None);
        assert_eq!(read_age(&mut db, person, 4), 5);
        assert!(wal.is_empty(), "the fold published and truncated");
    }

    /// A DELETE takes a row's key out of the index with the row, so the
    /// next INSERT may have that key back. Without this a store that
    /// creates and removes the same entity in a loop, which is what a
    /// benchmark's stationary write does, refuses the second round and
    /// keeps refusing it, because the record it cannot fold stays in
    /// the log.
    #[test]
    fn a_deleted_row_gives_its_key_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("reuse.zu1")).unwrap();
        bulk_load_keyed(
            &mut db,
            "person",
            "knows",
            4,
            &[(0, 1), (2, 3)],
            Some(&[100, 200, 300, 400]),
        )
        .unwrap();
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[1, 2, 3, 4])),
                ("id", PropValues::Int(&[100, 200, 300, 400])),
            ],
        )
        .unwrap();
        let person = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("reuse.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.delete(person, 1);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut g = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(g.lookup_key(&mut db, 200).unwrap(), None);
        assert_eq!(g.lookup_key(&mut db, 300).unwrap(), Some(2));
        // The same key again, on a new row, which is the whole point.
        let mut txn = mvcc.begin();
        txn.insert_nodes(
            person,
            vec![(0, vec![Cell::Int(9)]), (1, vec![Cell::Int(200)])],
        )
        .unwrap();
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut g = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(g.lookup_key(&mut db, 200).unwrap(), Some(4));
        assert_eq!(read_age(&mut db, person, 4), 9);
        assert!(wal.is_empty(), "the fold published and truncated");
        let path = dir.path().join("reuse.zu1");
        drop(g);
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A keyed table whose node table has no `id` column has nowhere to
    /// take an appended row's key from, so growing it is refused whole,
    /// with nothing published and the txn still in the log.
    #[test]
    fn a_keyed_table_with_no_id_column_refuses_growth() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("nokey.zu1")).unwrap();
        bulk_load_keyed(
            &mut db,
            "person",
            "knows",
            4,
            &[(0, 1), (2, 3)],
            Some(&[100, 200, 300, 400]),
        )
        .unwrap();
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[1, 2, 3, 4]))],
        )
        .unwrap();
        let person = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("nokey.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(person, vec![(0, vec![Cell::Int(5)])])
            .unwrap();
        txn.commit(&mut wal).unwrap();
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
        assert_eq!(mvcc.appended_rows(person, mvcc.epoch()), 1);
        assert!(!wal.is_empty(), "the log still holds the txn");
        let recovered = recover(&mut db, &mut wal).unwrap();
        assert_eq!(recovered.appended_rows(person, recovered.epoch()), 1);
    }

    /// Appending column data to a table without stored props is
    /// refused; edges and deletes alone fold fine on such a table.
    #[test]
    fn tables_without_props_take_edges_and_deletes_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("bare.zu1")).unwrap();
        bulk_load_as(&mut db, "node", "edge", 3, &[(0, 1)]).unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let node = catalog.node_by_name("node").unwrap().id;
        let edge = catalog.rel_by_name("edge").unwrap().id;
        let mut wal = Wal::open(&dir.path().join("bare.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel(edge, 2, 0);
        txn.delete(node, 1);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert!(mvcc.is_deleted(node, 1, 0));
        let mut g = GraphReader::load_table(&mut db, "edge").unwrap();
        assert_eq!(g.neighbors_dir(&mut db, 2, Direction::Fwd).unwrap(), &[0]);
        let mut txn = mvcc.begin();
        txn.insert_nodes(node, vec![(0, vec![Cell::Int(9)])])
            .unwrap();
        txn.commit(&mut wal).unwrap();
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
    }

    /// Growing the row domain rebuilds the CSR and leaves the load
    /// order alone, so the edge columns come through the fold naming
    /// the same edges they named before it.
    #[test]
    fn edge_columns_survive_a_rebuild_over_a_grown_row_domain() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relfold.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[10, 20, 30, 40]))],
        )
        .unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2, 3]))])
            .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let mut wal = Wal::open(&dir.path().join("relfold.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(person, vec![(0, vec![Cell::Int(50), Cell::Int(60)])])
            .unwrap();
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert_eq!(
            Catalog::load(&mut db)
                .unwrap()
                .node_by_id(person)
                .unwrap()
                .node_count,
            6
        );
        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let col = reader.col("since").unwrap();
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        for (i, (src, dst)) in [(0u64, 1u64), (1, 2), (2, 3)].into_iter().enumerate() {
            let row = graph.edge_ordinal(&mut db, src, dst).unwrap().unwrap();
            assert_eq!(row, i as u64);
            assert_eq!(reader.read_int(&mut db, col, row).unwrap(), i as u64 + 1);
        }
        let path = dir.path().join("relfold.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// And it comes through naming the same blocks, not just the same
    /// edges. A rel table is rebuilt whenever either end table grows,
    /// because the row domain its CSR is keyed by has moved, and a row
    /// appended to that domain adds an offset and no neighbour. So the
    /// neighbour lists are the lists the base holds and the columns
    /// over them name the same edges in the same order, and only the
    /// offsets have anything new to say. Writing the rest again would
    /// cost the whole of it: the lists are a word an edge and the
    /// columns are the values themselves, handed back to the allocator
    /// once a fold for nothing.
    #[test]
    fn a_grown_row_domain_leaves_the_edges_where_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relgrow.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[10, 20, 30, 40]))],
        )
        .unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2, 3]))])
            .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.node_by_name("person").unwrap().id;
        let was = GraphReader::load_table(&mut db, "knows")
            .unwrap()
            .directory()
            .clone();
        let mut wal = Wal::open(&dir.path().join("relgrow.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_nodes(person, vec![(0, vec![Cell::Int(50)])])
            .unwrap();
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let now = GraphReader::load_table(&mut db, "knows")
            .unwrap()
            .directory()
            .clone();
        assert_eq!(was.props, now.props, "a column nothing wrote to moved");
        for (g, (before, after)) in was.groups.iter().zip(&now.groups).enumerate() {
            assert_eq!(
                (&before.fwd.neighbors, &before.bwd.neighbors),
                (&after.fwd.neighbors, &after.bwd.neighbors),
                "group {g} rewrote a neighbour list no edge went into"
            );
        }
        assert_eq!(now.from_count, 5, "the row domain grew");
        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, catalog.rel_by_name("knows").unwrap().id)
                .unwrap()
                .unwrap(),
        );
        let col = reader.col("since").unwrap();
        for i in 0..3u64 {
            assert_eq!(reader.read_int(&mut db, col, i).unwrap(), i + 1);
        }
        let path = dir.path().join("relgrow.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A column is dense over the edges in load order, so an edge added
    /// in front of the ones the base held moves every one of them, and
    /// the values move with them.
    #[test]
    fn an_added_edge_moves_the_columns_into_the_new_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relprops.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2)]).unwrap();
        let notes: Vec<&[u8]> = vec![b"one", b"two"];
        crate::props::store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1, 2])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relprops.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        // (0, 0) sorts in front of both stored edges, so nothing keeps
        // the ordinal it had.
        txn.insert_rel_carrying(
            knows,
            0,
            0,
            vec![(0, Cell::Int(3)), (1, Cell::Str(b"new".to_vec()))],
        );
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let (since, note) = (reader.col("since").unwrap(), reader.col("note").unwrap());
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        for (src, dst, year, text) in [
            (0u64, 0u64, 3u64, "new"),
            (0, 1, 1, "one"),
            (1, 2, 2, "two"),
        ] {
            let row = graph.edge_ordinal(&mut db, src, dst).unwrap().unwrap();
            assert_eq!(reader.read_int(&mut db, since, row).unwrap(), year);
            let mut got = Vec::new();
            reader.read_str(&mut db, note, row, &mut got).unwrap();
            assert_eq!(got, text.as_bytes(), "{src} to {dst}");
        }
        let path = dir.path().join("relprops.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// An edge the overlay took away is gone from the CSR the fold
    /// rebuilds, on both sides, and the edge count the catalog holds
    /// says so.
    #[test]
    fn a_removed_edge_leaves_the_csr_the_fold_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let mut txn = f.mvcc.begin();
        txn.delete_rel(f.knows, 1, 2);
        txn.commit(&mut f.wal).unwrap();
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();

        // Three loaded, two added by the fixture, one taken away here.
        let catalog = Catalog::load(&mut f.db).unwrap();
        assert_eq!(catalog.rel_by_id(f.knows).unwrap().edge_count, 4);
        let mut reader = GraphReader::load_table(&mut f.db, "knows").unwrap();
        assert_eq!(
            reader.neighbors_dir(&mut f.db, 1, Direction::Fwd).unwrap(),
            &[] as &[u64]
        );
        assert_eq!(
            reader.neighbors_dir(&mut f.db, 2, Direction::Bwd).unwrap(),
            &[] as &[u64],
            "the end the edge arrived at loses it too"
        );
        assert_eq!(
            reader.neighbors_dir(&mut f.db, 2, Direction::Fwd).unwrap(),
            &[3],
            "the edges beside it stay"
        );
        let path = dir.path().join("fold.zu1");
        drop(f.db);
        crate::verify(&path).unwrap();
    }

    /// A column is dense over the edges in load order, so an edge taken
    /// away moves every edge behind it, and the fold has to cut the
    /// value out rather than leave the column pointing one edge along.
    #[test]
    fn a_removed_edge_takes_its_property_values_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relcut.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let notes: Vec<&[u8]> = vec![b"one", b"two", b"three"];
        crate::props::store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1, 2, 3])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relcut.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.delete_rel(knows, 0, 1);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let (since, note) = (reader.col("since").unwrap(), reader.col("note").unwrap());
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        assert!(graph.edge_ordinal(&mut db, 0, 1).unwrap().is_none());
        for (src, dst, year, text) in [(1u64, 2u64, 2u64, "two"), (2, 3, 3, "three")] {
            let row = graph.edge_ordinal(&mut db, src, dst).unwrap().unwrap();
            assert_eq!(reader.read_int(&mut db, since, row).unwrap(), year);
            let mut got = Vec::new();
            reader.read_str(&mut db, note, row, &mut got).unwrap();
            assert_eq!(got, text.as_bytes(), "{src} to {dst}");
        }
        let path = dir.path().join("relcut.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A value written onto an edge that was already there lands on the
    /// pair it names, wherever the rebuilt order puts that pair, and the
    /// edges beside it keep what they held.
    #[test]
    fn a_value_written_onto_an_edge_lands_on_the_pair_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relset.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let notes: Vec<&[u8]> = vec![b"one", b"two", b"three"];
        crate::props::store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1, 2, 3])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relset.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update_rel(knows, 1, 2, 0, Cell::Int(20));
        txn.update_rel(knows, 1, 2, 1, Cell::Str(b"changed".to_vec()));
        // (0, 0) sorts in front of everything, so the pair the values
        // were written onto is not where it was when they were staged.
        txn.insert_rel_carrying(
            knows,
            0,
            0,
            vec![(0, Cell::Int(9)), (1, Cell::Str(b"new".to_vec()))],
        );
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let (since, note) = (reader.col("since").unwrap(), reader.col("note").unwrap());
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        for (src, dst, year, text) in [
            (0u64, 0u64, 9u64, "new"),
            (0, 1, 1, "one"),
            (1, 2, 20, "changed"),
            (2, 3, 3, "three"),
        ] {
            let row = graph.edge_ordinal(&mut db, src, dst).unwrap().unwrap();
            assert_eq!(reader.read_int(&mut db, since, row).unwrap(), year);
            let mut got = Vec::new();
            reader.read_str(&mut db, note, row, &mut got).unwrap();
            assert_eq!(got, text.as_bytes(), "{src} to {dst}");
        }
        let path = dir.path().join("relset.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// An absence written onto an edge is a clear bit in the column's
    /// mask, and the edges beside it are left holding a value, which is
    /// what says the mask was built over the new edge order and not
    /// copied word for word out of the old one.
    #[test]
    fn an_absence_written_onto_an_edge_clears_its_bit_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relnull.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        let notes: Vec<&[u8]> = vec![b"one", b"two", b"three"];
        crate::props::store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1, 2, 3])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relnull.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update_rel(knows, 1, 2, 0, Cell::Null);
        txn.update_rel(knows, 1, 2, 1, Cell::Null);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let (since, note) = (reader.col("since").unwrap(), reader.col("note").unwrap());
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        let gone = graph.edge_ordinal(&mut db, 1, 2).unwrap().unwrap();
        assert!(!reader.is_valid(&mut db, since, gone).unwrap());
        assert!(!reader.is_valid(&mut db, note, gone).unwrap());
        for (src, dst, year, text) in [(0u64, 1u64, 1u64, "one"), (2, 3, 3, "three")] {
            let row = graph.edge_ordinal(&mut db, src, dst).unwrap().unwrap();
            assert!(reader.is_valid(&mut db, since, row).unwrap());
            assert_eq!(reader.read_int(&mut db, since, row).unwrap(), year);
            let mut got = Vec::new();
            reader.read_str(&mut db, note, row, &mut got).unwrap();
            assert_eq!(got, text.as_bytes(), "{src} to {dst}");
        }
        let path = dir.path().join("relnull.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A bit the last fold cleared stays clear through the next one,
    /// which is the half of the mask that comes out of the old column
    /// rather than out of the overlay.
    #[test]
    fn an_absence_on_an_edge_survives_the_next_fold() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relnull2.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2, 3]))])
            .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relnull2.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.update_rel(knows, 1, 2, 0, Cell::Null);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut txn = mvcc.begin();
        txn.update_rel(knows, 2, 3, 0, Cell::Int(30));
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let since = reader.col("since").unwrap();
        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        let gone = graph.edge_ordinal(&mut db, 1, 2).unwrap().unwrap();
        assert!(!reader.is_valid(&mut db, since, gone).unwrap());
        let changed = graph.edge_ordinal(&mut db, 2, 3).unwrap().unwrap();
        assert_eq!(reader.read_int(&mut db, since, changed).unwrap(), 30);
        let path = dir.path().join("relnull2.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// An edge added to a table that stores properties has to carry a
    /// value for every column, because a column is dense and there is
    /// nothing for it to hold otherwise. The fold refuses whole, with
    /// the log left holding the txn.
    #[test]
    fn an_added_edge_with_no_value_for_a_column_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relrefuse.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2)]).unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2]))])
            .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relrefuse.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel(knows, 3, 0);
        txn.commit(&mut wal).unwrap();
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(err.to_string().contains("'since'"), "{err}");
        assert!(!wal.is_empty(), "the log still holds the txn");
        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let col = reader.col("since").unwrap();
        assert_eq!(reader.read_int(&mut db, col, 1).unwrap(), 2);
    }

    /// A bulk load keeps both copies of a pair, and the fold keeps them
    /// in the order the load left them, with a copy the fold adds
    /// behind both. Every copy keeps the value it was written with,
    /// which is the whole of what the ordering is for.
    ///
    /// A table nothing new is written to has to fold as well, since a
    /// row appended to either end rebuilds it: a load with a pair that
    /// runs twice, which is most of what a real dataset is, would
    /// otherwise take no write at all.
    #[test]
    fn a_pair_that_runs_twice_folds_in_load_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("reldup.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (0, 1), (1, 2)]).unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2, 3]))])
            .unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("reldup.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel_carrying(knows, 0, 1, vec![(0, Cell::Int(9))]);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();

        let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(graph.edge_run(&mut db, 0, 1).unwrap(), Some((0, 3)));
        assert_eq!(graph.edge_run(&mut db, 1, 2).unwrap(), Some((3, 1)));
        let mut reader = PropsReader::new(
            crate::props::load_rel_props(&mut db, knows)
                .unwrap()
                .unwrap(),
        );
        let since = reader.col("since").unwrap();
        let read = |reader: &mut PropsReader, db: &mut Zu1File, row| {
            reader.read_int(db, since, row).unwrap()
        };
        assert_eq!(read(&mut reader, &mut db, 0), 1);
        assert_eq!(read(&mut reader, &mut db, 1), 2);
        assert_eq!(read(&mut reader, &mut db, 2), 9);
        assert_eq!(read(&mut reader, &mut db, 3), 3);
        let path = dir.path().join("reldup.zu1");
        drop(graph);
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A table that stores nothing on its edges has nowhere to put a
    /// value, and says so rather than dropping it.
    #[test]
    fn a_value_on_an_edge_of_a_table_without_columns_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("relnone.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1)]).unwrap();
        let knows = Catalog::load(&mut db)
            .unwrap()
            .rel_by_name("knows")
            .unwrap()
            .id;
        let mut wal = Wal::open(&dir.path().join("relnone.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel_carrying(knows, 3, 0, vec![(0, Cell::Int(9))]);
        txn.commit(&mut wal).unwrap();
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
    }

    /// A rel table nobody touched keeps its directory root: the fold
    /// rebuilds only what changed.
    #[test]
    fn untouched_rel_tables_keep_their_roots() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("two.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1)]).unwrap();
        bulk_load_as(&mut db, "person", "likes", 4, &[(2, 3)]).unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let knows = catalog.rel_by_name("knows").unwrap().id;
        let likes = catalog.rel_by_name("likes").unwrap().id;
        let likes_root = TableIndex::load(&mut db).unwrap().get(likes).unwrap();
        let mut wal = Wal::open(&dir.path().join("two.wal")).unwrap();
        let mut mvcc = Mvcc::new(0);
        let mut txn = mvcc.begin();
        txn.insert_rel(knows, 3, 0);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let index = TableIndex::load(&mut db).unwrap();
        assert_eq!(index.get(likes), Some(likes_root));
        assert_ne!(index.get(knows), None);
        let mut g = GraphReader::load_table(&mut db, "likes").unwrap();
        assert_eq!(g.neighbors_dir(&mut db, 2, Direction::Fwd).unwrap(), &[3]);
        let path = dir.path().join("two.zu1");
        drop(db);
        crate::verify(&path).unwrap();
    }

    /// A fold with nothing to fold is a no-op: same roots, same epoch,
    /// no header flip.
    #[test]
    fn empty_fold_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("noop.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).unwrap();
        let before = db.db_header().clone();
        let mut wal = Wal::open(&dir.path().join("noop.wal")).unwrap();
        let mut mvcc = Mvcc::new(7);
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert_eq!(*db.db_header(), before);
        assert_eq!(mvcc.epoch(), 7);
    }

    /// The tombstone chain codec rejects hostile payloads.
    #[test]
    fn tombstone_codec_rejects_bad_payloads() {
        let good = encode_tombstones(&[1, 5, 9]);
        assert_eq!(decode_tombstones(&good).unwrap(), vec![1, 5, 9]);
        for len in 0..good.len() {
            assert!(decode_tombstones(&good[..len]).is_err(), "prefix {len}");
        }
        let mut bad = good.clone();
        bad[0] = 99;
        assert!(decode_tombstones(&bad).is_err());
        assert!(decode_tombstones(&encode_tombstones(&[5, 5])).is_err());
        assert!(decode_tombstones(&encode_tombstones(&[9, 1])).is_err());
        let mut trailing = good;
        trailing.push(0);
        assert!(decode_tombstones(&trailing).is_err());
    }
}
