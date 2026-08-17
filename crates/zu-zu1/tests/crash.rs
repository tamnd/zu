//! Deterministic crash injection at every syscall boundary.
//!
//! A recorded workload stores a set of edge property columns, runs two
//! committed txns, a checkpoint fold, and two ingest commits whose
//! payload travels outside the WAL, against recording files that log
//! every write, length change, and sync. The harness then builds the
//! file image a crash could leave at
//! every boundary: the full prefix at each cut, the prefix with each
//! unsynced write dropped (reordering loss), and the prefix with the
//! final write torn at several lengths. Every image is recovered and
//! its logical state must be a committed prefix of the workload,
//! pre-txn or post-txn and nothing else, and a fold run on the
//! recovered image must not change that state.
//!
//! Durability rides on top of the corruption invariant: once a commit
//! has been acknowledged, meaning its WAL sync returned, every later
//! cut must recover to that commit or newer. The harness records the
//! log length at each acknowledgment and floors the allowed states per
//! cut, which is sound for every image class because drops only remove
//! writes issued after a file's last sync and tears only shorten the
//! newest write, so neither can touch an acknowledged frame. The
//! ingest commits lean on the same argument from the data file's side:
//! their sealed segments are synced before the WAL reference is, so an
//! acknowledged ingest has both the frame and the payload out of every
//! drop window.
//!
//! The edge property store has no WAL frame at all: it rewrites columns
//! and republishes the table index, and the checkpoint at the end of it
//! is what makes it visible. The same argument still floors it. Once
//! that checkpoint's second sync has returned, every write the store
//! issued sits before the data file's last sync, so no drop window
//! reaches one and no tear can shorten one, and every later cut has to
//! recover to the new columns.

use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::fold::{checkpoint_fold, recover};
use zu_zu1::graph::{Direction, GraphReader, bulk_load_as};
use zu_zu1::ingest::{ingest_edges, ingest_nodes};
use zu_zu1::props::{
    PropValues, PropsReader, load_props, load_rel_props, store_props, store_rel_props,
};
use zu_zu1::txn::{Cell, Mvcc};
use zu_zu1::vfs::{IoEvent, IoFile, IoLog, RealFile, RecordingFile};
use zu_zu1::wal::Wal;

/// The fixture's table ids. A crash image is opened from bytes, so
/// nothing carries over from the recording and every reader is handed
/// them.
#[derive(Debug, Clone, Copy)]
struct Tables {
    person: u32,
    knows: u32,
    likes: u32,
}

/// Everything a reader can observe about the fixture's tables at the
/// store's current epoch, base file and overlays merged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    total: u64,
    ages: Vec<u64>,
    names: Vec<Vec<u8>>,
    deleted: Vec<bool>,
    out: Vec<Vec<u64>>,
    /// One entry per edge of the `likes` table, source and destination
    /// first and then the two property values that edge carries. The
    /// endpoints are in there because an edge column is addressed by an
    /// ordinal and nothing else, so a recovered file that kept the
    /// values and renumbered the edges reads as a different state here
    /// rather than as the same one.
    likes: Vec<(u64, u64, u64, Vec<u8>)>,
}

