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

use zu_common::{Epoch, Result, ZuError};

use crate::catalog::{Catalog, RelTable, TableIndex};
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::fullzip::{read_blob_segment, write_blob_segment};
use crate::graph::{
    Direction, Directory, GraphReader, GroupMeta, build_direction, free_chain,
    free_directory_keeping_props, group_bases, group_rows, pad_direction,
};
use crate::keys::write_key_index;
use crate::meta;
use crate::props::{PropsDirectory, free_props, free_props_keeping_labels, load_props_at};
use crate::segment::{read_segment, write_segment};
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
    for raw in bytes[10..].chunks_exact(8) {
        let offset = u64::from_le_bytes(raw.try_into().unwrap());
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
pub fn recover(db: &mut Zu1File, wal: &Wal) -> Result<Mvcc> {
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
/// segments and truncates the WAL. On return the overlay store is
/// empty at the same epoch with persisted tombstones reseeded, and the
/// file reads identically to the pre-fold snapshot at that epoch. On
/// error nothing published: the header, the WAL, and the store are
/// exactly as they were, and any blocks written are unreferenced.
pub fn checkpoint_fold(db: &mut Zu1File, mvcc: &mut Mvcc, wal: &mut Wal) -> Result<()> {
    let epoch = mvcc.epoch();
    let mut catalog = Catalog::load(db)?;
    let mut index = TableIndex::load(db)?;
    let mut changed = false;
    // Node tables first, so rel rebuilds see the grown row domains.
    let mut grown: HashSet<u32> = HashSet::new();
    for table in mvcc.tables_touched() {
        let node = catalog.node_by_id(table).ok_or_else(|| {
            ZuError::InvalidArgument(format!("overlay names unknown node table {table}"))
        })?;
        let (name, base, primary) = (node.name.clone(), node.node_count, node.primary_label());
        let appended = mvcc.appended_rows(table, epoch);
        if appended > 0 || mvcc.has_updates(table, epoch) {
            changed |= fold_props(db, mvcc, &mut index, table, primary, base, epoch)?;
        }
        if appended > 0 {
            catalog.upsert_node(&name, base + appended)?;
            grown.insert(table);
            changed = true;
        }
        changed |= fold_tombstones(db, mvcc, &mut index, table, epoch)?;
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
        if !dirty.contains(&rel.id) && !grown.contains(&rel.from) && !grown.contains(&rel.to) {
            continue;
        }
        let edge_count = fold_rel(db, mvcc, &catalog, &mut index, &rel, epoch)?;
        catalog.upsert_rel(&rel.name, rel.from, rel.to, edge_count)?;
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
    // The publish: both roots and the WAL floor move in one flip.
    free_chain(db, db.db_header().catalog_root)?;
    free_chain(db, db.db_header().table_index_root)?;
    let catalog_root = meta::write_chain(db, &catalog.encode())?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().catalog_root = catalog_root;
    db.db_header_mut().table_index_root = index_root;
    db.db_header_mut().wal_seq = epoch;
    db.checkpoint()?;
    wal.truncate()?;
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
fn fold_props(
    db: &mut Zu1File,
    mvcc: &Mvcc,
    index: &mut TableIndex,
    table: u32,
    primary: u16,
    base: u64,
    epoch: Epoch,
) -> Result<bool> {
    let Some(root) = index.get(table) else {
        if mvcc.appends_carry_columns(table, epoch) || mvcc.has_updates(table, epoch) {
            return Err(ZuError::Unsupported {
                what: "folding column data into a table without stored props",
                id: table,
            });
        }
        return Ok(false);
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
    let mut columns = Vec::with_capacity(dir.columns.len());
    for (ci, col) in dir.columns.iter().enumerate() {
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
            for offset in 0..new_count {
                match mvcc.cell(table, base, offset, ci as u32, epoch) {
                    Some(Cell::Int(x)) if offset < base => {
                        values[offset as usize] = x;
                        valid.set(offset);
                    }
                    Some(Cell::Int(x)) => {
                        values.push(x);
                        valid.set(offset);
                    }
                    // A removed row keeps a word where its value was,
                    // because the lane is fixed width and a reader that
                    // has been told the bit is clear never looks at it.
                    Some(Cell::Null) => {
                        match offset < base {
                            true => values[offset as usize] = 0,
                            false => values.push(0),
                        }
                        valid.clear(offset);
                    }
                    Some(Cell::Str(_)) => return Err(mismatch(&col.name, offset)),
                    None if offset < base => {}
                    None => return Err(missing(&col.name, offset)),
                }
            }
            write_segment(db, &values)?
        } else {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_blob_segment(db, &col.meta, &mut bytes, &mut ends)?;
            let mut values: Vec<Vec<u8>> = Vec::with_capacity(new_count as usize);
            let mut start = 0usize;
            for &end in &ends {
                values.push(bytes[start..end as usize].to_vec());
                start = end as usize;
            }
            for offset in 0..new_count {
                match mvcc.cell(table, base, offset, ci as u32, epoch) {
                    Some(Cell::Str(s)) if offset < base => {
                        values[offset as usize] = s;
                        valid.set(offset);
                    }
                    Some(Cell::Str(s)) => {
                        values.push(s);
                        valid.set(offset);
                    }
                    // The blob side stores the absence as the empty
                    // string, which costs the row nothing: a blob is
                    // addressed by its ends and two equal ends are no
                    // bytes at all.
                    Some(Cell::Null) => {
                        match offset < base {
                            true => values[offset as usize].clear(),
                            false => values.push(Vec::new()),
                        }
                        valid.clear(offset);
                    }
                    Some(Cell::Int(_)) => return Err(mismatch(&col.name, offset)),
                    None if offset < base => {}
                    None => return Err(missing(&col.name, offset)),
                }
            }
            let refs: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
            write_blob_segment(db, &refs)?
        };
        columns.push(crate::props::PropColumn {
            name: col.name.clone(),
            ty: col.ty.clone(),
            meta,
            validity: valid.write(db)?,
        });
    }
    // The label bitset grows with the row domain, and an appended row
    // carries the table's own label and nothing else: nothing in an
    // overlay says otherwise yet, and a row of a table is what that
    // table is called.
    let labels = match &dir.labels {
        None => None,
        Some(meta) => {
            let mut words = Vec::with_capacity(new_count as usize);
            read_segment(db, meta, &mut words)?;
            words.resize(new_count as usize, 1u64 << primary);
            Some(write_segment(db, &words)?)
        }
    };
    free_props(db, root)?;
    let new_dir = PropsDirectory {
        node_count: new_count,
        columns,
        labels,
    };
    index.set(table, meta::write_chain(db, &new_dir.encode())?);
    Ok(true)
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
    edges.sort_unstable_by_key(|&(src, dst, _)| (src, dst));
    // A bulk load can hold a pair twice, because the sort that puts the
    // copies next to each other is stable and the copies keep file
    // order. This sort is not: the base edge and the overlay edge over
    // one pair would land in whichever order the sort left them, and
    // the column permutation would then be reading a coin toss. Until
    // the fold sorts by something that separates them, a second edge
    // over a pair the table already holds is refused.
    if old.props != NULL_BLOCK
        && edges
            .windows(2)
            .any(|w| (w[0].0, w[0].1) == (w[1].0, w[1].1))
    {
        return Err(ZuError::Unsupported {
            what: "a second edge over a pair a table that stores edge properties already holds",
            id: rel.id,
        });
    }
    let order: Vec<Came> = edges.iter().map(|&(_, _, came)| came).collect();
    let edges: Vec<(u32, u32)> = edges.into_iter().map(|(src, dst, _)| (src, dst)).collect();
    // A keyed table's index survives only while the row domain holds
    // still; growing it takes the key allocation the constraint slice
    // brings, so appending to a keyed table is refused for now.
    let key_by_row = match &old.keys {
        None => None,
        Some(_) if new_from != old.from_count => {
            return Err(ZuError::Unsupported {
                what: "folding appended rows into a keyed table",
                id: rel.id,
            });
        }
        Some(keys) => {
            let mut key_list = Vec::with_capacity(keys.keys.value_count as usize);
            read_segment(db, &keys.keys, &mut key_list)?;
            let mut rows = Vec::with_capacity(keys.rows.value_count as usize);
            read_segment(db, &keys.rows, &mut rows)?;
            let mut by_row = vec![0u64; new_from as usize];
            for (i, &row) in rows.iter().enumerate() {
                by_row[row as usize] = key_list[i];
            }
            Some(by_row)
        }
    };
    free_directory_keeping_props(db, root)?;
    let mut fwd = build_direction(db, "source", new_from, &edges)?;
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let mut bwd = build_direction(db, "destination", new_to, &rev)?;
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
    let props = match old.props {
        NULL_BLOCK => NULL_BLOCK,
        root => {
            let changed = mvcc.edge_updates(rel.id, epoch);
            fold_rel_props(db, rel, root, &order, &edges, &overlay, &changed)?
        }
    };
    let directory = Directory {
        from_count: new_from,
        to_count: new_to,
        edge_count: edges.len() as u64,
        keys: key_by_row
            .map(|keys| write_key_index(db, &keys))
            .transpose()?,
        props,
        groups,
    };
    index.set(rel.id, meta::write_chain(db, &directory.encode())?);
    Ok(directory.edge_count)
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
                        Came::Overlay(at) => match cell(*at)? {
                            Cell::Int(x) => *x,
                            Cell::Str(_) | Cell::Null => return Err(mismatch(&col.name)),
                        },
                    },
                });
            }
            write_segment(db, &values)?
        } else {
            let (mut bytes, mut ends) = (Vec::new(), Vec::new());
            read_blob_segment(db, &col.meta, &mut bytes, &mut ends)?;
            let mut old: Vec<&[u8]> = Vec::with_capacity(ends.len());
            let mut start = 0usize;
            for &end in &ends {
                old.push(&bytes[start..end as usize]);
                start = end as usize;
            }
            let mut values: Vec<&[u8]> = Vec::with_capacity(order.len());
            for (slot, came) in order.iter().enumerate() {
                values.push(match written(slot) {
                    Some(Cell::Str(s)) => s.as_slice(),
                    // The blob side says the absence with no bytes at
                    // all, which costs the edge nothing: a blob is
                    // addressed by its ends and two equal ends are
                    // empty.
                    Some(Cell::Null) => {
                        valid.clear(slot as u64);
                        &[]
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
                            Cell::Str(s) => s.as_slice(),
                            Cell::Int(_) | Cell::Null => return Err(mismatch(&col.name)),
                        },
                    },
                });
            }
            write_blob_segment(db, &values)?
        };
        columns.push(crate::props::PropColumn {
            name: col.name.clone(),
            ty: col.ty.clone(),
            meta,
            validity: valid.write(db)?,
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
    use crate::props::{PropValues, PropsReader, load_props, store_props};

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
        let wal = Wal::open(&dir.path().join("fold.wal")).unwrap();
        let mvcc = recover(&mut db, &wal).unwrap();
        assert_eq!(mvcc.epoch(), 1);
        assert_eq!(mvcc.appended_rows(f.person, 1), 0);
        assert!(mvcc.is_deleted(f.person, 2, 0));
        assert_eq!(read_age(&mut db, f.person, 4), 50);
        assert_eq!(read_name(&mut db, f.person, 5), b"raj");
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
        let mut mvcc = recover(&mut db, &wal).unwrap();
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
    /// while the row domain holds still; growing it is refused whole,
    /// with nothing published.
    #[test]
    fn keyed_tables_keep_their_index_and_refuse_growth() {
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
            &[("age", PropValues::Int(&[1, 2, 3, 4]))],
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
        // Growth: refused before anything publishes.
        let mut txn = mvcc.begin();
        txn.insert_nodes(person, vec![(0, vec![Cell::Int(5)])])
            .unwrap();
        txn.commit(&mut wal).unwrap();
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
        assert_eq!(mvcc.appended_rows(person, mvcc.epoch()), 1);
        assert!(!wal.is_empty(), "the log still holds the txn");
        let recovered = recover(&mut db, &wal).unwrap();
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

    /// A bulk load keeps both copies of a pair; folding a second one in
    /// over a table that stores properties does not, because the fold's
    /// sort would not say which copy came first.
    #[test]
    fn a_second_edge_over_a_pair_is_refused_on_a_table_with_columns() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("reldup.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2)]).unwrap();
        crate::props::store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1, 2]))])
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
        let err = checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap_err();
        assert!(matches!(err, ZuError::Unsupported { .. }), "{err}");
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
