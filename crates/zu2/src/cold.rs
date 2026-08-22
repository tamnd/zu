//! The cold tier: a second file for records that have stopped changing.
//!
//! Compaction is lookup based, so a pass copies whatever the index still
//! reaches and reclaims the rest, and a copy goes to the tail where it
//! waits to be reached again. A record nobody updates is therefore
//! copied once per lap of the log, forever, and it pays that for garbage
//! that other records made. `examples/coldtier.rs` measures it: with one
//! percent of the keys taking four fifths of the updates and memory held
//! to a quarter of the live set, the workload appends 245 MiB and
//! compaction copies 181 of them, taking 3.05 seconds against 0.94
//! seconds of updates. Nearly all of those bytes belong to records the
//! workload never touched.
//!
//! F2 (Kanellis, Chandramouli, Hart, Venkataraman, PVLDB 18(12), 2025)
//! answers that by splitting the log in two and compacting the halves on
//! different schedules, and this is that split. What is different is
//! what a half is here.
//!
//! The cold tier is write through. It has no page table, no flusher and
//! no eviction, and an append is a `pwrite` while a read is a `pread`. A
//! record only arrives here by surviving a pass over the oldest region
//! of the hot log, which means it went a whole lap without anybody
//! touching it, and a record like that is not worth a page of memory.
//! Skipping the page table is also what keeps the tier small: everything
//! that makes [`crate::log::Log`] the size it is, the chunked table, the
//! read-only boundary, the group commit, the eviction, is there to make
//! a hot log fast and none of it earns its place down here.
//!
//! Addresses stay one flat space. The cold file starts at [`BASE`],
//! which is the top half of the 48 bits an index entry can name, so an
//! address says which tier it is in by its own value and nothing has to
//! carry a tier flag beside it. Each half has 128 TiB, which is the same
//! ceiling the log had before divided in two.
//!
//! Two rules keep chains walkable across the tiers, and they are the
//! reason this design works at all.
//!
//! A record written here has no `previous`. It is the whole of what its
//! index entry answers for. That is only sound for an entry whose
//! foreign bit is clear, because such an entry answers for the key at
//! its head record and for nothing else, and every key that was ever
//! under it has an entry of its own (#466). So a foreign entry's record
//! is not eligible for the tier and stays in the hot log, where its
//! chain stays intact.
//!
//! A hot record may point here, because an update to a key that lives
//! here appends in the hot log with the cold address as its `previous`,
//! but nothing here points there. So a chain walk descends through hot
//! addresses, crosses at most once, and stops, which is what makes the
//! walk terminate without a tier-aware ordering argument.
//!
//! Reclamation is the same prefix punch the hot log does, on its own
//! schedule and against its own target, which is the whole point of
//! having two: garbage appears here only when a key that had settled is
//! written again, so the cold file goes stale slowly and a pass over it
//! is rare.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::addr::{Address, FIRST, PAGE_SIZE, page_of, page_start};
use crate::error::{Error, Result};
use crate::record::{self, RecordRef};
use crate::{file, log};

/// Where the cold tier's addresses start: the top half of what an index
/// entry can name.
pub const BASE: Address = 1 << 47;

/// How much of a cold record a read asks for before it knows how long
/// the record is. A YCSB record is ten fields of a hundred bytes and is
/// the largest thing this has to cover in one go, and the read is a
/// syscall whose cost is in reaching the device rather than in the bytes
/// it hands back, so this is generous on purpose.
const SPECULATE: usize = 1152;

/// Whether an address names a record in the cold tier.
#[inline]
pub const fn is_cold(address: Address) -> bool {
    address >= BASE
}

/// The suffix the cold file takes beside the log.
pub fn path_beside(path: &Path) -> PathBuf {
    let mut beside = path.to_path_buf().into_os_string();
    beside.push(".cold");
    PathBuf::from(beside)
}

