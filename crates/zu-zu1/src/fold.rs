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

use std::collections::HashSet;

use zu_common::{Epoch, GROUP_ROWS, Result, ZuError};

use crate::catalog::{Catalog, RelTable, TableIndex};
use crate::file::Zu1File;
use crate::fullzip::{read_blob_segment, write_blob_segment};
use crate::graph::{
    Direction, Directory, GraphReader, GroupMeta, build_direction, free_chain, free_directory,
};
use crate::keys::write_key_index;
use crate::meta;
use crate::props::{PropType, PropsDirectory, free_props};
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
/// above the header's `wal_seq` replay from the log, then every
/// persisted tombstone chain seeds back in at epoch 0.
pub fn recover(db: &mut Zu1File, wal: &Wal) -> Result<Mvcc> {
    let floor = db.db_header().wal_seq;
    let mut mvcc = Mvcc::recover(wal, floor)?;
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
        let (name, base) = (node.name.clone(), node.node_count);
        let appended = mvcc.appended_rows(table, epoch);
        if appended > 0 || mvcc.has_updates(table, epoch) {
            changed |= fold_props(db, mvcc, &mut index, table, base, epoch)?;
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

/// Merges one table's appended rows and update chains into a rebuilt
/// props directory. A table without stored props cannot absorb column
/// data, and its row count still grows through the catalog alone.
fn fold_props(
    db: &mut Zu1File,
    mvcc: &Mvcc,
    index: &mut TableIndex,
    table: u32,
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
        let meta = match col.ty {
            PropType::Int => {
                let mut values = Vec::with_capacity(new_count as usize);
                read_segment(db, &col.meta, &mut values)?;
                for offset in 0..new_count {
                    match mvcc.cell(table, base, offset, ci as u32, epoch) {
                        Some(Cell::Int(x)) if offset < base => values[offset as usize] = x,
                        Some(Cell::Int(x)) => values.push(x),
                        Some(Cell::Str(_)) => return Err(mismatch(&col.name, offset)),
                        None if offset < base => {}
                        None => return Err(missing(&col.name, offset)),
                    }
                }
                write_segment(db, &values)?
            }
            PropType::Str => {
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
                        Some(Cell::Str(s)) if offset < base => values[offset as usize] = s,
                        Some(Cell::Str(s)) => values.push(s),
                        Some(Cell::Int(_)) => return Err(mismatch(&col.name, offset)),
                        None if offset < base => {}
                        None => return Err(missing(&col.name, offset)),
                    }
                }
                let refs: Vec<&[u8]> = values.iter().map(|v| v.as_slice()).collect();
                write_blob_segment(db, &refs)?
            }
        };
        columns.push(crate::props::PropColumn {
            name: col.name.clone(),
            ty: col.ty,
            meta,
        });
    }
    free_props(db, root)?;
    let new_dir = PropsDirectory {
        node_count: new_count,
        columns,
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
    let new_count = catalog
        .node_by_id(rel.from)
        .expect("validated on decode")
        .node_count;
    if new_count > u64::from(u32::MAX) {
        return Err(ZuError::Unsupported {
            what: "folding a rel table past the u32 row domain",
            id: rel.id,
        });
    }
    let mut reader = GraphReader::load_table(db, &rel.name)?;
    let old = reader.directory().clone();
    let overlay: Vec<(u64, u64)> = mvcc.edges(rel.id, epoch).collect();
    let mut edges: Vec<(u32, u32)> = Vec::with_capacity(old.edge_count as usize + overlay.len());
    for node in 0..old.node_count {
        for &dst in reader.neighbors_dir(db, node, Direction::Fwd)? {
            edges.push((node as u32, dst as u32));
        }
    }
    for (src, dst) in overlay {
        if src >= new_count || dst >= new_count {
            return Err(ZuError::InvalidArgument(format!(
                "overlay edge ({src}, {dst}) references a row outside 0..{new_count}"
            )));
        }
        edges.push((src as u32, dst as u32));
    }
    edges.sort_unstable();
    // A keyed table's index survives only while the row domain holds
    // still; growing it takes the key allocation the constraint slice
    // brings, so appending to a keyed table is refused for now.
    let key_by_row = match &old.keys {
        None => None,
        Some(_) if new_count != old.node_count => {
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
            let mut by_row = vec![0u64; new_count as usize];
            for (i, &row) in rows.iter().enumerate() {
                by_row[row as usize] = key_list[i];
            }
            Some(by_row)
        }
    };
    free_directory(db, root)?;
    let fwd = build_direction(db, new_count, &edges)?;
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let bwd = build_direction(db, new_count, &rev)?;
    drop(rev);
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: (new_count - g as u64 * GROUP_ROWS as u64).min(GROUP_ROWS as u64) as u32,
            fwd,
            bwd,
        })
        .collect();
    let directory = Directory {
        node_count: new_count,
        edge_count: edges.len() as u64,
        keys: key_by_row
            .map(|keys| write_key_index(db, &keys))
            .transpose()?,
        groups,
    };
    index.set(rel.id, meta::write_chain(db, &directory.encode())?);
    Ok(directory.edge_count)
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
