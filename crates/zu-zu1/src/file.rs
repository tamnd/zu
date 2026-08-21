//! File headers, block I/O, and the dual-header checkpoint protocol.
//!
//! The layout is `docs/04-storage-zu1-format.md` §1: a write-once 4 KiB
//! FileHeader, two alternating 4 KiB DatabaseHeaders, padding to 256 KiB,
//! then fixed 256 KiB blocks. Opening reads 12 KiB and picks the valid
//! header with the highest epoch, so a torn header write never damages
//! committed state; new data goes to free blocks and becomes visible only
//! when the next header flip publishes it.

use std::path::Path;
use std::sync::Arc;

use zu_common::{Epoch, IdSet, Result, ZuError};

use crate::cache::{BlockCache, CacheStats, DecodedPool, PinnedBlock};
use crate::segment::ChunkDirectory;
use crate::vfs::{RealVfs, Vfs, VfsFile};
use crate::{BLOCK_SIZE, FORMAT_VERSION, MAGIC, MIN_READER_VERSION};

/// Memory limit the caches size themselves from when the caller sets
/// none, matching the 128 MiB budget the P9 gate runs under.
pub const DEFAULT_MEMORY_LIMIT: usize = 128 << 20;

/// The decoded-object pools of perf/04 section 2, shared by every
/// handle [`Zu1File::reopen`] forks off one open. Keys are the first
/// block pointer of the decoded segment; [`Zu1File::write_block`]
/// drops the key of any block it rewrites, so a recycled pointer can
/// never serve a stale decode.
#[derive(Debug)]
pub struct DecodedPools {
    /// Decoded CSR offset arrays, the O(1)-seek side of a group.
    pub csr_offsets: DecodedPool<Vec<u64>>,
    /// Decoded neighbor arrays, the bulk of a group.
    pub adjacency: DecodedPool<Vec<u64>>,
    /// Chunk directories: per-chunk end offsets and fences, the
    /// pk-lookup and probe steering data.
    pub fences: DecodedPool<ChunkDirectory>,
}

impl DecodedPools {
    /// Pools sized off `memory_limit` with the perf/04 split: 8% CSR
    /// offsets, 20% adjacency, 4% fences.
    fn new(memory_limit: usize) -> Self {
        Self {
            csr_offsets: DecodedPool::new(memory_limit * 8 / 100),
            adjacency: DecodedPool::new(memory_limit / 5),
            fences: DecodedPool::new(memory_limit / 25),
        }
    }
}

/// Byte index into the file where block `n` starts. Block 0 is the header
/// region, so index 0 doubles as the null pointer in chains and roots.
pub type BlockPtr = u64;

/// The null block pointer.
pub const NULL_BLOCK: BlockPtr = 0;

/// On-disk size of the FileHeader region.
pub const FILE_HEADER_SIZE: usize = 4096;

/// On-disk size of one DatabaseHeader slot.
pub const DB_HEADER_SIZE: usize = 4096;

/// Serialized DatabaseHeader body length; the crc32c follows immediately.
const DB_HEADER_BODY: usize = 56;

/// Where the state an open transaction is holding is kept, which is a
/// third slot of the same shape as the two the checkpoint flips
/// between, in the header block after them.
///
/// It is in the database file rather than beside it for two reasons. A
/// file of its own would need its directory entry to be durable before
/// the first write of the transaction, which is a sync nothing else
/// here pays, and the crash harness builds its images out of the writes
/// to the database and the log, so a third file would be a piece of the
/// commit protocol no image ever shows. The region was zeroed in every
/// file ever written, and a zeroed slot fails its crc, so a file from
/// before this reads as one with no transaction open, which is what it
/// is.
const TXN_SLOT: u64 = (FILE_HEADER_SIZE + 2 * DB_HEADER_SIZE) as u64;

/// Where the log floor sits inside the marker slot, after the header
/// and its crc, with a crc of its own over everything before it.
///
/// The floor is the newest epoch the log held when the transaction
/// began, and it is what says which frames are the transaction's own.
/// It cannot be read off the kept header, whose `wal_seq` is only as
/// new as the last fold: a rollback cutting the log back to that would
/// take frames with it that were committed before the transaction and
/// belong to whoever committed them. A marker whose floor does not
/// check out reads as no marker at all, which is the right reading of
/// a torn one: the write that puts it down returns before the first
/// commit inside the transaction reaches the log, so a transaction
/// caught mid marker has nothing durable to take back.
const TXN_FLOOR: usize = 64;

/// Write-once identity of a zu1 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub format_version: u16,
    pub min_reader_version: u16,
    pub block_size: u32,
    pub uuid: [u8; 16],
    pub flags: u64,
}

/// Puts the file's name in front of a corruption an open found.
///
/// The header decoders work on bytes and have no path to name, and a
/// process with several databases open learns nothing from "bad magic"
/// on its own. Anything that is not a corruption is passed through
/// untouched, since it already says which call failed.
fn named(error: ZuError, path: &Path) -> ZuError {
    match error {
        ZuError::Corrupt { what, detail } => ZuError::Corrupt {
            what,
            detail: format!("{}: {detail}", path.display()),
        },
        other => other,
    }
}

impl FileHeader {
    fn fresh() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            min_reader_version: MIN_READER_VERSION,
            block_size: BLOCK_SIZE,
            uuid: fresh_uuid(),
            flags: 0,
        }
    }

    fn encode(&self) -> [u8; FILE_HEADER_SIZE] {
        let mut buf = [0u8; FILE_HEADER_SIZE];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..10].copy_from_slice(&self.format_version.to_le_bytes());
        buf[10..12].copy_from_slice(&self.min_reader_version.to_le_bytes());
        buf[12..16].copy_from_slice(&self.block_size.to_le_bytes());
        buf[16..32].copy_from_slice(&self.uuid);
        buf[32..40].copy_from_slice(&self.flags.to_le_bytes());
        let crc = crc32c::crc32c(&buf[0..64]);
        buf[64..68].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "file header",
            detail: detail.to_string(),
        };
        if buf.len() < FILE_HEADER_SIZE {
            return Err(corrupt("short read"));
        }
        if buf[0..8] != MAGIC {
            return Err(corrupt("bad magic, not a zu1 file"));
        }
        let stored = u32::from_le_bytes(buf[64..68].try_into().unwrap());
        if crc32c::crc32c(&buf[0..64]) != stored {
            return Err(corrupt("crc mismatch"));
        }
        let header = Self {
            format_version: u16::from_le_bytes(buf[8..10].try_into().unwrap()),
            min_reader_version: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            block_size: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            uuid: buf[16..32].try_into().unwrap(),
            flags: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
        };
        if header.min_reader_version > FORMAT_VERSION {
            return Err(ZuError::Unsupported {
                what: "zu1 min_reader_version",
                id: u32::from(header.min_reader_version),
            });
        }
        if header.block_size != BLOCK_SIZE {
            return Err(ZuError::Unsupported {
                what: "zu1 block_size",
                id: header.block_size,
            });
        }
        Ok(header)
    }
}

/// One checkpoint's view of the file: roots of every meta-block chain and
/// the block high-water mark. The valid slot with the highest epoch wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseHeader {
    pub epoch: u64,
    pub catalog_root: BlockPtr,
    pub table_index_root: BlockPtr,
    pub free_list_root: BlockPtr,
    pub block_count: u64,
    pub wal_seq: u64,
    pub stats_root: BlockPtr,
}