fn state(db: &mut Zu1File, mvcc: &Mvcc, tables: Tables) -> State {
    let (person, knows) = (tables.person, tables.knows);
    let epoch = mvcc.epoch();
    let base_count = Catalog::load(db)
        .unwrap()
        .node_by_id(person)
        .unwrap()
        .node_count;
    let total = base_count + mvcc.appended_rows(person, epoch);
    let props = load_props(db, person).unwrap().unwrap();
    let mut reader = PropsReader::new(props);
    let mut graph = GraphReader::load_table(db, "knows").unwrap();
    let graph_rows = graph.directory().from_count;
    let mut ages = Vec::new();
    let mut names = Vec::new();
    let mut deleted = Vec::new();
    let mut out = Vec::new();
    for row in 0..total {
        ages.push(match mvcc.cell(person, base_count, row, 0, epoch) {
            Some(Cell::Int(x)) => x,
            Some(Cell::Str(_) | Cell::Null) => unreachable!("age is an int column nothing removes"),
            None => reader.read_int(db, 0, row).unwrap(),
        });
        names.push(match mvcc.cell(person, base_count, row, 1, epoch) {
            Some(Cell::Str(s)) => s,
            Some(Cell::Int(_) | Cell::Null) => unreachable!("name is a str column nothing removes"),
            None => {
                let mut buf = Vec::new();
                reader.read_str(db, 1, row, &mut buf).unwrap();
                buf
            }
        });
        deleted.push(mvcc.is_deleted(person, row, epoch));
        let mut nbrs = if row < graph_rows {
            graph
                .neighbors_dir(db, row, Direction::Fwd)
                .unwrap()
                .to_vec()
        } else {
            Vec::new()
        };
        mvcc.neighbors(knows, row, false, epoch, &mut nbrs);
        nbrs.sort_unstable();
        out.push(nbrs);
    }
    State {
        total,
        ages,
        names,
        deleted,
        out,
        likes: edge_props(db, tables.likes),
    }
}

/// Reads every edge of the `likes` table back with the property values
/// it carries, going through the ordinal lookup rather than counting
/// rows, because the lookup is what a query does.
fn edge_props(db: &mut Zu1File, likes: u32) -> Vec<(u64, u64, u64, Vec<u8>)> {
    let mut graph = GraphReader::load_table(db, "likes").unwrap();
    let mut reader = PropsReader::new(load_rel_props(db, likes).unwrap().unwrap());
    let since = reader.col("since").unwrap();
    let tag = reader.col("tag").unwrap();
    let mut edges = Vec::new();
    for src in 0..graph.directory().from_count {
        for dst in graph
            .neighbors_dir(db, src, Direction::Fwd)
            .unwrap()
            .to_vec()
        {
            let ordinal = graph.edge_ordinal(db, src, dst).unwrap().unwrap();
            let mut buf = Vec::new();
            reader.read_str(db, tag, ordinal, &mut buf).unwrap();
            edges.push((src, dst, reader.read_int(db, since, ordinal).unwrap(), buf));
        }
    }
    edges
}

/// Applies an event prefix to in-memory images of both files.
fn materialize(initial_db: &[u8], events: &[&IoEvent]) -> (Vec<u8>, Vec<u8>) {
    let mut db = initial_db.to_vec();
    let mut wal = Vec::new();
    for event in events {
        match event {
            IoEvent::Write {
                file,
                offset,
                bytes,
            } => {
                let image = if *file == IoFile::Db {
                    &mut db
                } else {
                    &mut wal
                };
                let end = *offset as usize + bytes.len();
                if image.len() < end {
                    image.resize(end, 0);
                }
                image[*offset as usize..end].copy_from_slice(bytes);
            }
            IoEvent::SetLen { file, len } => {
                let image = if *file == IoFile::Db {
                    &mut db
                } else {
                    &mut wal
                };
                image.resize(*len as usize, 0);
            }
            IoEvent::Sync { .. } => {}
        }
    }
    (db, wal)
}

/// Opens a crash image, recovers it, and returns its logical state; a
/// second fold on the recovered image must leave the state alone.
fn recovered_state(dir: &std::path::Path, db: &[u8], wal: &[u8], tables: Tables) -> State {
    let db_path = dir.join("image.zu1");
    let wal_path = dir.join("image.wal");
    std::fs::write(&db_path, db).unwrap();
    std::fs::write(&wal_path, wal).unwrap();
    let mut db = Zu1File::open(&db_path).unwrap();
    let mut wal = Wal::open(&wal_path).unwrap();
    let mut mvcc = recover(&mut db, &wal).unwrap();
    let before = state(&mut db, &mvcc, tables);
    checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
    let after = state(&mut db, &mvcc, tables);
    assert_eq!(before, after, "a fold must not change the logical state");
    std::fs::remove_file(&db_path).unwrap();
    std::fs::remove_file(&wal_path).unwrap();
    before
}

