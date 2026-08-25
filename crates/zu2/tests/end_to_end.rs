//! End to end tests: the four record operations, the graph plane, and
//! what survives a close and reopen.
//!
//! These run against a real file, because the thing worth testing is the
//! boundary between memory and disk. A test that only exercised the
//! in-memory tail would pass whatever the log did.

use std::sync::Arc;

use zu2::{Db, Direction, Durability, Options};

fn options(durability: Durability) -> Options {
    Options {
        durability,
        // Small enough that the tests cross page boundaries and exercise
        // the read-only region rather than staying in the mutable window.
        index_buckets: 1 << 10,
        max_pages: 64,
        max_nodes: 1 << 16,
        ..Options::default()
    }
}

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

fn value(i: u32) -> Vec<u8> {
    format!("field0=value{i:09}").into_bytes()
}

#[test]
fn the_four_operations_agree_with_a_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    let mut expected = std::collections::HashMap::new();
    let mut out = Vec::new();

    for i in 0..2000u32 {
        s.upsert(&key(i), &value(i)).expect("upsert");
        expected.insert(key(i), value(i));
    }
    // Overwrite half of them with a value of the same length, which is
    // the case the in-place path takes, and a quarter with a longer one,
    // which it cannot.
    for i in (0..2000u32).step_by(2) {
        let fresh = value(i + 100_000);
        s.upsert(&key(i), &fresh).expect("update");
        expected.insert(key(i), fresh);
    }
    for i in (0..2000u32).step_by(4) {
        let long = vec![b'x'; 200];
        s.upsert(&key(i), &long).expect("grow");
        expected.insert(key(i), long);
    }
    for i in (0..2000u32).step_by(7) {
        assert!(s.delete(&key(i)).expect("delete"), "delete missed {i}");
        expected.remove(&key(i));
    }

    for i in 0..2000u32 {
        let found = s.read(&key(i), &mut out).expect("read");
        match expected.get(&key(i)) {
            Some(want) => {
                assert!(found, "key {i} went missing");
                assert_eq!(&out, want, "key {i} has the wrong value");
            }
            None => assert!(!found, "deleted key {i} came back"),
        }
    }
    assert!(!s.read(b"absent", &mut out).expect("read"), "phantom key");
}

#[test]
fn a_read_modify_write_counts_without_losing_an_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("z.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    let mut scratch = Vec::new();
    for _ in 0..500 {
        s.rmw(b"counter", &mut scratch, |current, out| {
            let n = match current {
                Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("eight bytes")),
                None => 0,
            };
            out.extend_from_slice(&(n + 1).to_le_bytes());
        })
        .expect("rmw");
    }
    let mut out = Vec::new();
    assert!(s.read(b"counter", &mut out).expect("read"));
    assert_eq!(
        u64::from_le_bytes(out.try_into().expect("eight bytes")),
        500
    );
}

#[test]
fn what_was_acknowledged_is_there_after_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("z.zu2");
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..500u32 {
            s.upsert(&key(i), &value(i)).expect("upsert");
        }
        for i in (0..500u32).step_by(5) {
            s.delete(&key(i)).expect("delete");
        }
    }
    let db = Db::open(&path, options(Durability::Durable)).expect("open");
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..500u32 {
        let found = s.read(&key(i), &mut out).expect("read");
        if i % 5 == 0 {
            assert!(!found, "deleted key {i} came back from the log");
        } else {
            assert!(found, "key {i} did not survive the reopen");
            assert_eq!(out, value(i), "key {i} came back wrong");
        }
    }
    // The reopened database has to be writable, and a key written after
    // recovery has to be findable next to the recovered ones.
    s.upsert(b"after", b"recovery").expect("upsert");
    assert!(s.read(b"after", &mut out).expect("read"));
    assert_eq!(out, b"recovery");
}

