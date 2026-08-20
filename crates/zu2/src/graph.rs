//! The graph plane: adjacency where a hop is a load, not a lookup.
//!
//! The requirement this exists for is one sentence. A hop costs O(1) and
//! the neighbours it returns are contiguous. An indexed relational store
//! answers the same question with a B-tree descent per hop and a probe
//! per neighbour, and a document store answers it with an index probe
//! per hop plus a BSON decode per document. Both have the same exponent
//! in the number of hops and a much larger base.
//!
//! Three decisions get that.
//!
//! **Dense node ids.** A node has an external key, which lives in
//! the record plane's hash index, and an internal `u32` assigned in
//! creation order. Traversal uses the internal one, so the node table
//! is an array and node to neighbourhood is an index rather than a
//! lookup. The hash index is consulted exactly once, at the seed.
//!
//! **Two shapes of neighbourhood.** Up to [`INLINE_DEGREE`] neighbours
//! live in the node entry itself, in the cacheline the degree and the
//! version are already in, so the load that finds the node finds its
//! neighbours too. Past that they live in a sorted block of `u32`. Both
//! are read as a `&[u32]`, so a scan is sequential either way, which is
//! the property LiveGraph (PVLDB 13(7), 2020) argues is the one worth
//! protecting. Sorted, because neighbourhood intersection is what
//! triangle counting and most pattern matching reduce to.
//!
//! **One version cell per node, not per neighbour.** Sortledton
//! (PVLDB 15(6), 2022) and Teseo (PVLDB 14(6), 2021) version each
//! neighbour. The 2025 study of the design space (arXiv 2502.10959)
//! measures what that costs: 4.1 to 8.9x the memory of a static CSR,
//! with contention concentrated on the high degree nodes a traversal
//! visits most. Here the neighbourhood is a seqlock: a reader takes the
//! version, walks the slice, and takes the version again. The tradeoff,
//! stated rather than hidden, is that two writers on different edges of
//! the same node serialise. In exchange a node costs 8 bytes of
//! version rather than 8 bytes per edge, and a reader of a ten thousand
//! neighbour list does two atomic loads rather than ten thousand.
//!
//! Durability comes from the record plane. Every edge change is appended
//! to the same log, as a record of kind [`crate::record::KIND_EDGE`],
//! before it is applied in memory. So a transaction that creates a node,
//! sets its properties and links it to four others is a handful of
//! appends to one log made durable by one group commit fsync. That is
//! the half of the claim a relational or document store can match only
//! by giving up the traversal cost.

use std::cell::UnsafeCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::addr::Address;
use crate::db::{Core, Session};
use crate::epoch::Epochs;
use crate::error::{Error, Result};
use crate::record::{KIND_EDGE, KIND_VERTEX, LOCK};

/// Neighbours that fit in the node entry itself.
///
/// A node entry is one cacheline: 8 bytes of version, 4 of length, 4
/// of capacity, 8 of block pointer, and the 40 that are left are ten
/// neighbours. Degree distributions are power law, so this covers the
/// large majority of nodes in every graph the benchmark loads.
pub const INLINE_DEGREE: usize = 10;

/// Nodes per chunk of the node table. Chunks are allocated as the
/// graph grows and never move, so a node lookup is two loads however
/// large the graph gets and no reader ever sees a table being resized.
const CHUNK: usize = 1 << 14;

/// How many locks order the edge records of a node.
///
/// One per node would be a lock per node in a table meant to be one
/// cacheline an entry, and one for the whole graph would put every edge
/// writer in the same queue. A thousand of them makes a collision
/// between two writers on unrelated nodes rare and costs 32 KiB once.
const EDGE_STRIPES: usize = 1024;

/// The payload of an edge record: one operation byte and two ids.
const EDGE_PAYLOAD: usize = 9;

/// Which way an edge is being followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

/// A block a grow replaced, on its way to being freed once no reader can
/// still be inside it. The pointer is only ever handed to the epoch
/// queue, which is why asserting `Send` for it is sound.
struct Retired(*mut u32, usize);

// SAFETY: nothing reads through the pointer after this is built. It
// exists to carry ownership to whichever thread runs the deferred free.
unsafe impl Send for Retired {}

impl Drop for Retired {
    fn drop(&mut self) {
        // SAFETY: allocated by `Vec::with_capacity(self.1)` and leaked,
        // and the epoch guarantees no reader is still walking it.
        unsafe { drop(Vec::from_raw_parts(self.0, self.1, self.1)) }
    }
}

