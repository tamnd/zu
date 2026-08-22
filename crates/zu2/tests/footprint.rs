//! What the engine says it costs on disk against what the filesystem
//! says.
//!
//! #631. Every zu2 row of the go-ycsb sweep carries two space numbers,
//! `Db::disk_bytes` from the engine and a `du` taken from outside the
//! process after it exits, and at a million records on gamingpc they
//! were 27.9 MiB apart. The sign was the interesting part: `du` counts a
//! strict superset of the files, the log and the cold tier and the
//! checkpoint and the relink journal against the engine's log and cold
//! tier alone, and it still reported less. So the engine was claiming
//! blocks that were not there.
//!
//! Y3 (#376) asks that where an engine reports its own footprint beside
//! the filesystem's, the two are shown to agree. Until they do, neither
//! number can be quoted as the storage claim, and the compactness half
//! of the milestone rests on exactly that number.
//!
//! These tests do the comparison in a place it can be stepped through.
//! They deliberately measure at two moments, with the database open and
//! after it is closed, because a close trims and checkpoints and the
//! guess on the issue was that the gap is a trim landing after the
//! engine counted.

#![cfg(unix)]

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use zu2::{Db, Durability, Options};

const VALUE: usize = 1000;

/// Blocks on the device, counted the way `du` counts them, over every
/// file the database left at this path. `blocks()` is in 512 byte units
/// by POSIX regardless of the filesystem's own block size, and it counts
/// what is allocated rather than what the length says, so a hole punched
/// log reports the hole as free. That is the whole point of using it:
/// zu2's compaction punches holes and a length based figure would call
/// a compacted log the size it was before.
fn device_bytes(path: &Path) -> u64 {
    let dir = path.parent().expect("parent");
    let stem = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    let mut total = 0;
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().to_string();
        // The log is `x.zu2` and everything beside it is `x.zu2.something`,
        // so one prefix test finds the lot.
        if name == stem || name.starts_with(&format!("{stem}.")) {
            total += entry.metadata().expect("metadata").blocks() * 512;
        }
    }
    total
}

/// Names every file at the path with its size, for an assertion message
/// that says which file the gap is in rather than only that there is one.
fn breakdown(path: &Path) -> String {
    let dir = path.parent().expect("parent");
    let stem = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    let mut parts: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if name == stem || name.starts_with(&format!("{stem}.")) {
            let m = entry.metadata().expect("metadata");
            parts.push(format!(
                "{name} {} on device, {} long",
                m.blocks() * 512,
                m.len()
            ));
        }
    }
    parts.sort();
    parts.join(", ")
}

/// What the engine said, at the two moments the harness could ask.
struct Said {
    /// With the database still open, which is when the adapter's
    /// `printStorage` asks.
    open: u64,
    /// After the close, which is when the harness's `du` runs.
    closed_device: u64,
}

fn write(path: &Path, rows: u32) -> u64 {
    let db = Db::create(
        path,
        Options {
            durability: Durability::Async,
            index_buckets: (rows as usize / 4 + 1).next_power_of_two(),
            ..Options::default()
        },
    )
    .expect("create");
    {
        let mut s = db.session();
        let value = [b'v'; VALUE];
        for i in 0..rows {
            let k = format!("user{i:019}");
            s.upsert(k.as_bytes(), &value).expect("upsert");
        }
    }
    // Async durability leaves a tail the file does not have yet, and
    // measuring before that lands would compare an engine that has
    // counted the bytes against a filesystem that has not received them.
    db.sync().expect("sync");
    let said = db.disk_bytes().expect("disk_bytes");
    drop(db);
    said
}

/// The harness's comparison exactly: ask the engine while it is open,
/// then ask the filesystem after the process would have exited.
fn write_and_watch(path: &Path, rows: u32) -> Said {
    let open = write(path, rows);
    Said {
        open,
        closed_device: device_bytes(path),
    }
}