pub struct Cold {
    file: File,
    path: PathBuf,
    /// Where the next record goes, as an absolute address.
    tail: AtomicU64,
    /// The lowest address still holding a record, absolute. Everything
    /// below it is a hole in the file, and it is persisted in the first
    /// eight bytes the same way the log persists its own.
    begin: AtomicU64,
    /// Durable up to here.
    synced: AtomicU64,
    /// The page being filled, and the lock that serialises appends. A
    /// pass is single threaded and the maintenance mutex keeps two passes
    /// apart, so this is uncontended, and it is here so that the tier is
    /// safe on its own terms rather than because of what the callers
    /// happen to do.
    ///
    /// Buffering is what makes the tier worth having. Write through does
    /// not have to mean a system call a record: an unbuffered tier moved
    /// a third of the bytes the log's own compaction moved and took
    /// several times as long doing it, because four hundred thousand
    /// records is four hundred thousand `pwrite`s. A page at a time is
    /// one write per thousand records, and the page is still on the
    /// device before anything depends on it being there, because the
    /// caller syncs before it reclaims the region the records came from.
    appending: Mutex<Buffer>,
    /// Where the buffered page starts, published so a reader can tell
    /// that a record is in the file without taking the append lock.
    ///
    /// It only ever moves up, and it moves after the page below it has
    /// been written, so a record below the value a reader loads is a
    /// record the file holds. That is the whole of what a reader needed
    /// the lock for, and the lock is held across a page sized `pwrite`
    /// while a pass migrates, so wanting it was the difference between a
    /// read that costs a `pread` and a read that costs a `pread` plus
    /// however long somebody else's write takes. #557.
    filling: AtomicU64,
    /// Bytes written here since the tier was opened, and records.
    pub written: AtomicU64,
    pub records: AtomicU64,
}

impl Cold {
    /// The tier of a database being created, which replaces whatever is
    /// beside the path. A new log with an old cold file next to it would
    /// hand back records from a database that no longer exists.
    pub fn create(path: &Path) -> Result<Self> {
        Self::with(path, true)
    }

    /// The tier of a database being reopened. A database that never had
    /// a pass reach the tier has no cold file, and that is not an error:
    /// it gets an empty one.
    pub fn open(path: &Path) -> Result<Self> {
        Self::with(path, false)
    }

    fn with(path: &Path, fresh: bool) -> Result<Self> {
        let beside = path_beside(path);
        let file = if fresh {
            file::create_or_replace(&beside)?
        } else if beside.exists() {
            file::open_rw(&beside)?
        } else {
            file::create_new(&beside)?
        };
        file::make_sparse(&file);
        let cold = Self {
            file,
            path: beside,
            tail: AtomicU64::new(BASE + FIRST),
            begin: AtomicU64::new(BASE + FIRST),
            synced: AtomicU64::new(BASE + FIRST),
            appending: Mutex::new(Buffer::at(BASE + FIRST)),
            filling: AtomicU64::new(BASE + FIRST),
            written: AtomicU64::new(0),
            records: AtomicU64::new(0),
        };
        let begin = cold.read_begin()?;
        cold.begin.store(begin, Ordering::Release);
        cold.tail.store(begin, Ordering::Release);
        cold.synced.store(begin, Ordering::Release);
        Ok(cold)
    }

    /// The file offset an address falls at.
    #[inline]
    fn at(&self, address: Address) -> u64 {
        address - BASE
    }

    #[inline]
    pub fn tail(&self) -> Address {
        self.tail.load(Ordering::Acquire)
    }

    #[inline]
    pub fn begin(&self) -> Address {
        self.begin.load(Ordering::Acquire)
    }

    /// Addresses the tier spans, which is what it would cost on disk if
    /// nothing had been reclaimed.
    #[inline]
    pub fn span(&self) -> u64 {
        self.tail().saturating_sub(self.begin())
    }

    /// Bytes the file occupies, holes excluded.
    pub fn disk_bytes(&self) -> Result<u64> {
        Ok(file::disk_bytes(&self.file, &self.path)?)
    }

