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
use std::sync::atomic::{AtomicU64, Ordering};

/// Mappings still to be refused, which is how a test reaches the one
/// behaviour a kernel decides.
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Makes the next `count` mapping attempts fail the way a kernel at
/// `vm.max_map_count` fails them, and any later one succeed.
///
/// Process wide and not per database, because the thing it stands in for
/// is a per process limit. A test that calls this wants a test binary of
/// its own, since a mapping any other test in the same process asks for
/// while it is armed is refused too.
///
/// Deliberately not behind a cargo feature. The cost when it is not
/// armed is one relaxed compare on a path that runs once per 4 MiB page
/// on the flusher thread, which is nothing next to the `mmap` beside it,
/// and a fault hook that is only compiled under a feature is a fault
/// hook that stops compiling without anybody finding out. #769.
pub fn refuse_mappings(count: u64) {
    REFUSALS.store(count, Ordering::Relaxed);
}

/// Whether this mapping attempt is one of the refused ones, taking it
/// off the count if it is.
///
/// Only where there is a mapping call to refuse. On a platform without
/// one every attempt is refused already, so arming this changes nothing
/// there and the count is simply never read.
#[cfg(unix)]
fn refusing() -> bool {
    REFUSALS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
            left.checked_sub(1)
        })
        .is_ok()
}

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

/// Tells the kernel this file is read at scattered offsets, so it should
/// not read ahead.
///
/// The cold tier's steady state is a point read: one `pread` of about a
/// kilobyte at an address the hash index handed over, with no
/// relationship to the address before it. Linux defaults to
/// 128 KiB of readahead on a file opened this way, so a 1152 byte ask can
/// pull two orders of magnitude more than it needs, and every one of
/// those pages is charged to whatever memory the process is allowed. That
/// is not merely waste, it evicts the pages a later read wanted, which is
/// the shape of #720: under a 512 MiB budget our cold reads fell 43x
/// where pebble, which reads its own blocks and asks the kernel for
/// nothing, fell 5.6x.
///
/// It costs the sequential readers almost nothing, which is why it can go
/// on the file rather than on a call. Recovery and compaction walk this
/// tier with [`read_exact_at`] a whole 4 MiB page at a time, and a
/// request already thirty two times the readahead window does not need
/// readahead.
///
/// Returns whether the kernel took the advice. A platform without it is
/// slower and not wrong, the same reading [`punch`] and [`preallocate`]
/// get.
#[cfg(target_os = "linux")]
pub fn advise_random(file: &File) -> bool {
    use std::os::fd::AsRawFd;

    /// `POSIX_FADV_RANDOM` from `fcntl.h`.
    const RANDOM: i32 = 1;

    unsafe extern "C" {
        fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
    }
    // Zero length means to the end of the file, and it keeps meaning that
    // as the file grows, so this is said once at open and not again.
    //
    // SAFETY: the descriptor is owned by `file`.
    // It returns the error rather than setting errno, so zero is success.
    unsafe { posix_fadvise(file.as_raw_fd(), 0, 0, RANDOM) == 0 }
}

/// Turns readahead off. See the Linux version for what this is for;
/// macOS has no `posix_fadvise` and spells it as an `fcntl` whose
/// argument is whether to read ahead at all.
#[cfg(target_os = "macos")]
pub fn advise_random(file: &File) -> bool {
    use std::os::fd::AsRawFd;

    /// `F_RDAHEAD` from `sys/fcntl.h`. Argument zero turns it off.
    const F_RDAHEAD: i32 = 45;

    // Variadic for the same reason [`punch`] is: Apple's arm64 ABI puts a
    // variadic argument on the stack and a fixed one in a register.
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    // SAFETY: the descriptor is owned by `file`.
    unsafe { fcntl(file.as_raw_fd(), F_RDAHEAD, 0i32) != -1 }
}

/// Nowhere to say it after the file is open. Windows takes the same hint
/// as `FILE_FLAG_RANDOM_ACCESS`, which is a flag on the open and not a
/// call, so saying it here would mean reopening the file.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn advise_random(_file: &File) -> bool {
    false
}

/// Maps `[offset, offset + len)` of the file read only and shared, and
/// answers where it landed.
///
/// This is how a settled log page stops being heap and starts being page
/// cache. The bytes of such a page are read only, flushed, and identical
/// to the region of the file underneath them, which is exactly what a
/// shared mapping describes, and the difference it makes is not the size
/// of the resident set but its kind: heap the kernel can only swap
/// becomes cache the kernel can drop and fault back. #757.
///
/// Read only on purpose rather than out of caution. Nothing in the engine
/// writes to a page below the mutable window, so a mapping that faults on
/// a write is a mapping that says loudly in a test what would otherwise
/// be a silent corruption of a file.
///
/// `offset` has to be a multiple of the page size the kernel maps at, and
/// every caller here passes a 4 MiB log page start, so it is. A refusal
/// is not an error, the same reading [`punch`] and [`preallocate`] get:
/// the caller keeps the heap page it already had.
///
/// # Safety
///
/// The returned pointer is valid until it is passed to [`unmap`] with the
/// same length, and the file must not be shortened below the mapped range
/// while it lives. Nothing here does: the only truncation is `trim_tail`,
/// which never goes below the write frontier, and a mapped page is always
/// below it.
/// Where `at` is `Some`, the mapping is placed there and replaces
/// whatever was in that range, which is how a run of log pages becomes
/// one region of address space instead of one a page. See
/// [`reserve`] and #768.
#[cfg(unix)]
pub fn map_read(file: &File, offset: u64, len: usize) -> Option<*mut u8> {
    map_read_inner(file, offset, len, std::ptr::null_mut())
}

