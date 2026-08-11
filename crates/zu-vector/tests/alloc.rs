//! The zero-allocation gate (perf/02 section 2, perf/09 section 6).
//!
//! A counting global allocator wraps the system one; the steady-state
//! operator loop (reset arena, build vectors, filter, select, sum) must
//! perform zero heap allocations once the arena owns its blocks. This is
//! its own integration test binary so the allocator shim cannot distort
//! any other test.
//!
//! The count is per thread, not per process. A global allocator sees
//! every thread, and the test harness runs the body on a worker while
//! its own thread is alive; on a loaded machine that thread lands an
//! allocation inside the measured window and the gate fails on
//! somebody else's work. Counting only the thread doing the loop
//! measures the loop.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    // Const-initialized and Copy, so the slot itself neither allocates
    // on first touch nor registers a destructor. Anything else here
    // would recurse into the allocator being counted.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

fn bump() {
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

fn allocs() -> usize {
    ALLOCS.with(Cell::get)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
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

    // The counter has to be able to see a heap allocation on this
    // thread, or the gate below passes by measuring nothing.
    let sighted = allocs();
    drop(std::hint::black_box(vec![0u8; 64]));
    assert!(allocs() > sighted, "the counter is not counting");

    // Warm up: the arena grows its blocks here.
    let expected = run(&mut arena);

    let before = allocs();
    for _ in 0..100 {
        assert_eq!(run(&mut arena), expected);
    }
    let after = allocs();
    assert_eq!(
        after - before,
        0,
        "steady-state morsel loop must not touch the heap"
    );
}
