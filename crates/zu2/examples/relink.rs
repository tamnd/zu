//! How much of a file a reopen rewrites, which is the size of the
//! window #463 leaves open.
//!
//! An index entry names the head of a chain and every record behind it,
//! and those links were written under whatever table shape the run that
//! wrote them had. A reopen filling a differently shaped table cannot
//! install on top of a link that points somewhere else without dropping
//! every key that entry reached (#462), so the scan repairs the link:
//! eight bytes of `previous` and four bytes of checksum twenty four
//! bytes later. The pages that changed then go back to the file whole,
//! and a crash inside one of those writes leaves a record whose checksum
//! does not hold, which ends the durable prefix there and loses
//! everything above it.
//!
//! The cost of closing that hole depends on how often it is open. If a
//! reopen repairs a handful of records the answer is a small journal; if
//! it repairs most of them the answer is Z10 and nothing less. So this
//! counts, over the shapes a real reopen comes in:
//!
//! - fresh: loaded once, never updated, reopened at the same size
//! - fresh, resized: the same file reopened at a quarter of the table
//! - updated: three versions of every key, reopened at the same size
//! - updated, compacted: the same, with a compaction pass before the
//!   close, so the surviving records point at addresses that are gone
//! - grown: loaded into a table sixteen times too small and left to
//!   double, which is the case #462 came out of

use std::path::Path;

use zu2::{Db, Durability, Options};

const RECORDS: u64 = 400_000;
const VALUE_BYTES: usize = 100;

fn key(i: u64) -> Vec<u8> {
    format!("user{i:019}").into_bytes()
}

fn options(buckets: usize, grow: bool, compact: bool) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: buckets,
        grow_index: grow,
        max_pages: 1 << 14,
        max_nodes: 1 << 10,
        compact_below: if compact { 1 << 20 } else { 0 },
        ..Options::default()
    }
}

fn load(path: &Path, buckets: usize, grow: bool, versions: u64, compact: bool) {
    let db = Db::create(path, options(buckets, grow, compact)).expect("create");
    let mut session = db.session();
    for round in 0..versions {
        // A different length each round, so an update writes a record
        // rather than settling into the one that is already there. In
        // place is the common case and it leaves no link to repair,
        // which would make this measure nothing.
        let value = vec![b'x'; VALUE_BYTES + round as usize];
        for i in 0..RECORDS {
            session.upsert(&key(i), &value).expect("upsert");
        }
    }
    drop(session);
    if compact {
        db.compact().expect("compact");
    }
    db.sync().expect("sync");
}

fn reopen(name: &str, path: &Path, buckets: usize) {
    let db = Db::open(path, options(buckets, false, false)).expect("open");
    let r = db.recovered();
    let records = r.records.load(std::sync::atomic::Ordering::Relaxed);
    let relinked = r.relinked.load(std::sync::atomic::Ordering::Relaxed);
    let pages = r.pages.load(std::sync::atomic::Ordering::Relaxed);
    let mut session = db.session();
    let mut out = Vec::new();
    for i in 0..RECORDS {
        assert!(session.read(&key(i), &mut out).expect("read"), "lost {i}");
    }
    drop(session);
    println!(
        "{name:20} records {records:>8}  relinked {relinked:>8} ({:>5.1}%)  pages {pages:>5}",
        relinked as f64 * 100.0 / records.max(1) as f64,
    );
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let roomy = (RECORDS as usize / 4).next_power_of_two();
    println!("{RECORDS} records, {VALUE_BYTES} byte values, roomy is {roomy} buckets");

    let fresh = dir.path().join("fresh.zu2");
    load(&fresh, roomy, false, 1, false);
    reopen("fresh", &fresh, roomy);
    reopen("fresh, resized", &fresh, roomy / 4);

    let updated = dir.path().join("updated.zu2");
    load(&updated, roomy, false, 3, false);
    reopen("updated", &updated, roomy);

    let compacted = dir.path().join("compacted.zu2");
    load(&compacted, roomy, false, 3, true);
    reopen("updated, compacted", &compacted, roomy);

    let grown = dir.path().join("grown.zu2");
    load(&grown, roomy / 16, true, 1, false);
    reopen("grown", &grown, roomy);
}
