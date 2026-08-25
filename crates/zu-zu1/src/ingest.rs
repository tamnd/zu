//! The bulk-load WAL bypass of docs/08 sections 3 and 6.
//!
//! An ingest writes its payload once, as sealed segments in the data
//! file, and the WAL carries only an `IngestRef` frame naming them, so
//! the log stays the same handful of bytes whether the load is ten
//! rows or ten million. The commit protocol orders three durability
//! points: the segments and their manifest chain are written and the
//! data file synced first, then the WAL frame referencing them is
//! appended and synced, which is the commit point, and only then does
//! the overlay publish. A crash before the WAL sync leaves the
//! segments as unreferenced garbage in a file whose committed header
//! never knew them; a crash after it leaves a log whose replay reads
//! the segments back into the overlay.
//!
//! Two rules make the reference safe to trust across a crash. First,
//! ingest allocation never touches the committed free list: after a
//! crash that list is what the reopened file hands out, so blocks
//! whose only reference is a WAL frame must sit past the committed
//! high-water mark, where nothing allocates until a header flip
//! publishes the new watermark. The manifest records that watermark
//! and recovery raises the reopened handle's in-memory count to it
//! before reading. Second, the fold that seals the ingested data into
//! the base frees the manifest and segment blocks in the same flip, so
//! they return to circulation only once no replay can need them.

use zu_common::{Epoch, Result, ZuError};

use crate::catalog::Catalog;
use crate::file::{BlockPtr, Zu1File};
use crate::fullzip::write_blob_segment;
use crate::meta;
use crate::props::{PropValues, load_props};
use crate::rows::read_rows;
use crate::segment::{SegmentMeta, read_segment, write_segment};
use crate::txn::{IngestPayload, Mvcc};
use crate::wal::{Wal, WalColumn, WalRecord, WalValues};

const MANIFEST_VERSION: u16 = 1;
const KIND_NODES: u8 = 0;
const KIND_EDGES: u8 = 1;

fn corrupt(detail: String) -> ZuError {
    ZuError::Corrupt {
        what: "ingest manifest",
        detail,
    }
}

/// Which of the two storage shapes a sealed segment holds. The
/// manifest does not need the column's logical type: the props
/// directory already carries that, and what replay has to know is how
/// to read the bytes back, which is the lane or the blob and nothing
/// finer. A float column and a date column seal identically here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SealKind {
    Lane,
    Blob,
}

/// One sealed column or edge endpoint as the manifest stores it.
struct ManifestSegment {
    col: u32,
    ty: SealKind,
    meta: SegmentMeta,
}

/// The decoded manifest chain: what was ingested and where it landed.
struct Manifest {
    table: u32,
    kind: u8,
    rows: u64,
    segments: Vec<ManifestSegment>,
}

fn encode_manifest(table: u32, kind: u8, rows: u64, segments: &[ManifestSegment]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + segments.len() * 64);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&table.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    for seg in segments {
        out.extend_from_slice(&seg.col.to_le_bytes());
        out.push(match seg.ty {
            SealKind::Lane => 0,
            SealKind::Blob => 1,
        });
        seg.meta.encode(&mut out);
    }
    out
}

