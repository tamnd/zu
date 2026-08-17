//! Redo-only write-ahead log, the sidecar file next to a zu1 database.
//!
//! The record format is the engine-shared one from docs/08 section 3:
//! `len: u32 LE | crc32c: u32 LE | body`, where the body is
//! `epoch: u64 LE | kind: u8 | payload` and both length and crc cover
//! the body. The s3 engine batches the same records into objects when
//! M5 lands; the sqlite engine delegates to SQLite's own WAL and never
//! touches this file.
//!
//! Replay stops at the first frame that does not verify: a short
//! prefix, a length past the end of the file, or a crc mismatch all
//! mean the same thing, a torn tail from a crash mid-append, and
//! everything before the tear is intact because the single writer
//! appends frames in order. A frame whose crc verifies but whose
//! payload does not parse is different: that is corruption or version
//! skew, not a tear, and it surfaces as an error instead of a silent
//! stop. Delivery is transactional: records buffer per txn and reach
//! the sink only once their `TxnCommit` frame has been read, so a tear
//! after the last commit loses nothing that was promised durable.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use zu_common::{Epoch, Result, ZuError};

use crate::vfs::{RealFile, VfsFile};

/// Frame prefix: `len: u32 | crc32c: u32`.
const PREFIX: u64 = 8;
/// The one kind the replay floor pass matches before full decode.
const KIND_CHECKPOINT_NOTE: u8 = 9;
/// Body floor: `epoch: u64 | kind: u8` with an empty payload.
const MIN_BODY: u64 = 9;

fn corrupt(detail: String) -> ZuError {
    ZuError::Corrupt {
        what: "wal record",
        detail,
    }
}

/// Row-ordered values for one logged column, mirroring the two
/// property types the props store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalValues {
    Int(Vec<u64>),
    Str(Vec<Vec<u8>>),
    /// This many absences in a row, which is what a `REMOVE` writes.
    /// An absence carries nothing per row, so the count is the whole
    /// record: what a reader needs to know is which rows the record
    /// names, and the offsets beside it already say.
    Null(u32),
}