/// One node's neighbours, one cacheline.
#[repr(align(64))]
struct Neighbourhood {
    /// Seqlock: bit 63 is the writer's lock, the rest counts edits.
    version: AtomicU64,
    len: AtomicU32,
    /// Capacity of `block`, or zero while the neighbours are inline.
    cap: AtomicU32,
    /// The out-of-line neighbours, or null.
    block: AtomicPtr<u32>,
    /// The inline neighbours. Written only by the thread holding the
    /// lock and read only under the seqlock, which is why it is a cell
    /// rather than an array of atomics: the whole point of the layout is
    /// that a scan is a plain sequential read of `u32`, not one atomic
    /// load per element.
    inline: UnsafeCell<[u32; INLINE_DEGREE]>,
}

impl Default for Neighbourhood {
    fn default() -> Self {
        Self {
            version: AtomicU64::new(0),
            len: AtomicU32::new(0),
            cap: AtomicU32::new(0),
            block: AtomicPtr::new(std::ptr::null_mut()),
            inline: UnsafeCell::new([0; INLINE_DEGREE]),
        }
    }
}

impl Neighbourhood {
    /// The neighbours as they stand. Callers validate with the version.
    ///
    /// The two loads are ordered length then block, and a writer stores
    /// them block then length, so a reader that sees a length only
    /// reachable out of line is guaranteed to see the block it is in.
    ///
    /// # Safety
    ///
    /// The caller holds epoch protection, so a block the writer has
    /// retired is still mapped, and validates the version afterwards, so
    /// a torn read is discarded rather than believed.
    unsafe fn slice(&self) -> &[u32] {
        let len = self.len.load(Ordering::Acquire) as usize;
        let block = self.block.load(Ordering::Acquire);
        // SAFETY: len never exceeds INLINE_DEGREE while block is null,
        // and never exceeds cap once it is not.
        unsafe {
            if block.is_null() {
                std::slice::from_raw_parts(self.inline.get().cast::<u32>(), len.min(INLINE_DEGREE))
            } else {
                std::slice::from_raw_parts(block, len)
            }
        }
    }

    /// Runs `visit` on the neighbours under the seqlock.
    ///
    /// `visit` may run more than once, because a read that raced a
    /// writer is thrown away and retried, so it has to be pure.
    /// [`Session::neighbours_into`] is the copying form for callers that
    /// cannot promise that.
    ///
    /// # Safety
    ///
    /// The caller holds epoch protection for the whole call.
    unsafe fn read<R>(&self, visit: &mut impl FnMut(&[u32]) -> R) -> R {
        loop {
            let before = self.version.load(Ordering::Acquire);
            if before & LOCK != 0 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: the caller's epoch keeps any retired block alive,
            // and the version check below discards a torn read.
            let answer = visit(unsafe { self.slice() });
            if self.version.load(Ordering::Acquire) == before {
                return answer;
            }
        }
    }

    fn lock(&self) -> u64 {
        loop {
            let current = self.version.load(Ordering::Acquire);
            if current & LOCK == 0
                && self
                    .version
                    .compare_exchange(current, current | LOCK, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return current;
            }
            std::hint::spin_loop();
        }
    }

    fn unlock(&self, taken: u64) {
        self.version.store(taken + 1, Ordering::Release);
    }
}

// SAFETY: the inline cell is written only by the thread holding the
// seqlock and read only under a version check, which is the seqlock
// contract. Every other field is atomic.
unsafe impl Sync for Neighbourhood {}

/// A chunked array of neighbourhoods. Chunks never move and are never
/// freed before the graph is, so an index into one is stable.
struct Table {
    chunks: Box<[AtomicPtr<Neighbourhood>]>,
    growing: Mutex<()>,
}

impl Table {
    fn new(max_nodes: usize) -> Self {
        Self {
            chunks: (0..max_nodes.div_ceil(CHUNK).max(1))
                .map(|_| AtomicPtr::new(std::ptr::null_mut()))
                .collect(),
            growing: Mutex::new(()),
        }
    }

    fn capacity(&self) -> usize {
        self.chunks.len() * CHUNK
    }

