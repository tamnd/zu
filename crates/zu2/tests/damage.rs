//! What a reopen does when a block in the middle of the log is gone.
//!
//! Recovery scans from the compaction floor and stops at the first
//! record that does not parse, which bounds the durable prefix. That is
//! the right answer for a torn tail, since a crash mid-write leaves the
//! last record short and everything before it good.
//!
//! A hole in the middle is a different thing and it used to be read as
//! the first thing. Page padding was zeros and so is a block the device
//! lost, so recovery threw away the rest of the page the hole was in,
//! resumed at the next page boundary, and installed everything above.
//! What came back was a prefix with a suffix stapled on and a hole
//! between them, which is the one shape the log's durability contract
//! says cannot happen, and `Db::open` returned `Ok` with nothing
//! anywhere saying a byte had gone missing (#472).
//!
//! Three shapes, each on its own copy of the same file: a zeroed block,
//! a block of noise, and a single flipped bit. Noise and a flipped bit
//! were always caught by the checksum. Zeros are the interesting one,
//! and they are also the likely one, since a filesystem that loses a
//! block hands back zeros and so does a sparse region.

use std::io::{Read, Seek, SeekFrom, Write};

use zu2::{Db, Durability, Error, Options};

const KEYS: u64 = 20_000;
const VALUE: usize = 400;