impl WalValues {
    pub fn len(&self) -> usize {
        match self {
            WalValues::Int(v) => v.len(),
            WalValues::Str(v) => v.len(),
            WalValues::Null(n) => *n as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            WalValues::Int(v) => {
                out.push(0);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for x in v {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            WalValues::Str(v) => {
                out.push(1);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for s in v {
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    out.extend_from_slice(s);
                }
            }
            WalValues::Null(n) => {
                out.push(2);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }

    fn decode(r: &mut Reader) -> Result<Self> {
        let tag = r.u8()?;
        let count = r.u32()? as usize;
        match tag {
            0 => {
                let mut v = Vec::with_capacity(count.min(r.remaining() / 8));
                for _ in 0..count {
                    v.push(r.u64()?);
                }
                Ok(WalValues::Int(v))
            }
            1 => {
                let mut v = Vec::with_capacity(count.min(r.remaining() / 4));
                for _ in 0..count {
                    let len = r.u32()? as usize;
                    v.push(r.bytes(len)?.to_vec());
                }
                Ok(WalValues::Str(v))
            }
            2 => Ok(WalValues::Null(count as u32)),
            other => Err(corrupt(format!("unknown value tag {other}"))),
        }
    }
}

/// One logged column of a node insert: the column's position in the
/// table's props directory and its row-ordered values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalColumn {
    pub col: u32,
    pub values: WalValues,
}

/// One logical redo record. The epoch travels in the frame header, not
/// here: every record of a txn carries that txn's commit epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    TxnBegin,
    NodeInsert {
        table: u32,
        cols: Vec<WalColumn>,
    },
    RelInsert {
        rel: u32,
        src: Vec<u64>,
        dst: Vec<u64>,
        /// What the edges carry, one entry per property column the rel
        /// table stores and one value per edge in the same order as
        /// `src` and `dst`. Empty for a table that stores nothing on
        /// its edges, which is every table until a statement writes
        /// one that does.
        cols: Vec<WalColumn>,
    },
    Update {
        table: u32,
        group: u64,
        col: u32,
        offsets: Vec<u64>,
        values: WalValues,
    },
    Delete {
        table: u32,
        ids: Vec<u64>,
    },
    DdlCatalog {
        delta: Vec<u8>,
    },
    TxnCommit,
    IngestRef {
        table: u32,
        ptrs: Vec<u64>,
    },
    CheckpointNote,
}

impl WalRecord {
    fn kind(&self) -> u8 {
        match self {
            WalRecord::TxnBegin => 1,
            WalRecord::NodeInsert { .. } => 2,
            WalRecord::RelInsert { .. } => 3,
            WalRecord::Update { .. } => 4,
            WalRecord::Delete { .. } => 5,
            WalRecord::DdlCatalog { .. } => 6,
            WalRecord::TxnCommit => 7,
            WalRecord::IngestRef { .. } => 8,
            WalRecord::CheckpointNote => KIND_CHECKPOINT_NOTE,
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        let put_u64s = |out: &mut Vec<u8>, v: &[u64]| {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        };
        match self {
            WalRecord::TxnBegin | WalRecord::TxnCommit | WalRecord::CheckpointNote => {}
            WalRecord::NodeInsert { table, cols } => {
                out.extend_from_slice(&table.to_le_bytes());
                out.extend_from_slice(&(cols.len() as u32).to_le_bytes());
                for c in cols {
                    out.extend_from_slice(&c.col.to_le_bytes());
                    c.values.encode(out);
                }
            }
            WalRecord::RelInsert {
                rel,
                src,
                dst,
                cols,
            } => {
                out.extend_from_slice(&rel.to_le_bytes());
                out.extend_from_slice(&(src.len() as u32).to_le_bytes());
                put_u64s(out, src);
                put_u64s(out, dst);
                out.extend_from_slice(&(cols.len() as u32).to_le_bytes());
                for c in cols {
                    out.extend_from_slice(&c.col.to_le_bytes());
                    c.values.encode(out);
                }
            }
            WalRecord::Update {
                table,
                group,
                col,
                offsets,
                values,
            } => {
                out.extend_from_slice(&table.to_le_bytes());
                out.extend_from_slice(&group.to_le_bytes());
                out.extend_from_slice(&col.to_le_bytes());
                out.extend_from_slice(&(offsets.len() as u32).to_le_bytes());
                put_u64s(out, offsets);
                values.encode(out);
            }
            WalRecord::Delete { table, ids } => {
                out.extend_from_slice(&table.to_le_bytes());
                out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
                put_u64s(out, ids);
            }
            WalRecord::DdlCatalog { delta } => out.extend_from_slice(delta),
            WalRecord::IngestRef { table, ptrs } => {
                out.extend_from_slice(&table.to_le_bytes());
                out.extend_from_slice(&(ptrs.len() as u32).to_le_bytes());
                put_u64s(out, ptrs);
            }
        }
    }

    fn decode(kind: u8, r: &mut Reader) -> Result<Self> {
        let u64s = |r: &mut Reader, count: usize| -> Result<Vec<u64>> {
            let mut v = Vec::with_capacity(count.min(r.remaining() / 8));
            for _ in 0..count {
                v.push(r.u64()?);
            }
            Ok(v)
        };
        let rec = match kind {
            1 => WalRecord::TxnBegin,
            2 => {
                let table = r.u32()?;
                let col_count = r.u32()? as usize;
                let mut cols = Vec::with_capacity(col_count.min(r.remaining() / 9));
                for _ in 0..col_count {
                    let col = r.u32()?;
                    let values = WalValues::decode(r)?;
                    cols.push(WalColumn { col, values });
                }
                WalRecord::NodeInsert { table, cols }
            }
            3 => {
                let rel = r.u32()?;
                let count = r.u32()? as usize;
                let src = u64s(r, count)?;
                let dst = u64s(r, count)?;
                let col_count = r.u32()? as usize;
                let mut cols = Vec::with_capacity(col_count.min(r.remaining() / 9));
                for _ in 0..col_count {
                    let col = r.u32()?;
                    let values = WalValues::decode(r)?;
                    if values.len() != count {
                        return Err(corrupt(format!(
                            "rel insert carries {count} edges but {} values for column {col}",
                            values.len()
                        )));
                    }
                    cols.push(WalColumn { col, values });
                }
                WalRecord::RelInsert {
                    rel,
                    src,
                    dst,
                    cols,
                }
            }
            4 => {
                let table = r.u32()?;
                let group = r.u64()?;
                let col = r.u32()?;
                let count = r.u32()? as usize;
                let offsets = u64s(r, count)?;
                let values = WalValues::decode(r)?;
                if values.len() != offsets.len() {
                    return Err(corrupt(format!(
                        "update carries {} offsets but {} values",
                        offsets.len(),
                        values.len()
                    )));
                }
                WalRecord::Update {
                    table,
                    group,
                    col,
                    offsets,
                    values,
                }
            }
            5 => {
                let table = r.u32()?;
                let count = r.u32()? as usize;
                WalRecord::Delete {
                    table,
                    ids: u64s(r, count)?,
                }
            }
            6 => WalRecord::DdlCatalog {
                delta: r.rest().to_vec(),
            },
            7 => WalRecord::TxnCommit,
            8 => {
                let table = r.u32()?;
                let count = r.u32()? as usize;
                WalRecord::IngestRef {
                    table,
                    ptrs: u64s(r, count)?,
                }
            }
            9 => WalRecord::CheckpointNote,
            other => return Err(corrupt(format!("unknown record kind {other}"))),
        };
        if !r.rest().is_empty() {
            return Err(corrupt(format!(
                "{} trailing payload bytes after kind {kind}",
                r.remaining()
            )));
        }
        Ok(rec)
    }
}

/// Bounds-checked little-endian reader over one frame body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.remaining() {
            return Err(corrupt(format!(
                "payload wants {len} bytes with {} left",
                self.remaining()
            )));
        }
        let out = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    fn rest(&mut self) -> &'a [u8] {
        let out = &self.buf[self.pos..];
        self.pos = self.buf.len();
        out
    }
}