impl DatabaseHeader {
    fn encode(&self) -> [u8; DB_HEADER_SIZE] {
        let mut buf = [0u8; DB_HEADER_SIZE];
        for (i, v) in [
            self.epoch,
            self.catalog_root,
            self.table_index_root,
            self.free_list_root,
            self.block_count,
            self.wal_seq,
            self.stats_root,
        ]
        .into_iter()
        .enumerate()
        {
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        let crc = crc32c::crc32c(&buf[..DB_HEADER_BODY]);
        buf[DB_HEADER_BODY..DB_HEADER_BODY + 4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < DB_HEADER_SIZE {
            return None;
        }
        let stored =
            u32::from_le_bytes(buf[DB_HEADER_BODY..DB_HEADER_BODY + 4].try_into().unwrap());
        if crc32c::crc32c(&buf[..DB_HEADER_BODY]) != stored {
            return None;
        }
        let word = |i: usize| u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
        let block_count = word(4);
        // Every block pointer is checked against this count and then
        // multiplied by the block size to get a byte offset, so a count
        // whose last block does not have a byte offset is not a count.
        // The crc only says the bytes are the bytes someone wrote, and a
        // file the reader did not write gets no benefit of the doubt.
        if block_count > u64::MAX / u64::from(BLOCK_SIZE) {
            return None;
        }
        Some(Self {
            epoch: word(0),
            catalog_root: word(1),
            table_index_root: word(2),
            free_list_root: word(3),
            block_count,
            wal_seq: word(5),
            stats_root: word(6),
        })
    }
}

/// What a transaction the process died inside left on a file.
///
/// Both halves are needed to take it back. The file has to be published
/// past what the crash left, or the header the transaction wrote wins
/// the next open by being the newer of the two slots, and the log has
/// to be cut back to where it stood when the transaction began, or
/// replay puts what the transaction committed back as overlays over the
/// header that went back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted {
    /// The epoch the crash left published.
    pub published: Epoch,
    /// The newest epoch the log held before the transaction started.
    pub log_floor: Epoch,
}

/// Reads a marker slot: the state a transaction was holding and the
/// floor it began at, or nothing when either crc says these are not
/// bytes someone finished writing.
fn decode_marker(buf: &[u8]) -> Option<(DatabaseHeader, Epoch)> {
    let held = DatabaseHeader::decode(buf)?;
    let stored = u32::from_le_bytes(buf[TXN_FLOOR + 8..TXN_FLOOR + 12].try_into().ok()?);
    if crc32c::crc32c(&buf[..TXN_FLOOR + 8]) != stored {
        return None;
    }
    Some((
        held,
        Epoch::from_le_bytes(buf[TXN_FLOOR..TXN_FLOOR + 8].try_into().ok()?),
    ))
}

/// What a file looked like before a transaction started.
///
/// A statement commits by folding and flipping the header, so by the
/// time a second statement of the same transaction runs, the first one
/// is already published and there is nothing left in memory to drop.
/// Undoing it means putting the roots back, and the roots are three
/// pieces: the header itself, the blocks the file held free, and the
/// blocks the free list is written in.
#[derive(Debug)]
struct Savepoint {
    db: DatabaseHeader,
    free: Vec<BlockPtr>,
    free_chain: Vec<BlockPtr>,
    /// The blocks that were already free when the transaction began,
    /// which are the only ones it may write into: they are free in the
    /// state a rollback goes back to as well, so whatever the
    /// transaction leaves in them is garbage in a block nothing reads.
    reusable: IdSet<BlockPtr>,
    /// The newest epoch the log held when the transaction began. What
    /// it committed sits above this, and a rollback cuts the log back
    /// here rather than back to what the file had folded.
    log_floor: Epoch,
    /// Whether the kept state is on disk. Until it is, a crash leaves
    /// whatever the transaction has published with nothing on the file
    /// to say it was meant to be taken back.
    marked: bool,
    /// Whether the marker has to be down before the first publish
    /// rather than before the second. An explicit transaction promises
    /// across statements, so its first published statement is already
    /// something a crash has to take back; a statement promises only
    /// about itself, and one publish is one header flip, which a crash
    /// either leaves whole or does not leave at all.
    across_statements: bool,
    /// Whether anything has been published under this savepoint.
    published: bool,
    /// The epoch that was published when the transaction began, which
    /// is the newest epoch that reads the blocks held out of allocation
    /// while it runs, and so the epoch the reader leases are compared
    /// against when it lets go of them.
    began_at: u64,
}

/// The caches one file's handles hold in common, for a handle that
/// was opened rather than forked to take up. See [`Zu1File::adopt`].
#[derive(Clone)]
pub struct Shared {
    cache: Arc<BlockCache>,
    pools: Arc<DecodedPools>,
    forks: Option<Arc<std::sync::Mutex<Vec<Zu1File>>>>,
}

/// An open zu1 file: block I/O, the free list, and the header flip.
#[derive(Debug)]
pub struct Zu1File {
    file: Box<dyn VfsFile>,
    /// Where this handle was opened, kept so [`Self::reopen`] can hand
    /// a second read handle to a query worker. Block reads seek, so
    /// workers cannot share one file descriptor.
    path: std::path::PathBuf,
    /// Where the file came from, kept for the same reason the path is:
    /// a reopen has to go back to the same place, and so does the
    /// sidecar log, which is opened by name off this path later. For a
    /// database on disk this is the filesystem and carrying it costs a
    /// pointer; for one that is not, it is the only way back.
    vfs: Arc<dyn Vfs>,
    file_header: FileHeader,
    db: DatabaseHeader,
    /// Slot the current header was read from; the flip writes the other one.
    active_slot: usize,
    /// Committed-free blocks, reusable immediately: the committed epoch
    /// lists them as free, so a crash after overwriting them loses nothing.
    free: Vec<BlockPtr>,
    /// Freed this transaction. Still referenced by the committed epoch, so
    /// not reusable until the next checkpoint supersedes it.
    pending_free: Vec<BlockPtr>,
    /// Blocks holding the committed free-list chain itself; a checkpoint
    /// writes a fresh chain and recycles these.
    free_chain: Vec<BlockPtr>,
    /// Free blocks a transaction may not write into: it freed them
    /// itself, or they were pending when it began, so the state a
    /// rollback goes back to still reads them. A checkpoint publishes
    /// them as free, because the epoch it publishes has let go of them;
    /// they only stay out of allocation. Empty outside a transaction.
    frozen: Vec<BlockPtr>,
    /// The block cache every read goes through, shared with handles
    /// forked by [`Self::reopen`] so workers warm each other.
    cache: Arc<BlockCache>,
    /// Decoded-object pools above the block cache, shared the same way.
    pools: Arc<DecodedPools>,
    /// The last block this handle pinned. Segments span a couple of
    /// 256 KiB blocks, so a chunk scan pins the same pointer hundreds
    /// of times in a row, and with eight workers doing that the shared
    /// cache's shard mutex becomes the profile. The memo answers the
    /// repeat pins handle-locally; [`Self::write_block`] drops it the
    /// same way it drops the shared entries.
    pin_memo: Option<(BlockPtr, PinnedBlock)>,
    /// Retired fork handles waiting for the next [`Self::reopen`].
    /// Opening a file is cheap on Linux and painfully slow on Windows,
    /// where eight per-query opens were costing more than the query;
    /// pooling keeps the descriptors alive across queries. Entries set
    /// their own slot to `None` before going in, so the pool never
    /// holds a reference to itself.
    forks: Option<Arc<std::sync::Mutex<Vec<Zu1File>>>>,
    /// Whether this handle may write. A handle opened by
    /// [`Self::open_read_only`] holds a descriptor the operating system
    /// will refuse a write on, and this says so before the syscall does,
    /// so the caller reads which database refused rather than `EBADF`.
    writable: bool,
    /// The state a transaction can be put back to, held from
    /// [`Self::begin_savepoint`] to the commit or rollback that ends it.
    /// While one is held, allocation reuses only the blocks that were
    /// already free when it began, because a block the transaction
    /// frees is a block the state being kept still reads.
    savepoint: Option<Savepoint>,
    /// Blocks a checkpoint has listed as free that an older epoch still
    /// reads, held out of allocation until nothing is reading that
    /// epoch any more. Each entry is the epoch the checkpoint
    /// superseded and the blocks that epoch's roots reach.
    ///
    /// docs/08 §3 calls this epoch refcounts: with one connection there
    /// is never a reader behind the writer and the list empties as fast
    /// as it fills, and with several there is, because a statement on
    /// another connection reads the roots the writer has just replaced.
    /// The blocks are on the free list on disk either way, so a crash
    /// reclaims them: what waits is only this handle allocating into
    /// them, and only while somebody is reading them.
    retained: Vec<(u64, Vec<BlockPtr>)>,
    /// Whether something above this handle counts the readers and hands
    /// the retained blocks back itself, which is what a file behind
    /// `zu::shared::FileHandle` has. A handle used bare has nobody
    /// reading behind it, because reading it means holding it, so it
    /// releases what it retains as soon as it has retained it and
    /// allocates exactly as it did before any of this existed.
    defer_reclaim: bool,
    /// Blocks this handle has allocated since its last checkpoint,
    /// which is what a caller deferring one has to watch. Nothing
    /// allocated since then can be given back until a checkpoint
    /// publishes, because the header on disk still reads the blocks
    /// they replaced, so a writer that never checkpoints grows the file
    /// by everything it rewrites.
    unpublished: u64,
    /// The epoch a crash left published, when this handle opened a file
    /// with a transaction still open on it. The header in hand is the
    /// one that transaction was holding, so every read is already of
    /// the state going back in; what is left is to publish it, which
    /// [`Self::finish_rollback`] does once the log has been cut back to
    /// match.
    interrupted: Option<Interrupted>,
}

impl Zu1File {
    /// Creates a new database file. Fails if `path` already exists, so an
    /// existing database is never silently clobbered.
    pub fn create(path: &Path) -> Result<Self> {
        Self::create_in(RealVfs::shared(), path)
    }

    /// [`Self::create`] somewhere other than the filesystem. The vfs is
    /// kept, so the sidecar log and every reopened handle land in the
    /// same place this one did.
    pub fn create_in(vfs: Arc<dyn Vfs>, path: &Path) -> Result<Self> {
        let file = vfs.create_new(path)?;
        Self::create_within(vfs, file, path)
    }

    /// [`Self::create`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn create_on(file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
        Self::create_within(RealVfs::shared(), file, path)
    }

