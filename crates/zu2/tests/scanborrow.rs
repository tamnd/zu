//! What a borrowing scan is allowed to call stable.
//!
//! [`Session::scan_borrowed`] tells its caller, per record, whether the
//! value it was handed will outlive the call, and the caller keeps a
//! pointer to the ones it was told will. Getting that flag wrong is not
//! a slow answer, it is a wrong one: the caller reads the bytes later
//! and finds whatever landed there since.
//!
//! The flag used to be a test on the address alone, cold or above the
//! read only boundary. That misses the third way a record fails to be
//! where it says it is: its log page has been evicted, so the read went
//! into this session's one scratch buffer and the next record overwrote
//! it. Every record in the scan then came back pointing at the same
//! bytes.
//!
//! The read half, [`Session::read_borrowed`], made the same test and has
//! been given the same guard, but it is not testable from out here and
//! there is no test for it below: a read hands out one slice and the
//! contract ends at the next call on the session, and until that call
//! the scratch still holds the record the read put there. The flag was
//! wrong and the bytes were right.
//!
//! Found through go-ycsb (#751). A reopened workload E database serves
//! its first scans out of pages the warmer has not read back yet, so a
//! scan of fifty rows handed back fifty pointers to one row, and the
//! integrity check saw the same key fifty times in a row.

use zu2::{Db, Durability, Options};

fn key(i: u64) -> Vec<u8> {
    format!("usertable:user{i:019}").into_bytes()
}

/// A value that says which key it belongs to, so a slice read back late
/// can be checked against the key it was handed with.
fn value(i: u64) -> Vec<u8> {
    let mut v = key(i);
    v.resize(1000, b'.');
    v
}

/// Small enough that most of what is written leaves memory, which is the
/// state a reopen is in before the warmer has caught up.
fn options() -> Options {
    Options {
        durability: Durability::Async,
        ordered: true,
        memory_pages: 2,
        ..Options::default()
    }
}

#[test]
fn a_borrowed_scan_does_not_call_the_scratch_buffer_stable() {
    const N: u64 = 20_000;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("b.zu2");

    {
        let db = Db::create(&path, options()).expect("create");
        let mut session = db.session();
        for i in 0..N {
            session.upsert(&key(i), &value(i)).expect("upsert");
        }
        drop(session);
        db.sync().expect("sync");
    }

    let db = Db::open(&path, options()).expect("open");
    let mut session = db.session();
    // The pointer and not a copy of the bytes, which is the whole point:
    // a copy taken inside the callback is correct however wrong the flag
    // is, and every caller that takes one is why this went unnoticed.
    let mut stable: Vec<(Vec<u8>, *const u8, usize)> = Vec::new();
    let walked = unsafe {
        session.scan_borrowed(b"", 200, |k, v, keep| {
            if keep {
                stable.push((k.to_vec(), v.as_ptr(), v.len()));
            }
        })
    }
    .expect("scan");
    assert!(walked > 0, "the scan found nothing to walk");

    let mut wrong = 0;
    for (k, at, len) in &stable {
        // SAFETY: the scan said these outlive the call and the release
        // below has not happened yet, which is exactly the window the
        // contract covers.
        let v = unsafe { std::slice::from_raw_parts(*at, *len) };
        if !v.starts_with(k) {
            wrong += 1;
            if wrong == 1 {
                panic!(
                    "a value called stable does not belong to its key, key {:?} value {:?}",
                    String::from_utf8_lossy(k),
                    String::from_utf8_lossy(&v[..v.len().min(40)]),
                );
            }
        }
    }
    session.scan_release();
}