/// [`map_read`] into a range [`reserve`] handed out.
///
/// # Safety
///
/// `at` is inside a live reservation, `len` bytes fit inside it, and no
/// live mapping of this log occupies the range: `MAP_FIXED` replaces
/// whatever is there without asking, so a caller that gets this wrong
/// unmaps a page a reader is inside. The caller checks that; see
/// `Log::remap_settled`.
#[cfg(unix)]
pub unsafe fn map_read_at(file: &File, offset: u64, len: usize, at: *mut u8) -> Option<*mut u8> {
    map_read_inner(file, offset, len, at)
}

#[cfg(unix)]
fn map_read_inner(file: &File, offset: u64, len: usize, at: *mut u8) -> Option<*mut u8> {
    use std::os::fd::AsRawFd;

    if refusing() {
        return None;
    }

    /// `PROT_READ` from `sys/mman.h`, the same value on Linux and macOS.
    const PROT_READ: i32 = 1;
    /// `MAP_SHARED`, likewise.
    const MAP_SHARED: i32 = 1;

    unsafe extern "C" {
        fn mmap(
            addr: *mut core::ffi::c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut core::ffi::c_void;
    }
    /// `MAP_FIXED`, the same value on Linux and macOS.
    const MAP_FIXED: i32 = 0x10;

    let flags = if at.is_null() {
        MAP_SHARED
    } else {
        MAP_SHARED | MAP_FIXED
    };
    // SAFETY: the descriptor is owned by `file` and open for reading. A
    // null hint asks the kernel to choose the address; a non-null one is
    // a range inside a reservation this process made and the caller has
    // checked nothing live is in it.
    let at = unsafe {
        mmap(
            at.cast::<core::ffi::c_void>(),
            len,
            PROT_READ,
            flags,
            file.as_raw_fd(),
            offset as i64,
        )
    };
    // MAP_FAILED is (void *) -1 and is the only failure this reports.
    if at as isize == -1 {
        return None;
    }
    Some(at.cast::<u8>())
}

/// Nowhere to ask, so a settled page stays on the heap.
///
/// Windows has the call under a different name and a different shape, a
/// file mapping object and then a view of it, and the view has to be
/// unmapped before the object is closed. It is worth having and it is not
/// worth guessing at: the benchmark host that runs Windows runs these
/// through WSL, which is this file's Linux path, so nothing measured is
/// waiting on it.
#[cfg(not(unix))]
pub fn map_read(_file: &File, _offset: u64, _len: usize) -> Option<*mut u8> {
    None
}

/// # Safety
///
/// Never called, because the reservation it maps into is never made.
#[cfg(not(unix))]
pub unsafe fn map_read_at(
    _file: &File,
    _offset: u64,
    _len: usize,
    _at: *mut u8,
) -> Option<*mut u8> {
    None
}

/// Takes `len` bytes of address space and nothing else.
///
/// `PROT_NONE`, anonymous and `MAP_NORESERVE`, so this is a promise from
/// the kernel that nothing else will be put in the range rather than a
/// request for memory. Nothing is charged for it, nothing is resident in
/// it, and touching it is a fault.
///
/// What it is for is #768. A settled log page mapped with a null hint
/// gets an address the kernel chose, and the kernel hands them out in
/// the opposite order to the file offsets the log maps in, so two pages
/// that are neighbours in the file are never neighbours in memory and
/// the kernel cannot merge their mappings. That is one region of address
/// space per 4 MiB page: measured on server1, 512 pages mapped a page at
/// a time added 512 lines to `/proc/self/maps`, and the same 512 mapped
/// into a reservation added one. The cost of the first is a longer walk
/// on every fault, which is the fault `map_settled` traded a `pread`
/// for, and a ceiling at `vm.max_map_count`, which is 65530 by default
/// and is 256 GiB of log.
///
/// A refusal is not an error. The log maps with a null hint instead and
/// gets what it had before this existed.
#[cfg(unix)]
pub fn reserve(len: usize) -> Option<*mut u8> {
    /// `PROT_NONE`.
    const PROT_NONE: i32 = 0;
    /// `MAP_PRIVATE`, the same value on Linux and macOS.
    const MAP_PRIVATE: i32 = 0x02;
    /// `MAP_ANONYMOUS`, which is 0x20 on Linux and 0x1000 on macOS.
    #[cfg(target_os = "linux")]
    const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(not(target_os = "linux"))]
    const MAP_ANONYMOUS: i32 = 0x1000;
    /// `MAP_NORESERVE`, which macOS does not have and does not need:
    /// it does not do strict overcommit accounting.
    #[cfg(target_os = "linux")]
    const MAP_NORESERVE: i32 = 0x4000;
    #[cfg(not(target_os = "linux"))]
    const MAP_NORESERVE: i32 = 0;

    unsafe extern "C" {
        fn mmap(
            addr: *mut core::ffi::c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut core::ffi::c_void;
    }
    if len == 0 {
        return None;
    }
    // SAFETY: an anonymous mapping with a null hint, which takes no
    // descriptor and reads nothing.
    let at = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
            -1,
            0,
        )
    };
    if at as isize == -1 {
        return None;
    }
    Some(at.cast::<u8>())
}