    fn create_within(vfs: Arc<dyn Vfs>, mut file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
        let file_header = FileHeader::fresh();
        let db = DatabaseHeader {
            epoch: 1,
            ..DatabaseHeader::default()
        };
        file.write_all_at(&file_header.encode(), 0)?;
        file.write_all_at(&db.encode(), FILE_HEADER_SIZE as u64)?;
        // Slot B stays zeroed; an all-zero slot never passes its crc.
        file.set_len(u64::from(BLOCK_SIZE))?;
        file.sync_all()?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            vfs,
            file_header,
            db,
            active_slot: 0,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
            frozen: Vec::new(),
            savepoint: None,
            retained: Vec::new(),
            defer_reclaim: false,
            unpublished: 0,
            interrupted: None,
            pin_memo: None,
            forks: Some(Arc::new(std::sync::Mutex::new(Vec::new()))),
            writable: true,
            cache: Arc::new(BlockCache::new(DEFAULT_MEMORY_LIMIT / 2)),
            pools: Arc::new(DecodedPools::new(DEFAULT_MEMORY_LIMIT)),
        })
    }

    /// Opens an existing database: read 12 KiB, validate the file header,
    /// and adopt the valid database header with the highest epoch.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_in(RealVfs::shared(), path)
    }

    /// [`Self::open`] somewhere other than the filesystem.
    pub fn open_in(vfs: Arc<dyn Vfs>, path: &Path) -> Result<Self> {
        let file = vfs.open_rw(path)?;
        Self::open_kind(vfs, file, path, true)
    }

    /// Opens an existing database on a descriptor the operating system
    /// will not let this process write through. Every path that would
    /// have written refuses first, with the name of the database in the
    /// error, so a caller that asked for a read-only handle and then
    /// wrote learns which promise it broke rather than which syscall
    /// failed.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_read_only_in(RealVfs::shared(), path)
    }

    /// [`Self::open_read_only`] somewhere other than the filesystem.
    pub fn open_read_only_in(vfs: Arc<dyn Vfs>, path: &Path) -> Result<Self> {
        let file = vfs.open_r(path)?;
        Self::open_kind(vfs, file, path, false)
    }

    /// [`Self::open`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn open_on(file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
        Self::open_kind(RealVfs::shared(), file, path, true)
    }

    fn open_kind(
        vfs: Arc<dyn Vfs>,
        mut file: Box<dyn VfsFile>,
        path: &Path,
        writable: bool,
    ) -> Result<Self> {
        let mut head = [0u8; FILE_HEADER_SIZE + 2 * DB_HEADER_SIZE];
        // Read what the file has rather than what a database would
        // have. A file too small for the header is usually not a
        // database at all, and the read that would report it says only
        // "failed to fill whole buffer", which names neither the file
        // nor the reason. The bytes past the end stay zero, which no
        // header slot decodes as valid, so the magic gets to answer
        // first and a text file gets told it is a text file.
        let size = file.len()?;
        let have = usize::try_from(size).unwrap_or(head.len()).min(head.len());
        file.read_exact_at(&mut head[..have], 0)?;
        if have < FILE_HEADER_SIZE {
            return Err(ZuError::Corrupt {
                what: "file header",
                detail: format!(
                    "{}: {size} bytes, too short to be a zu1 database",
                    path.display()
                ),
            });
        }
        let file_header = FileHeader::decode(&head).map_err(|error| named(error, path))?;
        let a = DatabaseHeader::decode(&head[FILE_HEADER_SIZE..FILE_HEADER_SIZE + DB_HEADER_SIZE]);
        let b = DatabaseHeader::decode(&head[FILE_HEADER_SIZE + DB_HEADER_SIZE..]);
        let (db, active_slot) = match (a, b) {
            (Some(a), Some(b)) => {
                if a.epoch >= b.epoch {
                    (a, 0)
                } else {
                    (b, 1)
                }
            }
            (Some(a), None) => (a, 0),
            (None, Some(b)) => (b, 1),
            (None, None) => {
                return Err(ZuError::Corrupt {
                    what: "database header",
                    detail: format!("{}: no valid header in either slot", path.display()),
                });
            }
        };
        // A transaction was open when the process went away if the
        // third slot holds a header, and the state it was holding is
        // the state to read: the one in the slot the checkpoint last
        // flipped to is the middle of that transaction.
        let mut kept = [0u8; DB_HEADER_SIZE];
        let holding = match file.len()? >= TXN_SLOT + DB_HEADER_SIZE as u64 {
            true => {
                file.read_exact_at(&mut kept, TXN_SLOT)?;
                decode_marker(&kept)
            }
            false => None,
        };
        let (db, interrupted) = match holding {
            Some((held, log_floor)) if held.epoch <= db.epoch => (
                held,
                Some(Interrupted {
                    published: db.epoch,
                    log_floor,
                }),
            ),
            // The kept state is ahead of what is published, which is
            // what a transaction that folded without checkpointing
            // leaves behind: none of what it did reached the file, so
            // the published header is already the state to go back to
            // and the marker is worth only its floor, which says how
            // much of the log the transaction wrote and a replay must
            // not put back on.
            Some((_, log_floor)) => {
                let published = db.epoch;
                (
                    db,
                    Some(Interrupted {
                        published,
                        log_floor,
                    }),
                )
            }
            None => (db, None),
        };
        let mut this = Self {
            file,
            path: path.to_path_buf(),
            vfs,
            file_header,
            db,
            active_slot,
            interrupted,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
            frozen: Vec::new(),
            savepoint: None,
            retained: Vec::new(),
            defer_reclaim: false,
            unpublished: 0,
            pin_memo: None,
            forks: Some(Arc::new(std::sync::Mutex::new(Vec::new()))),
            writable,
            cache: Arc::new(BlockCache::new(DEFAULT_MEMORY_LIMIT / 2)),
            pools: Arc::new(DecodedPools::new(DEFAULT_MEMORY_LIMIT)),
        };
        let root = this.db.free_list_root;
        if root != NULL_BLOCK {
            let bytes = crate::meta::read_chain(&mut this, root)?;
            this.free = decode_free_list(&bytes, this.db.block_count)?;
            this.free_chain = crate::meta::chain_blocks(&mut this, root)?;
        }
        Ok(this)
    }

    /// A second read handle on the same file carrying this handle's
    /// current in-memory state. Data blocks are written to the file as
    /// they are staged and only the header flip waits for the
    /// checkpoint, so adopting this handle's header lets the new
    /// handle read exactly what this one reads, staged roots included.
    /// The free lists stay empty because a reopened handle exists to
    /// read; the morsel workers are the caller. The block cache is
    /// shared, so a fork starts warm and warms its siblings.
    ///
    /// Handles retired through [`Self::recycle`] come back first, with
    /// this handle's header and a cleared pin memo, so steady-state
    /// forking costs a mutex pop instead of an OS open.
    pub fn reopen(&self) -> Result<Self> {
        let pool = self
            .forks
            .as_ref()
            .expect("only pooled entries lack a pool");
        if let Some(mut fork) = pool.lock().unwrap().pop() {
            fork.db = self.db.clone();
            fork.active_slot = self.active_slot;
            fork.pin_memo = None;
            fork.forks = Some(Arc::clone(pool));
            return Ok(fork);
        }
        Ok(Self {
            file: if self.writable {
                self.vfs.open_rw(&self.path)?
            } else {
                self.vfs.open_r(&self.path)?
            },
            path: self.path.clone(),
            vfs: Arc::clone(&self.vfs),
            file_header: self.file_header.clone(),
            db: self.db.clone(),
            active_slot: self.active_slot,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
            frozen: Vec::new(),
            savepoint: None,
            retained: Vec::new(),
            defer_reclaim: false,
            unpublished: 0,
            interrupted: None,
            pin_memo: None,
            forks: Some(Arc::clone(pool)),
            writable: self.writable,
            cache: Arc::clone(&self.cache),
            pools: Arc::clone(&self.pools),
        })
    }

    /// What every handle on one file has to be sharing: the block
    /// cache, the decoded pools above it, and the pool of retired
    /// handles.
    pub fn shared(&self) -> Shared {
        Shared {
            cache: Arc::clone(&self.cache),
            pools: Arc::clone(&self.pools),
            forks: self.forks.as_ref().map(Arc::clone),
        }
    }

    /// Puts this handle on another handle's caches, which a handle
    /// opened rather than forked has to do before it reads anything.
    ///
    /// A cache is invalidated by the handle that writes the block, and
    /// it can only invalidate its own. So a second cache on one file is
    /// a cache holding whatever it last saw in a block, and a block the
    /// writer has since freed and written over reads back as the thing
    /// that used to be in it. What that looks like from a query is a
    /// props directory that decodes as somebody else's catalog.
    pub fn adopt(&mut self, shared: &Shared) {
        self.cache = Arc::clone(&shared.cache);
        self.pools = Arc::clone(&shared.pools);
        self.forks = shared.forks.as_ref().map(Arc::clone);
        self.pin_memo = None;
    }

    /// Retires a fork into its shared pool for the next
    /// [`Self::reopen`] to reuse. The handle drops its own reference
    /// to the pool on the way in, so the pool holding it does not keep
    /// itself alive. Dropping a fork without recycling is fine, the
    /// next reopen just pays the OS open again.
    pub fn recycle(mut self) {
        // A pooled handle must not sit on a pinned cache frame.
        self.pin_memo = None;
        if let Some(pool) = self.forks.take() {
            pool.lock().unwrap().push(self);
        }
    }

    /// Write-once file identity.
    pub fn file_header(&self) -> &FileHeader {
        &self.file_header
    }

    /// The committed database header this handle opened at, including any
    /// root updates staged since through [`Self::db_header_mut`].
    pub fn db_header(&self) -> &DatabaseHeader {
        &self.db
    }

    /// Stages root updates for the next checkpoint. Nothing is visible to
    /// other openers until [`Self::checkpoint`] publishes them.
    pub fn db_header_mut(&mut self) -> &mut DatabaseHeader {
        &mut self.db
    }

    /// Which header slot this handle is reading, which a caller passing
    /// the header to another handle on the same file passes with it.
    pub fn active_slot(&self) -> usize {
        self.active_slot
    }

    /// Takes the roots another handle on this file has reached.
    ///
    /// [`Self::reopen`] hands a new handle the roots the writer holds,
    /// and this is the same move made again later: a reader forked
    /// before a commit reads the blocks that commit wrote, because data
    /// blocks reach the file as they are staged and only the flip waits
    /// for a checkpoint, but it has no way to know where they went. The
    /// header is that way, so a reader given the writer's header reads
    /// the writer's database.
    ///
    /// Only the header moves. The free lists, the savepoint and the
    /// unpublished count belong to the handle that allocates, and a
    /// handle that follows another one does not: it reads. The pin memo
    /// goes because it names a block of the state being left behind.
    pub fn follow(&mut self, header: &DatabaseHeader, slot: usize) {
        self.db = header.clone();
        self.active_slot = slot;
        self.pin_memo = None;
    }

    /// Returns a block to write into: a committed-free block when one
    /// exists, otherwise one past the high-water mark. Either way the
    /// block becomes durable state only via the next checkpoint.
    ///
    /// A transaction narrows what counts as free. A checkpoint inside
    /// one publishes the blocks that transaction has freed, and the
    /// state the savepoint keeps still reads them, so those are held in
    /// [`Self::frozen`] until the transaction ends and this list holds
    /// only what was free before it began.
    pub fn allocate_block(&mut self) -> BlockPtr {
        self.unpublished += 1;
        if let Some(ptr) = self.free.pop() {
            return ptr;
        }
        self.db.block_count += 1;
        self.db.block_count
    }

    /// Blocks allocated since the last checkpoint. A writer that folds
    /// without publishing watches this, because until it publishes
    /// nothing it freed can be handed back out and the file grows by
    /// everything the folds rewrote.
    pub fn unpublished_blocks(&self) -> u64 {
        self.unpublished
    }

    /// Says a fold moved the roots this handle reads and stopped short
    /// of putting them on disk, which is what a writer that folds every
    /// commit and checkpoints on a threshold does most of the time.
    ///
    /// Two things follow. The epoch moves, because everything above
    /// keys its cached catalogs, readers and plans on it and the roots
    /// they describe have moved. And a savepoint open over the fold is
    /// now holding against something a crash would find, because the
    /// frames the fold folded are in the log and nothing has cut them,
    /// which is the same position a publish leaves it in and wants the
    /// same marker.
    pub fn stage_fold(&mut self) {
        self.db.epoch += 1;
        if let Some(saved) = &mut self.savepoint {
            saved.published = true;
        }
    }

    /// Says a commit went to the log and nothing folded it, which is
    /// where a deferred commit leaves the file.
    ///
    /// The roots do not move and the epoch does not either, because
    /// what the readers are handed is the patch. What does move is the
    /// log, and the frames are above the floor a rollback cuts back to
    /// with nothing on the file saying they are to be taken back. That
    /// is the same position [`Self::stage_fold`] leaves an open
    /// savepoint in, and it wants the same marker.
    pub fn stage_deferred(&mut self) {
        if let Some(saved) = &mut self.savepoint {
            saved.published = true;
        }
    }

    /// Where the log stood when the open transaction began, which a
    /// rollback needs because the frames above it are the ones going
    /// away and a fold that did not publish did not cut them.
    pub fn savepoint_floor(&self) -> Option<Epoch> {
        self.savepoint.as_ref().map(|saved| saved.log_floor)
    }

    /// Keeps what the file is now, so that [`Self::rollback_savepoint`]
    /// can put it back after any number of statements have committed on
    /// top of it.
    ///
    /// This is what makes an explicit transaction more than a word. A
    /// statement commits by folding its overlays into new segments and
    /// flipping the header, and there is no undoing that in memory
    /// afterwards, so the undo is at the file: keep the roots, refuse to
    /// reuse anything freed while the transaction runs, and publish the
    /// kept roots again if the transaction ends in a rollback.
    ///
    /// One savepoint at a time. Nested transactions are not a thing GQL
    /// has, and a second one here would quietly become the first.
    ///
    /// `across_statements` says whether what is being kept has to
    /// survive the process going away, which is what an explicit
    /// transaction needs and a single statement does not: a statement
    /// that publishes once publishes in one header flip, and a crash
    /// either leaves that flip whole or leaves the state before it. See
    /// [`Self::keep_savepoint`] for what a savepoint that has to
    /// survive costs.
    ///
    /// A transaction starts from a folded file, because what is kept
    /// here is the file's roots and a rollback publishes them again:
    /// whatever the log has not folded into them is not in them, and
    /// would go back along with the transaction. Callers fold first,
    /// which is what opening a writer does.
    ///
    /// `log_floor` is where the log stands as this is called, which is
    /// the newest epoch anything committed before the transaction. A
    /// rollback cuts the log back to it, so a caller handing over a
    /// floor older than what the log holds is asking for those frames
    /// to be dropped along with the transaction.
    pub fn begin_savepoint(&mut self, across_statements: bool, log_floor: Epoch) -> Result<()> {
        self.check_writable("a transaction")?;
        if self.savepoint.is_some() {
            return Err(ZuError::InvalidArgument(format!(
                "{} is already inside a transaction",
                self.path.display()
            )));
        }
        self.savepoint = Some(Savepoint {
            db: self.db.clone(),
            free: self.free.clone(),
            free_chain: self.free_chain.clone(),
            reusable: self.free.iter().copied().collect(),
            log_floor,
            marked: false,
            across_statements,
            published: false,
            began_at: self.db.epoch,
        });
        // Blocks freed before the transaction and not published yet are
        // referenced by the header it keeps, so they go straight into
        // the list it may not write into.
        self.frozen.append(&mut self.pending_free);
        Ok(())
    }

    /// Whether a transaction is holding this file to a state it can be
    /// put back to.
    pub fn in_savepoint(&self) -> bool {
        self.savepoint.is_some()
    }

    /// Ends the transaction by keeping what it did. Everything it wrote
    /// is already published, so this only drops what was being held and
    /// lets allocation reuse free blocks again.
    ///
    /// A transaction whose kept state reached the file has one more
    /// thing to do, which is to say it is over. The state is published
    /// before the saying, so a crash in between takes the transaction
    /// back, which is the right way round: the word that ends it had
    /// not returned yet.
    pub fn release_savepoint(&mut self) -> Result<()> {
        let held = self.savepoint.take();
        // What it freed is free for good now, so it goes back into the
        // list allocation draws from. The next checkpoint publishes it
        // as free either way; this is about reuse, not about the list.
        //
        // It goes by way of the retained list rather than straight in,
        // because a block the transaction freed is a block the epoch it
        // freed it under still reads, and a statement on another
        // connection may be on that epoch. With nobody behind, which is
        // one connection writing, the reclaim after this hands the
        // whole lot back and nothing is delayed.
        if !self.frozen.is_empty() {
            let freed = std::mem::take(&mut self.frozen);
            let epoch = held.as_ref().map_or(self.db.epoch, |held| held.began_at);
            self.retained.push((epoch, freed));
            if !self.defer_reclaim {
                self.release_retained(u64::MAX);
            }
        }
        match held.is_some_and(|held| held.marked) {
            true => self.forget_kept(),
            false => Ok(()),
        }
    }

    /// Puts the state a savepoint is keeping on the file, so that a
    /// process that goes away before the transaction ends leaves
    /// something that says where to go back to.
    ///
    /// This runs before the write it protects reaches the log, which is
    /// where a change becomes something a recovery would bring back,
    /// rather than before the publish behind it: a commit is durable at
    /// its log sync and the fold that publishes it comes after, so a
    /// marker written at the fold leaves that window uncovered. Every
    /// path that commits calls this first, and the checkpoint calls it
    /// too, for the publishes that reach the file without a frame.
    ///
    /// The two syncs are what it costs: one here and one at the word
    /// that ends the transaction. A statement pays neither until it
    /// commits a second time, because one commit and the fold behind it
    /// are one header flip, which a crash either leaves whole or does
    /// not leave at all.
    pub fn keep_savepoint(&mut self) -> Result<()> {
        let due = self
            .savepoint
            .as_ref()
            .is_some_and(|held| !held.marked && (held.across_statements || held.published));
        if !due {
            return Ok(());
        }
        let held = self.savepoint.as_ref().expect("checked just above");
        let mut kept = held.db.encode();
        kept[TXN_FLOOR..TXN_FLOOR + 8].copy_from_slice(&held.log_floor.to_le_bytes());
        let crc = crc32c::crc32c(&kept[..TXN_FLOOR + 8]);
        kept[TXN_FLOOR + 8..TXN_FLOOR + 12].copy_from_slice(&crc.to_le_bytes());
        self.file.write_all_at(&kept, TXN_SLOT)?;
        self.file.sync_all()?;
        self.savepoint.as_mut().expect("checked just above").marked = true;
        Ok(())
    }

    /// Clears what [`Self::keep_savepoint`] wrote, which is what says no
    /// transaction is open on this file any more.
    fn forget_kept(&mut self) -> Result<()> {
        self.file.write_all_at(&[0u8; DB_HEADER_SIZE], TXN_SLOT)?;
        self.file.sync_all()
    }

    /// What a transaction the process died inside left behind, if this
    /// handle opened a file with one open on it.
    ///
    /// The header this handle is reading is already the one that
    /// transaction was holding, so a reader needs nothing else. A
    /// writer owes the file a publish, which is what
    /// [`Self::finish_rollback`] is, and owes the log a cut back to the
    /// floor, which is the one thing it cannot do itself.
    pub fn interrupted(&self) -> Option<Interrupted> {
        self.interrupted
    }

    /// Publishes the state a transaction the process died inside was
    /// holding, and says the transaction is over.
    ///
    /// Nothing is put back in memory, because opening a file with a
    /// transaction open on it reads the kept state to begin with. What
    /// is left is the file: the kept header goes in with an epoch past
    /// the one the crash left, so the next open reads it rather than
    /// the middle of the transaction, and only then is the kept state
    /// cleared. A crash anywhere in here leaves the marker down and the
    /// next open does the same thing again.
    pub fn finish_rollback(&mut self) -> Result<()> {
        let Some(open) = self.interrupted.take() else {
            return Ok(());
        };
        self.check_writable("the rollback a crash left behind")?;
        if open.published > self.db.epoch {
            self.db.epoch = open.published;
            self.checkpoint()?;
        }
        self.forget_kept()
    }

    /// Ends the transaction by putting the file back where it started
    /// and publishing that, so a reader after this reads what a reader
    /// before the transaction read.
    ///
    /// The epoch is the one thing not put back: the kept header is
    /// republished with the next epoch after the newest, because a
    /// header written with a lower epoch than the other slot would be
    /// the older of the two and the transaction's last state would win
    /// the next open. Blocks the transaction wrote past the kept
    /// high-water mark are outside the file the header describes, so the
    /// next writer hands them out again; blocks it freed are still
    /// referenced by the header going back in, so what it freed is
    /// dropped rather than published.
    pub fn rollback_savepoint(&mut self) -> Result<()> {
        let Some(saved) = self.savepoint.take() else {
            return Err(ZuError::InvalidArgument(format!(
                "{} is not inside a transaction",
                self.path.display()
            )));
        };
        let epoch = self.db.epoch;
        let published = epoch != saved.db.epoch;
        self.db = saved.db;
        self.free = saved.free;
        self.free_chain = saved.free_chain;
        self.pending_free.clear();
        // What the transaction freed is live again in the header going
        // back in, so it is dropped rather than published.
        self.frozen.clear();
        // Same for the blocks a checkpoint inside the transaction was
        // holding for a reader: the header going back in reaches them
        // again. What older checkpoints are holding is untouched, being
        // blocks that header does not reach either.
        let floor = self.db.epoch;
        self.retained.retain(|(epoch, _)| *epoch < floor);
        self.pin_memo = None;
        // A transaction that staged blocks and published nothing is
        // undone by forgetting them, and a header flip would only cost
        // an epoch to say the same thing. This is the common shape:
        // every statement holds the file for the length of itself, and
        // most of them do not write.
        if !published {
            return match saved.marked {
                true => self.forget_kept(),
                false => Ok(()),
            };
        }
        self.db.epoch = epoch;
        self.checkpoint()?;
        match saved.marked {
            true => self.forget_kept(),
            false => Ok(()),
        }
    }

    /// Takes the committed-free list out of allocation, forcing every
    /// allocation to extend past the high-water mark until
    /// [`Self::restore_free`] puts it back. The ingest commit protocol
    /// needs this: after a crash the committed free list still names
    /// its blocks free, so data whose only reference is a WAL frame
    /// must not live in them or the next fold could hand them out and
    /// overwrite it.
    pub(crate) fn take_free(&mut self) -> Vec<BlockPtr> {
        std::mem::take(&mut self.free)
    }

    /// Restores the free list taken by [`Self::take_free`].
    pub(crate) fn restore_free(&mut self, saved: Vec<BlockPtr>) {
        debug_assert!(self.free.is_empty(), "nested take_free");
        self.free = saved;
    }

    /// Syncs staged data writes without publishing anything: the header
    /// slots do not move and a plain reopen still sees the committed
    /// epoch. The ingest path calls this so its sealed segments are
    /// durable before the WAL frame referencing them is.
    pub fn sync_data(&mut self) -> Result<()> {
        self.file.sync_data()
    }

    /// Gives back for allocation the blocks a checkpoint listed as free
    /// that no reader is on any more, and answers how many.
    ///
    /// `floor` is the oldest epoch anything is reading, so an entry the
    /// checkpoint tagged with an epoch below it is reachable from
    /// nothing: every reader has moved past it. A caller with no
    /// readers passes `u64::MAX` and gets the whole list back, which is
    /// the single-connection case and is why one connection allocates
    /// exactly as it did before this existed.
    pub fn release_retained(&mut self, floor: u64) -> usize {
        let mut ready = Vec::new();
        self.retained.retain(|(epoch, blocks)| {
            if *epoch < floor {
                ready.extend_from_slice(blocks);
                return false;
            }
            true
        });
        let count = ready.len();
        // The same split a checkpoint makes: inside a transaction, a
        // block it froze stays out of allocation until the transaction
        // ends, whatever the readers are doing.
        match &mut self.savepoint {
            Some(saved) => {
                let (free, frozen): (Vec<BlockPtr>, Vec<BlockPtr>) =
                    ready.into_iter().partition(|p| saved.reusable.contains(p));
                self.free.extend(free);
                self.frozen.extend(frozen);
            }
            None => self.free.extend(ready),
        }
        count
    }

    /// Says that a caller above this handle counts the readers, so a
    /// checkpoint holds what it frees until that caller says which
    /// epoch is the oldest anything is reading. Set on the one handle
    /// of a file that writes; every other handle only reads.
    pub fn defer_reclaim(&mut self, on: bool) {
        self.defer_reclaim = on;
    }

    /// How many blocks are waiting on a reader. Nonzero here is a
    /// connection reading an epoch the writer has moved past, and the
    /// file growing rather than reusing while it does.
    pub fn retained_blocks(&self) -> usize {
        self.retained.iter().map(|(_, blocks)| blocks.len()).sum()
    }

    /// Marks `ptr` free. The committed epoch still references it, so it
    /// becomes allocatable only after the next checkpoint; until then its
    /// contents must survive a crash. Frees staged on a handle that closes
    /// without a checkpoint are dropped, which leaks the blocks until
    /// VACUUM rewrites the file, never corrupts it.
    pub fn free_block(&mut self, ptr: BlockPtr) -> Result<()> {
        self.check_ptr(ptr)?;
        self.pending_free.push(ptr);
        Ok(())
    }

    /// Whether this handle may write, which is false for one opened by
    /// [`Self::open_read_only`].
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Where this handle's file is, which is what a caller needs to
    /// name the sidecar beside it: docs/04 puts the WAL at `<db>.wal`,
    /// and a caller holding only the handle would otherwise have to
    /// carry the path alongside it and hope the two agree.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where this handle's files come from, which is the other half of
    /// what naming the sidecar takes: the name says where beside the
    /// database it goes, and this says which world that name is in.
    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        &self.vfs
    }

    /// Refuses the call when this handle is read-only. Every durable
    /// change goes through [`Self::write_block`] or [`Self::checkpoint`],
    /// so the two of them are the whole gate: a block nothing wrote and
    /// a header that never flipped leave the file as it was found.
    fn check_writable(&self, what: &'static str) -> Result<()> {
        if self.writable {
            return Ok(());
        }
        Err(ZuError::InvalidArgument(format!(
            "{what} on {}, which is open read-only",
            self.path.display()
        )))
    }

    /// Writes one full block at `ptr` and drops any cached frame for it,
    /// so the next read refills from the file.
    pub fn write_block(&mut self, ptr: BlockPtr, data: &[u8]) -> Result<()> {
        assert_eq!(data.len(), BLOCK_SIZE as usize, "blocks are fixed size");
        self.check_writable("write")?;
        self.check_ptr(ptr)?;
        self.pin_memo = None;
        self.cache.remove(ptr);
        // The free list recycles pointers, so a rewrite can hand a new
        // segment an old pool key; dropping the key here keeps the
        // pools honest the same way the line above keeps the cache.
        self.pools.csr_offsets.remove(ptr);
        self.pools.adjacency.remove(ptr);
        self.pools.fences.remove(ptr);
        self.file.write_all_at(data, ptr * u64::from(BLOCK_SIZE))
    }

    /// Pins the block at `ptr` in the cache. A warm pin is a map probe
    /// and an `Arc` clone, no allocation, no copy, no I/O; a miss reads
    /// the block once into a recycled frame. This is the read primitive
    /// everything else builds on.
    pub fn pin_block(&mut self, ptr: BlockPtr) -> Result<PinnedBlock> {
        self.check_ptr(ptr)?;
        if let Some((p, pin)) = &self.pin_memo
            && *p == ptr
        {
            return Ok(pin.clone());
        }
        let pin = match self.cache.get(ptr) {
            Some(pin) => pin,
            None => {
                let file = &mut self.file;
                self.cache.insert(ptr, |buf| {
                    file.read_exact_at(buf, ptr * u64::from(BLOCK_SIZE))
                })?
            }
        };
        self.pin_memo = Some((ptr, pin.clone()));
        Ok(pin)
    }

    /// Reads one full block at `ptr` into an owned copy. Cold paths and
    /// tests use this; hot readers pin instead.
    pub fn read_block(&mut self, ptr: BlockPtr) -> Result<Vec<u8>> {
        Ok(self.pin_block(ptr)?.to_vec())
    }

    /// Reads `len` bytes at `offset` inside the block at `ptr` into an
    /// owned copy. A cold call now faults the whole block into the
    /// cache, which is the perf/04 trade: the first point read in a
    /// block pays 256 KiB once so every later read in it pays nothing.
    pub fn read_block_slice(
        &mut self,
        ptr: BlockPtr,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>> {
        assert!(
            offset + len <= BLOCK_SIZE as usize,
            "slice crosses the block edge"
        );
        Ok(self.pin_block(ptr)?[offset..offset + len].to_vec())
    }

    /// Block cache hit, miss, and eviction counts for this handle's
    /// shared cache.
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    /// The shared decoded-object pools, cheap to clone per lookup.
    pub fn pools(&self) -> Arc<DecodedPools> {
        Arc::clone(&self.pools)
    }

    /// Rebuilds the caches sized off `memory_limit`: half of it block
    /// frames per perf/04, 8% CSR offsets, 20% adjacency, 4% fences.
    /// Forks made before the call keep the old caches.
    pub fn set_memory_limit(&mut self, memory_limit: usize) {
        self.cache = Arc::new(BlockCache::new(memory_limit / 2));
        self.pools = Arc::new(DecodedPools::new(memory_limit));
    }

    /// Publishes the staged state: persist the free list, fsync data, bump
    /// the epoch, write the header into the inactive slot, fsync again. A
    /// crash between the two syncs leaves the previous epoch intact.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.check_writable("checkpoint")?;
        // A transaction that is about to have something of it published
        // puts what it is holding on the file first, so that a crash
        // between here and the word that ends the transaction is the
        // rollback the transaction was promised.
        self.keep_savepoint()?;
        // Everything already free, everything freed this transaction, and
        // the old free-list chain itself are all unreferenced once this
        // checkpoint publishes, so they form the new list. Chain storage
        // is reserved from the committed-free prefix when it can hold the
        // whole chain: those blocks are safe to overwrite before the flip
        // (pending and old-chain blocks are not, the committed epoch still
        // reads them), and reusing them keeps repeated checkpoints from
        // growing the file. A reserved block cannot also appear in the
        // list, so reserved blocks are drained out before serializing.
        let committed = std::mem::take(&mut self.free);
        let safe = committed.len();
        let mut all = committed;
        // The two the comment above names as unsafe to overwrite before
        // the flip are the two a reader left on this epoch is unsafe to
        // overwrite after it, for the same reason: they are the blocks
        // the epoch being superseded reads. They are listed as free
        // like the rest and held back from allocation below.
        let superseded = self.db.epoch;
        let reached: IdSet<BlockPtr> = self
            .pending_free
            .iter()
            .chain(self.free_chain.iter())
            .copied()
            .collect();
        // Inside a transaction the free blocks it may not write into
        // are held aside rather than listed, so they come back here to
        // be listed: the epoch being published has let go of them, and
        // it is only allocation they stay out of.
        all.append(&mut self.frozen);
        all.append(&mut self.pending_free);
        all.append(&mut self.free_chain);
        self.db.free_list_root = if all.is_empty() {
            NULL_BLOCK
        } else {
            let mut c = 0usize;
            while (all.len() - c) * 8 > c * crate::meta::CHAIN_CAPACITY {
                c += 1;
            }
            if c <= safe && all.len() > c {
                self.free = all.drain(..c).collect();
            }
            let mut bytes = Vec::with_capacity(all.len() * 8);
            for ptr in &all {
                bytes.extend_from_slice(&ptr.to_le_bytes());
            }
            let root = crate::meta::write_chain(self, &bytes)?;
            // At an exact chain-capacity boundary one reserved block goes
            // unused. It stays allocatable and the next checkpoint lists
            // it; only a process that never checkpoints again leaks it,
            // and VACUUM reclaims that.
            all.append(&mut self.free);
            root
        };
        self.file.sync_all()?;
        self.db.epoch += 1;
        let slot = 1 - self.active_slot;
        let offset = (FILE_HEADER_SIZE + slot * DB_HEADER_SIZE) as u64;
        self.file.write_all_at(&self.db.encode(), offset)?;
        self.file.sync_all()?;
        self.active_slot = slot;
        let root = self.db.free_list_root;
        if root != NULL_BLOCK {
            self.free_chain = crate::meta::chain_blocks(self, root)?;
        }
        // The blocks the superseded epoch reaches wait for whoever is
        // reading it; the caller releases them when nobody is.
        let (held, all): (Vec<BlockPtr>, Vec<BlockPtr>) =
            all.into_iter().partition(|p| reached.contains(p));
        if !held.is_empty() {
            self.retained.push((superseded, held));
        }
        // Outside a transaction every free block is allocatable again.
        // Inside one the list splits: what was free before it began is
        // allocatable, and what it freed on the way is not, because the
        // state a rollback goes back to still reads it.
        match &mut self.savepoint {
            Some(saved) => {
                let (free, frozen) = all.into_iter().partition(|p| saved.reusable.contains(p));
                saved.published = true;
                self.free = free;
                self.frozen = frozen;
            }
            None => self.free = all,
        }
        // Nobody behind, nobody to wait for: what was just retained is
        // allocatable again before this call returns.
        if !self.defer_reclaim {
            self.release_retained(u64::MAX);
        }
        self.unpublished = 0;
        Ok(())
    }

    fn check_ptr(&self, ptr: BlockPtr) -> Result<()> {
        if ptr == NULL_BLOCK || ptr > self.db.block_count {
            return Err(ZuError::Corrupt {
                what: "block pointer",
                detail: format!("{ptr} out of range 1..={}", self.db.block_count),
            });
        }
        Ok(())
    }
}