fn decode_manifest(bytes: &[u8]) -> Result<Manifest> {
    let head = bytes
        .get(..19)
        .ok_or_else(|| corrupt("truncated header".into()))?;
    let version = u16::from_le_bytes(head[..2].try_into().unwrap());
    if version != MANIFEST_VERSION {
        return Err(ZuError::Unsupported {
            what: "ingest manifest version",
            id: u32::from(version),
        });
    }
    let kind = head[2];
    if kind != KIND_NODES && kind != KIND_EDGES {
        return Err(corrupt(format!("unknown ingest kind {kind}")));
    }
    let table = u32::from_le_bytes(head[3..7].try_into().unwrap());
    let rows = u64::from_le_bytes(head[7..15].try_into().unwrap());
    let count = u32::from_le_bytes(head[15..19].try_into().unwrap()) as usize;
    let mut pos = 19;
    let mut segments = Vec::with_capacity(count.min(bytes.len() / 54));
    for _ in 0..count {
        let fixed = bytes
            .get(pos..pos + 5)
            .ok_or_else(|| corrupt("truncated segment entry".into()))?;
        let col = u32::from_le_bytes(fixed[..4].try_into().unwrap());
        let ty = match fixed[4] {
            0 => SealKind::Lane,
            1 => SealKind::Blob,
            other => return Err(corrupt(format!("unknown column type {other}"))),
        };
        let (meta, next) = SegmentMeta::decode(bytes, pos + 5)?;
        if meta.value_count != rows {
            return Err(corrupt(format!(
                "column {col} seals {} values over {rows} rows",
                meta.value_count
            )));
        }
        pos = next;
        segments.push(ManifestSegment { col, ty, meta });
    }
    if pos != bytes.len() {
        return Err(corrupt(format!(
            "{} trailing bytes after {count} segments",
            bytes.len() - pos
        )));
    }
    Ok(Manifest {
        table,
        kind,
        rows,
        segments,
    })
}

/// Writes the sealed segments and manifest with the committed free
/// list held aside, then syncs the data file. Returns the manifest
/// root; the caller commits the WAL reference.
fn seal<T>(
    db: &mut Zu1File,
    build: impl FnOnce(&mut Zu1File) -> Result<(Vec<ManifestSegment>, T)>,
    table: u32,
    kind: u8,
    rows: u64,
) -> Result<(BlockPtr, T)> {
    let saved = db.take_free();
    // One manifest is one packing scope, both because `free_ingest`
    // takes its segments away together and because an abandoned ingest
    // is thrown away by rewinding the block watermark: a block a sealed
    // segment shares with anything older would go with it.
    let held = db.pack_open();
    let result: Result<(BlockPtr, T)> = (|| {
        let (segments, extra) = build(db)?;
        let root = meta::write_chain(db, &encode_manifest(table, kind, rows, &segments))?;
        Ok((root, extra))
    })();
    db.pack_close(held);
    db.restore_free(saved);
    let (root, extra) = result?;
    db.sync_data()?;
    Ok((root, extra))
}

/// Commits the `IngestRef` frame for a sealed manifest and publishes
/// the payload to the overlays. The frame carries the manifest root
/// and the post-ingest block watermark, which recovery restores before
/// it reads anything.
fn commit_ref(
    db: &mut Zu1File,
    wal: &mut Wal,
    mvcc: &mut Mvcc,
    table: u32,
    root: BlockPtr,
    payload: IngestPayload,
) -> Result<Epoch> {
    let epoch = mvcc.epoch() + 1;
    wal.append(epoch, &WalRecord::TxnBegin)?;
    wal.append(
        epoch,
        &WalRecord::IngestRef {
            table,
            ptrs: vec![root, db.db_header().block_count],
        },
    )?;
    wal.commit(epoch)?;
    mvcc.publish_ingest(epoch, table, payload, root);
    Ok(epoch)
}

