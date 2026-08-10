//! The file shim under the storage layer.
//!
//! [`Zu1File`](crate::file::Zu1File) and [`Wal`](crate::wal::Wal) do
//! their I/O through [`VfsFile`], whose methods are exactly the
//! syscalls the engine issues: positioned reads and writes, length
//! changes, and the two sync flavors. Production code runs on
//! [`RealFile`], a thin pass-through. The crash harness runs on
//! [`RecordingFile`], which logs every mutating call before forwarding
//! it, so a test can cut the log at any syscall boundary, rebuild the
//! file image a crash at that point could leave, and check recovery
//! against it. Reads are not logged; a crash cannot lose a read.

use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use zu_common::Result;

/// The syscall surface of one open file. `len` is fstat, hence the
/// `Result`; emptiness is the caller's question to ask of the value.
#[allow(clippy::len_without_is_empty)]
pub trait VfsFile: Debug + Send {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()>;
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()>;
    fn set_len(&mut self, len: u64) -> Result<()>;
    fn sync_all(&mut self) -> Result<()>;
    fn sync_data(&mut self) -> Result<()>;
    fn len(&self) -> Result<u64>;
}

/// The production implementation: every call is the syscall it names.
#[derive(Debug)]
pub struct RealFile(File);

impl RealFile {
    /// Creates the file, failing when it already exists.
    pub fn create_new(path: &Path) -> Result<Self> {
        Ok(Self(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(path)?,
        ))
    }

    /// Opens an existing file for reading and writing.
    pub fn open_rw(path: &Path) -> Result<Self> {
        Ok(Self(OpenOptions::new().read(true).write(true).open(path)?))
    }

    /// Opens for reading and writing, creating when missing, the WAL
    /// contract.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        Ok(Self(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?,
        ))
    }
}

impl VfsFile for RealFile {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()> {
        Ok(self.0.read_exact_at(buf, offset)?)
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        Ok(self.0.write_all_at(buf, offset)?)
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        Ok(self.0.set_len(len)?)
    }

    fn sync_all(&mut self) -> Result<()> {
        Ok(self.0.sync_all()?)
    }

    fn sync_data(&mut self) -> Result<()> {
        Ok(self.0.sync_data()?)
    }

    fn len(&self) -> Result<u64> {
        Ok(self.0.metadata()?.len())
    }
}

/// Which file an [`IoEvent`] belongs to. Sync barriers are per file:
/// syncing the database never makes an unsynced WAL write durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoFile {
    Db,
    Wal,
}

/// One mutating syscall, in issue order across both files.
#[derive(Debug, Clone)]
pub enum IoEvent {
    Write {
        file: IoFile,
        offset: u64,
        bytes: Vec<u8>,
    },
    SetLen {
        file: IoFile,
        len: u64,
    },
    Sync {
        file: IoFile,
    },
}

/// The shared log a workload's recording files append to.
pub type IoLog = Arc<Mutex<Vec<IoEvent>>>;

/// A pass-through that logs every mutating syscall before issuing it.
#[derive(Debug)]
pub struct RecordingFile {
    inner: RealFile,
    id: IoFile,
    log: IoLog,
}

impl RecordingFile {
    pub fn new(inner: RealFile, id: IoFile, log: IoLog) -> Self {
        Self { inner, id, log }
    }
}

impl VfsFile for RecordingFile {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.inner.read_exact_at(buf, offset)
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.log.lock().unwrap().push(IoEvent::Write {
            file: self.id,
            offset,
            bytes: buf.to_vec(),
        });
        self.inner.write_all_at(buf, offset)
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(IoEvent::SetLen { file: self.id, len });
        self.inner.set_len(len)
    }

    fn sync_all(&mut self) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(IoEvent::Sync { file: self.id });
        self.inner.sync_all()
    }

    fn sync_data(&mut self) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(IoEvent::Sync { file: self.id });
        self.inner.sync_data()
    }

    fn len(&self) -> Result<u64> {
        self.inner.len()
    }
}
