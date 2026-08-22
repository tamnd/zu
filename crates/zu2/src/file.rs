//! Positioned file I/O, shared rather than exclusive.
//!
//! zu1's `VfsFile` takes `&mut self`, which is
//! right for a single-writer engine and wrong here: the flusher writes
//! while any number of readers pread records that have been evicted, so
//! every method takes `&self` and the kernel does the arbitration. The
//! platform split exists because Windows has no pread with exact
//! semantics, so `seek_read` and `seek_write` have to be looped.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Creates the log file, failing if it is already there.
pub fn create_new(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    make_sparse(&file);
    Ok(file)
}

/// Creates a file, truncating whatever was there. Only recovery's
/// relink journal uses this, and it wants the truncation: a leftover
/// journal longer than the one being written would otherwise keep its
/// tail and the checksum would be over the wrong bytes.
pub fn create_or_replace(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

/// Opens an existing log file for reading and writing.
pub fn open_rw(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    make_sparse(&file);
    Ok(file)
}

/// Makes the written bytes durable.
///
/// This is deliberately `fsync` and not [`File::sync_data`]. On macOS
/// the standard library issues `F_FULLFSYNC`, which drains the drive's
/// own write cache and costs several milliseconds. Sqlite, postgres and
/// mysql all default to plain `fsync` there, so calling `sync_data`
/// would have zu2 buying a stronger guarantee than everything it is
/// measured against and losing by a factor of five on a benchmark that
/// looks like it is about design. Same barrier for everyone is the only
/// way the numbers mean anything. On Linux `fdatasync` is the same
/// barrier as `fsync` for a file whose size the write already extended,
/// and it is what the rivals call.
#[cfg(unix)]
pub fn sync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        #[link_name = "fdatasync"]
        fn barrier(fd: i32) -> i32;
    }
    #[cfg(not(target_os = "linux"))]
    unsafe extern "C" {
        #[link_name = "fsync"]
        fn barrier(fd: i32) -> i32;
    }
    // SAFETY: the descriptor is owned by `file` and open for writing.
    if unsafe { barrier(file.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Makes the written bytes durable. Windows has one flush and it is
/// the same one the rivals call.
#[cfg(windows)]
pub fn sync(file: &File) -> io::Result<()> {
    file.sync_data()
}

/// The smallest range a hole can be punched at. Every filesystem this
/// runs on wants the offset and the length aligned to its block size,
/// and 4 KiB divides all of them. Compaction punches whole 4 MiB log
/// pages, so the alignment is free.
pub const BLOCK: u64 = 4096;

/// Gives the disk blocks behind `[offset, offset + len)` back to the
/// filesystem without changing the file's length. The bytes read back
/// as zeros afterwards.
///
/// This is how compaction actually returns space rather than merely
/// stopping using it. A log address is a file offset, so the file can
/// not be shifted down when its front falls out of use. What it can do
/// is become a hole: `fallocate` with `FALLOC_FL_PUNCH_HOLE` on Linux,
/// which ext4, xfs, btrfs and f2fs all implement, `F_PUNCHHOLE` on
/// macOS, which apfs implements, and `FSCTL_SET_ZERO_DATA` on Windows
/// against a file marked sparse.
///
/// Returns whether the space came back. A filesystem without the call
/// costs space and nothing else, because the log is correct either way
/// and a hole that was not punched is only bytes nobody reads, so this
/// reports rather than fails and the caller records what it got.
#[cfg(target_os = "linux")]
pub fn punch(file: &File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    /// Keep the file length where it is.
    const KEEP_SIZE: i32 = 0x01;
    /// Deallocate rather than allocate.
    const PUNCH_HOLE: i32 = 0x02;

    unsafe extern "C" {
        fn fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
    }
    if len == 0 {
        return true;
    }
    // SAFETY: the descriptor is owned by `file` and open for writing,
    // and both arguments are in range because the caller passes page
    // boundaries of a file it has already written.
    unsafe {
        fallocate(
            file.as_raw_fd(),
            PUNCH_HOLE | KEEP_SIZE,
            offset as i64,
            len as i64,
        ) == 0
    }
}

/// Gives the disk blocks back. See the Linux version for what this is
/// for; macOS spells it as an `fcntl` over a struct.
#[cfg(target_os = "macos")]
pub fn punch(file: &File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    const F_PUNCHHOLE: i32 = 99;

    /// `fpunchhole_t` from `sys/fcntl.h`.
    #[repr(C)]
    struct Punch {
        flags: u32,
        reserved: u32,
        offset: i64,
        length: i64,
    }

    // Variadic on purpose. Apple's arm64 ABI passes a variadic argument
    // on the stack and a fixed one in a register, so a declaration that
    // dropped the ellipsis would put the pointer somewhere fcntl does
    // not read it.
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    if len == 0 {
        return true;
    }
    let mut request = Punch {
        flags: 0,
        reserved: 0,
        offset: offset as i64,
        length: len as i64,
    };
    // SAFETY: the descriptor is owned by `file`, and the struct is the
    // shape F_PUNCHHOLE reads and lives across the call.
    unsafe { fcntl(file.as_raw_fd(), F_PUNCHHOLE, &raw mut request) == 0 }
}

/// Gives the disk blocks back. Windows wants the file marked sparse
/// first, which [`make_sparse`] does when the log is opened.
#[cfg(windows)]
pub fn punch(file: &File, offset: u64, len: u64) -> bool {
    use std::os::windows::io::AsRawHandle;

    const FSCTL_SET_ZERO_DATA: u32 = 0x980C8;

    /// `FILE_ZERO_DATA_INFORMATION`.
    #[repr(C)]
    struct ZeroData {
        from: i64,
        to: i64,
    }

    if len == 0 {
        return true;
    }
    let request = ZeroData {
        from: offset as i64,
        to: (offset + len) as i64,
    };
    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file`, the input buffer is the
    // shape the control code reads and its length is given exactly.
    unsafe {
        control(
            file.as_raw_handle(),
            FSCTL_SET_ZERO_DATA,
            (&raw const request).cast(),
            std::mem::size_of::<ZeroData>() as u32,
            &raw mut returned,
        )
    }
}

/// Marks the log sparse, without which a hole punch on Windows writes
/// zeros instead of releasing blocks. Called once when the file is
/// opened; a filesystem that refuses it costs space and nothing else.
#[cfg(windows)]
pub fn make_sparse(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;

    const FSCTL_SET_SPARSE: u32 = 0x900C4;

    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file` and the call takes no
    // input buffer.
    unsafe {
        control(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            &raw mut returned,
        )
    }
}

/// One `DeviceIoControl` with no output buffer, which is the shape both
/// of the calls above want.
///
/// # Safety
///
/// `handle` must be a live file handle and `input` must point at
/// `input_len` readable bytes.
#[cfg(windows)]
unsafe fn control(
    handle: std::os::windows::io::RawHandle,
    code: u32,
    input: *const std::ffi::c_void,
    input_len: u32,
    returned: *mut u32,
) -> bool {
    unsafe extern "system" {
        fn DeviceIoControl(
            handle: *mut std::ffi::c_void,
            code: u32,
            input: *const std::ffi::c_void,
            input_len: u32,
            output: *mut std::ffi::c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    // SAFETY: the caller's contract, and the output buffer is null with
    // a length of zero, which is what a control code that returns
    // nothing expects.
    unsafe {
        DeviceIoControl(
            handle.cast(),
            code,
            input,
            input_len,
            std::ptr::null_mut(),
            0,
            returned,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// Neither Linux, macOS nor Windows, so there is no hole to punch and
/// compaction reclaims addresses without reclaiming blocks.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn punch(_file: &File, _offset: u64, _len: u64) -> bool {
    false
}

/// Gets the blocks behind `[offset, offset + len)` allocated now, so a
/// later write there changes bytes the file already owns.
///
/// This is the other half of the hole punch and it exists for the same
/// reason: a durable commit is a write followed by a barrier, and what
/// the barrier costs depends on how much of the filesystem's own
/// bookkeeping the write dirtied. A write past the end of a file moves
/// the inode's size, allocates extents and journals both, and `fdatasync`
/// has to commit all of it before it can return. A write inside a range
/// that is already allocated commits data. `crates/zu2/examples/appendcost.rs`
/// measures the difference at 1.9x to 4.6x on the three Linux machines
/// this is benchmarked on.
///
/// `ftruncate` is not this. It moves the size and allocates nothing, so
/// the first write to a block still does the allocation and the journal
/// entry, which is why the example measures a sparse shape separately
/// and finds it no faster than growing.
///
/// Returns whether the space was provisioned. A filesystem without the
/// call is slower and not wrong, so the caller records what it got and
/// carries on, exactly as it does for [`punch`].
#[cfg(target_os = "linux")]
pub fn preallocate(file: &File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
    }
    if len == 0 {
        return true;
    }
    // Mode zero, which allocates and moves the size, rather than
    // KEEP_SIZE: the size has to move too, or the write that lands in
    // these blocks moves it and pays for that.
    //
    // SAFETY: the descriptor is owned by `file` and open for writing.
    unsafe { fallocate(file.as_raw_fd(), 0, offset as i64, len as i64) == 0 }
}

/// Gets the blocks allocated. See the Linux version for what this is
/// for; macOS spells it as an `fcntl` over a struct and does not move
/// the file's length, so that is a second call.
#[cfg(target_os = "macos")]
pub fn preallocate(file: &File, offset: u64, len: u64) -> bool {
    use std::os::fd::AsRawFd;

    const F_PREALLOCATE: i32 = 42;
    /// Allocate contiguously, which is a request rather than a promise.
    const ALLOCATECONTIG: u32 = 0x00000002;
    /// Allocate all of it or none of it.
    const ALLOCATEALL: u32 = 0x00000004;
    /// Lengths are relative to the physical end of the file.
    const PEOFPOSMODE: i32 = 3;

    /// `fstore_t` from `sys/fcntl.h`.
    #[repr(C)]
    struct Store {
        flags: u32,
        posmode: i32,
        offset: i64,
        length: i64,
        allocated: i64,
    }

    // Variadic for the same ABI reason the punch is.
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    if len == 0 {
        return true;
    }
    let mut request = Store {
        flags: ALLOCATECONTIG,
        posmode: PEOFPOSMODE,
        offset: 0,
        length: len as i64,
        allocated: 0,
    };
    // SAFETY: the descriptor is owned by `file`, and the struct is the
    // shape F_PREALLOCATE reads and lives across both calls.
    let contiguous = unsafe { fcntl(file.as_raw_fd(), F_PREALLOCATE, &raw mut request) == 0 };
    if !contiguous {
        // A fragmented volume cannot always answer the contiguous
        // request, and any allocation is worth more than none.
        request.flags = ALLOCATEALL;
        request.allocated = 0;
        // SAFETY: as above.
        if unsafe { fcntl(file.as_raw_fd(), F_PREALLOCATE, &raw mut request) != 0 } {
            return false;
        }
    }
    // The blocks are the file's now but its length still says otherwise,
    // and a write past the length would move it and journal that.
    file.set_len(offset + len).is_ok()
}

/// Moves the file's length so a write below it does not have to.
///
/// Windows has no allocation call a normal process can make.
/// `SetFileValidData` is the closest one and it hands the caller
/// whatever bytes happened to be on the disk, so it is gated behind
/// SE_MANAGE_VOLUME_NAME and a database has no business asking for it.
/// What is left is the size, which is the part of the metadata update
/// this can avoid, and NTFS allocates the blocks when the write arrives.
#[cfg(windows)]
pub fn preallocate(file: &File, offset: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    file.set_len(offset + len).is_ok()
}

/// Nowhere to ask, so every write pays for its own extent.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn preallocate(_file: &File, _offset: u64, _len: u64) -> bool {
    false
}

/// Drops the file's length back to `len`, giving up whatever was
/// provisioned above it.
///
/// The counterpart to [`preallocate`], and the reason the storage
/// numbers stay honest: what the log reserved to write into next is not
/// what the database costs, so the reservation goes back before anyone
/// measures the file and before it is closed.
pub fn truncate(file: &File, len: u64) -> io::Result<()> {
    file.set_len(len)
}

/// Nothing to mark outside Windows.
#[cfg(not(windows))]
pub fn make_sparse(_file: &File) -> bool {
    true
}

/// Bytes the file actually occupies, which is not its length once
/// compaction has punched holes in its front.
///
/// This is the number a storage comparison has to use. A log whose
/// length is 4 GiB and whose front 3 GiB is a hole costs a gigabyte,
/// and reporting the length would say otherwise.
#[cfg(unix)]
pub fn disk_bytes(file: &File, _path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    // `st_blocks` is in 512 byte units by definition, whatever the
    // filesystem's own block size is.
    Ok(file.metadata()?.blocks() * 512)
}

/// Bytes the file actually occupies. `GetCompressedFileSize` is the
/// Windows name for it and it accounts for sparse ranges.
#[cfg(windows)]
pub fn disk_bytes(file: &File, path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn GetCompressedFileSizeW(name: *const u16, high: *mut u32) -> u32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0u32;
    // SAFETY: the name is nul terminated and the high word is a live
    // local.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    if low == u32::MAX {
        // The call failed, so fall back to the length, which is an
        // over-report rather than a wrong shape.
        return Ok(file.metadata()?.len());
    }
    Ok(u64::from(high) << 32 | u64::from(low))
}

#[cfg(unix)]
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Reads as much as the file has, up to the length of `buf`, and returns
/// how much that was.
///
/// The exact version above is the right one wherever the caller knows
/// how many bytes it wants. A speculative read does not: it asks for
/// more than the record it is after in the hope of getting the whole of
/// it in one syscall, and a record near the end of the file has fewer
/// bytes behind it than that. Short is the answer there, not an error.
#[cfg(unix)]
pub fn read_upto_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

#[cfg(windows)]
pub fn read_upto_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        match file.seek_read(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

#[cfg(windows)]
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from the log",
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_write(&buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short write to the log",
            ));
        }
        done += n;
    }
    Ok(())
}