#[test]
fn a_graph_survives_a_reopen_with_its_edges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("g.zu2");
    let nodes = 400u32;
    {
        let db = Db::create(&path, options(Durability::Durable)).expect("create");
        let mut s = db.session();
        for i in 0..nodes {
            let id = s.add_node(&key(i)).expect("node");
            assert_eq!(id, i, "ids are handed out in creation order");
        }
        // A ring plus a chord, so every node has degree two out and a
        // few have more, which puts some neighbourhoods out of line.
        for i in 0..nodes {
            s.add_edge(i, (i + 1) % nodes).expect("edge");
            s.add_edge(i, (i * 7 + 1) % nodes).expect("edge");
        }
        // A hub, to force a block that doubles several times. From one,
        // because a self edge is a different question than this test is
        // asking.
        for i in 1..nodes {
            s.add_edge(0, i).expect("hub edge");
        }
        s.remove_edge(0, 3).expect("remove");
    }

    let db = Db::open(&path, options(Durability::Durable)).expect("open");
    let mut s = db.session();
    let mut scratch = Vec::new();
    assert_eq!(
        s.node_of(&key(11), &mut scratch).expect("node_of"),
        Some(11),
        "the key to id mapping did not survive"
    );
    assert_eq!(
        db.core().graph().nodes(),
        nodes,
        "the id counter did not survive"
    );
    let hub = s.neighbours(Direction::Out, 0, |n| n.to_vec());
    assert_eq!(
        hub.len(),
        (nodes - 2) as usize,
        "the hub lost edges: {} of {nodes}",
        hub.len()
    );
    assert!(!hub.contains(&3), "the removed edge came back");
    assert!(!hub.contains(&0), "a self edge appeared");
    assert!(hub.windows(2).all(|w| w[0] < w[1]), "neighbours unsorted");
    assert_eq!(s.degree(Direction::Out, 0), hub.len() as u32);
    // Every ring edge has a reverse, which is what makes an in-hop cost
    // the same as an out-hop.
    for i in 1..nodes {
        let back = s.neighbours(Direction::In, (i + 1) % nodes, |n| n.to_vec());
        assert!(back.contains(&i), "node {i} lost its reverse edge");
    }
}

#[test]
fn two_hops_are_the_distinct_neighbours_of_the_neighbours() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("h.zu2"), options(Durability::Async)).expect("create");
    let mut s = db.session();
    for i in 0..64u32 {
        s.add_node(&key(i)).expect("node");
    }
    // A grid of eight by eight, linked right and down, so the two hop
    // set from a corner is small enough to write out by hand.
    for row in 0..8u32 {
        for col in 0..8u32 {
            let here = row * 8 + col;
            if col + 1 < 8 {
                s.add_edge(here, here + 1).expect("right");
            }
            if row + 1 < 8 {
                s.add_edge(here, here + 8).expect("down");
            }
        }
    }
    let mut seen = Vec::new();
    let mut first = Vec::new();
    let mut out = Vec::new();
    s.two_hop(Direction::Out, 0, &mut seen, &mut first, &mut out);
    out.sort_unstable();
    // From 0 the first hop is {1, 8} and the second is {2, 9} from 1 and
    // {9, 16} from 8, deduplicated.
    assert_eq!(out, vec![2, 9, 16]);
    assert!(
        seen.iter().all(|&w| w == 0),
        "the bitmap was left dirty for the next probe"
    );
    // Run it again on the same buffers, which is what a benchmark does,
    // and it has to give the same answer.
    s.two_hop(Direction::Out, 0, &mut seen, &mut first, &mut out);
    out.sort_unstable();
    assert_eq!(out, vec![2, 9, 16]);
}

#[test]
fn concurrent_writers_and_readers_agree_on_every_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::create(&dir.path().join("c.zu2"), options(Durability::Durable)).expect("create"),
    );
    let threads = 4u32;
    let per_thread = 1000u32;
    let mut handles = Vec::new();
    for t in 0..threads {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let mut s = db.session();
            for i in 0..per_thread {
                let k = key(t * per_thread + i);
                s.upsert(&k, &value(i)).expect("upsert");
            }
            // Read back what this thread wrote while the others are
            // still writing, which is the case the tentative bit and the
            // foreign chains exist for.
            let mut out = Vec::new();
            for i in 0..per_thread {
                let k = key(t * per_thread + i);
                assert!(s.read(&k, &mut out).expect("read"), "lost {k:?}");
                assert_eq!(out, value(i));
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..threads * per_thread {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out, value(i % per_thread), "key {i} has the wrong value");
    }
}

