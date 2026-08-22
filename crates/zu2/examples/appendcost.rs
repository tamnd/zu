//! What a durable append costs, split into the two things it pays for.
//!
//! zu2 writes its log with `pwrite` at the record's own offset into a file
//! that has never been extended, so every flush past the end of the file
//! grows it, and `fdatasync` then has to commit the new size as well as the
//! data. sqlite's WAL is a file it reuses in place after the first
//! checkpoint cycle, so its syncs are data only. This program measures the
//! difference on whatever filesystem it is pointed at, because the gap is
//! the whole reason zu2's durable rows sit under sqlite's on the Linux
//! machines while beating them everywhere else.
//!
//! Three shapes, same bytes, same call:
//!
//! - grow: write at the end of the file, which is what zu2 does today
//! - sparse: the file was sized with `ftruncate` first, so the offset is
//!   inside it, but the blocks are not allocated until the write lands
//! - allocated: the file was filled and synced first, so the write only
//!   changes bytes that are already on the device
//!
//! Run it in the directory the benchmark uses, not in /tmp, or it measures
//! a different filesystem than the one the numbers came off.

use std::fs::File;
use std::io::Write;
use std::time::Instant;

const BLOCK: usize = 4096;
const ROUNDS: u64 = 200;

/// One positional write, the same call zu2's log makes.
fn write_at(f: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut written = 0;
        while written < buf.len() {
            let n = f.seek_write(&buf[written..], offset + written as u64)?;
            written += n;
        }
        Ok(())
    }
}

fn sync(f: &File) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            #[link_name = "fdatasync"]
            fn fdatasync(fd: i32) -> i32;
        }
        // SAFETY: the descriptor is open for the life of the call.
        if unsafe { fdatasync(f.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        f.sync_all()
    }
}

/// Asks the filesystem for the blocks without writing them, which is
/// what `file::preallocate` does and what the log now does before it
/// flushes. Separate from the allocated shape on purpose: `fallocate`
/// hands out unwritten extents, and on a filesystem that has to convert
/// one when the write lands the first write to a block is still a
/// metadata change. Whether that conversion costs what an allocation
/// costs is exactly what this shape measures.
fn allocate(f: &File, len: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
        }
        // SAFETY: the descriptor is open for writing and the range is
        // in bounds.
        unsafe { fallocate(f.as_raw_fd(), 0, 0, len as i64) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        f.set_len(len).is_ok()
    }
}

fn run(name: &str, dir: &std::path::Path, prepare: impl Fn(&File)) {
    let path = dir.join(format!("appendcost-{name}"));
    let f = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("open");
    prepare(&f);
    let block = [7u8; BLOCK];
    let started = Instant::now();
    for i in 0..ROUNDS {
        write_at(&f, &block, i * BLOCK as u64).expect("write");
        sync(&f).expect("sync");
    }
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{name:10} {:8.0} durable writes/s  {:8.0} us each",
        ROUNDS as f64 / elapsed,
        elapsed / ROUNDS as f64 * 1e6
    );
    drop(f);
    let _ = std::fs::remove_file(&path);
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    println!("in {}", dir.display());
    run("grow", &dir, |_| {});
    run("sparse", &dir, |f| {
        f.set_len(ROUNDS * BLOCK as u64).expect("truncate");
        f.sync_all().expect("sync");
    });
    run("fallocated", &dir, |f| {
        if !allocate(f, ROUNDS * BLOCK as u64) {
            println!("           (this filesystem has no allocation call)");
        }
    });
    run("allocated", &dir, |f| {
        let mut w = f.try_clone().expect("clone");
        let block = [0u8; BLOCK];
        for _ in 0..ROUNDS {
            w.write_all(&block).expect("fill");
        }
        w.sync_all().expect("sync");
    });
}