/// Nowhere to ask, so there is no reservation and the log maps the way
/// it did before #768.
#[cfg(not(unix))]
pub fn reserve(_len: usize) -> Option<*mut u8> {
    None
}

/// Puts `len` bytes of a reservation back the way [`reserve`] left them.
///
/// Not a `munmap`: that would take the range out of the reservation and
/// let the next unrelated `mmap` in the process land in the middle of
/// it. This puts `PROT_NONE` anonymous memory back over the range, which
/// is what was there before the page was mapped into it, so the
/// reservation stays whole.
///
/// # Safety
///
/// `at` is a range inside a live reservation, `len` matches what was
/// mapped there, and nothing is reading through it any more.
#[cfg(unix)]
pub unsafe fn unmap_within_reservation(at: *mut u8, len: usize) {
    /// `PROT_NONE`.
    const PROT_NONE: i32 = 0;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_FIXED: i32 = 0x10;
    #[cfg(target_os = "linux")]
    const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(not(target_os = "linux"))]
    const MAP_ANONYMOUS: i32 = 0x1000;
    #[cfg(target_os = "linux")]
    const MAP_NORESERVE: i32 = 0x4000;
    #[cfg(not(target_os = "linux"))]
    const MAP_NORESERVE: i32 = 0;

    unsafe extern "C" {
        fn mmap(
            addr: *mut core::ffi::c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut core::ffi::c_void;
    }
    // SAFETY: the caller promises the range is inside a reservation this
    // process holds and that nothing is inside it.
    unsafe {
        mmap(
            at.cast::<core::ffi::c_void>(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_NORESERVE,
            -1,
            0,
        );
    }
}

/// Never called, because the reservation is never made.
///
/// # Safety
///
/// Unreachable.
#[cfg(not(unix))]
pub unsafe fn unmap_within_reservation(_at: *mut u8, _len: usize) {}

/// Gives back a mapping [`map_read`] handed out.
///
/// # Safety
///
/// `at` came from [`map_read`] with this same `len` and nothing is
/// reading through it any more.
#[cfg(unix)]
pub unsafe fn unmap(at: *mut u8, len: usize) {
    unsafe extern "C" {
        fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    }
    // SAFETY: the caller promises the range is one this handed out and
    // that nothing is inside it.
    unsafe {
        munmap(at.cast::<core::ffi::c_void>(), len);
    }
}

/// Nothing is ever mapped, so nothing is ever given back.
///
/// # Safety
///
/// Unreachable: [`map_read`] never answers on this platform.
#[cfg(not(unix))]
pub unsafe fn unmap(_at: *mut u8, _len: usize) {}

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

/// Bytes a file at this path occupies, and zero when it is not there.
///
/// For the sidecars, which are named rather than held open: a store
/// costs what all of its files cost, and a checkpoint that is not on
/// disk yet costs nothing. Anything other than a missing file is still
/// an error, because a store whose checkpoint cannot be read is not a
/// store with a small checkpoint.
pub fn disk_bytes_at(path: &Path) -> io::Result<u64> {
    match File::open(path) {
        Ok(file) => disk_bytes(&file, path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The advice is a bare `extern "C"` declaration with a constant
    /// copied out of a system header, which is the same shape [`punch`]
    /// and [`preallocate`] have and the same way it can be silently
    /// wrong: a bad command number fails and the caller carries on
    /// slower, with nothing to say so. This is what says so.
    ///
    /// A regular file on a temp directory is the case both platforms
    /// support, so on those two it has to be taken. Elsewhere there is
    /// nothing to ask and the answer is allowed to be no.
    #[test]
    fn the_kernel_takes_the_random_access_advice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = create_new(&dir.path().join("advised")).expect("create");
        let taken = advise_random(&file);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(taken, "the platform has the call and it was refused");
        }
    }
}
