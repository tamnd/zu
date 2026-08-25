//! What `map_settled` costs the process in mappings, as opposed to in
//! memory.
//!
//! The region count comes from `/proc/self/maps`, so the half of each
//! test that is about regions runs on Linux and is skipped elsewhere.
//! The rest of each test runs everywhere, and so does the compiler: a
//! file behind a `cfg` at the crate level is not type checked on the
//! machine it is written on, which is how a Linux only test goes stale
//! without anybody finding out until CI.
//!
//! The property is #768. A settled page mapped with a null address hint
//! lands wherever the kernel put it, and the kernel hands out addresses
//! in the opposite order to the file offsets the log maps in, so two
//! pages that are neighbours in the file are never neighbours in memory
//! and their mappings cannot be merged. That is one region of address
//! space per 4 MiB page, which makes every fault walk a longer tree and
//! puts a ceiling on the database at `vm.max_map_count` pages. Mapping
//! into one reservation instead makes it one region however many pages
//! are in it.

use zu2::{Db, Durability, Options};

/// Lines of `/proc/self/maps`, which is one per region of address space
/// the process holds, and `None` where there is nowhere to ask.
fn regions() -> Option<usize> {
    if cfg!(target_os = "linux") {
        Some(
            std::fs::read_to_string("/proc/self/maps")
                .expect("/proc/self/maps")
                .lines()
                .count(),
        )
    } else {
        None
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

/// Sixteen pages of log with the window at one, so most of what is
/// written settles and is eligible to be mapped.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        map_settled: true,
        max_pages: 64,
        mutable_pages: 1,
        index_buckets: 1 << 12,
        max_nodes: 1 << 10,
        compact_below: 0,
        ..Options::default()
    }
}

#[test]
fn a_mapping_database_does_not_take_a_region_of_address_space_per_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let before = regions();
    let db = Db::create(&dir.path().join("m.zu2"), options()).expect("create");

    // 8 KiB a record over 6000 records is about 48 MiB, which is a
    // dozen pages, and the reservation is 66. So this settles enough
    // pages to tell the two behaviours apart and not enough to wrap the
    // ring, which is a different test and would need the log to lap.
    let value = vec![b'v'; 8 << 10];
    {
        let mut s = db.session();
        for i in 0..6000u32 {
            s.upsert(&key(i), &value).expect("upsert");
        }
    }
    db.sync().expect("sync");

    // The flusher is what maps, so this waits for it rather than for a
    // fixed time. Ten seconds is far past what a local disk needs for
    // fifty megabytes and is a ceiling rather than a delay.
    let mut mapped = 0;
    for _ in 0..1000 {
        mapped = db.mapped_pages();
        if mapped >= 8 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        mapped >= 8,
        "only {mapped} pages ever mapped, so this run says nothing about what mapping costs"
    );
    assert_eq!(db.remap_refused(), 0, "the kernel refused a mapping");

    if let (Some(before), Some(after)) = (before, regions()) {
        let added = after.saturating_sub(before);
        // Not zero and not one. The reservation is a region of its own,
        // the pages inside it merge into a second while the run below
        // them is still unmapped, the allocator takes regions of its own
        // for the page slots, and the test harness is a running program.
        // What this asserts is the shape: the count does not follow the
        // pages.
        assert!(
            added < mapped,
            "{added} regions of address space for {mapped} mapped pages, \
             which is the one a page of #768 rather than the reservation"
        );
    }

    // And the data is still there through them, which is the part a
    // mapping bug turns into a fault rather than a wrong answer.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in (0..6000u32).step_by(37) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out.len(), value.len(), "{i} came back the wrong length");
    }
}

/// A mapping that loses the race to an evictor goes back the way the
/// reservation takes it back, and not with a `munmap` that would punch a
/// hole in the reservation.
///
/// `remap_settled` maps outside the allocation lock and only publishes
/// under it, so a page can be evicted between the two and the mapping is
/// given back unpublished. Doing that with a plain `munmap` returns the
/// range to the process rather than to the log, and the next unrelated
/// `mmap` anywhere in the process is free to take it, at which point the
/// log's next `MAP_FIXED` over that page takes it away again. #776.
///
/// The visible consequence is regions: the reservation is one region
/// while it is whole and three the first time a page is punched out of
/// the middle of it. So this writes under a page bound, which is what
/// puts an evictor on the writer's thread while the flusher is mapping,
/// and then asks whether the count followed the races.
#[test]
fn a_mapping_that_loses_its_race_does_not_punch_a_hole_in_the_reservation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let before = regions();
    let db = Db::create(
        &dir.path().join("l.zu2"),
        Options {
            // Four pages of memory against sixty four of log, so the
            // writer is evicting behind itself for the whole run and the
            // flusher is mapping into the same slots.
            memory_pages: 4,
            ..options()
        },
    )
    .expect("create");

    let value = vec![b'v'; 8 << 10];
    {
        let mut s = db.session();
        for i in 0..20000u32 {
            s.upsert(&key(i % 6000), &value).expect("upsert");
        }
    }
    db.sync().expect("sync");
    for _ in 0..500 {
        if db.remap_lost() > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let lost = db.remap_lost();
    if lost == 0 {
        // The race is a race. A run that did not hit it has not shown
        // the giveback is wrong and has not shown it is right, so it
        // says so rather than passing quietly.
        eprintln!("no mapping ever lost its race, so this run tested nothing");
        return;
    }
    eprintln!("{lost} mappings lost their race");
    if let (Some(before), Some(after)) = (before, regions()) {
        let added = after.saturating_sub(before) as u64;
        assert!(
            added < lost,
            "{added} regions of address space after {lost} lost mappings, \
             which is a hole a race of #776 rather than a whole reservation"
        );
    }

    // And the pages that were mapped over those holes still read, which
    // is what a `MAP_FIXED` over somebody else's mapping turns into.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in (0..6000u32).step_by(37) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out.len(), value.len(), "{i} came back the wrong length");
    }
}

/// The reservation goes back at drop, all of it, and does not leak a
/// region per database opened.
///
/// A `munmap` of one page out of the reservation would split it, and a
/// reservation that was never given back would show up here as a count
/// that climbs with every open. Ten rounds, because one would only
/// prove the first one worked.
#[test]
fn opening_and_closing_a_mapping_database_gives_the_regions_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let value = vec![b'v'; 8 << 10];
    let mut settled = 0;
    let before = regions();
    for round in 0..10 {
        let db = Db::create(&dir.path().join(format!("r{round}.zu2")), options()).expect("create");
        {
            let mut s = db.session();
            for i in 0..2000u32 {
                s.upsert(&key(i), &value).expect("upsert");
            }
        }
        db.sync().expect("sync");
        for _ in 0..200 {
            if db.mapped_pages() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        settled += db.mapped_pages();
        drop(db);
    }
    assert!(
        settled > 0,
        "no round ever mapped a page, so nothing here was tested"
    );
    if let (Some(before), Some(after)) = (before, regions()) {
        assert!(
            after <= before + 4,
            "{before} regions before ten opens and closes, {after} after, \
             so something is not being given back"
        );
    }
}
