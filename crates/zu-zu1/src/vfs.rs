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
//!
//! [`Vfs`] is the other half: where [`VfsFile`] is one open file, a
//! [`Vfs`] is where files come from. A database is not one file but
//! two, the base and the sidecar log, and the second is opened by name
//! from the first's path long after the first was opened. So a database
//! that lives somewhere other than the filesystem cannot be a handle
//! passed in at the open; it has to be a thing that answers a path.
//! [`RealVfs`] is the filesystem and is what every open uses unless it
//! was told otherwise. [`MemVfs`] is a map from path to bytes, which is
//! what a database with no file on disk is made of.

use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
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

/// Where files come from.
///
/// The four opens are the four the engine issues, named for what they
/// promise rather than for the flags they would pass: a create that
/// refuses to clobber, a read-write open of something that is already
/// there, a read-only one, and the log's open-or-create. `exists` and
/// `remove` are the two questions asked about a file without opening
/// it, both of them about the sidecar log.
///
/// Implementations are shared by every handle on a database, so this is
/// `Send + Sync` and takes `&self`.
pub trait Vfs: Debug + Send + Sync {
    fn create_new(&self, path: &Path) -> Result<Box<dyn VfsFile>>;
    fn open_rw(&self, path: &Path) -> Result<Box<dyn VfsFile>>;
    fn open_r(&self, path: &Path) -> Result<Box<dyn VfsFile>>;
    fn open_or_create(&self, path: &Path) -> Result<Box<dyn VfsFile>>;
    fn exists(&self, path: &Path) -> bool;
    fn remove(&self, path: &Path) -> Result<()>;

    /// Whether what this hands back survives the process. A database on
    /// one that says no has nothing to recover after a crash, and the
    /// work done to make it recoverable is work nobody will collect.
    fn durable(&self) -> bool {
        true
    }
}

/// The filesystem, which is where a database lives unless it was told
/// otherwise.
#[derive(Debug, Default)]
pub struct RealVfs;

impl RealVfs {
    /// The one every open uses when nobody passed one in.
    pub fn shared() -> Arc<dyn Vfs> {
        static REAL: std::sync::OnceLock<Arc<dyn Vfs>> = std::sync::OnceLock::new();
        Arc::clone(REAL.get_or_init(|| Arc::new(RealVfs)))
    }
}

impl Vfs for RealVfs {
    fn create_new(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RealFile::create_new(path)?))
    }

    fn open_rw(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RealFile::open_rw(path)?))
    }

    fn open_r(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RealFile::open_r(path)?))
    }

    fn open_or_create(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(RealFile::open_or_create(path)?))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ZuError::Io(std::io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ))),
        }
    }
}

/// The bytes of one file that is not on a disk.
type Image = Arc<Mutex<Vec<u8>>>;

/// A filesystem made of memory: a map from path to bytes.
///
/// One of these is one database, because a database is a base file and
/// a sidecar log and the two have to find each other by name. Nothing
/// here is written anywhere, so nothing here survives the process, and
/// the last handle going away is what frees it.
///
/// The paths are still paths. A memory database is opened under a name
/// of its own so that the write side, which is registered per path per
/// process, keeps working exactly as it does for a file: two memory
/// databases are two names and never meet, and two connections to one
/// are one name and share a writer.
#[derive(Debug, Default)]
pub struct MemVfs {
    files: Mutex<HashMap<PathBuf, Image>>,
}

impl MemVfs {
    pub fn new() -> MemVfs {
        MemVfs::default()
    }

    fn image(&self, path: &Path) -> Option<Image> {
        self.files.lock().ok()?.get(path).map(Arc::clone)
    }

    /// How many bytes this filesystem is holding, across every file in
    /// it. What a caller who asked for a database in memory would want
    /// to know about the memory.
    pub fn bytes(&self) -> u64 {
        let Ok(files) = self.files.lock() else {
            return 0;
        };
        files
            .values()
            .map(|image| image.lock().map(|bytes| bytes.len() as u64).unwrap_or(0))
            .sum()
    }
}

fn missing(path: &Path) -> ZuError {
    ZuError::Io(std::io::Error::new(
        ErrorKind::NotFound,
        format!("{}: no such file in memory", path.display()),
    ))
}

impl Vfs for MemVfs {
    fn create_new(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        if files.contains_key(path) {
            return Err(ZuError::Io(std::io::Error::new(
                ErrorKind::AlreadyExists,
                format!("{}: already in memory", path.display()),
            )));
        }
        let image: Image = Arc::new(Mutex::new(Vec::new()));
        files.insert(path.to_path_buf(), Arc::clone(&image));
        Ok(Box::new(MemFile {
            image,
            writable: true,
        }))
    }