#[test]
fn the_engine_and_the_filesystem_agree_about_the_log_after_a_close() {
    const ROWS: u32 = 20_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("footprint.zu2");

    let Said {
        open: said,
        closed_device: saw,
    } = write_and_watch(&path, ROWS);

    // Two allowances, and both are one sided.
    //
    // The filesystem is allowed to be larger, because it counts files
    // the engine does not answer for: the checkpoint and, if a compaction
    // ran, the relink journal. That is a superset and it is expected.
    //
    // It is not allowed to be meaningfully smaller. Blocks the engine
    // claims and the device does not have are the #631 gap, and one page
    // of slack is there for the tail block rounding rather than for a
    // trend.
    let slack = 4 << 20;
    assert!(
        saw + slack >= said,
        "the engine claims {said} bytes on device and the filesystem has \
         {saw}, which is {} short. Files: {}",
        said.saturating_sub(saw),
        breakdown(&path)
    );
    // And the engine should not be wildly under either, or the number is
    // not describing the database at all. The payload alone is 20 MiB.
    assert!(
        said >= (ROWS as u64 * VALUE as u64),
        "the engine claims {said} bytes for {} bytes of payload. Files: {}",
        ROWS as u64 * VALUE as u64,
        breakdown(&path)
    );
}

#[test]
fn the_gap_does_not_grow_with_the_database() {
    // The reason this one exists rather than just the row above: a fixed
    // overhead that the engine does not count is a documentation problem
    // and a proportional one is a bug. #631 was found at a million
    // records and 2.6 per cent, which is the shape of the second.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut gaps: Vec<(u32, i64, u64)> = Vec::new();
    for rows in [5_000u32, 20_000, 80_000] {
        let path = dir.path().join(format!("grow{rows}.zu2"));
        let Said {
            open: said,
            closed_device: saw,
        } = write_and_watch(&path, rows);
        eprintln!(
            "rows {rows}: engine {said}, device {saw}, gap {}",
            saw as i64 - said as i64
        );
        gaps.push((rows, saw as i64 - said as i64, said));
    }
    // Compare the largest against the smallest as a fraction of what the
    // engine claimed. A constant overhead shrinks as a fraction and a
    // proportional one does not.
    let frac = |&(_, gap, said): &(u32, i64, u64)| gap.unsigned_abs() as f64 / said as f64;
    let small = frac(&gaps[0]);
    let large = frac(&gaps[2]);
    assert!(
        large <= small.max(0.01),
        "the disagreement is {:.2} per cent of the database at {} rows and \
         {:.2} per cent at {} rows, so it is not a fixed overhead: {gaps:?}",
        small * 100.0,
        gaps[0].0,
        large * 100.0,
        gaps[2].0,
    );
}

/// The size #631 was found at, which is a gigabyte of database and too
/// slow for the default suite, so it is opt in:
///
/// ```text
/// cargo test --release -p zu2 --test footprint -- --ignored --nocapture
/// ```
///
/// It is here rather than in a scratch program because the gap on that
/// issue appeared at a million records and not at the sizes above, and a
/// reproduction that only exists in somebody's shell history is not a
/// reproduction.
#[test]
#[ignore = "writes a gigabyte"]
fn the_two_numbers_agree_at_the_size_the_gap_was_found_at() {
    const ROWS: u32 = 1_000_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("million.zu2");
    let Said {
        open: said,
        closed_device: saw,
    } = write_and_watch(&path, ROWS);
    eprintln!(
        "rows {ROWS}: engine {said}, device {saw}, gap {} ({:.2} per cent). Files: {}",
        saw as i64 - said as i64,
        (saw as f64 - said as f64) / said as f64 * 100.0,
        breakdown(&path)
    );
    let slack = 4 << 20;
    assert!(
        saw + slack >= said,
        "the engine claims {said} bytes on device and the filesystem has {saw}. Files: {}",
        breakdown(&path)
    );
}