    /// The end of the file as an address, which is where a scan stops.
    pub fn end(&self) -> Result<Address> {
        Ok(BASE + self.file.metadata()?.len())
    }

    /// Reads the persisted floor out of the first eight bytes, or the
    /// first address when the file has never been reclaimed.
    fn read_begin(&self) -> Result<Address> {
        if self.file.metadata()?.len() < FIRST {
            return Ok(BASE + FIRST);
        }
        let mut marker = [0u8; 8];
        file::read_exact_at(&self.file, &mut marker, 0)?;
        let word = u64::from_le_bytes(marker);
        Ok(if word == 0 { BASE + FIRST } else { word })
    }

    /// Puts the floor in the file, which has to be durable before a
    /// single block below it is released. The other order would name
    /// bytes that are gone.
    fn write_begin(&self, begin: Address) -> Result<()> {
        file::write_all_at(&self.file, &begin.to_le_bytes(), 0)?;
        file::sync(&self.file)?;
        Ok(())
    }

    /// Adopts a tail a reopen worked out, so that new records land above
    /// what the file already holds.
    ///
    /// The buffer is put back over the page the tail lands in, prefix and
    /// all. A page is written from its own start, so the bytes the run
    /// before this one left in that page have to be in the buffer or the
    /// first flush after a reopen would write zeros over them.
    pub fn resume_at(&self, address: Address) {
        let mut buffer = self.appending.lock().expect("zu2 cold append");
        *buffer = Buffer::at(address);
        self.filling.store(buffer.base, Ordering::Release);
        let held = buffer.len;
        if held > 0 {
            let from = self.at(buffer.base);
            // Best effort: a short file here means the page holds nothing
            // to preserve, which is the ordinary case for a tail that
            // ends where the file does.
            // SAFETY: the buffer is a whole page of words and `held` is
            // an offset inside it.
            let into = unsafe {
                std::slice::from_raw_parts_mut(buffer.data.as_mut_ptr().cast::<u8>(), held)
            };
            let _ = file::read_exact_at(&self.file, into, from);
        }
        self.tail.store(address, Ordering::Release);
        self.synced.store(address, Ordering::Release);
    }

    /// Appends a record and returns where it went.
    ///
    /// The record has no `previous`, for the reason in the module
    /// comment, and it keeps the version of the record it was copied
    /// from so that a replay can tell which of two records for a key is
    /// the newer one (#436).
    ///
    /// A record never straddles a page here either, and where the
    /// allocator skips it leaves a pad record behind, so that a scan of
    /// this file can read zeros as damage exactly the way the log's scan
    /// does (#472).
    pub fn append(
        &self,
        version: u64,
        key: &[u8],
        value: &[u8],
        tombstone: bool,
        kind: u32,
    ) -> Result<Address> {
        let size = record::size_of(key.len(), value.len());
        if size > PAGE_SIZE {
            return Err(Error::RecordTooLarge {
                size,
                page: PAGE_SIZE,
            });
        }

        let mut buffer = self.appending.lock().expect("zu2 cold append");
        if buffer.room() < size {
            if buffer.room() >= record::HEADER {
                // The pad record that says the rest of the page is
                // nothing rather than a block the device lost (#472).
                let at = buffer.len;
                // SAFETY: a whole header fits in what is left of the page
                // and the buffer is 8 byte aligned at its start, which is
                // where records are aligned from.
                let spot = buffer.spot(at);
                unsafe {
                    record::write_at(
                        spot,
                        crate::addr::NULL,
                        0,
                        &[],
                        &[],
                        false,
                        record::KIND_PAD,
                    );
                }
                buffer.len += record::HEADER;
            }
            self.flush(&mut buffer)?;
            *buffer = Buffer::at(page_start(page_of(buffer.base) + 1));
            // After the flush, so a reader that sees this value can rely
            // on everything below it being in the file.
            self.filling.store(buffer.base, Ordering::Release);
        }

        let at = buffer.base + buffer.len as u64;
        let offset = buffer.len;
        // SAFETY: the record fits in what is left of the page, and the
        // buffer is 8 byte aligned at an offset a record may start at.
        let spot = buffer.spot(offset);
        unsafe {
            record::write_at(
                spot,
                crate::addr::NULL,
                version,
                key,
                value,
                tombstone,
                kind,
            );
        }
        buffer.len += size;
        self.tail.store(at + size as u64, Ordering::Release);
        self.written.fetch_add(size as u64, Ordering::Relaxed);
        self.records.fetch_add(1, Ordering::Relaxed);
        Ok(at)
    }