    fn get(&self, node: u32) -> Option<&Neighbourhood> {
        let chunk = self.chunks.get(node as usize / CHUNK)?;
        let base = chunk.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        // SAFETY: a non-null chunk holds CHUNK initialised entries and
        // is never freed while the graph lives.
        Some(unsafe { &*base.add(node as usize % CHUNK) })
    }

    fn ensure(&self, node: u32) -> Option<&Neighbourhood> {
        let index = node as usize / CHUNK;
        if index >= self.chunks.len() {
            return None;
        }
        if self.chunks[index].load(Ordering::Acquire).is_null() {
            let _guard = self.growing.lock().expect("zu2 node table");
            if self.chunks[index].load(Ordering::Acquire).is_null() {
                let fresh: Box<[Neighbourhood]> =
                    (0..CHUNK).map(|_| Neighbourhood::default()).collect();
                let raw = Box::into_raw(fresh).cast::<Neighbourhood>();
                self.chunks[index].store(raw, Ordering::Release);
            }
        }
        self.get(node)
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        for chunk in &self.chunks {
            let base = chunk.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if base.is_null() {
                continue;
            }
            // SAFETY: allocated as a Box<[Neighbourhood]> of exactly
            // CHUNK entries, and nothing is running at drop time.
            let owned = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(base, CHUNK)) };
            for entry in owned.iter() {
                let block = entry.block.swap(std::ptr::null_mut(), Ordering::AcqRel);
                if !block.is_null() {
                    drop(Retired(block, entry.cap.load(Ordering::Acquire) as usize));
                }
            }
        }
    }
}

/// The adjacency, both directions.
pub struct Graph {
    out: Table,
    inward: Table,
    next: AtomicU32,
    stripes: Box<[Mutex<()>]>,
}

impl Graph {
    pub fn new(max_nodes: usize) -> Self {
        Self {
            out: Table::new(max_nodes),
            inward: Table::new(max_nodes),
            next: AtomicU32::new(0),
            stripes: (0..EDGE_STRIPES).map(|_| Mutex::new(())).collect(),
        }
    }