#[test]
fn a_record_that_left_memory_still_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("e.zu2"),
        Options {
            durability: Durability::Durable,
            // Two pages resident out of the eight or so this writes, so
            // most of what it reads back has to come off disk.
            memory_pages: 2,
            max_pages: 64,
            index_buckets: 1 << 12,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let big = vec![b'v'; 8192];
    let records = 1500u32;
    for i in 0..records {
        s.upsert(&key(i), &big).expect("upsert");
    }
    let mut out = Vec::new();
    for i in 0..records {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), big.len(), "key {i} came back short");
    }
}

/// The bound holds while the writing is going on, and not only after it
/// stops.
///
/// A page can only be evicted once its bytes are durable, so under
/// `Async` nothing used to connect the writer to the flusher and the
/// log held the whole load: at a bound of two pages, 80 MiB of writes
/// peaked at 48 MiB held and 320 MiB peaked at 240, four times the load
/// for five times the overshoot. #775 put back pressure on the append
/// path, and this is what says so.
///
/// The ceiling is generous on purpose. The writer's patience runs out
/// after ten milliseconds and then it takes the memory anyway, which is
/// the right way round for liveness, so a machine whose flusher is
/// starved can go over. What it cannot do is go over by an amount that
/// scales with the load, and eight pages against a load of eighty is
/// the assertion that it does not: to gain a single page over the bound
/// a writer has to get through five hundred appends of eight kilobytes,
/// which at ten milliseconds of patience each is five seconds of a
/// flusher that has stopped answering.
#[test]
fn the_page_bound_holds_while_the_writing_is_going() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 2;
    let db = Arc::new(
        Db::create(
            &dir.path().join("w.zu2"),
            Options {
                durability: Durability::Async,
                mutable_pages: 1,
                memory_pages: bound,
                max_pages: 512,
                index_buckets: 1 << 12,
                compact_below: 0,
                ..Options::default()
            },
        )
        .expect("create"),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let peak = Arc::clone(&peak);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(db.resident_pages(), Ordering::Relaxed);
            }
        })
    };
    {
        let mut s = db.session();
        let big = vec![b'v'; 8192];
        // 320 MiB of appends, eighty times the bound.
        for i in 0..40_000u32 {
            s.upsert(&key(i), &big).expect("upsert");
        }
    }
    stop.store(true, Ordering::Relaxed);
    watcher.join().expect("watcher");

    let peak = peak.load(Ordering::Relaxed);
    assert!(
        peak <= 8,
        "a database with a bound of {bound} pages held {peak} while writing 320 MiB"
    );

    // And it still reads back, which is what the back pressure is not
    // allowed to cost.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in (0..40_000u32).step_by(397) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), 8192, "key {i} came back short");
    }
}

