//! The zero-allocation gate (perf/02 section 2, perf/09 section 6).
//!
//! A counting global allocator wraps the system one; the steady-state
//! operator loop (reset arena, build vectors, filter, select, sum) must
//! perform zero heap allocations once the arena owns its blocks. This is
//! its own integration test binary so the allocator shim cannot distort
//! any other test.

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

use zu_vector::{Bitmap, CmpOp, MorselArena, PhysType, SelVector, ValueVector, kernels};

#[test]
fn steady_state_morsel_loop_allocates_nothing() {
    let source: Vec<i64> = (0..2048).map(|i| (i * 37) % 1000).collect();
    let mut arena = MorselArena::new();

    let run = |arena: &mut MorselArena| -> i64 {
        arena.reset();
        let v = ValueVector::flat_from(arena, PhysType::Int64, &source);
        let c = ValueVector::constant(arena, PhysType::Int64, 500i64, source.len());
        let mut bits = Bitmap::new_in(arena, source.len(), false);
        kernels::compare(CmpOp::Lt, &v, &c, None, &mut bits).unwrap();
        let sel = SelVector::from_bitmap(arena, &bits);
        kernels::sum_i64(&v, Some(&sel))
    };

    // Warm up: the arena grows its blocks here.
    let expected = run(&mut arena);

    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 0..100 {
        assert_eq!(run(&mut arena), expected);
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        0,
        "steady-state morsel loop must not touch the heap"
    );
}