/// Bulk-appends rows to a node table: every stored column of the table
/// exactly once, each carrying one value per new row, sealed straight
/// into the data file with only an `IngestRef` in the log. Visibility
/// matches any other commit: the rows exist at the returned epoch and
/// not before, and the next fold seals them into the base.
pub fn ingest_nodes(
    db: &mut Zu1File,
    wal: &mut Wal,
    mvcc: &mut Mvcc,
    table: u32,
    cols: &[(u32, PropValues)],
) -> Result<Epoch> {
    let rows = cols.first().map_or(0, |(_, v)| v.len() as u64);
    if rows == 0 {
        return Err(ZuError::InvalidArgument(
            "an ingest must carry at least one row".into(),
        ));
    }
    if cols.iter().any(|(_, v)| v.len() as u64 != rows) {
        return Err(ZuError::InvalidArgument(
            "ragged ingest: columns disagree on row count".into(),
        ));
    }
    if Catalog::load(db)?.node_by_id(table).is_none() {
        return Err(ZuError::InvalidArgument(format!(
            "ingest names unknown node table {table}"
        )));
    }
    let dir = load_props(db, table)?.ok_or(ZuError::Unsupported {
        what: "ingesting column data into a table without stored props",
        id: table,
    })?;
    let mut covered = vec![false; dir.columns.len()];
    for &(col, ref values) in cols {
        let stored = dir.columns.get(col as usize).ok_or_else(|| {
            ZuError::InvalidArgument(format!("ingest names no stored column at position {col}"))
        })?;
        let ty = values.ty();
        if stored.ty != ty {
            return Err(ZuError::InvalidArgument(format!(
                "ingest column '{}' holds {ty}, the stored column holds {}",
                stored.name, stored.ty
            )));
        }
        // An ingest seals one segment a column, and a zoned column is
        // two planes, so what would be sealed here is the instants
        // alone and the offsets beside them would be dropped without
        // anything saying so. That is a wrong answer rather than a
        // missing one, which is why it is refused and not deferred.
        if crate::props::zoned(&stored.ty) {
            return Err(ZuError::Unsupported {
                what: "ingesting into a zoned column, which is two planes and not one",
                id: col,
            });
        }
        // An appended row would have no bit in the column's validity
        // mask, and there is no way in this call to say whether it
        // holds a value. Refused here rather than at the fold, so the
        // message names the column; G3's write statements are what has
        // to answer it.
        if stored.validity.is_some() {
            return Err(ZuError::Unsupported {
                what: "appending rows to a column that holds a null",
                id: col,
            });
        }
        if std::mem::replace(&mut covered[col as usize], true) {
            return Err(ZuError::InvalidArgument(format!(
                "ingest carries column '{}' twice",
                stored.name
            )));
        }
    }
    if let Some(missing) = covered.iter().position(|&c| !c) {
        return Err(ZuError::InvalidArgument(format!(
            "ingest carries no values for column '{}'",
            dir.columns[missing].name
        )));
    }
    let (root, wal_cols) = seal(
        db,
        |db| {
            let mut segments = Vec::with_capacity(cols.len());
            let mut wal_cols = Vec::with_capacity(cols.len());
            for &(col, ref values) in cols {
                // The overlay carries lane columns as words, whatever
                // the words mean, and the stored column's type is what
                // reads them back. So the seal only splits two ways.
                let (ty, meta, wal_values) = match (values.lane(), values) {
                    (Some(words), _) => (
                        SealKind::Lane,
                        write_segment(db, &words)?,
                        WalValues::Int(words.into_owned()),
                    ),
                    (None, PropValues::Str(v) | PropValues::Bytes(v)) => (
                        SealKind::Blob,
                        write_blob_segment(db, v)?,
                        WalValues::Str(v.iter().map(|s| s.to_vec()).collect()),
                    ),
                    (None, _) => unreachable!("every variable width column is a blob"),
                };
                segments.push(ManifestSegment { col, ty, meta });
                wal_cols.push(WalColumn {
                    col,
                    values: wal_values,
                });
            }
            Ok((segments, wal_cols))
        },
        table,
        KIND_NODES,
        rows,
    )?;
    commit_ref(
        db,
        wal,
        mvcc,
        table,
        root,
        IngestPayload::Nodes {
            cols: wal_cols,
            rows,
        },
    )
}