/// The bound is a bound on the steady state, so it has to hold once the
/// writing stops.
///
/// Eviction runs where a thread opens a page and it can only drop a
/// page whose bytes are already on the device, so a burst of async
/// appends outruns the flusher and leaves the last pages resident. That
/// used to be expected while the burst was going on, until #775 made the
/// writer wait for the flusher rather than run away from it. What is not
/// last thing to open a page is the last append, so when the flusher
/// catches up a moment later there is nobody left to notice, and a
/// database that has stopped writing sits above its bound for as long
/// as it stays open. A read heavy run after a load is exactly that
/// shape, and it is the shape every memory number in the series was
/// taken on. #636.
#[test]
fn the_page_bound_holds_after_the_writing_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 2;
    let db = Db::create(
        &dir.path().join("m.zu2"),
        Options {
            durability: Durability::Async,
            // One mutable page, so the bound is not clamped up to the
            // window. Two pages is 8 MiB and the load below is ten
            // times that.
            mutable_pages: 1,
            memory_pages: bound,
            max_pages: 64,
            index_buckets: 1 << 12,
            // Compaction would move the head on its own and this is
            // about eviction.
            compact_below: 0,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let big = vec![b'v'; 8192];
    for i in 0..10_000u32 {
        s.upsert(&key(i), &big).expect("upsert");
    }
    drop(s);
    db.sync().expect("sync");

    // The flusher is what makes a page evictable, so give it the
    // moment it needs to get to the last of them. This is generous: the
    // bytes are already on the device by the time sync returns and what
    // is being waited for is a thread noticing.
    //
    // The page the log ends in is above the bound rather than inside
    // it, which is the engine's own reading of `memory_pages` and is
    // what `an_evicted_page_gives_its_memory_back` in log.rs asserts
    // too. So the number to hold is one more than the bound, and what
    // this test is here to catch is the twenty it held before.
    let want = bound + 1;
    let mut resident = db.resident_pages();
    for _ in 0..100 {
        if resident <= want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        resident = db.resident_pages();
    }
    assert!(
        resident <= want,
        "a database that stopped writing holds {resident} pages against a bound of {bound}"
    );

    // And it still reads, which is the thing the bound is not allowed
    // to cost.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in (0..10_000u32).step_by(97) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), big.len(), "key {i} came back short");
    }
}

/// A settled page becomes a mapping of the file, and reading through one
/// gives back what was written.
///
/// The point of #757 is that the resident set stops being anonymous
/// memory, so the assertion is on the split rather than on the total:
/// most of what is resident should be mapped, the window should not be,
/// and every record should read back the same either way. Compaction runs
/// beside it, because a pass that reclaims a page has to give a mapping
/// back with `munmap` and not with `dealloc`, and getting that wrong is
/// the kind of thing that shows up as a crash here or nowhere.
#[test]
fn a_settled_page_becomes_a_mapping_of_the_file() {
    if !cfg!(unix) {
        // No mapping call on this platform, so the pages stay on the
        // heap and there is nothing to assert. See `file::map_read`.
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("p.zu2"),
        Options {
            durability: Durability::Async,
            mutable_pages: 1,
            map_settled: true,
            max_pages: 64,
            index_buckets: 1 << 12,
            // No eviction, so what is resident stays resident and the
            // only thing that can change about it is its kind.
            memory_pages: usize::MAX,
            compact_below: 0,
            ..Options::default()
        },
    )
    .expect("create");

    let mut s = db.session();
    let big = vec![b'm'; 8192];
    for i in 0..10_000u32 {
        s.upsert(&key(i), &big).expect("upsert");
    }
    drop(s);
    db.sync().expect("sync");

    // The maintainer converts a page after the flush that settles it, so
    // this waits for a thread to notice rather than for any work.
    let mut mapped = db.mapped_pages();
    let mut resident = db.resident_pages();
    for _ in 0..100 {
        if mapped * 4 >= resident * 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        mapped = db.mapped_pages();
        resident = db.resident_pages();
    }
    assert!(
        mapped > 0,
        "{resident} pages resident and not one of them was mapped"
    );
    assert!(
        mapped * 4 >= resident * 3,
        "only {mapped} of {resident} resident pages are mapped, so most of \
         the resident set is still anonymous"
    );

    // Reading through a mapping is the whole point and it is also the
    // thing that faults, so read every key rather than a sample.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..10_000u32 {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), big.len(), "key {i} came back short");
        assert_eq!(out[0], b'm', "key {i} came back as something else");
    }
    drop(s);

    // And a pass that takes the pages back has to unmap them.
    db.compact().expect("compact");
    let mut s = db.session();
    for i in (0..10_000u32).step_by(89) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
    }
}

