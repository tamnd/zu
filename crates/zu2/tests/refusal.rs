//! What a refused mapping costs, and for how long.
//!
//! `map_settled` asks the kernel for a mapping a page and the kernel is
//! allowed to say no. It says no when the process is at
//! `vm.max_map_count`, which is pressure that lifts, so a database that
//! stepped over the pages it was refused would turn a busy minute into a
//! run that never maps anything again. #769.
//!
//! The refusal comes from `file::refuse_mappings`, which is process wide,
//! so this is a test binary of its own and every test in it is
//! `#[serial]` by construction: there is one test.
//!
//! On a platform with no mapping call every attempt is refused anyway and
//! nothing here is reachable, so the whole test is skipped rather than
//! asserting something that is true for the wrong reason.

use zu2::{Db, Durability, Options};

fn key(i: u32) -> Vec<u8> {
    format!("user{i:09}").into_bytes()
}

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

/// Writes `count` records and waits for the flusher to catch up.
fn write(db: &Db, from: u32, count: u32) {
    let value = vec![b'v'; 8 << 10];
    {
        let mut s = db.session();
        for i in from..from + count {
            s.upsert(&key(i), &value).expect("upsert");
        }
    }
    db.sync().expect("sync");
}

/// Waits up to five seconds for `f`, which is the flusher thread doing
/// its work rather than a fixed sleep.
fn until(f: impl Fn() -> bool) -> bool {
    for _ in 0..500 {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// A page the kernel refused is come back to once the kernel stops
/// refusing, rather than left anonymous for the life of the database.
///
/// Three phases. Map some pages, so the run is known to be mapping at
/// all. Arm a wall of refusals and write forty eight megabytes past it,
/// so a dozen pages settle that the kernel says no to. Then take the
/// wall down and write one and a half megabytes, which is less than a
/// page and so settles nothing new, and count the pages that get mapped.
///
/// The sizes are the whole test. Phase three cannot map a page of its
/// own, so every page it maps is one from under the wall, and before
/// be32b17, when `remap_from` was a high water mark and nothing else,
/// there were none to map: the walk had already stepped past them and
/// they stayed anonymous for the life of the database.
#[test]
fn a_refused_page_is_come_back_to_once_the_kernel_stops_refusing() {
    if !cfg!(unix) {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::create(&dir.path().join("r.zu2"), options()).expect("create");

    write(&db, 0, 2000);
    assert!(
        until(|| db.mapped_pages() >= 2),
        "only {} pages mapped before anything was refused, so this run \
         cannot tell a refusal from a database that never maps",
        db.mapped_pages()
    );
    let mapped = db.mapped_pages();
    assert_eq!(db.remap_refused(), 0, "refused before anything was armed");

    // A wall rather than a single refusal, because the flusher runs
    // while this test does and a count of one would be spent by whatever
    // it was already doing. A thousand is more mapping attempts than the
    // rest of this test makes.
    zu2::file::refuse_mappings(1000);
    write(&db, 2000, 6000);
    assert!(
        until(|| db.remap_refused() > 0),
        "nothing was refused with a wall of a thousand refusals armed"
    );
    let refused = db.remap_refused();
    assert_eq!(
        db.mapped_pages(),
        mapped,
        "a page was mapped while every mapping was being refused"
    );

    // Down comes the wall. The write is what wakes the flusher, since
    // nothing else calls the walk, and it is deliberately smaller than a
    // page so that nothing it writes can settle and be mapped on its own
    // account.
    zu2::file::refuse_mappings(0);
    write(&db, 8000, 200);
    let back = until(|| db.mapped_pages() >= mapped + 6);
    eprintln!(
        "{mapped} pages mapped, then {refused} refusals, then {} mapped",
        db.mapped_pages()
    );
    assert!(
        back,
        "{} pages mapped after {refused} refusals cleared, against {mapped} \
         before them, and the write that followed was smaller than a page, \
         so the pages under the refusal were stepped over for good",
        db.mapped_pages()
    );

    // And every record is still readable through the pages that came
    // back, which is the part a mapping bug turns into a fault.
    let mut s = db.session();
    let mut out = Vec::new();
    for i in (0..8200u32).step_by(53) {
        assert!(s.read(&key(i), &mut out).expect("read"), "lost {i}");
        assert_eq!(out.len(), 8 << 10, "{i} came back the wrong length");
    }
}