    fn open_rw(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        let image = self.image(path).ok_or_else(|| missing(path))?;
        Ok(Box::new(MemFile {
            image,
            writable: true,
        }))
    }

    fn open_r(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        let image = self.image(path).ok_or_else(|| missing(path))?;
        Ok(Box::new(MemFile {
            image,
            writable: false,
        }))
    }

    fn open_or_create(&self, path: &Path) -> Result<Box<dyn VfsFile>> {
        let mut files = self.files.lock().map_err(|_| poisoned())?;
        let image = Arc::clone(
            files
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(Vec::new()))),
        );
        Ok(Box::new(MemFile {
            image,
            writable: true,
        }))
    }

    fn exists(&self, path: &Path) -> bool {
        self.files
            .lock()
            .map(|files| files.contains_key(path))
            .unwrap_or(false)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        if let Ok(mut files) = self.files.lock() {
            files.remove(path);
        }
        Ok(())
    }

    /// Nothing here outlives the process, so a caller that syncs is
    /// promised nothing and the engine is free to say so.
    fn durable(&self) -> bool {
        false
    }
}

fn poisoned() -> ZuError {
    ZuError::Io(std::io::Error::other(
        "a thread panicked holding this database's memory",
    ))
}

/// One file of a [`MemVfs`], which is a vector of bytes and a promise
/// about whether this handle may write to it.
///
/// The handles on one file share the bytes, because two handles on one
/// file share the file. That is what makes a fork for reading see what
/// the write side put there, which on a real filesystem the kernel does
/// and here has to be said.
#[derive(Debug)]
pub struct MemFile {
    image: Image,
    writable: bool,
}

impl MemFile {
    fn refused(&self) -> Result<()> {
        if self.writable {
            return Ok(());
        }
        Err(ZuError::Io(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "this database was opened read-only",
        )))
    }
}

