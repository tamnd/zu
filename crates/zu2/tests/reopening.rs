//! What a reopen owes the run that came before it.
//!
//! An entry names the head of a chain and everything behind it, so the
//! index is only as good as the links in the log, and the links were
//! written under whatever table that run happened to have. A run that
//! outgrew its table, or a reopen that asks for a different one, gives
//! the scan records whose links point at addresses that mean nothing in
//! the table it is filling. Installing on top of them dropped whatever
//! the entry reached and lost keys that had been acknowledged and made
//! durable, with nothing in the file wrong to show for it (#462).
//!
//! These are the two shapes of that: the table growing under the run,
//! which is the default and needs nobody to ask for it, and a reopen
//! that names a smaller table than the one the file was written with.

use zu2::{Db, Durability, Options};

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

fn options(buckets: usize, grow: bool) -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: buckets,
        grow_index: grow,
        max_pages: 64,
        max_nodes: 1 << 10,
        // Off, so what reopens is the log the writes left and not one a
        // pass happened to have rewritten.
        compact_below: 0,
        ..Options::default()
    }
}

const N: u32 = 20_000;

/// The default: a table sized by a hint that the key set passes, so the
/// index doubles a few times while the records are being written. Every
/// doubling makes the links below it stale, and there is no warning
/// anywhere because the run itself is fine. The loss only shows up on
/// the next open.
#[test]
fn a_database_that_outgrew_its_table_reopens_with_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("g.zu2");
    // Sixteen buckets is a hundred and twenty eight slots for twenty
    // thousand keys, so the table has to double seven times to hold
    // them.
    let small = options(16, true);
    {
        let db = Db::create(&path, small).expect("create");
        let mut s = db.session();
        for i in 0..N {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        // Second versions of a third of them, so the chains hold more
        // than one record per key.
        for i in (0..N).step_by(3) {
            s.upsert(&key(i), &value(i + N)).expect("update");
        }
        db.sync().expect("sync");
        assert!(
            db.index_grows() > 0,
            "the table never grew, so this proves nothing"
        );
    }

    let db = Db::open(&path, small).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..N {
        let want = if i % 3 == 0 { value(i + N) } else { value(i) };
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, want, "key {i} came back at the wrong version");
    }
}

/// The same thing from the other side: the file was written with a table
/// the reopen does not ask for. Nothing stops a host from doing this and
/// nothing about the file says what size it was written with, so the
/// scan has to cope rather than assume.
#[test]
fn a_reopen_with_a_smaller_table_keeps_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.zu2");
    {
        let db = Db::create(&path, options(1 << 12, false)).expect("create");
        let mut s = db.session();
        for i in 0..N {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        db.sync().expect("sync");
    }

    // Eight buckets for twenty thousand keys, so nearly every record
    // arrives at a full bucket and has to take an entry over.
    let db = Db::open(&path, options(8, false)).expect("reopen");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, value(i), "key {i} read back the wrong value");
    }
}

/// The repair happens in memory, and memory is not where the record
/// lives. A page that leaves and comes back brings the link it had on
/// the device, so a scan that repaired one and did not write it back
/// would hand the keys behind it out until the first eviction and then
/// stop.
#[test]
fn a_repaired_link_survives_its_page_leaving_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("e.zu2");
    // A kilobyte a record and twenty thousand of them is five pages, so
    // a reopen that keeps two of them in memory has to evict.
    let value = vec![b'x'; 1000];
    let small = options(16, true);
    {
        let db = Db::create(&path, small).expect("create");
        let mut s = db.session();
        for i in 0..N {
            s.upsert(&key(i), &value).expect("upsert");
        }
        db.sync().expect("sync");
    }

    let evicting = Options {
        memory_pages: 2,
        ..small
    };
    let db = Db::open(&path, evicting).expect("reopen");
    let mut s = db.session();
    // Writing is what makes a page leave: the log opens pages at the
    // tail and retires the ones that have fallen out of the window, so
    // twenty more megabytes puts every page the scan repaired on the
    // device and nowhere else.
    let bulk = vec![b'y'; 5000];
    for i in N..N + 4_000 {
        s.upsert(&key(i), &bulk).expect("upsert");
    }
    let mut out = Vec::new();
    for i in 0..N {
        assert!(s.read(&key(i), &mut out).expect("read"), "key {i} is gone");
        assert_eq!(out, value, "key {i} read back the wrong value");
    }
}
