//! The zero-allocation gate on warm block reads (perf/04 section 8,
//! perf/09 section 5 P8).
//!
//! A counting global allocator wraps the system one; once a block is in
//! the cache, pinning it again must perform zero heap allocations. This
//! is its own integration test binary so the allocator shim cannot
//! distort any other test.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTER: Counting = Counting;

use zu_zu1::BLOCK_SIZE;
use zu_zu1::file::Zu1File;

#[test]
fn warm_block_pins_allocate_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Zu1File::create(&dir.path().join("warm.zu1")).unwrap();
    let mut ptrs = Vec::new();
    let mut data = vec![0u8; BLOCK_SIZE as usize];
    for i in 0..4u8 {
        data.fill(i + 1);
        let ptr = db.allocate_block();
        db.write_block(ptr, &data).unwrap();
        ptrs.push(ptr);
    }

    // Cold pass: every block faults in exactly once.
    for &ptr in &ptrs {
        let pin = db.pin_block(ptr).unwrap();
        assert_eq!(pin.len(), BLOCK_SIZE as usize);
    }
    let stats = db.cache_stats();
    assert_eq!(stats.misses, 4);

    let before = ALLOCS.load(Ordering::Relaxed);
    for round in 0..100u8 {
        for (i, &ptr) in ptrs.iter().enumerate() {
            let pin = db.pin_block(ptr).unwrap();
            assert_eq!(pin[0], i as u8 + 1, "round {round}");
        }
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(after - before, 0, "warm pins must not touch the heap");
    let stats = db.cache_stats();
    assert_eq!(stats.misses, 4, "warm pins must not touch the file");
    assert_eq!(stats.hits, 400);
}