    /// Puts what the buffer holds in the file, without an fsync and
    /// without touching where the buffer is. Writing the same page again
    /// later writes the same bytes plus the new ones, so a flush is
    /// repeatable and a partial page can go down as often as a sync asks
    /// for it.
    fn flush(&self, buffer: &mut Buffer) -> Result<()> {
        if buffer.len == 0 {
            return Ok(());
        }
        file::write_all_at(&self.file, buffer.filled(), self.at(buffer.base))?;
        Ok(())
    }

    /// Puts everything appended so far on the device.
    pub fn sync(&self) -> Result<()> {
        let tail = self.tail();
        if self.synced.load(Ordering::Acquire) >= tail {
            return Ok(());
        }
        {
            let mut buffer = self.appending.lock().expect("zu2 cold append");
            self.flush(&mut buffer)?;
        }
        file::sync(&self.file)?;
        self.synced.fetch_max(tail, Ordering::AcqRel);
        Ok(())
    }

    /// Reads a record into `into`, which is left holding it 8 byte
    /// aligned.
    ///
    /// The checksum is verified here, because this is the only way a
    /// cold record ever reaches a reader: there are no resident pages
    /// down here, so nothing else would ever look at it (see
    /// [`crate::log::Log::load`], which checks for the same reason).
    pub fn load(&self, address: Address, into: &mut Vec<u64>) -> Result<()> {
        if address < self.begin() || address >= self.tail() {
            return Err(Error::Malformed {
                address,
                why: "outside the cold tier",
            });
        }
        let mut written = self.filling.load(Ordering::Acquire);
        if address >= written {
            // The page being filled is not in the file yet, and a record
            // that has just been migrated is read the moment somebody
            // looks the key up. This is the only case that wants the
            // append lock, and it is the rare one.
            let buffer = self.appending.lock().expect("zu2 cold append");
            if address >= buffer.base {
                let from = (address - buffer.base) as usize;
                return buffer.read(from, address, into);
            }
            written = buffer.base;
        }
        // One read where the obvious two would be a header and then the
        // record it describes. A cold read is the slowest thing in the
        // read path, because there are no resident pages down here and
        // every one of these is a syscall that may go to the device, so
        // paying two of them for a record that nearly always fits in the
        // first is the wrong trade. SPECULATE covers the YCSB record and
        // everything smaller, and a record longer than it costs the
        // second read it would have cost anyway. #557.
        //
        // The ask is bounded by the page being filled and the answer is
        // allowed to be shorter than the ask, which is not the same
        // thing. A page whose last record left less than a header of
        // room takes no pad record, so the file ends a few bytes below
        // the page boundary, and asking for a whole speculation over the
        // last record in that page is asking for bytes nobody has
        // written. What comes back is the record, which is what this is
        // for, and demanding the rest turned a good read into an
        // unexpected end of file.
        let ceiling = (written - address) as usize;
        let ask = SPECULATE.min(ceiling).max(record::HEADER);
        into.clear();
        into.resize(ask.div_ceil(8), 0);
        // SAFETY: the buffer is `ask` bytes and 8 byte aligned.
        let speculated =
            unsafe { std::slice::from_raw_parts_mut(into.as_mut_ptr().cast::<u8>(), ask) };
        let have = file::read_upto_at(&self.file, speculated, self.at(address))?;
        if have < record::HEADER {
            return Err(Error::Malformed {
                address,
                why: "the cold file ends inside a record header",
            });
        }
        // SAFETY: as above, and the lengths are only used to size a
        // second read.
        let size = unsafe {
            let r = RecordRef::new(into.as_ptr().cast());
            record::size_of(r.key_len(), r.value_len())
        };
        if size > PAGE_SIZE {
            return Err(Error::Malformed {
                address,
                why: "a cold record longer than a page",
            });
        }
        if size > have {
            into.resize(size.div_ceil(8), 0);
            // SAFETY: the buffer is size bytes and 8 byte aligned, and
            // the first `have` of them are already the record's.
            let rest = unsafe {
                std::slice::from_raw_parts_mut(
                    into.as_mut_ptr().cast::<u8>().add(have),
                    size - have,
                )
            };
            file::read_exact_at(&self.file, rest, self.at(address) + have as u64)?;
        }
        // SAFETY: as above, and a whole record is there now.
        let intact = unsafe { RecordRef::new(into.as_ptr().cast()).intact() };
        if !intact {
            return Err(Error::Malformed {
                address,
                why: "checksum does not hold",
            });
        }
        Ok(())
    }

