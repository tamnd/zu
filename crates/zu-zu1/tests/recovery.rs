//! The zu1 row of the docs/08 section 7 recovery matrix, end to end:
//! pick the valid max-epoch header, replay the WAL tail, open. The
//! crash harness proves every cut recovers to a committed prefix;
//! these tests pin the two properties the matrix row adds on top,
//! that recovery composes across a fold and that it never scans data
//! blocks, so its cost is headers plus meta chains plus the WAL and
//! stays flat as the database grows.

use std::sync::{Arc, Mutex};

use zu_zu1::BLOCK_SIZE;
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::fold::{checkpoint_fold, recover};
use zu_zu1::graph::{Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropValues, PropsReader, load_props, store_props};
use zu_zu1::txn::Cell;
use zu_zu1::vfs::{RealFile, VfsFile};
use zu_zu1::wal::Wal;

/// A pass-through that counts bytes read, for pinning what recovery
/// touches.
#[derive(Debug)]
struct CountingFile {
    inner: RealFile,
    read: Arc<Mutex<u64>>,
}

impl VfsFile for CountingFile {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> zu_common::Result<()> {
        *self.read.lock().unwrap() += buf.len() as u64;
        self.inner.read_exact_at(buf, offset)
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> zu_common::Result<()> {
        self.inner.write_all_at(buf, offset)
    }

    fn set_len(&mut self, len: u64) -> zu_common::Result<()> {
        self.inner.set_len(len)
    }

    fn sync_all(&mut self) -> zu_common::Result<()> {
        self.inner.sync_all()
    }

    fn sync_data(&mut self) -> zu_common::Result<()> {
        self.inner.sync_data()
    }

    fn len(&self) -> zu_common::Result<u64> {
        self.inner.len()
    }
}

#[test]
fn reopen_picks_the_newest_header_and_replays_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("matrix.zu1");
    let wal_path = dir.path().join("matrix.wal");
    {
        let mut db = Zu1File::create(&db_path).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        store_props(
            &mut db,
            "person",
            &[("age", PropValues::Int(&[10, 20, 30, 40]))],
        )
        .unwrap();
        let mut wal = Wal::open(&wal_path).unwrap();
        let mut mvcc = recover(&mut db, &wal).unwrap();
        let mut txn = mvcc.begin();
        txn.update(0, 0, 0, Cell::Int(11));
        txn.insert_rel(1, 2, 0);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        // The tail: a commit the fold never saw, alive only in the WAL.
        let mut txn = mvcc.begin();
        txn.update(0, 1, 0, Cell::Int(22));
        txn.delete(0, 3);
        txn.insert_rel(1, 0, 3);
        txn.commit(&mut wal).unwrap();
    }
    let mut db = Zu1File::open(&db_path).unwrap();
    let wal = Wal::open(&wal_path).unwrap();
    let mvcc = recover(&mut db, &wal).unwrap();
    let epoch = mvcc.epoch();
    let catalog = Catalog::load(&mut db).unwrap();
    let person = catalog.node_by_name("person").unwrap().id;
    let knows = catalog.rel_by_name("knows").unwrap().id;
    // The fold's flip won the header pick: its floor skips txn1 on
    // replay and its rewritten props hold the folded value.
    assert!(db.db_header().wal_seq > 0, "the fold persisted its floor");
    let props = load_props(&mut db, person).unwrap().unwrap();
    let mut reader = PropsReader::new(props);
    assert_eq!(reader.read_int(&mut db, 0, 0).unwrap(), 11);
    assert!(mvcc.cell(person, 4, 0, 0, epoch).is_none());
    // The tail replayed above the floor: txn2 is back as an overlay.
    assert_eq!(mvcc.cell(person, 4, 1, 0, epoch), Some(Cell::Int(22)));
    assert!(mvcc.is_deleted(person, 3, epoch));
    let mut graph = GraphReader::load_table(&mut db, "knows").unwrap();
    let mut nbrs = graph
        .neighbors_dir(&mut db, 0, Direction::Fwd)
        .unwrap()
        .to_vec();
    mvcc.neighbors(knows, 0, false, epoch, &mut nbrs);
    nbrs.sort_unstable();
    assert_eq!(nbrs, vec![1, 3]);
    let mut base = graph
        .neighbors_dir(&mut db, 2, Direction::Fwd)
        .unwrap()
        .to_vec();
    base.sort_unstable();
    assert_eq!(base, vec![0, 3], "txn1's edge folded into the base CSR");
}

#[test]
fn recovery_reads_headers_and_meta_not_data_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("flat.zu1");
    let wal_path = dir.path().join("flat.wal");
    let rows = 200_000u64;
    {
        let mut db = Zu1File::create(&db_path).unwrap();
        let edges: Vec<(u32, u32)> = (0..rows as u32 - 1).map(|i| (i, i + 1)).collect();
        bulk_load_as(&mut db, "person", "knows", rows, &edges).unwrap();
        let ages: Vec<u64> = (0..rows).collect();
        store_props(&mut db, "person", &[("age", PropValues::Int(&ages))]).unwrap();
        let mut wal = Wal::open(&wal_path).unwrap();
        let mut mvcc = recover(&mut db, &wal).unwrap();
        // A fold with a delete persists a tombstone chain, so recovery
        // below exercises every read it can issue: headers, the table
        // index, and the tombstone metas.
        let mut txn = mvcc.begin();
        txn.delete(0, 7);
        txn.commit(&mut wal).unwrap();
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut txn = mvcc.begin();
        txn.update(0, 5, 0, Cell::Int(99));
        txn.insert_rel(1, 0, 7);
        txn.commit(&mut wal).unwrap();
    }
    let file_len = std::fs::metadata(&db_path).unwrap().len();
    assert!(
        file_len > 10 * BLOCK_SIZE as u64,
        "the fixture must dwarf the recovery read bound, holds {file_len} bytes"
    );
    let read = Arc::new(Mutex::new(0u64));
    let mut db = Zu1File::open_on(
        Box::new(CountingFile {
            inner: RealFile::open_rw(&db_path).unwrap(),
            read: read.clone(),
        }),
        &db_path,
    )
    .unwrap();
    let wal = Wal::open(&wal_path).unwrap();
    let mvcc = recover(&mut db, &wal).unwrap();
    // Open plus recover touches the 12 KiB head, the free list chain
    // (twice, payload then block list), the table index chain, and the
    // tombstone chain, each a single meta block here. Data segments
    // fill the rest of the file and none of them may be read.
    let bytes = *read.lock().unwrap();
    assert!(
        bytes < 6 * BLOCK_SIZE as u64 && bytes < file_len / 3,
        "recovery read {bytes} bytes of a {file_len} byte file; it must stop at \
         headers and meta chains"
    );
    // Not vacuous: the recovered store holds the folded tombstone and
    // the replayed tail.
    let epoch = mvcc.epoch();
    assert!(mvcc.is_deleted(0, 7, epoch));
    assert_eq!(mvcc.cell(0, rows, 5, 0, epoch), Some(Cell::Int(99)));
    let mut nbrs = Vec::new();
    mvcc.neighbors(1, 0, false, epoch, &mut nbrs);
    assert_eq!(nbrs, vec![7]);
}