#[test]
fn every_cut_recovers_to_a_committed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("crash.zu1");
    let wal_path = dir.path().join("crash.wal");
    // Setup outside the recording: the fixture graph and props are the
    // committed base every crash image starts from.
    {
        let mut db = Zu1File::create(&db_path).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        // A second rel table over the same people, which is the one that
        // carries edge properties. It has to be a table of its own: an
        // overlay edge arrives with no value for a column that holds one
        // per edge, so a table that stores edge properties refuses the
        // fold, and `knows` takes overlay edges in every txn below.
        bulk_load_as(&mut db, "person", "likes", 4, &[(0, 2), (1, 3), (3, 0)]).unwrap();
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
        let tags: Vec<&[u8]> = vec![b"blue", b"green", b"red"];
        store_rel_props(
            &mut db,
            "likes",
            &[
                ("since", PropValues::Int(&[7, 8, 9])),
                ("tag", PropValues::Str(&tags)),
            ],
        )
        .unwrap();
    }
    let initial_db = std::fs::read(&db_path).unwrap();
    let log: IoLog = Default::default();
    let (tables, allowed, acks) = {
        let mut db = Zu1File::open_on(
            Box::new(RecordingFile::new(
                RealFile::open_rw(&db_path).unwrap(),
                IoFile::Db,
                log.clone(),
            )),
            &db_path,
        )
        .unwrap();
        let mut wal = Wal::open_on(
            Box::new(RecordingFile::new(
                RealFile::open_or_create(&wal_path).unwrap(),
                IoFile::Wal,
                log.clone(),
            )),
            &wal_path,
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let tables = Tables {
            person: catalog.node_by_name("person").unwrap().id,
            knows: catalog.rel_by_name("knows").unwrap().id,
            likes: catalog.rel_by_name("likes").unwrap().id,
        };
        let person = tables.person;
        let knows = tables.knows;
        let mut mvcc = recover(&mut db, &wal).unwrap();
        let s0 = state(&mut db, &mvcc, tables);
        // The edge property write path, inside the recording: it frees
        // the old columns, writes the new ones, republishes the table
        // index and checkpoints, all of it in the data file with no WAL
        // frame behind it, so every cut through it has to land on one
        // set of columns or the other.
        let tags: Vec<&[u8]> = vec![b"amber", b"violet", b"grey"];
        store_rel_props(
            &mut db,
            "likes",
            &[
                ("since", PropValues::Int(&[70, 80, 90])),
                ("tag", PropValues::Str(&tags)),
            ],
        )
        .unwrap();
        let ack_props = log.lock().unwrap().len();
        let s_props = state(&mut db, &mvcc, tables);
        assert_ne!(s0, s_props, "the store must change what a reader sees");
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
        txn.commit(&mut wal).unwrap();
        let ack1 = log.lock().unwrap().len();
        let s1 = state(&mut db, &mvcc, tables);
        let mut txn = mvcc.begin();
        txn.update(person, 0, 0, Cell::Int(11));
        txn.insert_rel(knows, 3, 4);
        txn.delete(person, 4);
        txn.commit(&mut wal).unwrap();
        let ack2 = log.lock().unwrap().len();
        let s2 = state(&mut db, &mvcc, tables);
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        assert_eq!(
            state(&mut db, &mvcc, tables),
            s2,
            "the fold must seal exactly the committed state"
        );
        let names: Vec<&[u8]> = vec![b"zoe"];
        ingest_nodes(
            &mut db,
            &mut wal,
            &mut mvcc,
            person,
            &[(0, PropValues::Int(&[70])), (1, PropValues::Str(&names))],
        )
        .unwrap();
        let ack3 = log.lock().unwrap().len();
        let s3 = state(&mut db, &mvcc, tables);
        ingest_edges(&mut db, &mut wal, &mut mvcc, knows, &[6, 0], &[0, 6]).unwrap();
        let ack4 = log.lock().unwrap().len();
        let s4 = state(&mut db, &mvcc, tables);
        // Every acknowledgment carries the state it floors at rather
        // than being counted, because the workload now passes through a
        // state no commit acknowledged and the number of
        // acknowledgments behind a cut is no longer its index.
        (
            tables,
            vec![s0, s_props, s1, s2, s3, s4],
            [(ack_props, 1), (ack1, 2), (ack2, 3), (ack3, 4), (ack4, 5)],
        )
    };
    let events = log.lock().unwrap().clone();
    assert!(events.len() > 20, "the workload records real syscalls");
    // Which of the workload's states some image actually recovered to,
    // so that a harness that stopped reaching a state, because a state
    // stopped being distinguishable or because the writes it sits
    // between went away, says so instead of passing on the ones left.
    let reached = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let check = |image_db: &[u8], image_wal: &[u8], what: String, floor: usize| {
        let got = recovered_state(dir.path(), image_db, image_wal, tables);
        let at = allowed[floor..].iter().position(|s| *s == got);
        assert!(
            at.is_some(),
            "{what} recovered to {got:?}, outside the committed prefixes at or after \
             the last acknowledged commit (floor {floor})"
        );
        reached.borrow_mut().insert(floor + at.unwrap());
    };
    let mut images = 0usize;
    for cut in 0..=events.len() {
        // Every commit acknowledged before this cut must survive it:
        // its frames sit before a WAL sync that the prefix keeps, the
        // drops start after that sync, and tears only shorten the
        // newest write, which postdates the sync as well. The edge
        // property store floors the same way off the data file's own
        // sync.
        let floor = acks
            .iter()
            .filter(|&&(ack, _)| cut >= ack)
            .map(|&(_, state)| state)
            .max()
            .unwrap_or(0);
        let prefix: Vec<&IoEvent> = events[..cut].iter().collect();
        // The crash with everything issued so far persisted.
        let (db, wal) = materialize(&initial_db, &prefix);
        check(&db, &wal, format!("cut {cut}"), floor);
        images += 1;
        // The crash that lost one unsynced write: for each file, every
        // mutating event after its last sync inside the prefix may
        // vanish independently of the ones around it.
        for file in [IoFile::Db, IoFile::Wal] {
            let last_sync = events[..cut]
                .iter()
                .rposition(|e| matches!(e, IoEvent::Sync { file: f } if *f == file));
            let window = last_sync.map_or(0, |i| i + 1);
            for drop in window..cut {
                let owner = match &events[drop] {
                    IoEvent::Write { file: f, .. } | IoEvent::SetLen { file: f, .. } => *f,
                    IoEvent::Sync { .. } => continue,
                };
                if owner != file {
                    continue;
                }
                let survivors: Vec<&IoEvent> = events[..cut]
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != drop)
                    .map(|(_, e)| e)
                    .collect();
                let (db, wal) = materialize(&initial_db, &survivors);
                check(&db, &wal, format!("cut {cut} dropping event {drop}"), floor);
                images += 1;
            }
        }
        // The crash that tore the newest write.
        if cut > 0
            && let IoEvent::Write {
                file,
                offset,
                bytes,
            } = &events[cut - 1]
        {
            for keep in [1, bytes.len() / 2, bytes.len().saturating_sub(1)] {
                if keep == 0 || keep >= bytes.len() {
                    continue;
                }
                let torn = IoEvent::Write {
                    file: *file,
                    offset: *offset,
                    bytes: bytes[..keep].to_vec(),
                };
                let mut prefix: Vec<&IoEvent> = events[..cut - 1].iter().collect();
                prefix.push(&torn);
                let (db, wal) = materialize(&initial_db, &prefix);
                check(&db, &wal, format!("cut {cut} torn to {keep} bytes"), floor);
                images += 1;
            }
        }
    }
    assert_eq!(
        reached.borrow().len(),
        allowed.len(),
        "every state the workload passed through must be where some crash image landed, \
         and only {:?} were reached",
        reached.borrow()
    );
    println!(
        "{images} crash images checked over {} syscalls",
        events.len()
    );
}