    /// Orders the log records of the edges out of `src` against the
    /// adjacency they are applied to.
    ///
    /// An edge writer holds this across appending its record and
    /// applying it, and a compaction pass holds it across asking the
    /// adjacency about an edge and appending its copy. Neither can do
    /// its half atomically without it. A pass reading between a
    /// writer's append and its apply is told the old answer and puts a
    /// copy of it on the log above the record that supersedes it, and a
    /// replay then hands back the state the pass saw rather than the one
    /// the writers agreed on. That was #437. Two writers on the same
    /// edge have the same problem in a smaller way: without an order
    /// they can append in one order and apply in the other, and memory
    /// and the log disagree from then on.
    ///
    /// It is deliberately not the entry lock from [`Neighbourhood`].
    /// That one is a spinlock every reader of the node takes, and
    /// holding it across a log append would stall readers of a hub for
    /// as long as an append takes. Readers never come here.
    pub(crate) fn order_edges(&self, src: u32) -> std::sync::MutexGuard<'_, ()> {
        // Dense ids, so the low bits are already spread.
        let stripe = src as usize & (EDGE_STRIPES - 1);
        self.stripes[stripe].lock().expect("zu2 edge order")
    }

    fn table(&self, direction: Direction) -> &Table {
        match direction {
            Direction::Out => &self.out,
            Direction::In => &self.inward,
        }
    }

    /// Nodes created so far. Ids are dense, so this is also the
    /// exclusive upper bound on a node id in use.
    pub fn nodes(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }

    /// The largest node id plus one this graph was sized for.
    pub fn capacity(&self) -> usize {
        self.out.capacity()
    }

    /// The next id, or nothing when the graph has no room for it.
    ///
    /// A compare and swap rather than a fetch and add because the check
    /// has to be part of the allocation. Adding first and looking after
    /// moves the counter whether or not the id was usable, so a caller
    /// that keeps asking past the end walks `nodes()` up past
    /// `capacity()` and it reports nodes that cannot exist. This is once
    /// per node and every node writes a record, so the loop costs
    /// nothing worth counting.
    pub(crate) fn allocate(&self) -> Option<u32> {
        let capacity = self.capacity();
        loop {
            let next = self.next.load(Ordering::Acquire);
            if next as usize >= capacity {
                return None;
            }
            if self
                .next
                .compare_exchange_weak(next, next + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(next);
            }
        }
    }

    /// Whether both ends of an edge are inside the table, which is what
    /// [`Graph::apply`] would refuse for. Asked before the record is
    /// appended rather than after, because a record on the log that
    /// cannot be applied is one a replay cannot get past either (#455).
    pub(crate) fn holds(&self, src: u32, dst: u32) -> Result<()> {
        let highest = src.max(dst);
        if highest as usize >= self.capacity() {
            return Err(Error::NodeOutOfRange {
                node: highest,
                max: self.capacity(),
            });
        }
        Ok(())
    }

    fn note_node(&self, node: u32) {
        self.next.fetch_max(node + 1, Ordering::AcqRel);
    }

    /// Applies an edge change to memory. The log record is written first
    /// by the caller, which is what makes it durable.
    pub(crate) fn apply(&self, epochs: &Epochs, add: bool, src: u32, dst: u32) -> Result<()> {
        let highest = src.max(dst);
        if highest as usize >= self.capacity() {
            return Err(Error::NodeOutOfRange {
                node: highest,
                max: self.capacity(),
            });
        }
        self.note_node(highest);
        if add {
            self.link(epochs, Direction::Out, src, dst);
            self.link(epochs, Direction::In, dst, src);
        } else {
            self.unlink(Direction::Out, src, dst);
            self.unlink(Direction::In, dst, src);
        }
        Ok(())
    }

    fn link(&self, epochs: &Epochs, direction: Direction, from: u32, to: u32) {
        let Some(entry) = self.table(direction).ensure(from) else {
            return;
        };
        let taken = entry.lock();
        let len = entry.len.load(Ordering::Acquire) as usize;
        let block = entry.block.load(Ordering::Acquire);
        // SAFETY: the lock is held, so this thread is the only writer,
        // and every slice below is built from the length and capacity
        // that lock protects.
        unsafe {
            if block.is_null() && len < INLINE_DEGREE {
                let slots = &mut *entry.inline.get();
                if insert_sorted(&mut slots[..], len, to) {
                    entry.len.store(len as u32 + 1, Ordering::Release);
                }
                entry.unlock(taken);
                return;
            }
            let cap = entry.cap.load(Ordering::Acquire) as usize;
            if block.is_null() || len == cap {
                grow(epochs, entry, len, cap);
            }
            let block = entry.block.load(Ordering::Acquire);
            let cap = entry.cap.load(Ordering::Acquire) as usize;
            let slots = std::slice::from_raw_parts_mut(block, cap);
            if insert_sorted(slots, len, to) {
                entry.len.store(len as u32 + 1, Ordering::Release);
            }
        }
        entry.unlock(taken);
    }

    fn unlink(&self, direction: Direction, from: u32, to: u32) {
        let Some(entry) = self.table(direction).get(from) else {
            return;
        };
        let taken = entry.lock();
        let len = entry.len.load(Ordering::Acquire) as usize;
        let block = entry.block.load(Ordering::Acquire);
        // SAFETY: the lock is held.
        unsafe {
            let slots: &mut [u32] = if block.is_null() {
                std::slice::from_raw_parts_mut(entry.inline.get().cast::<u32>(), INLINE_DEGREE)
            } else {
                std::slice::from_raw_parts_mut(block, entry.cap.load(Ordering::Acquire) as usize)
            };
            if let Ok(at) = slots[..len].binary_search(&to) {
                // The length drops before the hole closes, so a reader
                // that raced sees a short list rather than one past the
                // end, and the version check discards it either way.
                entry.len.store(len as u32 - 1, Ordering::Release);
                slots.copy_within(at + 1..len, at);
            }
        }
        entry.unlock(taken);
    }

    /// The degree of a node, which is one load and no traversal.
    pub fn degree(&self, direction: Direction, node: u32) -> u32 {
        match self.table(direction).get(node) {
            Some(entry) => entry.len.load(Ordering::Acquire),
            None => 0,
        }
    }

    /// Runs `visit` on a node's neighbours.
    ///
    /// # Safety
    ///
    /// The caller announces an epoch for the whole call, and `visit` is
    /// pure, because a read that raced a writer runs it again.
    pub(crate) unsafe fn with_neighbours<R>(
        &self,
        direction: Direction,
        node: u32,
        visit: &mut impl FnMut(&[u32]) -> R,
    ) -> R {
        match self.table(direction).get(node) {
            // SAFETY: forwarded from this function's own contract.
            Some(entry) => unsafe { entry.read(visit) },
            None => visit(&[]),
        }
    }
}