/// Bulk-appends edges to a rel table: both endpoint lists sealed as
/// segments in the data file, one `IngestRef` in the log. The edges
/// join the overlay like any committed `insert_rel` batch and the next
/// fold merges them into the CSR.
pub fn ingest_edges(
    db: &mut Zu1File,
    wal: &mut Wal,
    mvcc: &mut Mvcc,
    rel: u32,
    src: &[u64],
    dst: &[u64],
) -> Result<Epoch> {
    if src.is_empty() {
        return Err(ZuError::InvalidArgument(
            "an ingest must carry at least one edge".into(),
        ));
    }
    if src.len() != dst.len() {
        return Err(ZuError::InvalidArgument(format!(
            "ingest carries {} sources and {} destinations",
            src.len(),
            dst.len()
        )));
    }
    let catalog = Catalog::load(db)?;
    let table = catalog
        .rel_by_id(rel)
        .ok_or_else(|| ZuError::InvalidArgument(format!("ingest names unknown rel table {rel}")))?;
    // Every edge joins two rows that are there, checked here and not
    // left to the fold. The fold checks it too and has to, since it is
    // what builds the CSR, but by the time it runs the frame naming
    // these edges is committed: the fold's refusal leaves a log the
    // next writer replays, refuses again, and cannot get past, so one
    // edge to a row that is not there would cost the database every
    // writer it has. Refused here, nothing is written and the file is
    // as it was.
    //
    // The domain is what the fold will see and not what the catalog
    // says: rows appended in this session and not folded yet are rows,
    // and an edge to one of them is a good edge.
    let epoch = mvcc.epoch();
    let rows = |end: u32| {
        catalog.node_by_id(end).map_or(0, |node| node.node_count) + mvcc.appended_rows(end, epoch)
    };
    let (from_rows, to_rows) = (rows(table.from), rows(table.to));
    if let Some((at, (&from, &to))) = src
        .iter()
        .zip(dst)
        .enumerate()
        .find(|&(_, (&from, &to))| from >= from_rows || to >= to_rows)
    {
        return Err(ZuError::InvalidArgument(format!(
            "edge {at} of this ingest joins ({from}, {to}), and the tables it runs between \
             hold {from_rows} and {to_rows} rows"
        )));
    }
    let (root, ()) = seal(
        db,
        |db| {
            let segments = vec![
                ManifestSegment {
                    col: 0,
                    ty: SealKind::Lane,
                    meta: write_segment(db, src)?,
                },
                ManifestSegment {
                    col: 1,
                    ty: SealKind::Lane,
                    meta: write_segment(db, dst)?,
                },
            ];
            Ok((segments, ()))
        },
        rel,
        KIND_EDGES,
        src.len() as u64,
    )?;
    commit_ref(
        db,
        wal,
        mvcc,
        rel,
        root,
        IngestPayload::Edges {
            src: src.to_vec(),
            dst: dst.to_vec(),
        },
    )
}

/// Resolves one committed `IngestRef` during recovery: restores the
/// block watermark the frame recorded, reads the manifest, and reads
/// every sealed segment back into an overlay payload.
pub(crate) fn resolve(
    db: &mut Zu1File,
    table: u32,
    ptrs: &[u64],
) -> Result<(IngestPayload, BlockPtr)> {
    let [root, watermark] = ptrs else {
        return Err(corrupt(format!(
            "IngestRef carries {} pointers, expected manifest root and watermark",
            ptrs.len()
        )));
    };
    if *watermark > db.db_header().block_count {
        db.db_header_mut().block_count = *watermark;
    }
    let manifest = decode_manifest(&meta::read_chain(db, *root)?)?;
    if manifest.table != table {
        return Err(corrupt(format!(
            "manifest seals table {}, frame names table {table}",
            manifest.table
        )));
    }
    let read_values = |db: &mut Zu1File, seg: &ManifestSegment| -> Result<WalValues> {
        Ok(match seg.ty {
            SealKind::Lane => {
                let mut values = Vec::with_capacity(manifest.rows as usize);
                read_segment(db, &seg.meta, &mut values)?;
                WalValues::Int(values)
            }
            SealKind::Blob => {
                let (mut bytes, mut ends) = (Vec::new(), Vec::new());
                read_rows(db, &seg.meta, &mut bytes, &mut ends)?;
                let mut values = Vec::with_capacity(ends.len());
                let mut start = 0usize;
                for &end in &ends {
                    values.push(bytes[start..end as usize].to_vec());
                    start = end as usize;
                }
                WalValues::Str(values)
            }
        })
    };
    let payload = match manifest.kind {
        KIND_NODES => {
            let mut cols = Vec::with_capacity(manifest.segments.len());
            for seg in &manifest.segments {
                cols.push(WalColumn {
                    col: seg.col,
                    values: read_values(db, seg)?,
                });
            }
            IngestPayload::Nodes {
                cols,
                rows: manifest.rows,
            }
        }
        _ => {
            let endpoint = |db: &mut Zu1File, col: u32| -> Result<Vec<u64>> {
                let seg = manifest
                    .segments
                    .iter()
                    .find(|s| s.col == col)
                    .ok_or_else(|| corrupt(format!("edge manifest misses endpoint {col}")))?;
                match read_values(db, seg)? {
                    WalValues::Int(v) => Ok(v),
                    WalValues::Str(_) | WalValues::Null(_) => {
                        Err(corrupt("edge endpoints must be ints".into()))
                    }
                }
            };
            IngestPayload::Edges {
                src: endpoint(db, 0)?,
                dst: endpoint(db, 1)?,
            }
        }
    };
    Ok((payload, *root))
}

