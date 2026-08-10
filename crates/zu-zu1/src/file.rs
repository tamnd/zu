//! File headers, block I/O, and the dual-header checkpoint protocol.
//!
//! The layout is `docs/04-storage-zu1-format.md` §1: a write-once 4 KiB
//! FileHeader, two alternating 4 KiB DatabaseHeaders, padding to 256 KiB,
//! then fixed 256 KiB blocks. Opening reads 12 KiB and picks the valid
//! header with the highest epoch, so a torn header write never damages
//! committed state; new data goes to free blocks and becomes visible only
//! when the next header flip publishes it.

use std::path::Path;

use zu_common::{Result, ZuError};

use crate::vfs::{RealFile, VfsFile};
use crate::{BLOCK_SIZE, FORMAT_VERSION, MAGIC, MIN_READER_VERSION};

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

/// Write-once identity of a zu1 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub format_version: u16,
    pub min_reader_version: u16,
    pub block_size: u32,
    pub uuid: [u8; 16],
    pub flags: u64,
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
        Some(Self {
            epoch: word(0),
            catalog_root: word(1),
            table_index_root: word(2),
            free_list_root: word(3),
            block_count: word(4),
            wal_seq: word(5),
            stats_root: word(6),
        })
    }
}

/// An open zu1 file: block I/O, the free list, and the header flip.
#[derive(Debug)]
pub struct Zu1File {
    file: Box<dyn VfsFile>,
    /// Where this handle was opened, kept so [`Self::reopen`] can hand
    /// a second read handle to a query worker. Block reads seek, so
    /// workers cannot share one file descriptor.
    path: std::path::PathBuf,
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
}

impl Zu1File {
    /// Creates a new database file. Fails if `path` already exists, so an
    /// existing database is never silently clobbered.
    pub fn create(path: &Path) -> Result<Self> {
        Self::create_on(Box::new(RealFile::create_new(path)?), path)
    }

    /// [`Self::create`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn create_on(mut file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
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
            file_header,
            db,
            active_slot: 0,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
        })
    }

    /// Opens an existing database: read 12 KiB, validate the file header,
    /// and adopt the valid database header with the highest epoch.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_on(Box::new(RealFile::open_rw(path)?), path)
    }

    /// [`Self::open`] on an explicit file handle; the crash harness
    /// passes a recording one.
    pub fn open_on(mut file: Box<dyn VfsFile>, path: &Path) -> Result<Self> {
        let mut head = [0u8; FILE_HEADER_SIZE + 2 * DB_HEADER_SIZE];
        file.read_exact_at(&mut head, 0)?;
        let file_header = FileHeader::decode(&head)?;
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
                    detail: "no valid header in either slot".to_string(),
                });
            }
        };
        let mut this = Self {
            file,
            path: path.to_path_buf(),
            file_header,
            db,
            active_slot,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
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
    /// read; the morsel workers are the caller.
    pub fn reopen(&self) -> Result<Self> {
        Ok(Self {
            file: Box::new(RealFile::open_rw(&self.path)?),
            path: self.path.clone(),
            file_header: self.file_header.clone(),
            db: self.db.clone(),
            active_slot: self.active_slot,
            free: Vec::new(),
            pending_free: Vec::new(),
            free_chain: Vec::new(),
        })
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

    /// Returns a block to write into: a committed-free block when one
    /// exists, otherwise one past the high-water mark. Either way the
    /// block becomes durable state only via the next checkpoint.
    pub fn allocate_block(&mut self) -> BlockPtr {
        if let Some(ptr) = self.free.pop() {
            return ptr;
        }
        self.db.block_count += 1;
        self.db.block_count
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

    /// Writes one full block at `ptr`.
    pub fn write_block(&mut self, ptr: BlockPtr, data: &[u8]) -> Result<()> {
        assert_eq!(data.len(), BLOCK_SIZE as usize, "blocks are fixed size");
        self.check_ptr(ptr)?;
        self.file.write_all_at(data, ptr * u64::from(BLOCK_SIZE))
    }

    /// Reads one full block at `ptr`.
    pub fn read_block(&mut self, ptr: BlockPtr) -> Result<Vec<u8>> {
        self.check_ptr(ptr)?;
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        self.file
            .read_exact_at(&mut buf, ptr * u64::from(BLOCK_SIZE))?;
        Ok(buf)
    }

    /// Reads `len` bytes at `offset` inside the block at `ptr`. The point
    /// access path uses this so a random read touches bytes on the order
    /// of one encoded chunk instead of one 256 KiB block.
    pub fn read_block_slice(
        &mut self,
        ptr: BlockPtr,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>> {
        self.check_ptr(ptr)?;
        assert!(
            offset + len <= BLOCK_SIZE as usize,
            "slice crosses the block edge"
        );
        let mut buf = vec![0u8; len];
        self.file
            .read_exact_at(&mut buf, ptr * u64::from(BLOCK_SIZE) + offset as u64)?;
        Ok(buf)
    }

    /// Publishes the staged state: persist the free list, fsync data, bump
    /// the epoch, write the header into the inactive slot, fsync again. A
    /// crash between the two syncs leaves the previous epoch intact.
    pub fn checkpoint(&mut self) -> Result<()> {
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
        self.free = all;
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
    for chunk in bytes.chunks_exact(8) {
        let ptr = u64::from_le_bytes(chunk.try_into().unwrap());
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