/// Moves a neighbourhood out of line, or doubles the block it is already
/// in. The block it replaces goes to the epoch queue rather than the
/// allocator, because a reader may still be walking it.
///
/// # Safety
///
/// The caller holds the entry's lock.
unsafe fn grow(epochs: &Epochs, entry: &Neighbourhood, len: usize, cap: usize) {
    let wanted = if cap == 0 {
        (INLINE_DEGREE * 2).max(16)
    } else {
        cap * 2
    };
    let mut fresh = Vec::<u32>::with_capacity(wanted);
    // SAFETY: the lock is held, so the source is stable, and the
    // destination has room for `wanted`, which is more than `len`.
    unsafe {
        let old = entry.block.load(Ordering::Acquire);
        let source = if old.is_null() {
            entry.inline.get().cast::<u32>()
        } else {
            old
        };
        std::ptr::copy_nonoverlapping(source, fresh.as_mut_ptr(), len);
        fresh.set_len(wanted);
    }
    let raw = fresh.leak().as_mut_ptr();
    // Capacity is published before the pointer, and the pointer before
    // the length the caller stores, so no reader pairs a new length with
    // an old block.
    entry.cap.store(wanted as u32, Ordering::Release);
    let old = entry.block.swap(raw, Ordering::AcqRel);
    if !old.is_null() {
        let retired = Retired(old, cap);
        epochs.defer(Box::new(move || drop(retired)));
    }
}

/// Inserts `value` into the sorted prefix `slots[..len]`, keeping it
/// sorted. Returns false when it was already there, so a repeated edge
/// does not grow the neighbourhood.
fn insert_sorted(slots: &mut [u32], len: usize, value: u32) -> bool {
    match slots[..len].binary_search(&value) {
        Ok(_) => false,
        Err(at) => {
            slots.copy_within(at..len, at + 1);
            slots[at] = value;
            true
        }
    }
}

