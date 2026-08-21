//! Whether the log's page table bounds the live set or the lifetime of
//! the database.
//!
//! It bounded the lifetime, which is #470. `Log::allocate` refused a
//! claim whose page was past a table indexed by absolute page, and the
//! tail only goes up. Compaction moved the floor up behind it and freed
//! the pages below, but a page index once used was never used again, so
//! `max_pages` was not a size at all: it was a budget for every byte the
//! database would ever append. This run used to end at round 47 with
//! "log is full: 16 pages allocated", holding one megabyte of live data
//! after writing eighty three.
//!
//! It now runs to its round limit, and the two columns that say why are
//! span and blocks. Both hold flat while written climbs past four times
//! the table. The apparent length climbs with it and is not a mistake:
//! addresses are monotonic and the file mirrors the address space one to
//! one, so everything below the floor is a hole. Blocks is what the
//! filesystem is actually holding and it is the number that matters.
//!
//! One key set, small enough that the live bytes never grow, updated in
//! a loop with compaction on.

use std::path::Path;

use zu2::{Db, Durability, Options};

/// The file's apparent length and the bytes the filesystem is really
/// holding for it. The second is what a hole punch changes, and the two
/// diverge by exactly the compacted prefix.
fn on_disk(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => (m.len(), m.blocks() * 512),
        Err(_) => (0, 0),
    }
}

const KEYS: u64 = 4_000;
const VALUE: usize = 400;
const PAGES: usize = 16;

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("lifetime.zu2");
    let db = Db::create(
        &path,
        Options {
            durability: Durability::Async,
            index_buckets: 1 << 13,
            max_pages: PAGES,
            max_nodes: 1 << 10,
            // Compact whenever the log is over 8 MiB, so the floor is
            // being pushed up the whole time.
            compact_below: 8 << 20,
            ..Options::default()
        },
    )
    .expect("create");

    let live = KEYS as usize * (VALUE + 64);
    println!(
        "{PAGES} pages is {} MiB of table, the live set is about {} MiB",
        PAGES * 4,
        live / (1 << 20)
    );

    let mut session = db.session();
    let mut round = 0u64;
    let mut written = 0u64;
    loop {
        let value = vec![b'x'; VALUE];
        let mut failed = None;
        for i in 0..KEYS {
            match session.upsert(format!("k{i:016}").as_bytes(), &value) {
                Ok(()) => written += (VALUE + 64) as u64,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failed {
            println!(
                "round {round}: {e}, after writing {} MiB into a {} MiB table",
                written / (1 << 20),
                PAGES * 4
            );
            break;
        }
        round += 1;
        // Compact every round and keep compacting until a pass finds
        // nothing, so nothing here can be blamed on a lazy schedule.
        while db.compact().expect("compact") > 0 {}
        if round.is_multiple_of(16) {
            let (length, blocks) = on_disk(&path);
            println!(
                "round {round:>3}: written {:>4} MiB, span {:>3} MiB, blocks {:>3} MiB, length {:>4} MiB, tail is in page {:>3} of {PAGES}",
                written / (1 << 20),
                db.log_span() / (1 << 20),
                blocks / (1 << 20),
                length / (1 << 20),
                db.log_bytes() / (4 << 20),
            );
        }
        if round > 200 {
            println!(
                "survived 200 rounds and {} MiB written",
                written / (1 << 20)
            );
            break;
        }
    }
}
