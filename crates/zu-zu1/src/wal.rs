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

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zu_common::{Epoch, Result, ZuError};

use crate::vfs::{RealVfs, Vfs, VfsFile};

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
    /// The edges one txn took away, named by the rows they run between
    /// because that is the only name an edge has: the file holds them
    /// in a CSR that a fold rebuilds, so there is no offset to log.
    RelDelete {
        rel: u32,
        src: Vec<u64>,
        dst: Vec<u64>,
    },
    /// What one txn changed on edges that were already there, named the
    /// way a removed edge is: by the rows the edge runs between. One
    /// record carries one column, so the values run beside the pairs the
    /// way a node update's run beside its offsets.
    RelUpdate {
        rel: u32,
        col: u32,
        src: Vec<u64>,
        dst: Vec<u64>,
        values: WalValues,
    },
    /// Which labels one txn put on rows and which it took off them. The
    /// two masks are a bit per label of the graph's dictionary, and the
    /// rows named share them, so one record is one shape of change over
    /// however many rows were changed that way. A label is not a column,
    /// so there is no column here and no value: the row's label word is
    /// one word, and what a change to it says is which bits go on and
    /// which come off.
    LabelUpdate {
        table: u32,
        offsets: Vec<u64>,
        add: u64,
        remove: u64,
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
            WalRecord::RelDelete { .. } => 10,
            WalRecord::RelUpdate { .. } => 11,
            WalRecord::LabelUpdate { .. } => 12,
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
            WalRecord::RelDelete { rel, src, dst } => {
                out.extend_from_slice(&rel.to_le_bytes());
                out.extend_from_slice(&(src.len() as u32).to_le_bytes());
                put_u64s(out, src);
                put_u64s(out, dst);
            }
            WalRecord::RelUpdate {
                rel,
                col,
                src,
                dst,
                values,
            } => {
                out.extend_from_slice(&rel.to_le_bytes());
                out.extend_from_slice(&col.to_le_bytes());
                out.extend_from_slice(&(src.len() as u32).to_le_bytes());
                put_u64s(out, src);
                put_u64s(out, dst);
                values.encode(out);
            }
            WalRecord::LabelUpdate {
                table,
                offsets,
                add,
                remove,
            } => {
                out.extend_from_slice(&table.to_le_bytes());
                out.extend_from_slice(&add.to_le_bytes());
                out.extend_from_slice(&remove.to_le_bytes());
                out.extend_from_slice(&(offsets.len() as u32).to_le_bytes());
                put_u64s(out, offsets);
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
            10 => {
                let rel = r.u32()?;
                let count = r.u32()? as usize;
                WalRecord::RelDelete {
                    rel,
                    src: u64s(r, count)?,
                    dst: u64s(r, count)?,
                }
            }
            11 => {
                let rel = r.u32()?;
                let col = r.u32()?;
                let count = r.u32()? as usize;
                let src = u64s(r, count)?;
                let dst = u64s(r, count)?;
                let values = WalValues::decode(r)?;
                if values.len() != count {
                    return Err(corrupt(format!(
                        "rel update names {count} edges but carries {} values",
                        values.len()
                    )));
                }
                WalRecord::RelUpdate {
                    rel,
                    col,
                    src,
                    dst,
                    values,
                }
            }
            12 => {
                let table = r.u32()?;
                let add = r.u64()?;
                let remove = r.u64()?;
                let count = r.u32()? as usize;
                if add & remove != 0 {
                    return Err(corrupt(format!(
                        "label update both sets and clears {:#x}",
                        add & remove
                    )));
                }
                WalRecord::LabelUpdate {
                    table,
                    offsets: u64s(r, count)?,
                    add,
                    remove,
                }
            }
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

/// Syncs issued and commits that asked for one, since the process
/// started.
///
/// The second divided by the first is commits a flush, which is the
/// only statement about group commit that survives being measured on a
/// busy machine or a slow disk: it counts syscalls, not microseconds.
/// One writer reads one however fast the drive is, and eight writers
/// that share properly read near eight however slow it is. A latency
/// ratio answers the same question in units of whatever the drive was
/// doing that minute.
///
/// A commit counts whether it waited or not. One that found its bytes
/// already on the platter is the case the counter exists to see: it got
/// its promise out of somebody else's flush, which is what sharing
/// means.
///
/// Per process rather than per log, because a log object does not
/// outlive the burst it is counting: a rollback drops the writer and a
/// caller that wants the raw file folds it away, and either one puts a
/// fresh [`Commits`] behind the same database with its counters back at
/// zero. A pair of deltas around a measured window is what these are
/// for, and one process writes one store at a time in the bench that
/// takes them.
static SYNCS: AtomicU64 = AtomicU64::new(0);
static COMMITS: AtomicU64 = AtomicU64::new(0);

/// The two counters above, as they stand.
pub fn commit_counters() -> (u64, u64) {
    (
        SYNCS.load(Ordering::Relaxed),
        COMMITS.load(Ordering::Relaxed),
    )
}

/// The open sidecar log: an append position, a staging buffer and the
/// frame codec around them. Commit durability is one `fdatasync` per
/// commit; the 1 ms group-commit window from docs/08 arrives with the
/// writer queue.
///
/// A transaction's frames are built in memory and go to the file in one
/// write at commit, because a small write is nearly all syscall: a one
/// cell update is a begin frame, an update frame and a commit frame,
/// three pwrites of about thirty bytes each, and on this laptop those
/// three cost more processor time than everything the statement did
/// before them. Nothing about durability changes, since the commit
/// frame is still what reaches the disk last and the sync still follows
/// it.
///
/// The room those frames go into is asked for ahead of them. A write
/// that makes a file longer is a write plus an allocation plus a size
/// the sync has to carry, and a commit that does that every time pays
/// for it every time: measured on this laptop, a hundred byte frame
/// and its sync cost 34 us of processor time and 3.75 ms of wall
/// appending, against 25 us and 3.17 ms into space the file already
/// had. So the log takes its space in chunks and writes zeros over it,
/// and a commit writes inside what it already has.
///
/// Zeros are not padding, they are the end of the log. A frame is a
/// length and a crc, and a length of zero is shorter than the smallest
/// record, so the scan that replay and open both use stops at the
/// first byte of unused reservation. That is the same stop a torn tail
/// gets and it is why nothing else is needed to say where the log
/// ends. It is also why every path that takes frames away, the cut
/// after a checkpoint and the rollback of a transaction the process
/// died inside, writes zeros over what it took rather than leaving
/// bytes that once parsed.
/// The sync a commit waits on, shared by every writer on one log.
///
/// A commit is durable once the log is on the platter through the byte
/// its commit frame ends at. The log is append only, so that is a
/// prefix property: one sync makes durable everything staged before it,
/// whoever staged it. Which is what lets commits share one.
///
/// A writer that finds nobody syncing becomes the leader, and syncs the
/// log as far as it has been staged rather than only as far as its own
/// frames go, so the writers that staged behind it while it waited for
/// the write side come along for free. A writer that finds a sync
/// already running waits for it, and either it covered them or they
/// lead the next one. So a burst of n commits costs one sync rather
/// than n, and the leader pauses before issuing it so that the writers
/// who are still staging when it takes the lead are in that one too.
///
/// The sync runs on a second handle on the log, because by then the
/// writer that staged the frames has given the write side back and the
/// log belongs to somebody else.
#[derive(Debug)]
pub struct Commits {
    /// The handle the sync is issued on. Only the leader takes this,
    /// and `Marks::running` is what stops there being two.
    file: Mutex<Box<dyn VfsFile>>,
    marks: Mutex<Marks>,
    /// Woken by every sync that finishes, leader or not.
    done: Condvar,
}

/// How far the log is written and how far it is durable.
#[derive(Debug, Default)]
struct Marks {
    /// Bytes handed to the file, whether or not they are on the platter.
    staged: u64,
    /// Bytes that are on the platter. Never above `staged`.
    durable: u64,
    /// A sync is running and will make this many bytes durable when it
    /// lands. `None` when nobody is syncing.
    running: Option<u64>,
    /// Set when a sync failed, so the writers waiting on it are told
    /// rather than left believing a sync happened. Cleared by the next
    /// sync that succeeds.
    failed: bool,
    /// Commits inside [`Commits::sync_through`] that the log still owes
    /// something to, the leader among them.
    waiting: u64,
    /// How many of those the last flush found, which is this log's
    /// answer to whether anything is going on: one writer on its own
    /// reads 1 every time.
    group: u64,
    /// What a sync on this file costs, in nanoseconds, smoothed over
    /// the ones this process has issued. Zero until it has issued one.
    cost: u64,
}

/// The share of a sync period a leader will spend gathering writers
/// that have not staged yet, and the most it will spend whatever the
/// share comes to.
///
/// The window is a fraction of the flush it saves, so it is right on a
/// drive that syncs in a hundred microseconds and on one that takes
/// fifty milliseconds, and the cap is there because the second of those
/// should not put five milliseconds in front of a commit however much
/// it might save. An eighth of three milliseconds is 375 us, against
/// the staging it is waiting for, which is tens of microseconds.
const GATHER_SHARE: u64 = 8;
const GATHER_CAP: Duration = Duration::from_micros(500);

impl Commits {
    fn new(file: Box<dyn VfsFile>, len: u64) -> Self {
        Self {
            file: Mutex::new(file),
            marks: Mutex::new(Marks {
                staged: len,
                // Whatever was in the log before this process opened it
                // is the last process's problem and already on disk.
                durable: len,
                running: None,
                failed: false,
                waiting: 0,
                group: 0,
                cost: 0,
            }),
            done: Condvar::new(),
        }
    }

    /// Says the log has been written out to `len` bytes.
    fn staged(&self, len: u64) {
        let mut marks = self.marks.lock().expect("wal marks");
        marks.staged = marks.staged.max(len);
    }

    /// Says the log is `len` bytes long, and whether the cut that made
    /// it that length is on the platter, which is where a rollback and
    /// a post checkpoint truncate respectively leave it.
    fn reset(&self, len: u64, synced: bool) {
        let mut marks = self.marks.lock().expect("wal marks");
        marks.staged = len;
        marks.durable = if synced { len } else { 0 };
        if synced {
            marks.failed = false;
        }
    }

    /// Whether the log owes nothing: every byte staged is on the
    /// platter, so what the write side holds can be shown to a reader.
    pub fn settled(&self) -> bool {
        let marks = self.marks.lock().expect("wal marks");
        !marks.failed && marks.durable >= marks.staged
    }

    /// How long a leader should hold its flush back to let the writers
    /// that are still staging catch it, which is nothing at all unless
    /// there are any.
    ///
    /// A flush covers the bytes that were written before it started, so
    /// a writer that staged into the middle of one waits it out and then
    /// waits for the next: two sync periods, and a burst of n commits
    /// costs two flushes rather than one. Pausing for a fraction of a
    /// flush before issuing it is what turns those two into one, because
    /// staging is tens of microseconds against a flush's thousands, so
    /// nearly everyone in flight arrives inside the pause.
    ///
    /// The pause is only worth its own cost where there is a group to
    /// gather, so it asks what the last flush found. One writer on its
    /// own finds itself every time and waits for nobody, which is what
    /// keeps this off the single connection path the write budget is
    /// read on. It also stays off until the log has timed a sync, since
    /// a fraction of an unknown is not a number.
    fn gather(marks: &Marks) -> Duration {
        if marks.group < 2 || marks.cost == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(marks.cost / GATHER_SHARE).min(GATHER_CAP)
    }

    /// Returns once the log is durable through `need` bytes, syncing it
    /// here if nobody else is already doing it.
    ///
    /// This is the commit point. It runs with the write side let go of,
    /// so the writers behind this one are staging their own frames
    /// while this sync is in the air, and one sync covers all of them.
    pub fn sync_through(&self, need: u64) -> Result<()> {
        COMMITS.fetch_add(1, Ordering::Relaxed);
        let mut marks = self.marks.lock().expect("wal marks");
        let mut counted = false;
        loop {
            // A log shorter than the byte asked for no longer holds the
            // frames this was waiting on, and the only two ways that
            // happens both leave nothing to wait for: a checkpoint that
            // sealed them into the base file and synced it before
            // cutting, or a rollback that took them away.
            if !marks.failed && (marks.durable >= need || marks.staged < need) {
                if counted {
                    marks.waiting -= 1;
                }
                return Ok(());
            }
            if !counted {
                counted = true;
                marks.waiting += 1;
            }
            if marks.running.is_some() {
                // Somebody is syncing. Whether they reach far enough or
                // not, waiting is right: if they do this is over, and
                // if they do not the next turn of this loop leads.
                marks = self.done.wait(marks).expect("wal marks");
                continue;
            }
            // Lead. Claim the flush before pausing for the stragglers,
            // so that a writer arriving during the pause waits for this
            // one rather than starting a second: the whole point of the
            // pause is that its flush is the one they all ride.
            let gather = Self::gather(&marks);
            if !gather.is_zero() {
                marks.running = Some(marks.staged);
                drop(marks);
                std::thread::sleep(gather);
                marks = self.marks.lock().expect("wal marks");
            }
            // Sync as far as the log has been staged, not only as far as
            // this commit needs, because the bytes are already there and
            // covering them costs the same syscall. Never past that: a
            // mark above what the file holds would have the next commit
            // believe a sync it never got.
            let target = marks.staged;
            marks.running = Some(target);
            drop(marks);

            let began = Instant::now();
            let synced = self.file.lock().expect("wal sync handle").sync_data();
            let took = began.elapsed().as_nanos() as u64;

            marks = self.marks.lock().expect("wal marks");
            marks.running = None;
            match &synced {
                Ok(()) => {
                    marks.durable = marks.durable.max(target);
                    marks.failed = false;
                    // Half the last answer and half this one, which
                    // follows a drive that changes its mind without
                    // letting one slow flush decide the window.
                    marks.cost = match marks.cost {
                        0 => took,
                        was => (was + took) / 2,
                    };
                    // Everyone the flush found, this leader included.
                    // Read after it landed rather than before, because
                    // the writers that arrived while it ran are exactly
                    // the ones the next pause is for.
                    marks.group = marks.waiting;
                    SYNCS.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => marks.failed = true,
            }
            marks.waiting -= 1;
            self.done.notify_all();
            return synced;
        }
    }
}

pub struct Wal {
    file: Box<dyn VfsFile>,
    /// What makes a commit durable, shared with every other writer on
    /// this log so that their syncs are one sync.
    commits: Arc<Commits>,
    len: u64,
    /// Bytes the file has been given and zeroed, which is where a
    /// commit may write without asking the filesystem for space.
    /// Always at least `len`.
    reserved: u64,
    /// Frames appended but not yet pushed at the file, in log order.
    buf: Vec<u8>,
}

/// How large the staging buffer is allowed to get before it goes to the
/// file mid-transaction. A bulk insert stages one frame per batch and
/// there is no reason to hold a whole load in memory to save syscalls
/// that are already amortized over a large frame.
const SPILL: usize = 256 * 1024;

/// The first reservation, and the amount it doubles from.
///
/// A database nobody writes to should not carry a megabyte of log it
/// will never use, and a database somebody is hammering should not ask
/// the filesystem for room every few hundred commits. Doubling from 64
/// KiB gets both: the first commit reserves little, and a log that
/// keeps growing reaches [`RESERVE_MAX`] in five steps.
const RESERVE_MIN: u64 = 64 * 1024;
/// The largest single reservation. At about a hundred bytes a commit
/// this is ten thousand commits between one reservation and the next,
/// which is far more than the checkpoint trigger lets the log hold.
const RESERVE_MAX: u64 = 1024 * 1024;

impl Wal {
    /// Opens the log at `path`, creating it when missing, and truncates
    /// the torn tail so the next append starts at the last intact
    /// frame. Crashing mid-append leaves a frame that fails its length
    /// or crc check; everything before it is kept.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_in(&RealVfs::shared(), path)
    }

    /// [`Self::open`] somewhere other than the filesystem, for a log
    /// that belongs to a database that is not on disk either.
    pub fn open_in(vfs: &Arc<dyn Vfs>, path: &Path) -> Result<Self> {
        Self::open_on(vfs.open_or_create(path)?)
    }

    /// [`Self::open`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn open_on(mut file: Box<dyn VfsFile>) -> Result<Self> {
        let reserved = file.len()?;
        let mut bytes = vec![0u8; reserved as usize];
        file.read_exact_at(&mut bytes, 0)?;
        let mut end = 0u64;
        while let Some((body, next)) = next_frame(&bytes, end) {
            let _ = body;
            end = next;
        }
        let commits = Arc::new(Commits::new(file.dup()?, end));
        let mut wal = Wal {
            file,
            commits,
            len: end,
            reserved,
            buf: Vec::new(),
        };
        // What is past the end is either a torn frame or the zeros of a
        // reservation, and only the first has to go. A tear can leave
        // whole frames behind it, since the disk is free to persist the
        // back of a write and not its middle, and those would parse
        // again once a new frame filled the hole in front of them.
        if bytes[end as usize..].iter().any(|&b| b != 0) {
            wal.erase(end, reserved)?;
            wal.file.sync_data()?;
        }
        Ok(wal)
    }

    /// Writes zeros over `[from, to)`, in one write when the span is
    /// small and in chunks when it is not.
    fn erase(&mut self, from: u64, to: u64) -> Result<()> {
        let mut at = from;
        while at < to {
            let span = (to - at).min(RESERVE_MAX) as usize;
            self.file.write_all_at(&vec![0u8; span], at)?;
            at += span as u64;
        }
        Ok(())
    }

    /// Makes sure the file has `need` bytes of zeroed room past the log
    /// end, taking more in one go than any one commit needs.
    ///
    /// The zeros are written rather than the length being set, because
    /// a length set past the end of a file is a hole on every
    /// filesystem this runs on and the first write into a hole pays
    /// the allocation this is here to avoid. The sync is the file's
    /// new size reaching the disk before anything relies on the room
    /// being there; it happens once a reservation, so a commit does not
    /// see it.
    fn reserve(&mut self, need: u64) -> Result<()> {
        if self.len + need <= self.reserved {
            return Ok(());
        }
        let mut to = self.reserved;
        let mut step = self.reserved.clamp(RESERVE_MIN, RESERVE_MAX);
        while to < self.len + need {
            to += step;
            step = (step * 2).min(RESERVE_MAX);
        }
        self.erase(self.reserved, to)?;
        self.file.sync_all()?;
        self.reserved = to;
        Ok(())
    }

    /// Bytes of intact frames, the input to the checkpoint trigger.
    /// Staged frames count: they are what the next commit is about to
    /// make durable, and a trigger that ignored them would read a log
    /// mid-transaction as shorter than it is about to be.
    pub fn len(&self) -> u64 {
        self.len + self.buf.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stages one frame. Nothing reaches the file until the buffer
    /// fills or the commit pushes it, and durability comes from
    /// [`Wal::commit`] either way: a crash before it leaves the staged
    /// frames unwritten, or leaves a spilled prefix with no commit
    /// frame after it, and replay drops the whole uncommitted txn in
    /// both cases.
    pub fn append(&mut self, epoch: Epoch, rec: &WalRecord) -> Result<()> {
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; PREFIX as usize]);
        self.buf.extend_from_slice(&epoch.to_le_bytes());
        self.buf.push(rec.kind());
        rec.encode_payload(&mut self.buf);
        let body = &self.buf[at + PREFIX as usize..];
        let head = [
            (body.len() as u32).to_le_bytes(),
            crc32c::crc32c(body).to_le_bytes(),
        ];
        self.buf[at..at + PREFIX as usize].copy_from_slice(head.as_flattened());
        if self.buf.len() >= SPILL {
            self.flush()?;
        }
        Ok(())
    }

    /// Pushes the staged frames at the file, without syncing.
    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.reserve(self.buf.len() as u64)?;
        self.file.write_all_at(&self.buf, self.len)?;
        self.len += self.buf.len() as u64;
        self.buf.clear();
        // A sync that starts now covers these bytes, so the leader is
        // told about them before it picks how far to go.
        self.commits.staged(self.len);
        Ok(())
    }

    /// Stages the txn's `TxnCommit` frame, writes everything the txn
    /// staged in one go and syncs the file. The commit record hitting
    /// disk is the commit point: replay delivers a txn exactly when
    /// that frame verifies, and it is last in the buffer, so a write
    /// the disk tears anywhere leaves a txn replay refuses.
    ///
    /// This is the whole commit, sync included, for the caller with
    /// nobody to share the sync with. A caller that wants to let the
    /// write side go before it waits for the platter wants
    /// [`Wal::stage_commit`] and then [`Commits::sync_through`], which
    /// is the same two steps with the lock let go of in between.
    pub fn commit(&mut self, epoch: Epoch) -> Result<()> {
        let need = self.stage_commit(epoch)?;
        self.commits.sync_through(need)
    }

    /// Writes the txn's frames and its `TxnCommit` out to the log
    /// without waiting for the platter, and returns the byte the log
    /// has to be durable through for this txn to have committed.
    ///
    /// Nothing is committed when this returns. What it buys is that the
    /// write side can go back to the next writer now, so their frames
    /// are staged while this one's sync is in the air and one sync
    /// covers them both.
    pub fn stage_commit(&mut self, epoch: Epoch) -> Result<u64> {
        self.append(epoch, &WalRecord::TxnCommit)?;
        self.flush()?;
        Ok(self.len)
    }

    /// What makes this log's commits durable, to be held past the write
    /// side going back.
    pub fn commits(&self) -> &Arc<Commits> {
        &self.commits
    }

    /// Drops the frames of a transaction the process died inside, which
    /// are the ones committed after it began: the marker it left says
    /// where the log stood then, so everything above that floor is the
    /// transaction's own and goes with it.
    ///
    /// Epochs only go up along the log, so this is a prefix cut rather
    /// than a rewrite. Frames at or below the floor were committed
    /// before the transaction and stay, whether the base file has
    /// folded them already or replay is about to bring them back.
    pub fn rollback_above(&mut self, floor: Epoch) -> Result<()> {
        // Staged frames belong to the transaction going away, and they
        // never reached the file, so dropping them is the whole of the
        // rollback for everything the buffer still holds.
        self.buf.clear();
        let mut bytes = vec![0u8; self.len as usize];
        self.file.read_exact_at(&mut bytes, 0)?;
        let mut end = 0u64;
        while let Some((body, next)) = next_frame(&bytes, end) {
            if u64::from_le_bytes(body[..8].try_into().unwrap()) > floor {
                break;
            }
            end = next;
        }
        if end == self.len {
            return Ok(());
        }
        self.erase(end, self.len)?;
        self.file.sync_data()?;
        self.len = end;
        // The log just got shorter and the cut is on the platter, so
        // the marks go back to it. Leaving them where they were would
        // have the next commit's offset look like one already synced.
        self.commits.reset(end, true);
        Ok(())
    }

    /// Empties the log after a checkpoint has folded and published
    /// everything it held.
    ///
    /// The cut does not sync, and it is the one place in the write path
    /// that does not have to. The header the checkpoint published names
    /// the epoch it folded through, and replay skips every frame at or
    /// below it, so a crash that finds the old bytes still on disk
    /// replays nothing: what those frames say is already in the base
    /// file. It costs one full sync per statement to make the file
    /// shorter sooner, which is a quarter of what a one cell write pays
    /// for durability it already has.
    pub fn truncate(&mut self) -> Result<()> {
        self.buf.clear();
        // The room stays, the frames go: what the next commit wants is
        // exactly what this was holding, so handing it back to the
        // filesystem only to ask for it again is work with nothing at
        // the end of it. Zeros are what say the log is empty.
        self.erase(0, self.len)?;
        self.len = 0;
        // The zeros are not synced, and deliberately: the header the
        // checkpoint published is what makes them unnecessary. So the
        // log is back to empty and none of it is claimed durable, and
        // the next commit's sync is what puts the cut on the platter
        // along with its own frames.
        self.commits.reset(0, false);
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
        // A second handle on the log this one is holding, rather than a
        // second open of the name it was opened under: the name is not
        // always something to open, and reading a log through its own
        // file is the thing that is true either way.
        let mut file = self.file.dup()?;
        let mut bytes = vec![0u8; self.len as usize];
        file.read_exact_at(&mut bytes, 0)?;
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::vfs::RealFile;

    /// A log file that counts the syncs issued on it, on any of its
    /// handles, which is what a claim about group commit is about.
    #[derive(Debug)]
    struct CountingFile {
        inner: Box<dyn VfsFile>,
        syncs: Arc<AtomicU64>,
    }

    impl CountingFile {
        fn open(path: &Path) -> (Box<dyn VfsFile>, Arc<AtomicU64>) {
            let syncs = Arc::new(AtomicU64::new(0));
            let file = CountingFile {
                inner: Box::new(RealFile::open_or_create(path).unwrap()),
                syncs: Arc::clone(&syncs),
            };
            (Box::new(file), syncs)
        }
    }

    impl VfsFile for CountingFile {
        fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()> {
            self.inner.read_exact_at(buf, offset)
        }

        fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
            self.inner.write_all_at(buf, offset)
        }

        fn set_len(&mut self, len: u64) -> Result<()> {
            self.inner.set_len(len)
        }

        fn sync_all(&mut self) -> Result<()> {
            self.syncs.fetch_add(1, Ordering::Relaxed);
            self.inner.sync_all()
        }

        fn sync_data(&mut self) -> Result<()> {
            self.syncs.fetch_add(1, Ordering::Relaxed);
            self.inner.sync_data()
        }

        fn len(&self) -> Result<u64> {
            self.inner.len()
        }

        fn dup(&self) -> Result<Box<dyn VfsFile>> {
            Ok(Box::new(CountingFile {
                inner: self.inner.dup()?,
                syncs: Arc::clone(&self.syncs),
            }))
        }
    }

    fn staged(wal: &mut Wal, epoch: Epoch) -> u64 {
        wal.append(
            epoch,
            &WalRecord::Delete {
                table: 1,
                ids: vec![epoch],
            },
        )
        .unwrap();
        wal.stage_commit(epoch).unwrap()
    }

    /// Three transactions staged and one sync, because the log is
    /// append only: the sync that puts the last one on the platter puts
    /// the two before it there too. This is the whole of group commit,
    /// and what the writers do around it is only let go of the write
    /// side so their frames land in the same batch.
    #[test]
    fn one_sync_commits_everything_staged_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("group.wal");
        let (file, syncs) = CountingFile::open(&path);
        let mut wal = Wal::open_on(file).unwrap();
        let commits = Arc::clone(wal.commits());

        let first = staged(&mut wal, 1);
        let second = staged(&mut wal, 2);
        let third = staged(&mut wal, 3);
        // The room the log took at the first write is synced once a
        // reservation and has nothing to do with a commit, so the
        // count that matters is the one from here.
        let reserving = syncs.load(Ordering::Relaxed);

        // The writer of the first one leads, and syncs as far as the
        // log has been staged rather than as far as its own frames go.
        commits.sync_through(first).unwrap();
        assert_eq!(syncs.load(Ordering::Relaxed) - reserving, 1);

        commits.sync_through(second).unwrap();
        commits.sync_through(third).unwrap();
        assert_eq!(
            syncs.load(Ordering::Relaxed) - reserving,
            1,
            "the two behind it were already covered"
        );
    }

    /// The pause in front of a flush is for gathering writers, so it
    /// happens where there are writers to gather and nowhere else. One
    /// connection committing on its own is the case that must not pay
    /// it: it is the case the write budget is read on, and the pause
    /// would be latency bought for nobody.
    #[test]
    fn a_leader_waits_only_where_the_last_flush_found_a_group() {
        let alone = Marks {
            cost: 3_200_000,
            group: 1,
            ..Marks::default()
        };
        assert_eq!(Commits::gather(&alone), Duration::ZERO);

        let crowd = Marks { group: 4, ..alone };
        assert_eq!(
            Commits::gather(&crowd),
            Duration::from_micros(400),
            "an eighth of the flush it is worth waiting for"
        );

        let untimed = Marks { cost: 0, ..crowd };
        assert_eq!(
            Commits::gather(&untimed),
            Duration::ZERO,
            "a fraction of an unknown is not a number"
        );

        let slow = Marks {
            cost: 4_000_000_000,
            ..crowd
        };
        assert_eq!(
            Commits::gather(&slow),
            GATHER_CAP,
            "a drive that takes four seconds does not get to put half of one in front of a commit"
        );
    }

    /// And the pause is in front of the flush rather than after it,
    /// which is the whole of it: the writers it is waiting for stage
    /// while it waits, and the flush that follows reaches past them
    /// because it reads how far the log is staged when it starts.
    #[test]
    fn the_pause_comes_before_the_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gather.wal");
        let (file, _) = CountingFile::open(&path);
        let mut wal = Wal::open_on(file).unwrap();
        let commits = Arc::clone(wal.commits());
        let need = staged(&mut wal, 1);

        // What a burst leaves behind it, put here by hand because one
        // thread cannot be a burst: a flush that found four writers and
        // took four milliseconds.
        {
            let mut marks = commits.marks.lock().unwrap();
            marks.group = 4;
            marks.cost = 4_000_000;
        }

        let began = Instant::now();
        commits.sync_through(need).unwrap();
        assert!(
            began.elapsed() >= Duration::from_micros(500),
            "the commit waited out the window before it asked the disk for anything"
        );
    }

    /// A commit waiting on a byte the log no longer reaches is over:
    /// the only ways the log gets shorter are a checkpoint that sealed
    /// those frames into the base file and synced it first, and a
    /// rollback that took them away. Waiting for a sync that will never
    /// name that byte again would hang the writer forever.
    #[test]
    fn a_commit_the_checkpoint_swallowed_stops_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.wal");
        let (file, syncs) = CountingFile::open(&path);
        let mut wal = Wal::open_on(file).unwrap();
        let commits = Arc::clone(wal.commits());

        let need = staged(&mut wal, 1);
        wal.truncate().unwrap();
        let before = syncs.load(Ordering::Relaxed);

        commits.sync_through(need).unwrap();
        assert_eq!(
            syncs.load(Ordering::Relaxed),
            before,
            "the header the checkpoint published is what makes it durable"
        );
    }

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
            WalRecord::RelDelete {
                rel: 5,
                src: vec![1, 7],
                dst: vec![3, 9],
            },
            WalRecord::RelUpdate {
                rel: 5,
                col: 1,
                src: vec![0, 4],
                dst: vec![1, 6],
                values: WalValues::Str(vec![b"met once".to_vec(), Vec::new()]),
            },
            WalRecord::RelUpdate {
                rel: 5,
                col: 0,
                src: vec![2],
                dst: vec![3],
                values: WalValues::Null(1),
            },
            WalRecord::LabelUpdate {
                table: 3,
                offsets: vec![0, 5, 17],
                add: 0b0000_0110,
                remove: 0b0001_0000,
            },
            WalRecord::LabelUpdate {
                table: 3,
                offsets: vec![2],
                add: 0,
                remove: 1 << 63,
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

    /// The frames of a transaction the process died inside go, and the
    /// ones committed before it stay: the floor is where the log stood
    /// when the transaction began, and epochs only go up along the
    /// log, so the cut is where the first frame above it starts.
    #[test]
    fn a_rollback_cuts_the_log_back_to_where_the_transaction_began() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.wal");
        let mut wal = Wal::open(&path).unwrap();
        for epoch in 1..=3 {
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

        wal.rollback_above(1).unwrap();

        let mut seen = Vec::new();
        wal.replay(0, |epoch, rec| {
            if let WalRecord::Delete { ids, .. } = rec {
                seen.push((epoch, ids[0]));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [(1, 1)], "epochs 2 and 3 went with the transaction");

        // And the cut is on the file, not in this handle.
        let reopened = Wal::open(&path).unwrap();
        assert_eq!(reopened.len(), wal.len());
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
        // The log is what the frames occupy; the reservation behind it
        // is zeros, and a cut inside those is the same cut as the one
        // at the log end. Walking them all would be sixty thousand
        // opens saying one thing.
        let logged = wal.len() as usize;
        drop(wal);
        for cut in 0..=logged {
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
        // Every byte of the log, and the first byte of the reservation
        // behind it, which stands for all of them: what a flip there
        // makes is a frame past the end of the log, and where in the
        // zeros it sits changes nothing about what the scan does with
        // it.
        let logged = wal.len() as usize;
        drop(wal);
        for hit in 0..=logged {
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
        let end;
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(1, &WalRecord::TxnBegin).unwrap();
            wal.commit(1).unwrap();
            end = wal.len();
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
        // A tear writes over the reservation rather than past the end
        // of the file, so this is what one leaves behind.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[end as usize..end as usize + 5].copy_from_slice(&[0x77; 5]);
        std::fs::write(&path, &bytes).unwrap();
        let mut wal = Wal::open(&path).unwrap();
        assert_eq!(wal.len(), end, "tail gone on open");
        assert!(
            std::fs::read(&path).unwrap()[end as usize..]
                .iter()
                .all(|&b| b == 0),
            "and gone from the file, not just from the count"
        );
        let epochs: Vec<Epoch> = collect(&wal, 0).iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![1, 1], "uncommitted epoch 2 stays invisible");
        wal.append(3, &WalRecord::TxnBegin).unwrap();
        wal.commit(3).unwrap();
        let epochs: Vec<Epoch> = collect(&wal, 0).iter().map(|(e, _)| *e).collect();
        assert_eq!(epochs, vec![1, 1, 3, 3]);
    }

    /// A label update that both sets and clears the same bit says
    /// nothing a reader can act on, so it reads as corruption rather
    /// than as one of the two halves winning silently.
    #[test]
    fn a_label_update_that_contradicts_itself_is_refused() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&3u32.to_le_bytes());
        payload.extend_from_slice(&0b0110u64.to_le_bytes());
        payload.extend_from_slice(&0b0010u64.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        let mut r = Reader {
            buf: &payload,
            pos: 0,
        };
        let err = WalRecord::decode(12, &mut r).unwrap_err();
        assert!(format!("{err}").contains("both sets and clears"), "{err}");
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