    /// Hands the front of the tier back to the filesystem, after the
    /// caller has moved everything live out of it.
    pub fn reclaim_to(&self, upto: Address, epochs: &crate::epoch::Epochs) -> Result<u64> {
        let from = self.begin();
        debug_assert_eq!(
            self.at(upto) % PAGE_SIZE as u64,
            0,
            "reclaim to a page boundary"
        );
        if upto <= from {
            return Ok(0);
        }
        self.sync()?;
        self.write_begin(upto)?;
        self.begin.store(upto, Ordering::Release);
        // A reader that passed the bounds check at the top of `load` is
        // about to pread, and down here that pread is the only copy of
        // the record: there are no resident pages in this tier. Punching
        // between the check and the read gives it a page of zeros and a
        // checksum that does not hold, which is what #563 was, one run
        // in four of the reclaim test. The new floor is published above,
        // so a reader that starts now stops before it asks; this waits
        // out the ones that started before it.
        epochs.wait_for_quiescence();
        // Never the first block: it holds the floor, and a hole there
        // would zero the very thing that says where the tier starts.
        let floor = self.at(from).max(file::BLOCK);
        if self.at(upto) <= floor {
            return Ok(0);
        }
        if file::punch(&self.file, floor, self.at(upto) - floor) {
            Ok(self.at(upto) - floor)
        } else {
            Ok(0)
        }
    }