impl Session<'_> {
    fn graph(&self) -> &Graph {
        self.core_ref().graph()
    }

    /// Creates a node under an external key and returns its dense id.
    /// The key to id mapping is an ordinary record, so looking a node
    /// up by key is a hash probe and nothing more, and it is paid once
    /// per traversal rather than once per hop.
    pub fn add_node(&mut self, key: &[u8]) -> Result<u32> {
        let Some(node) = self.graph().allocate() else {
            return Err(Error::NodeOutOfRange {
                node: self.graph().nodes(),
                max: self.graph().capacity(),
            });
        };
        self.write(key, &node.to_le_bytes(), false, KIND_VERTEX)?;
        Ok(node)
    }

    /// The dense id of the node with this key.
    pub fn node_of(&mut self, key: &[u8], scratch: &mut Vec<u8>) -> Result<Option<u32>> {
        if !self.read(key, scratch)? || scratch.len() != 4 {
            return Ok(None);
        }
        Ok(Some(u32::from_le_bytes(
            scratch[..4].try_into().expect("four bytes"),
        )))
    }

    /// Links `src` to `dst`, durably.
    pub fn add_edge(&mut self, src: u32, dst: u32) -> Result<()> {
        self.edge(true, src, dst)
    }

    /// Unlinks `src` from `dst`, durably.
    pub fn remove_edge(&mut self, src: u32, dst: u32) -> Result<()> {
        self.edge(false, src, dst)
    }

    fn edge(&mut self, add: bool, src: u32, dst: u32) -> Result<()> {
        let core = self.core;
        // Before anything is written. The record goes down before the
        // adjacency is touched, which is what makes an edge durable, and
        // it used to mean an edge the graph has no room for left its
        // record behind on the way out. A replay cannot apply that
        // record either, so the file could not be opened again: one
        // rejected call and everything in it was gone (#455).
        core.graph().holds(src, dst)?;
        // Before the epoch is announced, so a thread waiting here is not
        // a thread a flush is waiting for, and dropped before the commit
        // so that the device is never waited on under it.
        let order = core.graph().order_edges(src);
        // Inside the epoch, and this is not optional. The flusher decides
        // that a page is complete by publishing its target and waiting
        // for every announced session to leave, so an append that
        // announced nothing is an append the flusher does not know to
        // wait for. It would write the record's bytes half laid down,
        // the checksum would not hold, and the next recovery would stop
        // there and lose every edge after it. The record plane got this
        // right from the start because its write path announces; this
        // one did not, and it showed up as a reopened graph that had
        // dropped a run of its edges.
        self.slot.protect();
        let outcome = (|| -> Result<Address> {
            // The record goes down first. An edge on the log that is not
            // in memory is replayed on the next open; an edge in memory
            // that is not on the log would be a lost write.
            let end = self.append_untracked(KIND_EDGE, &encode_edge(add, src, dst))?;
            let core = self.core_ref();
            core.graph().apply(core.epochs(), add, src, dst)?;
            Ok(end)
        })();
        self.slot.unprotect();
        drop(order);
        self.make_durable(outcome?)
    }

    /// Runs `visit` on a node's neighbours without copying them.
    ///
    /// `visit` may run more than once when it races a writer, so it has
    /// to be pure. This is the form the traversal benchmark uses,
    /// because it is the one that shows what the layout is worth: the
    /// neighbours are handed over as a slice of the storage itself.
    pub fn neighbours<R>(
        &mut self,
        direction: Direction,
        node: u32,
        mut visit: impl FnMut(&[u32]) -> R,
    ) -> R {
        self.slot.protect();
        // SAFETY: the epoch is announced for the whole call, so a block
        // a writer replaced is still mapped.
        let answer = unsafe { self.graph().with_neighbours(direction, node, &mut visit) };
        self.slot.unprotect();
        answer
    }

    /// Copies a node's neighbours out, for callers that cannot make
    /// the purity promise [`Session::neighbours`] asks for.
    pub fn neighbours_into(&mut self, direction: Direction, node: u32, out: &mut Vec<u32>) {
        self.neighbours(direction, node, |slice| {
            out.clear();
            out.extend_from_slice(slice);
        });
    }

    /// The out or in degree, one load.
    pub fn degree(&mut self, direction: Direction, node: u32) -> u32 {
        self.graph().degree(direction, node)
    }

    /// The distinct nodes two hops out, collected into `out`.
    ///
    /// `seen` and `first` are the caller's, so a benchmark can reuse one
    /// allocation across every probe and the number it prints is the
    /// traversal rather than the allocator.
    pub fn two_hop(
        &mut self,
        direction: Direction,
        node: u32,
        seen: &mut Vec<u64>,
        first: &mut Vec<u32>,
        out: &mut Vec<u32>,
    ) {
        let words = (self.graph().nodes() as usize).div_ceil(64).max(1);
        if seen.len() < words {
            seen.resize(words, 0);
        }
        out.clear();
        self.neighbours_into(direction, node, first);
        self.slot.protect();
        let graph = self.core_ref().graph();
        for &near in first.iter() {
            let mut collect = |slice: &[u32]| {
                for &far in slice {
                    let word = far as usize / 64;
                    let bit = 1u64 << (far % 64);
                    if word < seen.len() && seen[word] & bit == 0 {
                        seen[word] |= bit;
                        out.push(far);
                    }
                }
            };
            // SAFETY: the epoch is announced around the whole walk. The
            // closure is not pure, but a retry can only re-add a
            // neighbour the bitmap already holds, which it filters.
            unsafe { graph.with_neighbours(direction, near, &mut collect) };
        }
        self.slot.unprotect();
        // Leave the bitmap clean for the next probe without paying for
        // the whole thing: only the bits this traversal set are cleared.
        for &far in out.iter() {
            seen[far as usize / 64] &= !(1u64 << (far % 64));
        }
    }
}

/// The bytes an edge change is logged as.
pub(crate) fn encode_edge(add: bool, src: u32, dst: u32) -> [u8; EDGE_PAYLOAD] {
    let mut payload = [0u8; EDGE_PAYLOAD];
    payload[0] = u8::from(add);
    payload[1..5].copy_from_slice(&src.to_le_bytes());
    payload[5..9].copy_from_slice(&dst.to_le_bytes());
    payload
}

/// Reads back what [`encode_edge`] wrote. `None` for a payload that is
/// not an edge change, which a recovery scan can meet in a log written
/// by an older build.
pub(crate) fn decode_edge(payload: &[u8]) -> Option<(bool, u32, u32)> {
    if payload.len() != EDGE_PAYLOAD {
        return None;
    }
    let add = payload[0] != 0;
    let src = u32::from_le_bytes(payload[1..5].try_into().expect("four bytes"));
    let dst = u32::from_le_bytes(payload[5..9].try_into().expect("four bytes"));
    Some((add, src, dst))
}

