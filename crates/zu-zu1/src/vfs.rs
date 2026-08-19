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
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use zu_common::{Result, ZuError};

/// Opens `path` through `how` and puts the path in the message when the
/// open fails.
///
/// The operating system says "No such file or directory" and nothing
/// about which one, which is the difference between an error a caller
/// can act on and an error they have to reproduce under a debugger to
/// understand. The kind is carried across unchanged, so code that asks
/// [`std::io::Error::kind`] still gets its answer; only the text grows.
fn opened(how: &OpenOptions, path: &Path) -> Result<File> {
    how.open(path).map_err(|error| {
        ZuError::Io(std::io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        ))
    })
}

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

    /// A second handle on the same file.
    ///
    /// Group commit is what wants this: the sync that makes a batch of
    /// commits durable runs while the writer that staged them has
    /// already let go of the log, so it cannot be the same handle. A
    /// sync is a promise about the file rather than about the
    /// descriptor, so syncing this one makes durable what was written
    /// through the other.
    fn dup(&self) -> Result<Box<dyn VfsFile>>;
}

/// The production implementation: every call is the syscall it names.
#[derive(Debug)]
pub struct RealFile(File);

impl RealFile {
    /// Creates the file, failing when it already exists.
    pub fn create_new(path: &Path) -> Result<Self> {
        Ok(Self(opened(
            OpenOptions::new().read(true).write(true).create_new(true),
            path,
        )?))
    }

    /// Opens an existing file for reading and writing.
    pub fn open_rw(path: &Path) -> Result<Self> {
        Ok(Self(opened(
            OpenOptions::new().read(true).write(true),
            path,
        )?))
    }

    /// Opens an existing file for reading only, so the operating
    /// system refuses a write this process should not have attempted
    /// and a database on a read-only mount opens at all.
    pub fn open_r(path: &Path) -> Result<Self> {
        Ok(Self(opened(OpenOptions::new().read(true), path)?))
    }

    /// Opens for reading and writing, creating when missing, the WAL
    /// contract.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        Ok(Self(opened(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false),
            path,
        )?))
    }
}

impl VfsFile for RealFile {
    #[cfg(unix)]
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()> {
        Ok(FileExt::read_exact_at(&self.0, buf, offset)?)
    }

    #[cfg(unix)]
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        Ok(FileExt::write_all_at(&self.0, buf, offset)?)
    }

    // Windows has no pread/pwrite with exact semantics: seek_read and
    // seek_write may transfer fewer bytes than asked, so loop until the
    // buffer is done. They also move the handle's file pointer, which
    // is harmless here because every read and write in this crate goes
    // through these positioned calls.
    #[cfg(windows)]
    fn read_exact_at(&mut self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            match self.0.seek_read(buf, offset) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "read past end of file",
                    )
                    .into());
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    fn write_all_at(&mut self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            match self.0.seek_write(buf, offset) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "wrote zero bytes",
                    )
                    .into());
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
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

    fn dup(&self) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RealFile(self.0.try_clone()?)))
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

    /// The copy records onto the same log, so a sync issued through it
    /// lands in the event stream where it happened and the crash
    /// harness sees a group commit exactly as it sees any other.
    fn dup(&self) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RecordingFile {
            inner: RealFile(self.inner.0.try_clone()?),
            id: self.id,
            log: Arc::clone(&self.log),
        }))
    }
}