fn options() -> Options {
    Options {
        durability: Durability::Async,
        index_buckets: 1 << 14,
        max_pages: 64,
        max_nodes: 1 << 10,
        // Off, so what is damaged is the log the writes left and not
        // one a compaction pass happened to have rewritten.
        compact_below: 0,
        ..Options::default()
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("k{i:016}").into_bytes()
}

/// Where the damage goes: the first byte of page one.
///
/// A page start is always a record start, since a record never straddles
/// a page, and a record start is what makes the zeroed shape interesting
/// at all. Damage that lands in the middle of a record is caught by that
/// record's checksum whatever the bytes are, which is the case that
/// always worked. Damage that lands on a header is the case that did
/// not, and picking the address by hand is how the test stays about that
/// case instead of about wherever `len / 2` happened to fall.
const DAMAGE_AT: u64 = 4 << 20;

/// A file of `KEYS` records, synced and closed, and how long it is.
fn written(path: &std::path::Path) -> u64 {
    let db = Db::create(path, options()).expect("create");
    let mut session = db.session();
    let value = vec![b'x'; VALUE];
    for i in 0..KEYS {
        session.upsert(&key(i), &value).expect("upsert");
    }
    drop(session);
    db.sync().expect("sync");
    drop(db);
    std::fs::metadata(path).expect("metadata").len()
}

fn patch(path: &std::path::Path, at: u64, with: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to damage");
    file.seek(SeekFrom::Start(at)).expect("seek");
    file.write_all(with).expect("write");
    file.sync_all().expect("sync");
}

fn byte_at(path: &std::path::Path, at: u64) -> u8 {
    let mut file = std::fs::File::open(path).expect("open to read");
    file.seek(SeekFrom::Start(at)).expect("seek");
    let mut one = [0u8; 1];
    file.read_exact(&mut one).expect("read");
    one[0]
}

/// The three damage shapes, as a name and what to write over the middle
/// of the file. An empty patch means flip one bit of whatever is there.
const SHAPES: [(&str, usize); 3] = [
    ("a zeroed 4 KiB block", 0),
    ("4 KiB of noise", 1),
    ("one flipped bit", 2),
];

fn damage(path: &std::path::Path, at: u64, shape: usize) {
    match shape {
        0 => patch(path, at, &[0u8; 4096]),
        1 => patch(
            path,
            at,
            &(0..4096u32).map(|i| (i * 31 + 7) as u8).collect::<Vec<u8>>(),
        ),
        _ => patch(path, at, &[byte_at(path, at) ^ 0x40]),
    }
}

/// Which keys an open finds, or the error it gave instead.
fn keys_found(path: &std::path::Path, options: Options) -> Result<Vec<u64>, Error> {
    let db = Db::open(path, options)?;
    let mut session = db.session();
    let mut out = Vec::new();
    let mut found = Vec::new();
    for i in 0..KEYS {
        if session.read(&key(i), &mut out).expect("read") {
            found.push(i);
        }
    }
    Ok(found)
}

/// The gaps in a key set, as inclusive ranges, which is what tells a
/// prefix apart from a prefix with a suffix stapled on.
fn gaps(found: &[u64]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut run: Option<(u64, u64)> = None;
    let mut next = 0u64;
    for &i in found {
        if i > next {
            run = Some((next, i - 1));
        }
        if let Some(g) = run.take() {
            out.push(g);
        }
        next = i + 1;
    }
    if next < KEYS {
        out.push((next, KEYS - 1));
    }
    out
}

/// The point of the whole change. Every shape of damage in the same
/// place loses the same thing, and what survives is a prefix.
#[test]
fn every_damage_shape_loses_the_same_suffix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.zu2");
    let len = written(&good);
    let whole = keys_found(&good, options()).expect("undamaged opens");
    assert_eq!(whole.len() as u64, KEYS, "undamaged file lost keys");
    println!(
        "{len} bytes, {KEYS} keys, undamaged: {} keys, {} gap(s)",
        whole.len(),
        gaps(&whole).len()
    );

    let salvaging = Options {
        salvage: true,
        ..options()
    };
    let mut agreed: Option<Vec<u64>> = None;
    for (name, shape) in SHAPES {
        let copy = dir.path().join(format!("{}.zu2", name.replace(' ', "_")));
        std::fs::copy(&good, &copy).expect("copy");
        damage(&copy, DAMAGE_AT, shape);
        let found = keys_found(&copy, salvaging).expect("salvaged open");
        let holes = gaps(&found);
        println!(
            "{name:>22} at byte {DAMAGE_AT}: {} of {KEYS} keys, {} gap(s): {}",
            found.len(),
            holes.len(),
            holes
                .iter()
                .map(|(a, b)| format!("{a}..={b}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(
            holes.len(),
            1,
            "{name} left {} gaps, so what came back is not a prefix",
            holes.len()
        );
        assert_eq!(
            holes[0].1,
            KEYS - 1,
            "{name} left live records above the damage"
        );
        match &agreed {
            None => agreed = Some(found),
            Some(first) => assert_eq!(&found, first, "{name} lost a different suffix"),
        }
    }
}

/// A hole is not a torn tail and the two are told apart by looking
/// above. Records up there were acknowledged, so an open that quietly
/// dropped them would be a database missing keys it promised.
#[test]
fn a_hole_is_reported_rather_than_opened_short() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.zu2");
    written(&good);
    patch(&good, DAMAGE_AT, &[0u8; 4096]);

    match keys_found(&good, options()) {
        Err(Error::LogHole { at, above }) => {
            assert!(above > at, "the hole is at {at} and reads above at {above}");
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(found) => panic!("opened short with {} of {KEYS} keys", found.len()),
    }

    // And the operator can decide to take the prefix anyway, which is
    // the only thing to do when the alternative is losing it as well.
    // On its own copy, because a salvaged open appends to the prefix and
    // the file it leaves is not the file it was given.
    let copy = dir.path().join("salvaged.zu2");
    std::fs::copy(&good, &copy).expect("copy");
    let db = Db::open(
        &copy,
        Options {
            salvage: true,
            ..options()
        },
    )
    .expect("salvaged open");
    assert!(
        db.recovered()
            .discarded
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "a salvaged open did not say how much it threw away"
    );
}

/// A torn tail still says nothing, because there is nothing above it
/// and nothing above it was ever acknowledged.
#[test]
fn a_torn_tail_is_not_a_hole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.zu2");
    let len = written(&good);
    // The tail of the last record, which is what a write that was in flight
    // when the machine went down leaves behind.
    patch(&good, len - 100, &[0xa5u8; 100]);
    let found = keys_found(&good, options()).expect("a torn tail opens");
    assert!(
        found.len() as u64 >= KEYS - 2,
        "a torn tail cost {} keys",
        KEYS - found.len() as u64
    );
}

/// A file written before pad records existed has no pad records in it,
/// so a zero header in the middle of a page is its page padding and
/// reading it strictly would refuse most of a log that is perfectly
/// good. Such a file says what it is in its marker word and keeps the
/// old reading, which means it keeps the old exposure too. That is the
/// honest trade, and it is why the format is stamped rather than
/// assumed.
#[test]
fn an_older_file_keeps_the_older_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.zu2");
    written(&good);
    // The marker word is the format in the top byte and the log's floor
    // under it, so clearing that one byte is what an older file looks
    // like from here.
    let mut marker = [0u8; 8];
    let mut file = std::fs::File::open(&good).expect("open to read marker");
    file.read_exact(&mut marker).expect("read marker");
    drop(file);
    assert_ne!(marker[7], 0, "a fresh file was not stamped");
    marker[7] = 0;
    patch(&good, 0, &marker);
    patch(&good, DAMAGE_AT, &[0u8; 4096]);

    let found = keys_found(&good, options()).expect("an older file still opens");
    let holes = gaps(&found);
    assert_eq!(holes.len(), 1, "the damage did something else entirely");
    assert!(
        holes[0].1 < KEYS - 1,
        "an older file was read under the new rule, so it lost its whole suffix"
    );
}