/// Applies an edge record's payload during recovery.
pub(crate) fn replay_edge(core: &Core, payload: &[u8]) -> Result<()> {
    let Some((add, src, dst)) = decode_edge(payload) else {
        return Ok(());
    };
    // An edge the graph has no room for is not a bad record here, it is
    // a file opened with less room than it was written with. Dropping
    // the edge would open a database that has quietly lost part of its
    // graph, so this refuses, and it says what would work rather than
    // repeating what the write path says (#455).
    core.graph()
        .apply(core.epochs(), add, src, dst)
        .map_err(|error| match error {
            Error::NodeOutOfRange { node, max } => Error::GraphTooSmall {
                needs: node as usize + 1,
                max,
            },
            other => other,
        })
}

/// Restores the id counter from a node record during recovery, so that
/// a node which never got an edge does not have its id handed out to
/// the next one.
pub(crate) fn replay_node(core: &Core, value: &[u8]) {
    if value.len() != 4 {
        return;
    }
    let node = u32::from_le_bytes(value.try_into().expect("four bytes"));
    if (node as usize) < core.graph().capacity() {
        core.graph().note_node(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::Slotted;

    fn neighbours_of(epochs: &Epochs, graph: &Graph, direction: Direction, node: u32) -> Vec<u32> {
        let session = Slotted::claim(epochs).expect("slot");
        session.protect();
        let mut visit = |slice: &[u32]| slice.to_vec();
        // SAFETY: the epoch is announced and the closure only reads.
        let out = unsafe { graph.with_neighbours(direction, node, &mut visit) };
        session.unprotect();
        out
    }

    #[test]
    fn a_neighbourhood_stays_sorted() {
        let mut slots = [0u32; 8];
        let mut len = 0;
        for value in [5u32, 1, 9, 3, 7] {
            assert!(insert_sorted(&mut slots, len, value));
            len += 1;
        }
        assert_eq!(&slots[..len], &[1, 3, 5, 7, 9]);
        assert!(!insert_sorted(&mut slots, len, 5), "duplicate accepted");
    }

    #[test]
    fn a_node_entry_is_one_cacheline() {
        assert_eq!(std::mem::size_of::<Neighbourhood>(), 64);
        assert_eq!(std::mem::align_of::<Neighbourhood>(), 64);
    }

    #[test]
    fn a_neighbourhood_survives_the_move_out_of_line() {
        let epochs = Epochs::new(4);
        let graph = Graph::new(1024);
        // Descending, and far enough to double the block twice, so both
        // the first block and its replacements are exercised.
        for dst in (1u32..=40).rev() {
            graph.apply(&epochs, true, 7, dst).expect("link");
        }
        let out = neighbours_of(&epochs, &graph, Direction::Out, 7);
        assert_eq!(out.len(), 40);
        assert!(out.windows(2).all(|w| w[0] < w[1]), "not sorted: {out:?}");
        assert_eq!(graph.degree(Direction::Out, 7), 40);
        assert_eq!(graph.degree(Direction::In, 40), 1, "reverse edge missing");
    }

    #[test]
    fn a_repeated_edge_is_not_stored_twice() {
        let epochs = Epochs::new(4);
        let graph = Graph::new(1024);
        for _ in 0..8 {
            graph.apply(&epochs, true, 3, 4).expect("link");
        }
        assert_eq!(graph.degree(Direction::Out, 3), 1);
        assert_eq!(neighbours_of(&epochs, &graph, Direction::Out, 3), vec![4]);
    }

    #[test]
    fn removing_an_edge_removes_both_directions() {
        let epochs = Epochs::new(4);
        let graph = Graph::new(1024);
        graph.apply(&epochs, true, 1, 2).expect("link");
        graph.apply(&epochs, true, 1, 3).expect("link");
        graph.apply(&epochs, false, 1, 2).expect("unlink");
        assert_eq!(neighbours_of(&epochs, &graph, Direction::Out, 1), vec![3]);
        assert_eq!(graph.degree(Direction::In, 2), 0);
        assert_eq!(graph.degree(Direction::In, 3), 1);
    }

    #[test]
    fn a_node_past_the_table_is_refused_rather_than_dropped() {
        let epochs = Epochs::new(4);
        let graph = Graph::new(16);
        let past = graph.capacity() as u32;
        assert!(graph.apply(&epochs, true, 0, past).is_err());
    }
}