/// Decodes a free-list chain payload: concatenated little-endian u64
/// block pointers, each nonnull, in range, and unique. `zu verify` uses
/// this too, so a corrupt list is an open error, not a later overwrite of
/// live data.
pub fn decode_free_list(bytes: &[u8], block_count: u64) -> Result<Vec<BlockPtr>> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "free list",
        detail,
    };
    if !bytes.len().is_multiple_of(8) {
        return Err(corrupt(format!(
            "payload of {} bytes is ragged",
            bytes.len()
        )));
    }
    let mut ptrs = Vec::with_capacity(bytes.len() / 8);
    for chunk in bytes.as_chunks::<8>().0 {
        let ptr = u64::from_le_bytes(*chunk);
        if ptr == NULL_BLOCK || ptr > block_count {
            return Err(corrupt(format!(
                "block {ptr} out of range 1..={block_count}"
            )));
        }
        ptrs.push(ptr);
    }
    let mut sorted = ptrs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != ptrs.len() {
        return Err(corrupt("duplicate block".to_string()));
    }
    Ok(ptrs)
}

/// Random database identity. Not cryptographic: mixed from the clock and
/// the process id, then stamped with the RFC 4122 v4 bits so external
/// tools display it as an ordinary UUID.
fn fresh_uuid() -> [u8; 16] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut x = nanos as u64
        ^ ((nanos >> 64) as u64).rotate_left(17)
        ^ u64::from(std::process::id()).rotate_left(32);
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(8) {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        chunk.copy_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out[6] = (out[6] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("test.zu1")
    }

    #[test]
    fn create_then_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let created = Zu1File::create(&path).unwrap();
        assert_eq!(created.db_header().epoch, 1);
        let opened = Zu1File::open(&path).unwrap();
        assert_eq!(opened.file_header(), created.file_header());
        assert_eq!(opened.db_header(), created.db_header());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            u64::from(BLOCK_SIZE),
            "minimum file is the header region padded to one block"
        );
    }

    #[test]
    fn create_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        Zu1File::create(&path).unwrap();
        assert!(Zu1File::create(&path).is_err());
    }

    #[test]
    fn uuid_is_stable_and_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let a = Zu1File::create(&dir.path().join("a.zu1")).unwrap();
        let b = Zu1File::create(&dir.path().join("b.zu1")).unwrap();
        assert_ne!(a.file_header().uuid, b.file_header().uuid);
        let a2 = Zu1File::open(&dir.path().join("a.zu1")).unwrap();
        assert_eq!(a2.file_header().uuid, a.file_header().uuid);
    }

    #[test]
    fn rejects_bad_magic_and_bad_crc() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        Zu1File::create(&path).unwrap();
        let good = std::fs::read(&path).unwrap();

        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(Zu1File::open(&path), Err(ZuError::Corrupt { .. })));

        let mut bad = good.clone();
        bad[20] ^= 0x01; // uuid byte: crc must catch it
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(Zu1File::open(&path), Err(ZuError::Corrupt { .. })));
    }

    #[test]
    fn rejects_future_min_reader_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        Zu1File::create(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[10..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let crc = crc32c::crc32c(&bytes[0..64]);
        bytes[64..68].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Zu1File::open(&path),
            Err(ZuError::Unsupported { .. })
        ));
    }

    #[test]
    fn checkpoint_alternates_slots_and_survives_a_torn_flip() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        db.db_header_mut().wal_seq = 7;
        db.checkpoint().unwrap();
        assert_eq!(db.db_header().epoch, 2);
        drop(db);
        let opened = Zu1File::open(&path).unwrap();
        assert_eq!(opened.db_header().epoch, 2);
        assert_eq!(opened.db_header().wal_seq, 7);
        drop(opened);

        // Epoch 2 lives in slot B. Tear it: the file must fall back to
        // epoch 1 from slot A instead of failing.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[FILE_HEADER_SIZE + DB_HEADER_SIZE + 3] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let recovered = Zu1File::open(&path).unwrap();
        assert_eq!(recovered.db_header().epoch, 1);
        assert_eq!(recovered.db_header().wal_seq, 0);
    }

    #[test]
    fn both_headers_torn_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        Zu1File::create(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[FILE_HEADER_SIZE + 3] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(Zu1File::open(&path), Err(ZuError::Corrupt { .. })));
    }

    #[test]
    fn a_block_count_too_big_to_address_is_not_a_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        Zu1File::create(&path).unwrap();

        // Slot B gets a header that is correct in every way the reader
        // used to check: the crc is over the bytes that are there and
        // the epoch beats anything a real writer will reach. Only the
        // block count is impossible, and a block at the top of it has
        // no byte offset to read from. Found by the zu1_file fuzzer.
        let doctored = DatabaseHeader {
            epoch: u64::MAX,
            block_count: u64::MAX / 2,
            ..Default::default()
        };
        let at = FILE_HEADER_SIZE + DB_HEADER_SIZE;
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[at..at + DB_HEADER_SIZE].copy_from_slice(&doctored.encode());
        std::fs::write(&path, &bytes).unwrap();

        let opened = Zu1File::open(&path).unwrap();
        assert_eq!(opened.db_header().epoch, 1, "slot A should win");
    }

    #[test]
    fn block_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&temp_path(&dir)).unwrap();
        let ptr = db.allocate_block();
        assert_eq!(ptr, 1);
        let mut data = vec![0u8; BLOCK_SIZE as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        db.write_block(ptr, &data).unwrap();
        assert_eq!(db.read_block(ptr).unwrap(), data);
        assert!(db.read_block(2).is_err());
        assert!(db.read_block(NULL_BLOCK).is_err());
    }

    #[test]
    fn freed_blocks_recycle_only_after_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&temp_path(&dir)).unwrap();
        let a = db.allocate_block();
        let b = db.allocate_block();
        db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
        db.write_block(b, &vec![2; BLOCK_SIZE as usize]).unwrap();
        db.free_block(a).unwrap();
        // Same transaction: the committed epoch could still need block a,
        // so allocation must extend the file instead.
        assert_eq!(db.allocate_block(), 3);
        db.checkpoint().unwrap();
        // Published: a is genuinely free now.
        assert_eq!(db.allocate_block(), a);
        assert!(db.free_block(NULL_BLOCK).is_err());
        assert!(db.free_block(99).is_err());
    }

    /// With somebody reading behind the writer, a checkpoint lists a
    /// freed block as free and still does not allocate into it, because
    /// the epoch being read is the epoch that reaches it.
    #[test]
    fn a_retained_block_waits_for_the_reader_of_the_epoch_that_reaches_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&temp_path(&dir)).unwrap();
        db.defer_reclaim(true);
        let a = db.allocate_block();
        db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
        db.checkpoint().unwrap();
        let reading = db.db_header().epoch;
        db.free_block(a).unwrap();
        db.checkpoint().unwrap();
        assert_eq!(db.retained_blocks(), 1, "held for the reader");
        assert_eq!(db.release_retained(reading), 0, "the reader is on it");
        assert_ne!(db.allocate_block(), a, "so the file grows instead");
        assert_eq!(db.release_retained(reading + 1), 1, "the reader left");
        assert_eq!(db.retained_blocks(), 0);
        assert_eq!(db.allocate_block(), a, "and the block is reused");
    }

    #[test]
    fn free_list_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let (a, b);
        {
            let mut db = Zu1File::create(&path).unwrap();
            a = db.allocate_block();
            b = db.allocate_block();
            db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
            db.write_block(b, &vec![2; BLOCK_SIZE as usize]).unwrap();
            db.free_block(a).unwrap();
            db.free_block(b).unwrap();
            db.checkpoint().unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut got = [db.allocate_block(), db.allocate_block()];
        got.sort_unstable();
        assert_eq!(got, [a, b]);
        // Both free blocks are handed out; the next one extends the file
        // past the free-list chain block.
        assert_eq!(db.allocate_block(), db.db_header().block_count);
    }

    /// A transaction that publishes two epochs of its own and is then
    /// rolled back leaves the file reading what it read before, and
    /// leaves it reading that after a reopen rather than only in the
    /// handle that rolled back.
    #[test]
    fn a_rollback_puts_back_what_the_transaction_published_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let before = db.allocate_block();
        db.write_block(before, &vec![7; BLOCK_SIZE as usize])
            .unwrap();
        db.db_header_mut().catalog_root = before;
        db.checkpoint().unwrap();
        let epoch = db.db_header().epoch;

        db.begin_savepoint(false, 0).unwrap();
        assert!(db.in_savepoint());
        for fill in [8u8, 9] {
            let root = db.allocate_block();
            db.write_block(root, &vec![fill; BLOCK_SIZE as usize])
                .unwrap();
            // What a fold does: write the new segment, hand the old one
            // back, publish.
            let old = db.db_header().catalog_root;
            db.free_block(old).unwrap();
            db.db_header_mut().catalog_root = root;
            db.checkpoint().unwrap();
        }
        assert!(db.db_header().epoch > epoch, "the statements published");
        db.rollback_savepoint().unwrap();
        assert!(!db.in_savepoint());
        assert_eq!(db.db_header().catalog_root, before);
        assert_eq!(db.read_block(before).unwrap()[0], 7, "and it still reads");

        let mut reopened = Zu1File::open(&path).unwrap();
        assert_eq!(reopened.db_header().catalog_root, before);
        assert_eq!(reopened.read_block(before).unwrap()[0], 7);
        assert!(
            reopened.db_header().epoch > db.db_header().epoch - 1,
            "the rollback published an epoch of its own"
        );
    }

    /// The other end: a transaction that commits keeps everything it
    /// published, and the blocks it freed on the way are free again.
    #[test]
    fn a_released_savepoint_keeps_what_the_transaction_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let first = db.allocate_block();
        db.write_block(first, &vec![7; BLOCK_SIZE as usize])
            .unwrap();
        db.db_header_mut().catalog_root = first;
        db.checkpoint().unwrap();

        db.begin_savepoint(false, 0).unwrap();
        let second = db.allocate_block();
        db.write_block(second, &vec![8; BLOCK_SIZE as usize])
            .unwrap();
        db.free_block(first).unwrap();
        db.db_header_mut().catalog_root = second;
        db.checkpoint().unwrap();
        db.release_savepoint().unwrap();

        assert_eq!(db.db_header().catalog_root, second);
        assert_eq!(
            db.allocate_block(),
            first,
            "the block the transaction freed is allocatable again"
        );
        let reopened = Zu1File::open(&path).unwrap();
        assert_eq!(reopened.db_header().catalog_root, second);
    }

    /// What a savepoint costs a statement, which is nothing until the
    /// statement publishes twice. One publish is one header flip and a
    /// crash either leaves it or does not, so there is nothing for a
    /// marker to say; two publishes is a state a crash can land in the
    /// middle of, and from there on the file carries where to go back
    /// to.
    #[test]
    fn a_statement_writes_the_marker_only_once_it_has_published_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let kept = db.db_header().clone();

        db.begin_savepoint(false, 4).unwrap();
        db.checkpoint().unwrap();
        assert_eq!(marker(&path), None, "one publish needs no marker");

        db.checkpoint().unwrap();
        assert_eq!(
            marker(&path),
            Some((kept, 4)),
            "the second publish is one a crash could land before"
        );

        db.release_savepoint().unwrap();
        assert_eq!(marker(&path), None, "and the word at the end clears it");
    }

    /// An explicit transaction pays at the first publish instead,
    /// because a statement of it that has published is already
    /// something a crash has to take back.
    #[test]
    fn a_transaction_writes_the_marker_before_it_publishes_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let kept = db.db_header().clone();

        db.begin_savepoint(true, 7).unwrap();
        assert_eq!(marker(&path), None, "nothing published, nothing to keep");
        db.checkpoint().unwrap();
        assert_eq!(marker(&path), Some((kept, 7)));

        db.rollback_savepoint().unwrap();
        assert_eq!(marker(&path), None, "and the rollback clears it too");
    }

    /// Opening a file a transaction was open on reads the state that
    /// transaction was holding rather than the middle of it, and says
    /// which epoch the crash left so a writer can publish over it.
    #[test]
    fn opening_a_file_with_a_transaction_open_on_it_reads_what_it_was_holding() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let first = db.allocate_block();
        db.write_block(first, &vec![7; BLOCK_SIZE as usize])
            .unwrap();
        db.db_header_mut().catalog_root = first;
        db.checkpoint().unwrap();
        let kept = db.db_header().clone();

        db.begin_savepoint(true, 3).unwrap();
        let second = db.allocate_block();
        db.write_block(second, &vec![8; BLOCK_SIZE as usize])
            .unwrap();
        db.db_header_mut().catalog_root = second;
        db.checkpoint().unwrap();
        let died_at = db.db_header().epoch;
        // The process stops here: no rollback, no release, and the
        // handle never gets to do anything about it.
        std::mem::forget(db);

        let mut crashed = Zu1File::open(&path).unwrap();
        assert_eq!(crashed.db_header().catalog_root, first);
        assert_eq!(
            crashed.interrupted(),
            Some(Interrupted {
                published: died_at,
                log_floor: 3,
            }),
        );
        assert_eq!(crashed.read_block(first).unwrap()[0], 7);

        crashed.finish_rollback().unwrap();
        assert_eq!(crashed.interrupted(), None);
        assert!(
            crashed.db_header().epoch > died_at,
            "the state going back in is published over the one the crash left"
        );
        assert_eq!(marker(&path), None);

        let reopened = Zu1File::open(&path).unwrap();
        assert_eq!(reopened.db_header().catalog_root, kept.catalog_root);
        assert_eq!(reopened.interrupted(), None);
    }

    /// The state an open transaction is holding and the floor it began
    /// at, read off the file the way the next open reads them.
    fn marker(path: &Path) -> Option<(DatabaseHeader, Epoch)> {
        let bytes = std::fs::read(path).unwrap();
        let at = TXN_SLOT as usize;
        decode_marker(&bytes[at..at + DB_HEADER_SIZE])
    }

    /// One transaction at a time, and a rollback outside one is a
    /// caller mistake rather than a checkpoint.
    #[test]
    fn a_savepoint_does_not_nest_and_a_rollback_needs_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        assert!(db.rollback_savepoint().is_err());
        db.begin_savepoint(false, 0).unwrap();
        assert!(db.begin_savepoint(false, 0).is_err());
        db.rollback_savepoint().unwrap();
        db.begin_savepoint(false, 0).unwrap();
    }

    /// A block freed inside a transaction is not handed out inside it,
    /// because the state a rollback goes back to still reads it.
    #[test]
    fn a_transaction_does_not_reuse_what_it_freed() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let a = db.allocate_block();
        db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
        db.checkpoint().unwrap();
        db.begin_savepoint(false, 0).unwrap();
        db.free_block(a).unwrap();
        db.checkpoint().unwrap();
        assert_ne!(db.allocate_block(), a, "still held for the rollback");
        db.release_savepoint().unwrap();
        assert_eq!(db.allocate_block(), a, "and free once it is not");
    }

    /// The other half of that rule, and the one that keeps a write
    /// statement from growing the file: a block that was already free
    /// when the transaction began is free in the state a rollback goes
    /// back to as well, so the transaction may write into it.
    #[test]
    fn a_transaction_reuses_what_was_free_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        // Two of them, because the free-list chain takes one of the
        // blocks a checkpoint finds free and the point here is the
        // other one.
        let (a, b) = (db.allocate_block(), db.allocate_block());
        for ptr in [a, b] {
            db.write_block(ptr, &vec![1; BLOCK_SIZE as usize]).unwrap();
            db.free_block(ptr).unwrap();
        }
        db.checkpoint().unwrap();
        let watermark = db.db_header().block_count;

        db.begin_savepoint(false, 0).unwrap();
        let got = db.allocate_block();
        assert!(got == a || got == b, "free before, so free to write");
        db.write_block(got, &vec![2; BLOCK_SIZE as usize]).unwrap();
        db.checkpoint().unwrap();
        assert_eq!(
            db.db_header().block_count,
            watermark,
            "and the file did not grow to say it"
        );
        db.rollback_savepoint().unwrap();
        assert_eq!(db.db_header().block_count, watermark);
    }

    #[test]
    fn free_list_chain_recycles_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let a = db.allocate_block();
        db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
        db.free_block(a).unwrap();
        db.checkpoint().unwrap();
        // Every checkpoint rewrites the one-block chain, recycling the old
        // chain block, so repeated checkpoints cannot grow the file.
        let watermark = db.db_header().block_count;
        for _ in 0..5 {
            db.checkpoint().unwrap();
        }
        assert_eq!(db.db_header().block_count, watermark);
    }

    #[test]
    fn corrupt_free_list_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let chain_ptr;
        {
            let mut db = Zu1File::create(&path).unwrap();
            let a = db.allocate_block();
            db.write_block(a, &vec![1; BLOCK_SIZE as usize]).unwrap();
            db.free_block(a).unwrap();
            db.checkpoint().unwrap();
            chain_ptr = db.db_header().free_list_root;
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[(chain_ptr * u64::from(BLOCK_SIZE)) as usize + 20] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(Zu1File::open(&path), Err(ZuError::Corrupt { .. })));
    }

    #[test]
    fn decode_free_list_rejects_bad_payloads() {
        assert!(decode_free_list(&[0u8; 7], 10).is_err(), "ragged length");
        assert!(
            decode_free_list(&0u64.to_le_bytes(), 10).is_err(),
            "null block"
        );
        assert!(
            decode_free_list(&11u64.to_le_bytes(), 10).is_err(),
            "past the high-water mark"
        );
        let mut dup = Vec::new();
        dup.extend_from_slice(&3u64.to_le_bytes());
        dup.extend_from_slice(&3u64.to_le_bytes());
        assert!(decode_free_list(&dup, 10).is_err(), "duplicate");
        let mut good = Vec::new();
        good.extend_from_slice(&3u64.to_le_bytes());
        good.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(decode_free_list(&good, 10).unwrap(), vec![3, 7]);
    }

    #[test]
    fn unpublished_blocks_stay_invisible_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let ptr = db.allocate_block();
        db.write_block(ptr, &vec![0xAB; BLOCK_SIZE as usize])
            .unwrap();
        // No checkpoint: the reopened file must not know about the block.
        drop(db);
        let mut reopened = Zu1File::open(&path).unwrap();
        assert_eq!(reopened.db_header().block_count, 0);
        assert!(reopened.read_block(ptr).is_err());
    }

    /// The worker-fork handle is the opposite of a fresh open: it
    /// adopts the caller's in-memory header, so staged blocks that no
    /// checkpoint has published yet read back through it.
    #[test]
    fn reopen_carries_staged_state_to_a_second_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut db = Zu1File::create(&path).unwrap();
        let ptr = db.allocate_block();
        let data = vec![0xCD; BLOCK_SIZE as usize];
        db.write_block(ptr, &data).unwrap();
        let mut fork = db.reopen().unwrap();
        assert_eq!(fork.db_header(), db.db_header());
        assert_eq!(fork.read_block(ptr).unwrap(), data);
    }
}