    /// Walks the records in `[from, end)` in address order, and returns
    /// where it stopped, which is the end of the durable prefix.
    ///
    /// Same reading as the log's scan: a zero header is the end, a pad
    /// record is the end of its page, and a checksum that does not hold
    /// stops the walk. There is no padless format to allow for, because
    /// the tier did not exist before pad records did.
    pub fn walk<F>(&self, from: Address, end: Address, mut each: F) -> Result<Address>
    where
        F: FnMut(RecordRef<'_>, Address) -> Result<()>,
    {
        let mut page = vec![0u64; PAGE_SIZE / 8];
        let mut at = from;
        while at < end {
            let number = page_of(at);
            let bytes = (end - page_start(number)).min(PAGE_SIZE as u64) as usize;
            // SAFETY: the buffer is a whole page and 8 byte aligned.
            let into =
                unsafe { std::slice::from_raw_parts_mut(page.as_mut_ptr().cast::<u8>(), bytes) };
            file::read_exact_at(&self.file, into, self.at(page_start(number)))?;
            let mut offset = (at - page_start(number)) as usize;
            let mut stopped = None;
            while offset + record::HEADER <= bytes {
                // SAFETY: a whole header is inside the buffer, and
                // nothing past it is touched before the lengths have
                // been checked against what is left of the page.
                let size = unsafe {
                    let r = RecordRef::new(page.as_ptr().cast::<u8>().add(offset));
                    let size = record::size_of(r.key_len(), r.value_len());
                    if r.key_len() == 0 && r.value_len() == 0 {
                        if r.kind() == record::KIND_PAD && r.intact() {
                            break;
                        }
                        stopped = Some(page_start(number) + offset as u64);
                        break;
                    }
                    if offset + size > bytes || !r.intact() {
                        stopped = Some(page_start(number) + offset as u64);
                        break;
                    }
                    each(
                        RecordRef::new(page.as_ptr().cast::<u8>().add(offset)),
                        page_start(number) + offset as u64,
                    )?;
                    size
                };
                offset += size;
            }
            if let Some(stopped) = stopped {
                return Ok(stopped);
            }
            at = page_start(number + 1);
        }
        Ok(end.min(at))
    }
}

/// The page an append is filling, before it goes to the device.
///
/// `base` is where the page's first record starts, which is [`FIRST`] in
/// the very first page because the eight bytes below it hold the floor,
/// and a page boundary everywhere else. `data` holds the bytes from
/// `base` on, so a flush is one write of `data[..len]` at `base` and the
/// floor is never written over.
struct Buffer {
    base: Address,
    /// A page of words rather than bytes, because a record header is
    /// read and written through a pointer and wants 8 byte alignment,
    /// which a `Vec<u8>` does not promise.
    data: Vec<u64>,
    len: usize,
}

impl Buffer {
    /// A buffer over the page `address` falls in, filled up to it.
    fn at(address: Address) -> Self {
        let base = page_start(page_of(address)).max(BASE + FIRST);
        Self {
            base,
            data: vec![0u64; PAGE_SIZE / 8],
            len: (address - base) as usize,
        }
    }

    /// The page as bytes, up to what has been filled.
    fn filled(&self) -> &[u8] {
        // SAFETY: the buffer is a whole page of words and `len` never
        // passes it.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr().cast::<u8>(), self.len) }
    }

    /// Where a record starting at `at` bytes into the page goes.
    fn spot(&mut self, at: usize) -> *mut u8 {
        // SAFETY: as above, and every caller has checked the record fits.
        unsafe { self.data.as_mut_ptr().cast::<u8>().add(at) }
    }

    /// Bytes left in the page.
    fn room(&self) -> usize {
        PAGE_SIZE - (self.base - page_start(page_of(self.base))) as usize - self.len
    }

    /// Copies the record at `from` out into `into`, 8 byte aligned, the
    /// way [`Cold::load`] does out of the file.
    fn read(&self, from: usize, address: Address, into: &mut Vec<u64>) -> Result<()> {
        if from + record::HEADER > self.len {
            return Err(Error::Malformed {
                address,
                why: "a cold address above the page being filled",
            });
        }
        // SAFETY: a whole header is in the buffer and the buffer starts
        // 8 byte aligned at an address a record may start at.
        let size = unsafe {
            let r = RecordRef::new(self.data.as_ptr().cast::<u8>().add(from));
            record::size_of(r.key_len(), r.value_len())
        };
        if from + size > self.len {
            return Err(Error::Malformed {
                address,
                why: "a cold record that runs past the page being filled",
            });
        }
        into.clear();
        into.resize(size.div_ceil(8), 0);
        // SAFETY: the buffer is size bytes and 8 byte aligned.
        let bytes = unsafe { std::slice::from_raw_parts_mut(into.as_mut_ptr().cast::<u8>(), size) };
        bytes.copy_from_slice(&self.filled()[from..from + size]);
        Ok(())
    }
}

/// Whether the tier is worth having at all for a log in this format. A
/// file written before pad records existed is read under the older and
/// more forgiving rule, and mixing that with a tier whose scan does not
/// have to be forgiving is a distinction nobody needs.
pub fn usable(format: u8) -> bool {
    format != log::FORMAT_PADLESS
}
