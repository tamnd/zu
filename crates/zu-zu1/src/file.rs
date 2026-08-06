//! File headers, block I/O, and the dual-header checkpoint protocol.
//!
//! The layout is `docs/04-storage-zu1-format.md` §1: a write-once 4 KiB
//! FileHeader, two alternating 4 KiB DatabaseHeaders, padding to 256 KiB,
//! then fixed 256 KiB blocks. Opening reads 12 KiB and picks the valid
//! header with the highest epoch, so a torn header write never damages
//! committed state; new data goes to free blocks and becomes visible only
//! when the next header flip publishes it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use zu_common::{Result, ZuError};

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

/// An open zu1 file: block I/O plus the header flip.
#[derive(Debug)]
pub struct Zu1File {
    file: File,
    file_header: FileHeader,
    db: DatabaseHeader,
    /// Slot the current header was read from; the flip writes the other one.
    active_slot: usize,
}

impl Zu1File {
    /// Creates a new database file. Fails if `path` already exists, so an
    /// existing database is never silently clobbered.
    pub fn create(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let file_header = FileHeader::fresh();
        let db = DatabaseHeader {
            epoch: 1,
            ..DatabaseHeader::default()
        };
        file.write_all(&file_header.encode())?;
        file.write_all(&db.encode())?;
        // Slot B stays zeroed; an all-zero slot never passes its crc.
        file.set_len(u64::from(BLOCK_SIZE))?;
        file.sync_all()?;
        Ok(Self {
            file,
            file_header,
            db,
            active_slot: 0,
        })
    }

    /// Opens an existing database: read 12 KiB, validate the file header,
    /// and adopt the valid database header with the highest epoch.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut head = [0u8; FILE_HEADER_SIZE + 2 * DB_HEADER_SIZE];
        file.read_exact(&mut head)?;
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
        Ok(Self {
            file,
            file_header,
            db,
            active_slot,
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

    /// Extends the high-water mark by one block and returns its pointer.
    /// The block becomes durable state only via the next checkpoint.
    pub fn allocate_block(&mut self) -> BlockPtr {
        self.db.block_count += 1;
        self.db.block_count
    }

    /// Writes one full block at `ptr`.
    pub fn write_block(&mut self, ptr: BlockPtr, data: &[u8]) -> Result<()> {
        assert_eq!(data.len(), BLOCK_SIZE as usize, "blocks are fixed size");
        self.check_ptr(ptr)?;
        self.file
            .seek(SeekFrom::Start(ptr * u64::from(BLOCK_SIZE)))?;
        self.file.write_all(data)?;
        Ok(())
    }

    /// Reads one full block at `ptr`.
    pub fn read_block(&mut self, ptr: BlockPtr) -> Result<Vec<u8>> {
        self.check_ptr(ptr)?;
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        self.file
            .seek(SeekFrom::Start(ptr * u64::from(BLOCK_SIZE)))?;
        self.file.read_exact(&mut buf)?;
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
            .seek(SeekFrom::Start(ptr * u64::from(BLOCK_SIZE) + offset as u64))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Publishes the staged state: fsync data, bump the epoch, write the
    /// header into the inactive slot, fsync again. A crash between the two
    /// syncs leaves the previous epoch intact.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.file.sync_all()?;
        self.db.epoch += 1;
        let slot = 1 - self.active_slot;
        let offset = (FILE_HEADER_SIZE + slot * DB_HEADER_SIZE) as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&self.db.encode())?;
        self.file.sync_all()?;
        self.active_slot = slot;
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
}