/// The memory bound counts anonymous pages, so a mapped page is free to
/// keep and the resident set goes well past the bound while the memory
/// the process owns does not.
///
/// This is #759 and it is the shape lmdb has: a million records of lmdb
/// on server2 held 17.6 MiB of anonymous memory against 1090 MiB of
/// data, because everything but its own structures was a mapping the
/// kernel could drop. Before this the bound was a distance from the
/// eviction floor to the tail, so a mapped page counted the same as a
/// heap page and got thrown out of memory to make room for a heap page
/// that cost strictly more. The assertions are on both halves: the
/// anonymous count stays inside the bound, the resident count leaves it
/// far behind, and every key reads back.
#[test]
fn a_mapped_page_is_outside_the_memory_bound() {
    if !cfg!(unix) {
        // No mapping call on this platform, so every page is anonymous
        // and the bound is the old one. See `file::map_read`.
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let bound = 4;
    let db = Db::create(
        &dir.path().join("b.zu2"),
        Options {
            durability: Durability::Async,
            mutable_pages: 1,
            map_settled: true,
            max_pages: 256,
            index_buckets: 1 << 12,
            memory_pages: bound,
            compact_below: 0,
            ..Options::default()
        },
    )
    .expect("create");

    let mut s = db.session();
    let big = vec![b'b'; 8192];
    for i in 0..20_000u32 {
        s.upsert(&key(i), &big).expect("upsert");
    }
    drop(s);
    db.sync().expect("sync");

    // The maintainer does the conversion after the flush that settles a
    // page, so this waits for a thread to get to it rather than for any
    // work to happen.
    let mut resident = db.resident_pages();
    let mut anonymous = db.anonymous_pages();
    for _ in 0..200 {
        if resident > bound * 4 && anonymous <= bound + 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        resident = db.resident_pages();
        anonymous = db.anonymous_pages();
    }
    let mapped = db.mapped_pages();
    assert!(
        resident > bound * 4,
        "{resident} pages resident against a bound of {bound}, so the mapped \
         pages are still being evicted along with the rest"
    );
    assert!(
        anonymous <= bound + 1,
        "{anonymous} anonymous pages against a bound of {bound}, {mapped} \
         mapped, {resident} resident"
    );

    // A page below the eviction floor that is still resident is the new
    // thing here, and the read path was never written for the floor in
    // the first place, so read every key rather than a sample.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in 0..20_000u32 {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost key {i}");
        assert_eq!(out.len(), big.len(), "key {i} came back short");
        assert_eq!(out[0], b'b', "key {i} came back as something else");
    }
}

#[test]
fn an_overflowing_bucket_keeps_every_key_through_updates() {
    // Sixteen buckets, eight entries each, so 128 slots hold 4000 keys.
    // Almost every write displaces an entry and chains behind it, which
    // is the case where a record that chains to the wrong address drops
    // whole keys out of the index without anything else noticing.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(
        &dir.path().join("o.zu2"),
        Options {
            durability: Durability::Async,
            index_buckets: 16,
            max_pages: 1 << 12,
            ..Options::default()
        },
    )
    .expect("create");
    let mut s = db.session();
    let records = 4000u32;
    for i in 0..records {
        s.upsert(&key(i), &value(i)).expect("upsert");
    }
    // Three rounds of updates, so a key gets rewritten while it is deep
    // in someone else's chain as well as while it is at the head. The
    // value length changes every round, which refuses the in-place path
    // and forces the append and swing that the chaining rule governs.
    // The order matters: newest first, so that a chain truncated by an
    // update is not quietly repaired by a later insert of the keys it
    // dropped, which is what an ascending sweep would do.
    for round in 1..=3u32 {
        let expect = |i: u32| {
            let mut v = value(i);
            v.resize(v.len() + round as usize, b'r');
            v
        };
        for i in (0..records).rev() {
            s.upsert(&key(i), &expect(i)).expect("update");
        }
        let mut out = Vec::new();
        for i in 0..records {
            assert!(
                s.read(&key(i), &mut out).expect("read"),
                "lost {i} in {round}"
            );
            assert_eq!(out, expect(i), "stale {i} in {round}");
        }
    }
}