/// The open sidecar log: an append position and the frame codec around
/// it. Commit durability is one `fdatasync` per commit; the 1 ms
/// group-commit window from docs/08 arrives with the writer queue.
pub struct Wal {
    file: Box<dyn VfsFile>,
    path: PathBuf,
    len: u64,
}

impl Wal {
    /// Opens the log at `path`, creating it when missing, and truncates
    /// the torn tail so the next append starts at the last intact
    /// frame. Crashing mid-append leaves a frame that fails its length
    /// or crc check; everything before it is kept.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_on(Box::new(RealFile::open_or_create(path)?), path)
    }

    /// [`Self::open`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn open_on(mut file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
        let mut bytes = vec![0u8; file.len()? as usize];
        file.read_exact_at(&mut bytes, 0)?;
        let mut end = 0u64;
        while let Some((body, next)) = next_frame(&bytes, end) {
            let _ = body;
            end = next;
        }
        if end < bytes.len() as u64 {
            file.set_len(end)?;
            file.sync_data()?;
        }
        Ok(Wal {
            file,
            path: path.to_path_buf(),
            len: end,
        })
    }

    /// Bytes of intact frames, the input to the checkpoint trigger.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends one frame without syncing. Durability comes from
    /// [`Wal::commit`]; a crash before it tears the tail and replay
    /// drops the whole uncommitted txn.
    pub fn append(&mut self, epoch: Epoch, rec: &WalRecord) -> Result<()> {
        let mut body = Vec::with_capacity(64);
        body.extend_from_slice(&epoch.to_le_bytes());
        body.push(rec.kind());
        rec.encode_payload(&mut body);
        let mut frame = Vec::with_capacity(PREFIX as usize + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32c::crc32c(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        self.file.write_all_at(&frame, self.len)?;
        self.len += frame.len() as u64;
        Ok(())
    }

    /// Appends the txn's `TxnCommit` frame and syncs the file. The
    /// record hitting disk is the commit point: replay delivers a txn
    /// exactly when this frame verifies.
    pub fn commit(&mut self, epoch: Epoch) -> Result<()> {
        self.append(epoch, &WalRecord::TxnCommit)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Empties the log after a checkpoint has folded and published
    /// everything it held.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.sync_data()?;
        self.len = 0;
        Ok(())
    }

    /// Replays committed transactions in log order through `sink`,
    /// which sees every frame of a txn including its `TxnBegin` and
    /// `TxnCommit`. Records with epoch at or below `floor` are already
    /// folded into the base file and skip. A `CheckpointNote` raises
    /// the floor the same way and is not delivered; the notes are
    /// gathered in a first pass so a note folds the txns before it in
    /// the log, which is where the txns it folded sit. Reads go
    /// through a second descriptor so an open writer keeps its append
    /// position.
    pub fn replay(
        &self,
        floor: Epoch,
        mut sink: impl FnMut(Epoch, &WalRecord) -> Result<()>,
    ) -> Result<()> {
        let mut file = File::open(&self.path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        bytes.truncate(self.len as usize);
        let mut floor = floor;
        let mut at = 0u64;
        while let Some((body, next)) = next_frame(&bytes, at) {
            at = next;
            if body[8] == KIND_CHECKPOINT_NOTE {
                let epoch = u64::from_le_bytes(body[..8].try_into().unwrap());
                floor = floor.max(epoch);
            }
        }
        let mut txn: Vec<(Epoch, WalRecord)> = Vec::new();
        let mut at = 0u64;
        while let Some((body, next)) = next_frame(&bytes, at) {
            at = next;
            let epoch = u64::from_le_bytes(body[..8].try_into().unwrap());
            let kind = body[8];
            let mut r = Reader {
                buf: &body[9..],
                pos: 0,
            };
            let rec = WalRecord::decode(kind, &mut r)?;
            match rec {
                WalRecord::CheckpointNote => {}
                WalRecord::TxnBegin => {
                    // A fresh begin abandons an unfinished txn whose
                    // frames were intact but never committed: the
                    // writer died mid-txn and a later writer moved on.
                    txn.clear();
                    txn.push((epoch, WalRecord::TxnBegin));
                }
                WalRecord::TxnCommit => {
                    if epoch > floor {
                        for (e, rec) in &txn {
                            sink(*e, rec)?;
                        }
                        sink(epoch, &WalRecord::TxnCommit)?;
                    }
                    txn.clear();
                }
                rec => txn.push((epoch, rec)),
            }
        }
        Ok(())
    }
}

/// Reads the frame starting at `at`, returning its body and the next
/// frame's offset, or `None` at the torn tail: a short prefix, a body
/// shorter than the smallest record, a length past the end of the
/// buffer, or a crc mismatch.
fn next_frame(bytes: &[u8], at: u64) -> Option<(&[u8], u64)> {
    let rest = &bytes[at.min(bytes.len() as u64) as usize..];
    if (rest.len() as u64) < PREFIX {
        return None;
    }
    let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as u64;
    let stored = u32::from_le_bytes(rest[4..8].try_into().unwrap());
    if len < MIN_BODY || PREFIX + len > rest.len() as u64 {
        return None;
    }
    let body = &rest[PREFIX as usize..(PREFIX + len) as usize];
    if crc32c::crc32c(body) != stored {
        return None;
    }
    Some((body, at + PREFIX + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_records() -> Vec<WalRecord> {
        vec![
            WalRecord::TxnBegin,
            WalRecord::NodeInsert {
                table: 3,
                cols: vec![
                    WalColumn {
                        col: 0,
                        values: WalValues::Int(vec![10, 20, 30]),
                    },
                    WalColumn {
                        col: 1,
                        values: WalValues::Str(vec![b"ada".to_vec(), vec![], b"grace".to_vec()]),
                    },
                ],
            },
            WalRecord::RelInsert {
                rel: 7,
                src: vec![1, 2, 3],
                dst: vec![4, 5, 6],
                cols: Vec::new(),
            },
            WalRecord::RelInsert {
                rel: 8,
                src: vec![1, 2],
                dst: vec![3, 4],
                cols: vec![
                    WalColumn {
                        col: 0,
                        values: WalValues::Int(vec![2020, 2021]),
                    },
                    WalColumn {
                        col: 1,
                        values: WalValues::Str(vec![b"since".to_vec(), vec![]]),
                    },
                ],
            },
            WalRecord::Update {
                table: 3,
                group: 0,
                col: 1,
                offsets: vec![2, 9],
                values: WalValues::Str(vec![b"kay".to_vec(), b"barbara".to_vec()]),
            },
            WalRecord::Delete {
                table: 3,
                ids: vec![11, 12],
            },
            WalRecord::DdlCatalog {
                delta: vec![0xAB; 100],
            },
            WalRecord::IngestRef {
                table: 4,
                ptrs: vec![256 * 1024, 512 * 1024],
            },
        ]
    }

    fn collect(wal: &Wal, floor: Epoch) -> Vec<(Epoch, WalRecord)> {
        let mut out = Vec::new();
        wal.replay(floor, |e, r| {
            out.push((e, r.clone()));
            Ok(())
        })
        .unwrap();
        out
    }

    /// One committed txn holding every payload kind survives the trip
    /// through disk byte for byte.
    #[test]
    fn every_kind_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        let recs = sample_records();
        {
            let mut wal = Wal::open(&path).unwrap();
            for rec in &recs {
                wal.append(5, rec).unwrap();
            }
            wal.commit(5).unwrap();
        }
        let wal = Wal::open(&path).unwrap();
        let got = collect(&wal, 0);
        assert_eq!(got.len(), recs.len() + 1);
        for (i, rec) in recs.iter().enumerate() {
            assert_eq!(got[i], (5, rec.clone()));
        }
        assert_eq!(got[recs.len()], (5, WalRecord::TxnCommit));
    }

    /// Truncating the log at every byte length yields a prefix of the
    /// committed txns and never an error: any tear is either before a
    /// commit frame, dropping that txn whole, or after it, keeping it
    /// whole.
    #[test]
    fn every_truncation_point_yields_committed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        let mut wal = Wal::open(&path).unwrap();
        for epoch in 1..=4u64 {
            wal.append(epoch, &WalRecord::TxnBegin).unwrap();
            wal.append(
                epoch,
                &WalRecord::Delete {
                    table: 1,
                    ids: vec![epoch],
                },
            )
            .unwrap();
            wal.commit(epoch).unwrap();
        }
        let full = std::fs::read(&path).unwrap();
        let complete = collect(&wal, 0);
        drop(wal);
        for cut in 0..=full.len() {
            let torn = dir.path().join("torn.wal");
            std::fs::write(&torn, &full[..cut]).unwrap();
            let wal = Wal::open(&torn).unwrap();
            assert!(wal.len() <= cut as u64, "cut {cut} kept torn bytes");
            let got = collect(&wal, 0);
            assert!(
                complete.starts_with(&got),
                "cut {cut} delivered a non-prefix"
            );
            let txns: Vec<Epoch> = got
                .iter()
                .filter(|(_, r)| *r == WalRecord::TxnCommit)
                .map(|(e, _)| *e)
                .collect();
            assert_eq!(
                got.len(),
                txns.len() * 3,
                "cut {cut} delivered a partial txn"
            );
        }
    }

    /// Flipping any single byte of the log delivers a prefix of the
    /// original txns: the scan stops at the damaged frame, or at a
    /// frame the damaged length field misaligns, and panics never.
    #[test]
    fn every_single_byte_corruption_yields_committed_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        let mut wal = Wal::open(&path).unwrap();
        for epoch in 1..=3u64 {
            wal.append(epoch, &WalRecord::TxnBegin).unwrap();
            wal.append(
                epoch,
                &WalRecord::Update {
                    table: 2,
                    group: 0,
                    col: 0,
                    offsets: vec![1],
                    values: WalValues::Int(vec![epoch]),
                },
            )
            .unwrap();
            wal.commit(epoch).unwrap();
        }
        let full = std::fs::read(&path).unwrap();
        let complete = collect(&wal, 0);
        drop(wal);
        for hit in 0..full.len() {
            let mut damaged = full.clone();
            damaged[hit] ^= 0xFF;
            let flipped = dir.path().join("flipped.wal");
            std::fs::write(&flipped, &damaged).unwrap();
            let wal = Wal::open(&flipped).unwrap();
            let mut got = Vec::new();
            let res = wal.replay(0, |e, r| {
                got.push((e, r.clone()));
                Ok(())
            });
            // A flipped length field can frame a stale region whose
            // bytes still crc, which surfaces as a decode error; what
            // never happens is delivering a record that was not written.
            if res.is_ok() {
                assert!(
                    complete.starts_with(&got),
                    "byte {hit} delivered a non-prefix"
                );
            }
        }
    }

    /// The checkpoint floor skips folded txns, whether it arrives as
    /// the replay argument or as a `CheckpointNote` in the log.
    #[test]
    fn floor_and_checkpoint_note_skip_folded_txns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        let mut wal = Wal::open(&path).unwrap();
        for epoch in 1..=4u64 {
            wal.append(epoch, &WalRecord::TxnBegin).unwrap();
            wal.commit(epoch).unwrap();
        }
        let from_floor = collect(&wal, 2);
        let epochs: Vec<Epoch> = from_floor.iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![3, 3, 4, 4]);
        wal.append(4, &WalRecord::CheckpointNote).unwrap();
        wal.append(5, &WalRecord::TxnBegin).unwrap();
        wal.commit(5).unwrap();
        let after_note = collect(&wal, 0);
        let epochs: Vec<Epoch> = after_note.iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![5, 5], "the note folds everything before it");
    }

    /// An uncommitted trailing txn is invisible to replay even though
    /// its frames are intact, and a torn tail behind it disappears on
    /// open so the next append writes over garbage, not after it.
    #[test]
    fn uncommitted_tail_is_invisible_and_append_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(1, &WalRecord::TxnBegin).unwrap();
            wal.commit(1).unwrap();
            wal.append(2, &WalRecord::TxnBegin).unwrap();
            wal.append(
                2,
                &WalRecord::Delete {
                    table: 1,
                    ids: vec![9],
                },
            )
            .unwrap();
            // No commit: epoch 2 must not replay.
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0x77; 5]);
        std::fs::write(&path, &bytes).unwrap();
        let mut wal = Wal::open(&path).unwrap();
        assert_eq!(wal.len() as usize, bytes.len() - 5, "tail gone on open");
        let epochs: Vec<Epoch> = collect(&wal, 0).iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![1, 1], "uncommitted epoch 2 stays invisible");
        wal.append(3, &WalRecord::TxnBegin).unwrap();
        wal.commit(3).unwrap();
        let epochs: Vec<Epoch> = collect(&wal, 0).iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![1, 1, 3, 3]);
    }

    /// Truncation resets the log for the next txn.
    #[test]
    fn truncate_empties_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.wal");
        let mut wal = Wal::open(&path).unwrap();
        wal.append(1, &WalRecord::TxnBegin).unwrap();
        wal.commit(1).unwrap();
        wal.truncate().unwrap();
        assert!(wal.is_empty());
        assert!(collect(&wal, 0).is_empty());
        wal.append(2, &WalRecord::TxnBegin).unwrap();
        wal.commit(2).unwrap();
        let epochs: Vec<Epoch> = collect(&wal, 0).iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![2, 2]);
    }
}