impl VfsFile for MemFile {
    /// Past the end is the end, which is what a real read reports and
    /// what the header reader at an unwritten slot depends on.
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> Result<()> {
        let image = self.image.lock().map_err(|_| poisoned())?;
        let from = offset as usize;
        let to = from.saturating_add(buf.len());
        if to > image.len() {
            return Err(ZuError::Io(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "read past end of file",
            )));
        }
        buf.copy_from_slice(&image[from..to]);
        Ok(())
    }

    /// A write past the end grows the file, and the gap between the old
    /// end and the write reads as zeros, which is what a real file does
    /// and what the header slots rely on.
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.refused()?;
        let mut image = self.image.lock().map_err(|_| poisoned())?;
        let from = offset as usize;
        let to = from.saturating_add(buf.len());
        if to > image.len() {
            image.resize(to, 0);
        }
        image[from..to].copy_from_slice(buf);
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        self.refused()?;
        let mut image = self.image.lock().map_err(|_| poisoned())?;
        image.resize(len as usize, 0);
        Ok(())
    }

    /// Nothing to push anywhere. A sync on a database that is memory is
    /// the promise it already keeps.
    fn sync_all(&mut self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&mut self) -> Result<()> {
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.image.lock().map_err(|_| poisoned())?.len() as u64)
    }

    fn dup(&self) -> Result<Box<dyn VfsFile>> {
        Ok(Box::new(MemFile {
            image: Arc::clone(&self.image),
            writable: self.writable,
        }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> MemVfs {
        MemVfs::new()
    }

    #[test]
    fn a_file_in_memory_reads_back_what_was_written_to_it() {
        let vfs = mem();
        let mut file = vfs.create_new(Path::new("db")).expect("create");
        file.write_all_at(b"hello", 0).expect("write");
        let mut buf = [0u8; 5];
        file.read_exact_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"hello");
        assert_eq!(file.len().expect("len"), 5);
    }

    /// The header reader asks for 12 KiB of a file that may be shorter,
    /// and what it needs back is the end of the file rather than a
    /// short read it would have to notice.
    #[test]
    fn a_read_past_the_end_is_the_end() {
        let vfs = mem();
        let mut file = vfs.create_new(Path::new("db")).expect("create");
        file.write_all_at(b"hi", 0).expect("write");
        let mut buf = [0u8; 8];
        let err = file.read_exact_at(&mut buf, 0).expect_err("past the end");
        match err {
            ZuError::Io(io) => assert_eq!(io.kind(), ErrorKind::UnexpectedEof),
            other => panic!("{other:?}"),
        }
    }

    /// A file is grown by writing past its end, and the hole reads as
    /// zeros, which is what the second header slot depends on: an
    /// all-zero slot is one no crc passes, and that is how a fresh
    /// database says it has only ever published one.
    #[test]
    fn a_write_past_the_end_grows_the_file_and_zeroes_the_gap() {
        let vfs = mem();
        let mut file = vfs.create_new(Path::new("db")).expect("create");
        file.write_all_at(b"tail", 8).expect("write");
        assert_eq!(file.len().expect("len"), 12);
        let mut buf = [0u8; 12];
        file.read_exact_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"\0\0\0\0\0\0\0\0tail");
    }

    #[test]
    fn setting_the_length_truncates_and_extends() {
        let vfs = mem();
        let mut file = vfs.create_new(Path::new("db")).expect("create");
        file.write_all_at(b"abcdef", 0).expect("write");
        file.set_len(3).expect("shrink");
        assert_eq!(file.len().expect("len"), 3);
        file.set_len(5).expect("grow");
        let mut buf = [0u8; 5];
        file.read_exact_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"abc\0\0");
    }

    /// Two handles on one file are two views of one thing, which is
    /// what makes a fork for reading see what the write side put there.
    #[test]
    fn two_handles_on_one_file_share_its_bytes() {
        let vfs = mem();
        let mut writing = vfs.create_new(Path::new("db")).expect("create");
        let mut reading = vfs.open_r(Path::new("db")).expect("open");
        writing.write_all_at(b"seen", 0).expect("write");
        let mut buf = [0u8; 4];
        reading.read_exact_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"seen");
        let mut duplicate = writing.dup().expect("dup");
        duplicate.write_all_at(b"also", 4).expect("write");
        assert_eq!(reading.len().expect("len"), 8);
    }

    #[test]
    fn a_read_only_handle_refuses_a_write() {
        let vfs = mem();
        drop(vfs.create_new(Path::new("db")).expect("create"));
        let mut file = vfs.open_r(Path::new("db")).expect("open");
        let err = file.write_all_at(b"no", 0).expect_err("refused");
        match err {
            ZuError::Io(io) => assert_eq!(io.kind(), ErrorKind::PermissionDenied),
            other => panic!("{other:?}"),
        }
        assert!(file.set_len(0).is_err(), "and it refuses a truncate too");
    }

    #[test]
    fn creating_a_name_that_is_taken_is_refused() {
        let vfs = mem();
        drop(vfs.create_new(Path::new("db")).expect("create"));
        let err = vfs.create_new(Path::new("db")).expect_err("taken");
        match err {
            ZuError::Io(io) => assert_eq!(io.kind(), ErrorKind::AlreadyExists),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opening_a_name_nothing_made_is_not_found() {
        let vfs = mem();
        let err = vfs.open_rw(Path::new("db")).expect_err("missing");
        match err {
            ZuError::Io(io) => assert_eq!(io.kind(), ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
        assert!(!vfs.exists(Path::new("db")));
    }

    /// The log's open, which is the one open that is allowed to make
    /// the file it opens.
    #[test]
    fn open_or_create_makes_the_file_and_then_finds_it() {
        let vfs = mem();
        let mut first = vfs.open_or_create(Path::new("db.wal")).expect("create");
        first.write_all_at(b"frame", 0).expect("write");
        let second = vfs.open_or_create(Path::new("db.wal")).expect("open");
        assert_eq!(
            second.len().expect("len"),
            5,
            "the same file, not a new one"
        );
        assert!(vfs.exists(Path::new("db.wal")));
    }

    #[test]
    fn removing_a_file_forgets_its_bytes() {
        let vfs = mem();
        drop(vfs.create_new(Path::new("db")).expect("create"));
        vfs.remove(Path::new("db")).expect("remove");
        assert!(!vfs.exists(Path::new("db")));
        assert!(vfs.create_new(Path::new("db")).is_ok(), "the name is free");
    }

    /// What a caller is owed when it asks how much memory a database is
    /// costing it, which for one on disk is a question about the disk.
    #[test]
    fn the_bytes_held_are_every_file_added_up() {
        let vfs = mem();
        assert_eq!(vfs.bytes(), 0);
        let mut base = vfs.create_new(Path::new("db")).expect("create");
        base.set_len(1024).expect("grow");
        let mut log = vfs.create_new(Path::new("db.wal")).expect("create");
        log.set_len(64).expect("grow");
        assert_eq!(vfs.bytes(), 1024 + 64);
    }

    #[test]
    fn the_filesystem_is_durable_and_memory_is_not() {
        assert!(RealVfs.durable());
        assert!(!mem().durable());
    }

    /// One shared filesystem rather than one per open: every database
    /// on disk is on the same disk, and a handle per open would be a
    /// pointer nobody reads.
    #[test]
    fn the_filesystem_is_one_thing() {
        assert!(Arc::ptr_eq(&RealVfs::shared(), &RealVfs::shared()));
    }
}