/// Frees a folded ingest's blocks: every sealed segment and the
/// manifest chain itself. Called inside the fold, so the frees publish
/// with the same flip that seals the data into the base.
pub(crate) fn free_ingest(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    let manifest = decode_manifest(&meta::read_chain(db, root)?)?;
    let mut going = crate::props::Sweep::default();
    going.drop_all(manifest.segments.iter().map(|seg| &seg.meta));
    going.sweep(db)?;
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::fold::{checkpoint_fold, recover};
    use crate::graph::{Direction, GraphReader, bulk_load_as};
    use crate::props::{PropsReader, store_props};
    use crate::txn::Cell;

    struct Fixture {
        db: Zu1File,
        wal: Wal,
        mvcc: Mvcc,
        person: u32,
        knows: u32,
    }

    fn seeded(dir: &std::path::Path) -> Fixture {
        let mut db = Zu1File::create(&dir.join("ingest.zu1")).unwrap();
        bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (1, 2)]).unwrap();
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
        let wal = Wal::open(&dir.join("ingest.wal")).unwrap();
        let mvcc = Mvcc::new(0);
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
        reader.read_int(db, 0, row).unwrap()
    }

    fn read_name(db: &mut Zu1File, person: u32, row: u64) -> Vec<u8> {
        let dir = load_props(db, person).unwrap().unwrap();
        let mut reader = PropsReader::new(dir);
        let mut out = Vec::new();
        reader.read_str(db, 1, row, &mut out).unwrap();
        out
    }

    /// Ingested rows read like any committed append, the log holds a
    /// reference instead of the payload, and the fold seals them into
    /// the base file.
    #[test]
    fn ingest_round_trips_and_folds() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let names: Vec<&[u8]> = vec![b"eva", b"raj"];
        let epoch = ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[
                (0, PropValues::Int(&[50, 60])),
                (1, PropValues::Str(&names)),
            ],
        )
        .unwrap();
        assert_eq!(epoch, 1);
        assert_eq!(f.mvcc.appended_rows(f.person, epoch), 2);
        assert_eq!(
            f.mvcc.cell(f.person, 4, 4, 0, epoch),
            Some(Cell::Int(50)),
            "the overlay serves ingested cells"
        );
        assert_eq!(
            f.mvcc.cell(f.person, 4, 5, 1, epoch),
            Some(Cell::Str(b"raj".to_vec()))
        );
        assert!(
            f.wal.len() < 128,
            "the log holds a reference, not the payload: {} bytes",
            f.wal.len()
        );
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        assert_eq!(
            Catalog::load(&mut f.db)
                .unwrap()
                .node_by_id(f.person)
                .unwrap()
                .node_count,
            6
        );
        assert_eq!(read_age(&mut f.db, f.person, 4), 50);
        assert_eq!(read_name(&mut f.db, f.person, 5), b"raj");
        let path = dir.path().join("ingest.zu1");
        drop(f);
        crate::verify(&path).unwrap();
    }

    /// A cold reopen before any fold reads the sealed segments back
    /// through the WAL reference: the payload survives on the data
    /// file alone.
    #[test]
    fn reopen_recovers_ingested_rows_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let names: Vec<&[u8]> = vec![b"eva"];
        ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[(0, PropValues::Int(&[50])), (1, PropValues::Str(&names))],
        )
        .unwrap();
        ingest_edges(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.knows,
            &[4, 0],
            &[0, 4],
        )
        .unwrap();
        let (person, knows) = (f.person, f.knows);
        drop(f.db);
        drop(f.wal);
        let mut db = Zu1File::open(&dir.path().join("ingest.zu1")).unwrap();
        let mut wal = Wal::open(&dir.path().join("ingest.wal")).unwrap();
        let mut mvcc = recover(&mut db, &mut wal).unwrap();
        assert_eq!(mvcc.epoch(), 2);
        assert_eq!(mvcc.appended_rows(person, 2), 1);
        assert_eq!(mvcc.cell(person, 4, 4, 0, 2), Some(Cell::Int(50)));
        assert_eq!(
            mvcc.cell(person, 4, 4, 1, 2),
            Some(Cell::Str(b"eva".to_vec()))
        );
        let mut nbrs = Vec::new();
        mvcc.neighbors(knows, 4, false, 2, &mut nbrs);
        assert_eq!(nbrs, vec![0]);
        let mut nbrs = Vec::new();
        mvcc.neighbors(knows, 4, true, 2, &mut nbrs);
        assert_eq!(nbrs, vec![0]);
        checkpoint_fold(&mut db, &mut mvcc, &mut wal).unwrap();
        let mut g = GraphReader::load_table(&mut db, "knows").unwrap();
        assert_eq!(g.neighbors_dir(&mut db, 4, Direction::Fwd).unwrap(), &[0]);
        assert_eq!(g.neighbors_dir(&mut db, 0, Direction::Bwd).unwrap(), &[4]);
        drop(db);
        crate::verify(&dir.path().join("ingest.zu1")).unwrap();
    }

    /// The log cost of an ingest is independent of its size: ten rows
    /// and fifty thousand rows write the same frames.
    #[test]
    fn wal_bytes_do_not_scale_with_the_payload() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let small: Vec<u64> = (0..10).collect();
        let small_names: Vec<Vec<u8>> = (0..10).map(|i| format!("s{i}").into_bytes()).collect();
        let small_refs: Vec<&[u8]> = small_names.iter().map(|n| n.as_slice()).collect();
        ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[
                (0, PropValues::Int(&small)),
                (1, PropValues::Str(&small_refs)),
            ],
        )
        .unwrap();
        let after_small = f.wal.len();
        let big: Vec<u64> = (0..50_000).collect();
        let big_names: Vec<Vec<u8>> = (0..50_000).map(|i| format!("b{i}").into_bytes()).collect();
        let big_refs: Vec<&[u8]> = big_names.iter().map(|n| n.as_slice()).collect();
        ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[(0, PropValues::Int(&big)), (1, PropValues::Str(&big_refs))],
        )
        .unwrap();
        assert_eq!(
            f.wal.len(),
            after_small * 2,
            "same frames for 10 rows and 50k rows"
        );
        assert_eq!(f.mvcc.appended_rows(f.person, f.mvcc.epoch()), 50_010);
    }

    /// Ingest allocation never draws from the committed free list:
    /// after a crash that list is live again, so every ingested block
    /// must sit past the committed watermark.
    #[test]
    fn ingest_skips_the_committed_free_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        // Seed a committed free list: burn two blocks, free them, and
        // publish so they are genuinely allocatable.
        let a = f.db.allocate_block();
        let b = f.db.allocate_block();
        f.db.write_block(a, &vec![0xAA; crate::BLOCK_SIZE as usize])
            .unwrap();
        f.db.write_block(b, &vec![0xBB; crate::BLOCK_SIZE as usize])
            .unwrap();
        f.db.free_block(a).unwrap();
        f.db.free_block(b).unwrap();
        f.db.checkpoint().unwrap();
        let watermark = f.db.db_header().block_count;
        let values: Vec<u64> = (0..1000).collect();
        let names: Vec<Vec<u8>> = (0..1000).map(|i| format!("n{i}").into_bytes()).collect();
        let refs: Vec<&[u8]> = names.iter().map(|n| n.as_slice()).collect();
        ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[(0, PropValues::Int(&values)), (1, PropValues::Str(&refs))],
        )
        .unwrap();
        let root = f.mvcc.ingest_roots(f.mvcc.epoch())[0];
        let manifest = decode_manifest(&meta::read_chain(&mut f.db, root).unwrap()).unwrap();
        for seg in &manifest.segments {
            for &ptr in &seg.meta.blocks {
                assert!(ptr > watermark, "segment block {ptr} under {watermark}");
            }
        }
        for ptr in meta::chain_blocks(&mut f.db, root).unwrap() {
            assert!(ptr > watermark, "manifest block {ptr} under {watermark}");
        }
        // The held-aside free list is intact afterwards: draining the
        // allocator down to the watermark surfaces both seeded blocks.
        let mut recycled = Vec::new();
        loop {
            let ptr = f.db.allocate_block();
            if ptr > watermark {
                break;
            }
            recycled.push(ptr);
        }
        assert!(recycled.contains(&a) && recycled.contains(&b));
    }

    /// The fold frees the sealed ingest blocks in the same flip that
    /// publishes the folded data: afterwards they come back out of the
    /// allocator instead of leaking to VACUUM.
    #[test]
    fn fold_frees_the_ingest_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let names: Vec<&[u8]> = vec![b"eva", b"raj"];
        ingest_nodes(
            &mut f.db,
            &mut f.wal,
            &mut f.mvcc,
            f.person,
            &[
                (0, PropValues::Int(&[50, 60])),
                (1, PropValues::Str(&names)),
            ],
        )
        .unwrap();
        let root = f.mvcc.ingest_roots(f.mvcc.epoch())[0];
        let mut ingested: Vec<BlockPtr> = Vec::new();
        let manifest = decode_manifest(&meta::read_chain(&mut f.db, root).unwrap()).unwrap();
        for seg in &manifest.segments {
            ingested.extend(seg.meta.blocks.iter().copied());
        }
        ingested.extend(meta::chain_blocks(&mut f.db, root).unwrap());
        checkpoint_fold(&mut f.db, &mut f.mvcc, &mut f.wal).unwrap();
        assert!(f.mvcc.ingest_roots(f.mvcc.epoch()).is_empty());
        // Drain the allocator without letting it grow the file: every
        // ingested block must resurface.
        let watermark = f.db.db_header().block_count;
        let mut recycled = Vec::new();
        loop {
            let ptr = f.db.allocate_block();
            if ptr > watermark {
                break;
            }
            recycled.push(ptr);
        }
        for ptr in ingested {
            assert!(recycled.contains(&ptr), "block {ptr} leaked past the fold");
        }
    }

    /// Every malformed request is rejected before anything reaches the
    /// file or the log.
    #[test]
    fn invalid_ingests_are_rejected_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let len_before = std::fs::metadata(dir.path().join("ingest.zu1"))
            .unwrap()
            .len();
        let names: Vec<&[u8]> = vec![b"eva"];
        let cases: Vec<(&str, Result<Epoch>)> = vec![
            (
                "empty",
                ingest_nodes(&mut f.db, &mut f.wal, &mut f.mvcc, f.person, &[]),
            ),
            (
                "ragged",
                ingest_nodes(
                    &mut f.db,
                    &mut f.wal,
                    &mut f.mvcc,
                    f.person,
                    &[(0, PropValues::Int(&[1, 2])), (1, PropValues::Str(&names))],
                ),
            ),
            (
                "unknown table",
                ingest_nodes(
                    &mut f.db,
                    &mut f.wal,
                    &mut f.mvcc,
                    999,
                    &[(0, PropValues::Int(&[1]))],
                ),
            ),
            (
                "missing column",
                ingest_nodes(
                    &mut f.db,
                    &mut f.wal,
                    &mut f.mvcc,
                    f.person,
                    &[(0, PropValues::Int(&[1]))],
                ),
            ),
            (
                "type mismatch",
                ingest_nodes(
                    &mut f.db,
                    &mut f.wal,
                    &mut f.mvcc,
                    f.person,
                    &[(0, PropValues::Str(&names)), (1, PropValues::Str(&names))],
                ),
            ),
            (
                "duplicate column",
                ingest_nodes(
                    &mut f.db,
                    &mut f.wal,
                    &mut f.mvcc,
                    f.person,
                    &[(0, PropValues::Int(&[1])), (0, PropValues::Int(&[2]))],
                ),
            ),
            (
                "unknown rel",
                ingest_edges(&mut f.db, &mut f.wal, &mut f.mvcc, 999, &[0], &[1]),
            ),
            (
                "no edges",
                ingest_edges(&mut f.db, &mut f.wal, &mut f.mvcc, f.knows, &[], &[]),
            ),
            (
                "ragged edges",
                ingest_edges(&mut f.db, &mut f.wal, &mut f.mvcc, f.knows, &[0, 1], &[1]),
            ),
        ];
        for (what, result) in cases {
            assert!(result.is_err(), "{what} must be rejected");
        }
        assert!(f.wal.is_empty(), "nothing reached the log");
        assert_eq!(f.mvcc.epoch(), 0);
        assert_eq!(
            std::fs::metadata(dir.path().join("ingest.zu1"))
                .unwrap()
                .len(),
            len_before,
            "nothing reached the file"
        );
    }

    /// The manifest codec rejects hostile payloads instead of
    /// misreading them.
    #[test]
    fn manifest_codec_rejects_bad_payloads() {
        let meta = SegmentMeta {
            value_count: 2,
            payload_len: 16,
            uncompressed_bytes: 16,
            min: 1,
            max: 2,
            crc: 0,
            structural: crate::segment::Structural::MiniBlock,
            sorted: false,
            start: 0,
            blocks: vec![7],
        };
        let good = encode_manifest(
            3,
            KIND_NODES,
            2,
            &[ManifestSegment {
                col: 0,
                ty: SealKind::Lane,
                meta,
            }],
        );
        let decoded = decode_manifest(&good).unwrap();
        assert_eq!(decoded.table, 3);
        assert_eq!(decoded.rows, 2);
        assert_eq!(decoded.segments.len(), 1);
        for len in 0..good.len() {
            assert!(decode_manifest(&good[..len]).is_err(), "prefix {len}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_manifest(&trailing).is_err());
        let mut bad_version = good.clone();
        bad_version[0] = 99;
        assert!(decode_manifest(&bad_version).is_err());
        let mut bad_kind = good.clone();
        bad_kind[2] = 7;
        assert!(decode_manifest(&bad_kind).is_err());
        let mut bad_rows = good;
        bad_rows[7] = 9;
        assert!(decode_manifest(&bad_rows).is_err(), "count mismatch");
    }

    /// Not a gate, a manual probe for the T4 ingest target. Run with
    /// `cargo test -q -p zu-zu1 --release ingest_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual throughput probe"]
    fn ingest_throughput_probe() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = seeded(dir.path());
        let n = 10_000_000u64;
        let src: Vec<u64> = (0..n).map(|i| i % 4).collect();
        let dst: Vec<u64> = (0..n).map(|i| (i + 1) % 4).collect();
        let start = std::time::Instant::now();
        ingest_edges(&mut f.db, &mut f.wal, &mut f.mvcc, f.knows, &src, &dst).unwrap();
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{n} edges in {secs:.3}s = {:.2}M edges/s single-threaded, wal holds {} bytes",
            n as f64 / secs / 1e6,
            f.wal.len()
        );
    }
}
