//! The grouping table behind GROUP BY and DISTINCT (perf/05 section 4).
//!
//! The old sink kept `BTreeMap<Vec<OrdValue>, _>`, which charges a key
//! Vec allocation and a chain of boxed-enum comparisons for every input
//! row. This is the replacement: open addressing with linear probing,
//! keys packed into one flat word buffer with string bytes appended to a
//! page heap, and aggregate states inline in a second flat buffer.
//!
//! The unit of work is a vector, not a row. A caller fills a [`KeyBatch`]
//! one key column at a time, so the per-column dispatch happens once per
//! vector instead of once per row, then hands the whole batch to
//! [`GroupTable::probe`], which hashes the vector, probes it, inserts the
//! misses, and writes back one group index per row. The caller then
//! updates the accumulators by group index, again one column at a time.
//! A row that hits an existing group costs one hash, one slot load, one
//! key compare, and the accumulator update, and allocates nothing.
//!
//! A slot is two words: a 16-bit hash tag beside the group index, and
//! the key's first word, or its hash when the key holds a string. Both
//! sit on one cache line, so a probe that lands on the wrong group
//! rejects without touching the key pages at all, and a single-column
//! fixed-width key, which is most group-bys, never reads them even on a
//! hit: the inline word is the whole key.
//!
//! A string of at most [`INLINE_MAX`] bytes is carried in the two words
//! its part occupies rather than in the heap, which is what a name or a
//! country or a status is, and while nothing in the table has spilled
//! past that the compare is a run down two word buffers: no walk over
//! the parts, no second buffer to reach into, no `memcmp` call for seven
//! bytes. That is worth about a third of the whole query on a ten
//! million row scan grouped on a name.
//!
//! Probing is the memory-bound part, one dependent random access per
//! row, so the probe loop warms the slot line eight rows ahead. That is
//! what makes the vector unit of work pay: eight probes are in flight at
//! once instead of one, and the same trick works on the accumulator
//! updates, which the caller runs as its own loop over group indices.
//!
//! Ordering is not the probe's job, but it is the table's. Groups are
//! built in insertion order and put out by key ascending, which is the
//! old engine's output, and [`GroupTable::rows`] is where that happens.
//! It is here rather than in the sink because the sink only had the
//! decoded `Value`s to sort, and sorting those meant building every
//! group's key and states before knowing where any of them went. The
//! table has the packed words, so it orders an index vector against
//! them and decodes each group once, straight into its finished row.

use std::cmp::Ordering;
use std::sync::Mutex;

use zu_query::exec::Value;
use zu_query::snapshot::TemporalLane;
use zu_vector::kernels::hash64;

use crate::compile::AggSpec;
use crate::sink::Acc;

/// What one key column holds, which decides how many words it occupies
/// and how it compares.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartKind {
    /// An integer or a dense row id: one word.
    Int,
    /// A date, a time, a datetime or a duration: one word, the same
    /// word an integer takes and compared the same way, with the lane
    /// carried so the group can be handed back as the value it was
    /// rather than as the count underneath it.
    Temporal(TemporalLane),
    /// A node: table id and row offset, two words.
    Node,
    /// A string: two words, holding the bytes themselves when there are
    /// at most [`INLINE_MAX`] of them and an offset and a length into
    /// the batch's or the table's byte buffer when there are more.
    Str,
}

/// Bytes a string key part carries in its own two words. Sixteen bytes
/// are there and the length needs one of them, so fifteen fit, which
/// covers a name, a country code, a status, a short label, most of what
/// a GROUP BY on a string is actually grouping on.
///
/// The point is not the copy it saves on the way in, which is small. It
/// is that an inline key hashes out of two registers and compares as two
/// word compares, so a row never reaches the byte buffer at all: no
/// second random line on the compare, and no `memcmp` call, which for
/// seven bytes costs more than the compare it is doing.
const INLINE_MAX: usize = 15;

/// The length byte of the second word when the bytes did not fit, which
/// is the one value a real inline length cannot take.
const HEAP_TAG: u64 = 0xFF << 56;

/// The other 56 bits of the second word, the length of a heap key.
const LEN_MASK: u64 = !HEAP_TAG;

/// Whether a string part's second word says its bytes are in the words.
#[inline(always)]
fn inline_str(w1: u64) -> bool {
    w1 & HEAP_TAG != HEAP_TAG
}

/// A string part's length, whichever of the two forms it is in.
#[inline(always)]
fn str_len(w1: u64) -> usize {
    (if inline_str(w1) {
        w1 >> 56
    } else {
        w1 & LEN_MASK
    }) as usize
}

/// The word holding `s`, which is at most eight bytes, little endian and
/// zero above them.
///
/// Written as fixed-width loads that overlap rather than as a copy into
/// a zeroed buffer, because a copy of a length the compiler cannot see
/// is a `memcpy` call, and a call per row is more than the whole rest of
/// the packing costs. Two loads cover four to eight bytes, three byte
/// loads cover one to three, and both read only bytes `s` has.
#[inline(always)]
fn load_short(s: &[u8]) -> u64 {
    let n = s.len();
    debug_assert!(n <= 8, "one word holds eight bytes");
    if n >= 4 {
        let head = u32::from_le_bytes(s[..4].try_into().expect("four bytes")) as u64;
        let tail = u32::from_le_bytes(s[n - 4..].try_into().expect("four bytes")) as u64;
        // Bytes four and up are the top of the overlapping tail load,
        // and at exactly four there are none of them.
        let above = if n > 4 { tail >> ((8 - n) * 8) } else { 0 };
        head | above << 32
    } else if n > 0 {
        // The first, the last, and the one in the middle, which for one
        // and two bytes is one of those two again.
        u64::from(s[0]) | u64::from(s[n / 2]) << (n / 2 * 8) | u64::from(s[n - 1]) << ((n - 1) * 8)
    } else {
        0
    }
}

/// Packs up to [`INLINE_MAX`] bytes into the two words of a string part.
#[inline(always)]
fn pack_inline(s: &[u8]) -> (u64, u64) {
    let n = s.len();
    debug_assert!(n <= INLINE_MAX, "the caller checked the length");
    let (lo, hi) = if n >= 8 {
        let lo = u64::from_le_bytes(s[..8].try_into().expect("eight bytes"));
        let tail = u64::from_le_bytes(s[n - 8..].try_into().expect("eight bytes"));
        (lo, if n > 8 { tail >> ((16 - n) * 8) } else { 0 })
    } else {
        (load_short(s), 0)
    };
    // The top byte of the second word is the length, and it is free
    // because seven bytes above the first eight are all an inline key
    // can hold.
    (lo, hi | (n as u64) << 56)
}

/// Unpacks an inline string part into a caller's scratch, which is
/// where a key that has to be read as bytes again is read from.
#[inline(always)]
fn unpack_inline(w0: u64, w1: u64, out: &mut [u8; INLINE_MAX + 1]) -> usize {
    out[..8].copy_from_slice(&w0.to_le_bytes());
    out[8..].copy_from_slice(&(w1 & LEN_MASK).to_le_bytes());
    (w1 >> 56) as usize
}

/// The bytes of a string part, out of the words when it is inline and
/// out of `buf` when it is not.
#[inline(always)]
fn str_bytes<'a>(w0: u64, w1: u64, buf: &'a [u8], scratch: &'a mut [u8; 16]) -> &'a [u8] {
    if inline_str(w1) {
        let n = unpack_inline(w0, w1, scratch);
        &scratch[..n]
    } else {
        let (off, len) = (w0 as usize, str_len(w1));
        &buf[off..off + len]
    }
}

/// The hash of one string part. An inline key is two words and never
/// touches `buf`.
#[inline(always)]
fn hash_str(w0: u64, w1: u64, buf: &[u8]) -> u64 {
    if inline_str(w1) {
        hash64(hash64(w0) ^ w1)
    } else {
        hash_bytes(&buf[w0 as usize..w0 as usize + str_len(w1)])
    }
}

impl PartKind {
    pub(crate) fn words(self) -> usize {
        match self {
            PartKind::Int | PartKind::Temporal(_) => 1,
            PartKind::Node | PartKind::Str => 2,
        }
    }
}

/// One vector of grouping keys, packed row major: `stride` words per
/// row, with string bytes appended to a side buffer and referenced from
/// the words as offset and length. Reused across vectors, so filling one
/// allocates nothing in steady state.
#[derive(Default)]
pub(crate) struct KeyBatch {
    words: Vec<u64>,
    bytes: Vec<u8>,
    stride: usize,
    rows: usize,
    /// Whether any string in the batch was too long for its words and
    /// went to `bytes`. See [`GroupTable::spilled`].
    spilled: bool,
}

impl KeyBatch {
    /// Sizes the batch for `rows` keys of `stride` words and zeroes it.
    pub(crate) fn reset(&mut self, stride: usize, rows: usize) {
        self.words.clear();
        self.words.resize(stride * rows, 0);
        self.bytes.clear();
        self.stride = stride;
        self.rows = rows;
        self.spilled = false;
    }

    /// The word buffer and its stride, for a caller writing one column
    /// with `iter_mut().skip(off).step_by(stride)`.
    pub(crate) fn words_mut(&mut self) -> (&mut [u64], usize) {
        (&mut self.words, self.stride)
    }

    /// Writes the same word into column `off` of every row, the shape a
    /// key on a pinned level takes.
    pub(crate) fn fill_word(&mut self, off: usize, v: u64) {
        for w in self.words.iter_mut().skip(off).step_by(self.stride) {
            *w = v;
        }
    }

    /// Writes one string cell. Strings are the one part a caller has to
    /// fill row by row, since the long ones only pack in order.
    pub(crate) fn set_str(&mut self, row: usize, off: usize, s: &[u8]) {
        let at = row * self.stride + off;
        let (w0, w1) = if s.len() <= INLINE_MAX {
            pack_inline(s)
        } else {
            let head = (self.bytes.len() as u64, HEAP_TAG | s.len() as u64);
            self.bytes.extend_from_slice(s);
            self.spilled = true;
            head
        };
        self.words[at] = w0;
        self.words[at + 1] = w1;
    }

    /// Drops the string bytes of the batch, keeping the word buffer.
    pub(crate) fn clear_bytes(&mut self) {
        self.bytes.clear();
    }

    fn row_words(&self, row: usize) -> &[u64] {
        &self.words[row * self.stride..(row + 1) * self.stride]
    }
}

/// An open-addressing group table: one entry per distinct key, holding
/// that key's aggregate states.
pub(crate) struct GroupTable {
    parts: Vec<PartKind>,
    /// Words one key occupies, the sum over the parts.
    stride: usize,
    /// Whether any part is a string, which is what decides between the
    /// fixed-width fast path and the part by part one.
    varlen: bool,
    keys: Vec<u64>,
    /// String bytes for every Str part of every stored key.
    heap: Vec<u8>,
    accs: Vec<Acc>,
    n_aggs: usize,
    /// Whether a key is one fixed-width word, in which case the word
    /// stored in the slot decides equality on its own.
    simple: bool,
    /// Probe slots. `[0]` is 0 when empty, else `tag:16 | index+1:48`;
    /// `[1]` is the key's first word, or its hash for a varlen key.
    ///
    /// Counting mode reads them differently: `[0]` is the key and `[1]`
    /// is that key's row count, so a slot is the whole group and an
    /// empty slot is a zero count.
    slots: Vec<[u64; 2]>,
    mask: usize,
    groups: usize,
    /// Counting mode: the slot each group sits in, in the order the
    /// groups were first seen, since the slot no longer carries an
    /// index of its own.
    order: Vec<u32>,
    /// Whether the slot holds the count instead of an index into the
    /// key and state buffers.
    counting: bool,
    /// One hash per row of the batch in flight.
    hashes: Vec<u64>,
    /// Whether any stored key has a string too long to live in its own
    /// words.
    ///
    /// While nothing has spilled, two keys are equal exactly when their
    /// words are, because a string's length decides its form and its
    /// bytes are the words. That turns the compare into a run down two
    /// word buffers, with no walk over the parts and no reach into a
    /// byte buffer, which is the whole reason for packing short strings
    /// inline in the first place.
    spilled: bool,
    /// Whether every string key stored so far was UTF-8.
    ///
    /// The check lives here rather than on the way in because a row that
    /// lands on a group it did not create has bytes equal to that
    /// group's, which were checked when it was created. So a scan of ten
    /// million rows over a thousand names validates a thousand names,
    /// not ten million. [`GroupTable::utf8`] is where it is answered.
    utf8: bool,
}

/// Slots start here and double past three quarters full. Seven eighths
/// halves the table and keeps four slots to a cache line, which sounds
/// like the better trade, and it measured 25 percent slower on the
/// hundred thousand group bench: the probe chains lengthen faster than
/// the smaller footprint pays back. Sixty-four costs a kilobyte and
/// covers the many-groups-of-one queries that never grow past a handful.
const INIT_SLOTS: usize = 64;

/// Groups a hand needs before ordering them and building their rows is
/// worth more than one thread. Four thousand of them is a few hundred
/// microseconds of sorting and decoding, which is well clear of what a
/// latch and a lock per hand cost, and it keeps every query that groups
/// into tens or hundreds on the one thread it was already answered on.
const SPLIT_ROWS: usize = 4096;

const IDX_MASK: u64 = (1 << 48) - 1;

const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// How far ahead the probe loop warms slot lines. Eight covers the L2
/// latency of the hosts in the fleet at a vector of 2048 rows, where the
/// eight rows the lookahead cannot cover are rounding.
const AHEAD: usize = 8;

/// Warms the cache line holding `v` for a row the loop has not reached.
/// There is no prefetch intrinsic stable on both x86 and aarch64, so
/// this is a plain load the optimizer is not allowed to drop. Nothing
/// depends on the value, so it retires without stalling anything and the
/// line is resident by the time the probe gets there.
#[inline(always)]
fn warm<T>(v: T) {
    std::hint::black_box(v);
}

impl GroupTable {
    pub(crate) fn new(parts: Vec<PartKind>, n_aggs: usize) -> GroupTable {
        let stride = parts.iter().map(|p| p.words()).sum();
        GroupTable {
            varlen: parts.contains(&PartKind::Str),
            simple: stride == 1 && !parts.contains(&PartKind::Str),
            parts,
            stride,
            keys: Vec::new(),
            heap: Vec::new(),
            accs: Vec::new(),
            n_aggs,
            slots: vec![[0; 2]; INIT_SLOTS],
            mask: INIT_SLOTS - 1,
            groups: 0,
            order: Vec::new(),
            counting: false,
            hashes: Vec::new(),
            spilled: false,
            utf8: true,
        }
    }

    /// A table for one fixed-width key with nothing but counters over
    /// it, which is what most of the GROUP BY queries in the wild are.
    ///
    /// The general table charges a row two random lines, the slot it
    /// probes and the group's states, and at a hundred thousand groups
    /// neither is in cache. Here the count lives in the slot beside the
    /// key, so a row touches one line and the states buffer is never
    /// built. A count of zero is what an empty slot means, and a group
    /// is created with one, so nothing else marks a slot as taken.
    pub(crate) fn counting(parts: Vec<PartKind>, n_aggs: usize) -> GroupTable {
        let mut t = GroupTable::new(parts, n_aggs);
        debug_assert!(t.simple, "counting mode holds the key in the slot");
        t.counting = true;
        t
    }

    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    /// How many distinct keys the table holds.
    pub(crate) fn groups(&self) -> usize {
        self.groups
    }

    /// Whether every string key in the table is UTF-8, which the caller
    /// asks once at the end rather than per row. See [`Self::utf8`].
    pub(crate) fn utf8(&self) -> bool {
        self.utf8
    }

    /// Every group's aggregate states, `n_aggs` per group, indexed by the
    /// group indices [`probe`] wrote.
    ///
    /// [`probe`]: GroupTable::probe
    pub(crate) fn accs_mut(&mut self) -> &mut [Acc] {
        &mut self.accs
    }

    /// Hashes and probes a whole key vector, creating a group for every
    /// key not seen before and writing one group index per row into
    /// `out`. Indices are handed out in order, so the first row that
    /// creates a group gets the index equal to the group count before
    /// the call, which is how DISTINCT reads first sight back out.
    pub(crate) fn probe(&mut self, batch: &KeyBatch, specs: &[AggSpec], out: &mut Vec<u32>) {
        debug_assert_eq!(batch.stride, self.stride, "key shape matches the table");
        let mut hashes = std::mem::take(&mut self.hashes);
        hashes.clear();
        if self.varlen {
            let bytes = &batch.bytes;
            hashes.extend((0..batch.rows).map(|row| self.hash_varlen(batch.row_words(row), bytes)));
        } else {
            // Fixed-width keys are the whole hash: no bytes to reach,
            // one round per word, and stride is one for the common
            // single column key.
            hashes.extend(batch.words.chunks_exact(self.stride.max(1)).map(|words| {
                let mut h = SEED;
                for &w in words {
                    h = hash64(h ^ w);
                }
                h
            }));
        }
        out.clear();
        out.resize(batch.rows, 0);
        // Asked once for the vector rather than once per row: while
        // neither side has spilled a string, the words are the key.
        let words_decide = !self.spilled && !batch.spilled;
        for row in 0..batch.rows {
            if row + AHEAD < batch.rows {
                let i = hashes[row + AHEAD] as usize & self.mask;
                warm(self.slots[i]);
            }
            out[row] = self.find_or_insert(hashes[row], batch, row, specs, words_decide) as u32;
        }
        self.hashes = hashes;
    }

    /// One column of integer keys straight into the slots, the whole
    /// per-row path of a counting GROUP BY in one loop.
    ///
    /// The general path writes the hash of every row down, then the
    /// group index of every row, then walks the indices again to
    /// accumulate into a states buffer somewhere else. All of that is
    /// memory the caller never reads, and the last of it is a second
    /// random line per row. Here a row is a hash, one slot, and an
    /// increment inside it. The lookahead stays, since the slot is
    /// still the line the loop waits on.
    pub(crate) fn count_ints(&mut self, words: &[u64]) {
        debug_assert!(self.counting, "counting mode holds the count in the slot");
        for (row, &w) in words.iter().enumerate() {
            if let Some(&next) = words.get(row + AHEAD) {
                let i = hash64(SEED ^ next) as usize & self.mask;
                warm(self.slots[i]);
            }
            self.bump(w, 1);
        }
    }

    /// The same where each key stands for `w[row]` rows, which is what
    /// a fused degree product hands over: the neighbors a source row
    /// would have produced all share its key, so they arrive as one
    /// increment instead of one row each.
    pub(crate) fn count_ints_weighted(&mut self, words: &[u64], w: &[i64]) {
        debug_assert!(self.counting, "counting mode holds the count in the slot");
        debug_assert_eq!(words.len(), w.len(), "one weight per key");
        for (row, (&word, &n)) in words.iter().zip(w).enumerate() {
            if let Some(&next) = words.get(row + AHEAD) {
                let i = hash64(SEED ^ next) as usize & self.mask;
                warm(self.slots[i]);
            }
            self.bump(word, n as u64);
        }
    }

    /// Adds `n` rows to key `w`'s count, creating its group on first
    /// sight. The key sits in the slot whole, so a slot that is taken
    /// and does not match is the only case that walks on.
    fn bump(&mut self, w: u64, n: u64) {
        let mut i = hash64(SEED ^ w) as usize & self.mask;
        loop {
            let slot = &mut self.slots[i];
            if slot[1] == 0 {
                *slot = [w, n];
                self.order.push(i as u32);
                self.groups += 1;
                if self.groups * 4 > self.slots.len() * 3 {
                    self.grow();
                }
                return;
            }
            if slot[0] == w {
                slot[1] += n;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn find_or_insert(
        &mut self,
        h: u64,
        batch: &KeyBatch,
        row: usize,
        specs: &[AggSpec],
        words_decide: bool,
    ) -> usize {
        let tag = (h >> 48).max(1);
        let inline = self.inline_word(h, batch.row_words(row));
        let mut i = h as usize & self.mask;
        loop {
            let slot = self.slots[i];
            if slot[0] == 0 {
                let g = self.groups;
                self.store(batch, row);
                self.accs.extend(specs.iter().map(Acc::new));
                self.slots[i] = [(tag << 48) | (g as u64 + 1), inline];
                self.groups += 1;
                if self.groups * 4 > self.slots.len() * 3 {
                    self.grow();
                }
                return g;
            }
            if slot[0] >> 48 == tag && slot[1] == inline {
                let g = ((slot[0] & IDX_MASK) - 1) as usize;
                let same = if self.simple {
                    true
                } else if words_decide {
                    self.words_eq(g, batch, row)
                } else {
                    self.key_eq(g, batch, row)
                };
                if same {
                    return g;
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// What a slot carries beside the index: the key's first word, which
    /// for a one word key is the key itself, or the hash when a string
    /// part means the first word is only an offset.
    fn inline_word(&self, h: u64, words: &[u64]) -> u64 {
        if self.varlen { h } else { words[0] }
    }

    /// Appends the batch row's key words, copying any string bytes out of
    /// the batch into the heap so the stored offsets point at bytes the
    /// table owns.
    fn store(&mut self, batch: &KeyBatch, row: usize) {
        let words = batch.row_words(row);
        if !self.varlen {
            self.keys.extend_from_slice(words);
            return;
        }
        let mut w = 0;
        for &part in &self.parts {
            match part {
                PartKind::Int | PartKind::Temporal(_) => self.keys.push(words[w]),
                PartKind::Node => {
                    self.keys.push(words[w]);
                    self.keys.push(words[w + 1]);
                }
                PartKind::Str => {
                    let (w0, w1) = (words[w], words[w + 1]);
                    let mut scratch = [0u8; INLINE_MAX + 1];
                    let bytes = str_bytes(w0, w1, &batch.bytes, &mut scratch);
                    self.utf8 &= std::str::from_utf8(bytes).is_ok();
                    if inline_str(w1) {
                        // The bytes are the key, so the key is the copy.
                        self.keys.push(w0);
                    } else {
                        let (off, len) = (w0 as usize, str_len(w1));
                        self.keys.push(self.heap.len() as u64);
                        self.heap.extend_from_slice(&batch.bytes[off..off + len]);
                        self.spilled = true;
                    }
                    self.keys.push(w1);
                }
            }
            w += part.words();
        }
    }

    /// Stored group `g` against a batch row, word for word, which is the
    /// whole comparison while [`Self::spilled`] is false.
    ///
    /// Written as a loop rather than as a slice compare because the two
    /// slices are `u64` and a slice compare on those is a `memcmp` call,
    /// and a call is more than the two or three compares it makes.
    fn words_eq(&self, g: usize, batch: &KeyBatch, row: usize) -> bool {
        let base = g * self.stride;
        let stored = &self.keys[base..base + self.stride];
        stored.iter().zip(batch.row_words(row)).all(|(a, b)| a == b)
    }

    fn key_eq(&self, g: usize, batch: &KeyBatch, row: usize) -> bool {
        let base = g * self.stride;
        let stored = &self.keys[base..base + self.stride];
        let words = batch.row_words(row);
        if !self.varlen {
            return stored == words;
        }
        let mut w = 0;
        for &part in &self.parts {
            match part {
                PartKind::Int | PartKind::Temporal(_) | PartKind::Node => {
                    if stored[w..w + part.words()] != words[w..w + part.words()] {
                        return false;
                    }
                }
                PartKind::Str => {
                    // The second word is the length and the form and,
                    // for an inline key, the bytes above the eighth, so
                    // two keys that differ in any of those are already
                    // apart here.
                    if stored[w + 1] != words[w + 1] {
                        return false;
                    }
                    if inline_str(words[w + 1]) {
                        if stored[w] != words[w] {
                            return false;
                        }
                    } else {
                        let len = str_len(words[w + 1]);
                        let a = stored[w] as usize;
                        let b = words[w] as usize;
                        if self.heap[a..a + len] != batch.bytes[b..b + len] {
                            return false;
                        }
                    }
                }
            }
            w += part.words();
        }
        true
    }

    /// Hash of a key holding at least one string, which has to walk the
    /// parts to know which words are offsets into `bytes`.
    fn hash_varlen(&self, words: &[u64], bytes: &[u8]) -> u64 {
        let mut h = SEED;
        let mut w = 0;
        for &part in &self.parts {
            h = match part {
                PartKind::Int | PartKind::Temporal(_) => hash64(h ^ words[w]),
                PartKind::Node => hash64(hash64(h ^ words[w]) ^ words[w + 1]),
                PartKind::Str => hash64(h ^ hash_str(words[w], words[w + 1], bytes)),
            };
            w += part.words();
        }
        h
    }

    /// The hash of a stored key, needed when the slots are rebuilt.
    fn hash_stored(&self, g: usize) -> u64 {
        let base = g * self.stride;
        let words = &self.keys[base..base + self.stride];
        if !self.varlen {
            let mut h = SEED;
            for &w in words {
                h = hash64(h ^ w);
            }
            return h;
        }
        self.hash_varlen_stored(words)
    }

    fn hash_varlen_stored(&self, words: &[u64]) -> u64 {
        let mut h = SEED;
        let mut w = 0;
        for &part in &self.parts {
            h = match part {
                PartKind::Int | PartKind::Temporal(_) => hash64(h ^ words[w]),
                PartKind::Node => hash64(hash64(h ^ words[w]) ^ words[w + 1]),
                PartKind::Str => hash64(h ^ hash_str(words[w], words[w + 1], &self.heap)),
            };
            w += part.words();
        }
        h
    }

    fn grow(&mut self) {
        let n = self.slots.len() * 2;
        if self.counting {
            // The slots are the groups here, so they move rather than
            // being rebuilt from a key buffer, and the order list is
            // rewritten with where each one landed.
            let old = std::mem::replace(&mut self.slots, vec![[0; 2]; n]);
            self.mask = n - 1;
            for at in &mut self.order {
                let slot = old[*at as usize];
                let mut i = hash64(SEED ^ slot[0]) as usize & self.mask;
                while self.slots[i][1] != 0 {
                    i = (i + 1) & self.mask;
                }
                self.slots[i] = slot;
                *at = i as u32;
            }
            return;
        }
        self.slots.clear();
        self.slots.resize(n, [0; 2]);
        self.mask = n - 1;
        for g in 0..self.groups {
            let h = self.hash_stored(g);
            let tag = (h >> 48).max(1);
            let inline = self.inline_word(h, &self.keys[g * self.stride..]);
            let mut i = h as usize & self.mask;
            while self.slots[i][0] != 0 {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = [(tag << 48) | (g as u64 + 1), inline];
        }
    }

    /// Folds another worker's partial in. Groups both sides hold merge
    /// their accumulators; groups only the other side has move over.
    pub(crate) fn merge_from(&mut self, other: &GroupTable) -> zu_common::Result<()> {
        debug_assert_eq!(self.parts.len(), other.parts.len());
        debug_assert_eq!(self.n_aggs, other.n_aggs);
        debug_assert_eq!(self.counting, other.counting);
        if self.counting {
            for &at in &other.order {
                let slot = other.slots[at as usize];
                self.bump(slot[0], slot[1]);
            }
            return Ok(());
        }
        let mut batch = KeyBatch::default();
        batch.reset(self.stride, 1);
        for g in 0..other.groups {
            other.read_into(g, &mut batch);
            // The hash reads key content, never the offsets, so the
            // other table's stored hash is this table's probe hash.
            // Part by part rather than word by word: a merge is one call
            // per group, not per row, so the general compare costs
            // nothing worth saving here.
            let target = self.find_or_insert(other.hash_stored(g), &batch, 0, &[], false);
            let src = &other.accs[g * other.n_aggs..(g + 1) * other.n_aggs];
            if self.accs.len() < (target + 1) * self.n_aggs {
                self.accs.extend_from_slice(src);
                continue;
            }
            let dst = &mut self.accs[target * self.n_aggs..(target + 1) * self.n_aggs];
            for (a, b) in dst.iter_mut().zip(src) {
                a.merge(b)?;
            }
        }
        Ok(())
    }

    /// Reads stored group `g` back into a one row batch, the form the
    /// probe path takes, so a merge can look a foreign key up here.
    fn read_into(&self, g: usize, batch: &mut KeyBatch) {
        let base = g * self.stride;
        let (words, _) = batch.words_mut();
        words.copy_from_slice(&self.keys[base..base + self.stride]);
        if !self.varlen {
            return;
        }
        // Only a string key needs the byte buffer emptied between
        // groups; the words above are overwritten either way.
        batch.clear_bytes();
        let mut w = 0;
        let mut scratch = [0u8; INLINE_MAX + 1];
        for &part in &self.parts {
            if part == PartKind::Str {
                let (w0, w1) = (self.keys[base + w], self.keys[base + w + 1]);
                // Through the bytes rather than by copying the words,
                // because an offset into this table's heap means nothing
                // in the batch the caller is about to probe with. An
                // inline key packs back to the words it came from.
                batch.set_str(0, w, str_bytes(w0, w1, &self.heap, &mut scratch));
            }
            w += part.words();
        }
    }

    /// Every group as its key values and its states, insertion ordered.
    #[cfg(test)]
    pub(crate) fn drain(mut self) -> Vec<(Vec<Value>, Vec<Acc>)> {
        let mut out = Vec::with_capacity(self.groups);
        if self.counting {
            // Every counter over the same group counted the same rows,
            // so the one count in the slot answers all of them.
            for &at in &self.order {
                let slot = self.slots[at as usize];
                out.push((
                    vec![self.counted_key(at)],
                    vec![Acc::Count(slot[1] as i64); self.n_aggs],
                ));
            }
            return out;
        }
        let mut accs = std::mem::take(&mut self.accs).into_iter();
        let mut vals = Vec::new();
        for g in 0..self.groups {
            self.key_into(g, &mut vals);
            out.push((
                std::mem::take(&mut vals),
                accs.by_ref().take(self.n_aggs).collect(),
            ));
        }
        out
    }

    /// Every group as one answer row, key columns ascending, with the
    /// keys and the finalized aggregates put back in the order the
    /// RETURN clause named them. `item_agg` is that order: true where
    /// the item is an aggregate and false where it is a key.
    ///
    /// One pass rather than a drain and a sort over what the drain
    /// built, because the drain built two vectors per group and the
    /// sort then compared groups through the `Value`s they decoded to.
    /// A hundred thousand groups was three hundred thousand small
    /// allocations and a sort chasing a pointer per compare, all of it
    /// thrown away a moment later. The order is settled over an index
    /// vector against the stored words, so nothing is decoded until the
    /// row is built and each row is built once, in its final place.
    pub(crate) fn rows(mut self, item_agg: &[bool], hands: usize) -> Vec<Vec<Value>> {
        let mut order: Vec<u32> = if self.counting {
            std::mem::take(&mut self.order)
        } else {
            (0..self.groups as u32).collect()
        };
        // Ordering and decoding are both per group and the groups are
        // settled by now, so the pool the scan has just finished with can
        // take both. `hands` is the run's worker count and not the
        // machine's, because a query asked to run on one worker is
        // answered on one worker, tail included. Under the split neither
        // is worth a latch and a lock per hand.
        let hands = hands.min(order.len() / SPLIT_ROWS);
        if hands < 2 {
            self.sort_groups(&mut order);
            return self.build(&order, item_agg);
        }
        let total = order.len();
        let mut buckets = self.split_groups(order, hands);
        {
            let me = &self;
            let (head, rest) = buckets.split_at_mut(1);
            let slots: Vec<Mutex<Option<Vec<Vec<Value>>>>> =
                rest.iter().map(|_| Mutex::new(None)).collect();
            let mine = {
                let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = rest
                    .iter_mut()
                    .zip(&slots)
                    .map(|(bucket, slot)| {
                        Box::new(move || {
                            me.sort_groups(bucket);
                            *slot.lock().unwrap() = Some(me.build(bucket, item_agg));
                        }) as Box<dyn FnOnce() + Send + '_>
                    })
                    .collect();
                let pending = crate::pool::submit(jobs);
                me.sort_groups(&mut head[0]);
                let mine = me.build(&head[0], item_agg);
                pending.wait();
                mine
            };
            let mut out = mine;
            out.reserve(total - out.len());
            for (slot, bucket) in slots.into_iter().zip(rest.iter_mut()) {
                // A slot still empty after the latch means that hand
                // panicked, and its rows are gone. Its bucket is still
                // here and still holds the same groups, sorted or not,
                // so doing the pair over keeps the answer whole and
                // costs the panic handler's path only.
                match slot.into_inner().unwrap() {
                    Some(rows) => out.extend(rows),
                    None => {
                        me.sort_groups(bucket);
                        out.extend(me.build(bucket, item_agg));
                    }
                }
            }
            out
        }
    }

    /// Puts a set of groups in key ascending order.
    ///
    /// Reads the key kind once and sorts under that rather than asking
    /// [`GroupTable::cmp_groups`] per pair, because this is the one place
    /// that compares a group tens of times over.
    fn sort_groups(&self, groups: &mut [u32]) {
        if self.counting {
            let part = self.parts[0];
            groups.sort_unstable_by(|&a, &b| {
                cmp_word(part, self.slots[a as usize][0], self.slots[b as usize][0])
            });
        } else {
            groups.sort_unstable_by(|&a, &b| self.cmp_keys(a as usize, b as usize));
        }
    }

    /// Splits the groups into `hands` ascending key ranges, so that
    /// sorting each range and laying them end to end is the sort.
    ///
    /// Sorting the whole vector first and then cutting it into equal
    /// slices would be the same ranges, but the sort would be the one
    /// serial thing left in a query that is otherwise all on the pool,
    /// and on a hundred thousand groups it is a couple of milliseconds
    /// of chasing a random word per compare. Splitting first costs one
    /// pass with a binary search over a handful of pivots per group,
    /// which is a few compares against a pivot list small enough to stay
    /// in cache, and leaves every compare after it on a hand.
    ///
    /// The pivots come from every `step`th group, taken before anything
    /// is ordered. Groups sit here in the order they were first seen,
    /// which is the order their keys turned up in the scan, so a stride
    /// through them is a fair sample of the keys whether the column
    /// arrived shuffled or already sorted. Uneven ranges cost a hand
    /// some idle time and nothing else, so eight samples a hand is
    /// enough to keep that small without the sample itself mattering.
    fn split_groups(&self, groups: Vec<u32>, hands: usize) -> Vec<Vec<u32>> {
        const PER_HAND: usize = 8;
        debug_assert!(groups.len() >= hands, "a range apiece at the least");
        let step = (groups.len() / (hands * PER_HAND)).max(1);
        let mut sample: Vec<u32> = groups.iter().copied().step_by(step).collect();
        self.sort_groups(&mut sample);
        let pivots: Vec<u32> = (1..hands)
            .map(|i| sample[i * sample.len() / hands])
            .collect();
        let room = groups.len() / hands;
        let mut buckets = vec![Vec::with_capacity(room + room / 4); hands];
        for g in groups {
            // How many pivots this group is at or past, which is its
            // range. The pivots ascend, so the answer is a partition
            // point rather than a walk.
            let at = pivots.partition_point(|&p| self.cmp_groups(p, g) != Ordering::Greater);
            buckets[at].push(g);
        }
        buckets
    }

    /// Two groups of this table, compared by the keys they hold.
    fn cmp_groups(&self, a: u32, b: u32) -> Ordering {
        if self.counting {
            cmp_word(
                self.parts[0],
                self.slots[a as usize][0],
                self.slots[b as usize][0],
            )
        } else {
            self.cmp_keys(a as usize, b as usize)
        }
    }

    /// The rows for one slice of the settled order.
    ///
    /// Takes the table by reference and reads each state out of it
    /// rather than moving the states away first, so that slices of the
    /// order can be built beside each other. An accumulator is a word or
    /// two and is `Copy`, so reading one is cheaper than the bookkeeping
    /// it would take to hand each thread the states it needs out of a
    /// buffer indexed by group rather than by place in the order.
    fn build(&self, order: &[u32], item_agg: &[bool]) -> Vec<Vec<Value>> {
        let mut out = Vec::with_capacity(order.len());
        let mut keys = Vec::new();
        for &g in order {
            if self.counting {
                keys.push(self.counted_key(g));
            } else {
                self.key_into(g as usize, &mut keys);
            }
            let (mut key_at, mut agg_at) = (0, g as usize * self.n_aggs);
            let mut row = Vec::with_capacity(item_agg.len());
            for &is_agg in item_agg {
                row.push(if is_agg {
                    let v = if self.counting {
                        Value::Int(self.slots[g as usize][1] as i64)
                    } else {
                        self.accs[agg_at].finalize()
                    };
                    agg_at += 1;
                    v
                } else {
                    key_at += 1;
                    std::mem::replace(&mut keys[key_at - 1], Value::Null)
                });
            }
            keys.clear();
            out.push(row);
        }
        out
    }

    /// The key of a group counting mode holds in slot `at`, which is
    /// the whole group: the slot carries the key rather than an index
    /// into the key buffer.
    fn counted_key(&self, at: u32) -> Value {
        let word = self.slots[at as usize][0] as i64;
        match self.parts[0] {
            PartKind::Int => Value::Int(word),
            PartKind::Temporal(lane) => Value::Temporal(lane.value(word)),
            PartKind::Node | PartKind::Str => {
                unreachable!("counting mode is one fixed-width word")
            }
        }
    }

    /// The stored key of group `g`, part by part, appended to `out`.
    fn key_into(&self, g: usize, out: &mut Vec<Value>) {
        let base = g * self.stride;
        let mut scratch = [0u8; INLINE_MAX + 1];
        let mut w = 0;
        for &part in &self.parts {
            out.push(match part {
                PartKind::Int => Value::Int(self.keys[base + w] as i64),
                PartKind::Temporal(lane) => Value::Temporal(lane.value(self.keys[base + w] as i64)),
                PartKind::Node => Value::Node {
                    table: self.keys[base + w] as u32,
                    offset: self.keys[base + w + 1],
                },
                PartKind::Str => {
                    let (w0, w1) = (self.keys[base + w], self.keys[base + w + 1]);
                    // The caller checked these bytes for UTF-8 on the
                    // way in, so the lossy read never replaces anything.
                    let bytes = str_bytes(w0, w1, &self.heap, &mut scratch);
                    Value::Str(String::from_utf8_lossy(bytes).into_owned())
                }
            });
            w += part.words();
        }
    }

    /// Group order: the stored keys compared left to right, which is
    /// the order the sink used to reach by decoding both groups and
    /// comparing the `Value`s. Reading the words gives the same answer
    /// for every kind a key part can be. An integer orders by its word.
    /// A temporal lane orders by its word too, because every group in
    /// one table shares the lane, so the kind that ranks ahead of the
    /// number in the general compare is the same on both sides. A node
    /// orders by table and then offset, which is how the two words sit.
    /// A string orders by its bytes, which is what comparing the two
    /// `String`s came to.
    fn cmp_keys(&self, a: usize, b: usize) -> Ordering {
        let (mut i, mut j) = (a * self.stride, b * self.stride);
        let mut sa = [0u8; INLINE_MAX + 1];
        let mut sb = [0u8; INLINE_MAX + 1];
        for &part in &self.parts {
            let ord = match part {
                PartKind::Int | PartKind::Temporal(_) => cmp_word(part, self.keys[i], self.keys[j]),
                PartKind::Node => (self.keys[i] as u32, self.keys[i + 1])
                    .cmp(&(self.keys[j] as u32, self.keys[j + 1])),
                PartKind::Str => str_bytes(self.keys[i], self.keys[i + 1], &self.heap, &mut sa)
                    .cmp(str_bytes(
                        self.keys[j],
                        self.keys[j + 1],
                        &self.heap,
                        &mut sb,
                    )),
            };
            if ord != Ordering::Equal {
                return ord;
            }
            i += part.words();
            j += part.words();
        }
        Ordering::Equal
    }
}

/// Two words of a one word key part, compared as the values they decode
/// to. A date is the one lane narrower than the word it rides in, so it
/// compares through the same narrowing the decode does rather than over
/// the bits above it.
fn cmp_word(part: PartKind, a: u64, b: u64) -> Ordering {
    if part == PartKind::Temporal(TemporalLane::Date) {
        (a as i32).cmp(&(b as i32))
    } else {
        (a as i64).cmp(&(b as i64))
    }
}

/// Hash of a string key part: eight bytes at a time through the same
/// finalizer the integer keys use, length folded in so that "ab" and
/// "ab\0" cannot land on the same value.
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = b.len() as u64;
    let (octets, rest) = b.as_chunks::<8>();
    for c in octets {
        h = hash64(h ^ u64::from_le_bytes(*c));
    }
    let mut tail = 0u64;
    for (i, &c) in rest.iter().enumerate() {
        tail |= u64::from(c) << (i * 8);
    }
    hash64(h ^ tail)
}

#[cfg(test)]
mod tests {
    use zu_query::exec::OrdValue;

    use super::*;

    /// Feeds ints one per row and counts them, the shape the sink drives.
    fn count_ints(t: &mut GroupTable, vals: &[i64]) {
        let specs = [AggSpec::CountStar];
        let mut batch = KeyBatch::default();
        batch.reset(t.stride(), vals.len());
        let (words, stride) = batch.words_mut();
        for (w, &v) in words.iter_mut().step_by(stride).zip(vals) {
            *w = v as u64;
        }
        let mut gids = Vec::new();
        t.probe(&batch, &specs, &mut gids);
        for &g in &gids {
            t.accs_mut()[g as usize].add_star(1);
        }
    }

    fn count_of(a: &Acc) -> i64 {
        match *a {
            Acc::Count(n) => n,
            _ => panic!("count states"),
        }
    }

    #[test]
    fn counts_group_by_one_int_key() {
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        let vals: Vec<i64> = (0..1000).map(|i| i % 7).collect();
        count_ints(&mut t, &vals);
        let rows = t.drain();
        assert_eq!(rows.len(), 7);
        assert_eq!(rows.iter().map(|(_, a)| count_of(&a[0])).sum::<i64>(), 1000);
    }

    #[test]
    fn survives_more_groups_than_initial_slots() {
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        let vals: Vec<i64> = (0..10_000).collect();
        count_ints(&mut t, &vals);
        let rows = t.drain();
        assert_eq!(rows.len(), 10_000, "every key kept its own group");
        for (vals, accs) in rows {
            assert!(matches!(vals[0], Value::Int(_)));
            assert!(matches!(accs[0], Acc::Count(1)));
        }
    }

    #[test]
    fn string_keys_compare_by_bytes() {
        let mut t = GroupTable::new(vec![PartKind::Str], 1);
        let specs = [AggSpec::CountStar];
        let strs = [
            "a",
            "bb",
            "a",
            "a longer string past the inline limit",
            "bb",
        ];
        let mut batch = KeyBatch::default();
        batch.reset(t.stride(), strs.len());
        for (row, s) in strs.iter().enumerate() {
            batch.set_str(row, 0, s.as_bytes());
        }
        let mut gids = Vec::new();
        t.probe(&batch, &specs, &mut gids);
        for &g in &gids {
            t.accs_mut()[g as usize].add_star(1);
        }
        let rows = t.drain();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0[0], Value::Str("a".into()));
        assert_eq!(count_of(&rows[0].1[0]), 2);
    }

    /// Every length from empty to past the inline limit, each a distinct
    /// key, so a packing that drops or duplicates a byte at some length
    /// shows up as two keys sharing a group or one key splitting.
    #[test]
    fn string_keys_of_every_length_stay_apart() {
        let strs: Vec<String> = (0..40)
            .map(|n| (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect())
            .collect();
        let mut t = GroupTable::new(vec![PartKind::Str], 1);
        let specs = [AggSpec::CountStar];
        let mut batch = KeyBatch::default();
        // Twice over, so the second pass of each key has to find the
        // group the first pass made rather than make one of its own.
        batch.reset(t.stride(), strs.len() * 2);
        for (row, s) in strs.iter().chain(&strs).enumerate() {
            batch.set_str(row, 0, s.as_bytes());
        }
        let mut gids = Vec::new();
        t.probe(&batch, &specs, &mut gids);
        for &g in &gids {
            t.accs_mut()[g as usize].add_star(1);
        }
        let rows = t.drain();
        assert_eq!(rows.len(), strs.len(), "one group per distinct length");
        for (row, s) in rows.iter().zip(&strs) {
            assert_eq!(row.0[0], Value::Str(s.clone()), "the key read back whole");
            assert_eq!(count_of(&row.1[0]), 2, "both passes landed on it");
        }
    }

    /// The packing itself, against the copy it stands in for.
    #[test]
    fn short_strings_pack_into_their_words() {
        for n in 0..=INLINE_MAX {
            let s: Vec<u8> = (0..n as u8)
                .map(|i| i.wrapping_mul(37).wrapping_add(1))
                .collect();
            let (w0, w1) = pack_inline(&s);
            assert!(inline_str(w1), "{n} bytes fit in the words");
            assert_eq!(str_len(w1), n, "the length rode along");
            let mut scratch = [0u8; INLINE_MAX + 1];
            assert_eq!(
                str_bytes(w0, w1, &[], &mut scratch),
                &s[..],
                "{n} bytes back"
            );
        }
    }

    #[test]
    fn mixed_parts_key_on_every_column() {
        let mut t = GroupTable::new(vec![PartKind::Node, PartKind::Str, PartKind::Int], 1);
        let specs = [AggSpec::CountStar];
        let keys: [(u64, u64, &str, i64); 5] = [
            (1, 5, "x", 9),
            (1, 5, "x", 9),
            (1, 5, "x", 8),
            (2, 5, "x", 9),
            (1, 5, "y", 9),
        ];
        let mut batch = KeyBatch::default();
        batch.reset(t.stride(), keys.len());
        for (row, &(table, offset, s, n)) in keys.iter().enumerate() {
            let (words, stride) = batch.words_mut();
            words[row * stride] = table;
            words[row * stride + 1] = offset;
            words[row * stride + 4] = n as u64;
            batch.set_str(row, 2, s.as_bytes());
        }
        let mut gids = Vec::new();
        t.probe(&batch, &specs, &mut gids);
        assert_eq!(t.groups(), 4, "only the repeated key shares a group");
    }

    #[test]
    fn merge_folds_a_second_partial() {
        let mut a = GroupTable::new(vec![PartKind::Int], 1);
        let mut b = GroupTable::new(vec![PartKind::Int], 1);
        count_ints(&mut a, &(0..100).map(|i| i % 10).collect::<Vec<_>>());
        count_ints(&mut b, &(0..100).map(|i| i % 15).collect::<Vec<_>>());
        a.merge_from(&b).unwrap();
        let rows = a.drain();
        assert_eq!(rows.len(), 15, "keys 10 to 14 came from the second partial");
        assert_eq!(rows.iter().map(|(_, s)| count_of(&s[0])).sum::<i64>(), 200);
    }

    #[test]
    fn merge_folds_string_keys_through_the_heap() {
        let specs = [AggSpec::CountStar];
        let mut tables: Vec<GroupTable> = ["a", "bb"]
            .iter()
            .map(|s| {
                let mut t = GroupTable::new(vec![PartKind::Str], 1);
                let mut batch = KeyBatch::default();
                batch.reset(t.stride(), 2);
                batch.set_str(0, 0, s.as_bytes());
                batch.set_str(1, 0, b"shared");
                let mut gids = Vec::new();
                t.probe(&batch, &specs, &mut gids);
                for &g in &gids {
                    t.accs_mut()[g as usize].add_star(1);
                }
                t
            })
            .collect();
        let b = tables.pop().unwrap();
        let mut a = tables.pop().unwrap();
        a.merge_from(&b).unwrap();
        let rows = a.drain();
        assert_eq!(rows.len(), 3, "a, bb, and the shared key");
        let shared = rows
            .iter()
            .find(|(v, _)| v[0] == Value::Str("shared".into()))
            .expect("the shared key survived the merge");
        assert_eq!(count_of(&shared.1[0]), 2);
    }

    #[test]
    fn group_indices_report_first_sight() {
        let mut t = GroupTable::new(vec![PartKind::Int], 0);
        let mut batch = KeyBatch::default();
        batch.reset(1, 4);
        let (words, _) = batch.words_mut();
        words.copy_from_slice(&[3, 3, 4, 3]);
        let mut gids = Vec::new();
        t.probe(&batch, &[], &mut gids);
        // A row is the first sight of its key exactly when its index is
        // the next one the table had not handed out yet.
        let mut next = 0;
        let first: Vec<bool> = gids
            .iter()
            .map(|&g| {
                let new = g as usize == next;
                next += usize::from(new);
                new
            })
            .collect();
        assert_eq!(first, [true, false, true, false]);
        assert_eq!(t.groups(), 2);
    }

    #[test]
    fn counting_mode_agrees_with_the_general_table() {
        let vals: Vec<u64> = (0..50_000).map(|i: u64| (i * 7919) % 3000).collect();
        let mut c = GroupTable::counting(vec![PartKind::Int], 1);
        c.count_ints(&vals);
        let mut g = GroupTable::new(vec![PartKind::Int], 1);
        count_ints(&mut g, &vals.iter().map(|&v| v as i64).collect::<Vec<_>>());
        let mut counted = c.drain();
        let mut general = g.drain();
        assert_eq!(counted.len(), 3000, "the table grew past its first slots");
        for rows in [&mut counted, &mut general] {
            rows.sort_by_key(|(v, _)| match v[0] {
                Value::Int(n) => n,
                _ => panic!("int key"),
            });
        }
        let key_of = |r: &(Vec<Value>, Vec<Acc>)| r.0[0].clone();
        assert_eq!(
            counted.iter().map(key_of).collect::<Vec<_>>(),
            general.iter().map(key_of).collect::<Vec<_>>(),
        );
        let count_at = |r: &(Vec<Value>, Vec<Acc>)| count_of(&r.1[0]);
        assert_eq!(
            counted.iter().map(count_at).collect::<Vec<_>>(),
            general.iter().map(count_at).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn counting_mode_merges_a_second_partial() {
        let mut a = GroupTable::counting(vec![PartKind::Int], 2);
        let mut b = GroupTable::counting(vec![PartKind::Int], 2);
        a.count_ints(&(0..100).map(|i: u64| i % 10).collect::<Vec<_>>());
        b.count_ints(&(0..100).map(|i: u64| i % 15).collect::<Vec<_>>());
        a.merge_from(&b).unwrap();
        let rows = a.drain();
        assert_eq!(rows.len(), 15, "keys 10 to 14 came from the second partial");
        assert_eq!(rows.iter().map(|(_, s)| count_of(&s[0])).sum::<i64>(), 200);
        for (_, accs) in &rows {
            assert_eq!(accs.len(), 2, "one count per aggregate over the group");
            assert_eq!(count_of(&accs[0]), count_of(&accs[1]));
        }
    }

    #[test]
    fn counting_mode_keeps_the_zero_key() {
        // Zero is a real key and an empty slot is a zero count, so the
        // two only stay apart because a group starts at one.
        let mut t = GroupTable::counting(vec![PartKind::Int], 1);
        t.count_ints(&[0, 0, 5]);
        let rows = t.drain();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0[0], Value::Int(0));
        assert_eq!(count_of(&rows[0].1[0]), 2);
    }

    /// The order the drain used to be put in by the sink, so that the
    /// word compare can be held to it.
    fn by_value(rows: &mut [(Vec<Value>, Vec<Acc>)]) {
        rows.sort_by(|a, b| {
            a.0.iter()
                .zip(&b.0)
                .map(|(x, y)| OrdValue(x.clone()).cmp(&OrdValue(y.clone())))
                .find(|o| *o != std::cmp::Ordering::Equal)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    #[test]
    fn rows_come_out_by_key_ascending_and_in_clause_order() {
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        // Negative keys among them, because the words are unsigned and
        // the order is not.
        count_ints(&mut t, &[5, -3, 5, 0, 12, -3, -3]);
        // count(*) first and the key second, which is what
        // `RETURN count(*), n.k` asks for, so a row that just
        // concatenated the two halves would come out backwards.
        let rows = t.rows(&[true, false], 1);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(3), Value::Int(-3)],
                vec![Value::Int(1), Value::Int(0)],
                vec![Value::Int(2), Value::Int(5)],
                vec![Value::Int(1), Value::Int(12)],
            ]
        );
    }

    #[test]
    fn a_counting_table_orders_its_slots_too() {
        // Insertion order here is 7, 1, 4, which is neither the slot
        // order nor the answer.
        let mut t = GroupTable::counting(vec![PartKind::Int], 1);
        t.count_ints(&[7, 1, 4, 7, 7]);
        let rows = t.rows(&[false, true], 1);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(4), Value::Int(1)],
                vec![Value::Int(7), Value::Int(3)],
            ]
        );
    }

    /// The point of the whole change: ordering over the packed words has
    /// to be the ordering over the values they decode to, column by
    /// column, for a key that has one of every kind in it.
    #[test]
    fn the_word_order_is_the_value_order() {
        let parts = vec![PartKind::Node, PartKind::Str, PartKind::Int];
        let specs = [AggSpec::CountStar];
        // Both string forms are here on purpose: "a longer string past
        // the inline limit" lives in the heap and the rest live in their
        // words, and they still have to sort against each other by their
        // bytes rather than by where the bytes are.
        let keys: [(u64, u64, &str, i64); 8] = [
            (2, 5, "x", 9),
            (1, 5, "x", 9),
            (1, 5, "x", -8),
            (1, 4, "x", 9),
            (1, 5, "a longer string past the inline limit", 9),
            (1, 5, "y", 9),
            (1, 5, "", 9),
            (0, 0, "zz", 0),
        ];
        let mut t = GroupTable::new(parts.clone(), 1);
        let mut batch = KeyBatch::default();
        batch.reset(t.stride(), keys.len());
        for (row, &(table, offset, s, n)) in keys.iter().enumerate() {
            let (words, stride) = batch.words_mut();
            words[row * stride] = table;
            words[row * stride + 1] = offset;
            words[row * stride + 4] = n as u64;
            batch.set_str(row, 2, s.as_bytes());
        }
        let mut gids = Vec::new();
        t.probe(&batch, &specs, &mut gids);
        for &g in &gids {
            t.accs_mut()[g as usize].add_star(1);
        }
        // The same table twice, since one of the two ways of reading it
        // consumes it.
        let mut same = GroupTable::new(parts, 1);
        same.merge_from(&t).unwrap();
        let mut expect = same.drain();
        by_value(&mut expect);
        let want: Vec<Vec<Value>> = expect
            .into_iter()
            .map(|(mut vals, accs)| {
                vals.push(accs.into_iter().next().expect("one count").finalize());
                vals
            })
            .collect();
        assert_eq!(t.rows(&[false, false, false, true], 1), want);
    }

    /// The split is a split of the work and not of the answer, so a
    /// table wide enough to go to the pool has to put out exactly what
    /// it puts out on one hand, in the same order.
    #[test]
    fn many_hands_answer_what_one_hand_answers() {
        let vals: Vec<i64> = (0..200_000).map(|i| (i * 7919) % 20_000).collect();
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        count_ints(&mut t, &vals);
        let mut same = GroupTable::new(vec![PartKind::Int], 1);
        same.merge_from(&t).unwrap();
        assert!(
            t.groups() > SPLIT_ROWS * 2,
            "wide enough that the split is taken"
        );
        assert_eq!(t.rows(&[false, true], 8), same.rows(&[false, true], 1));
    }

    /// The counting table takes the same split, and its groups live in
    /// the slots rather than in the key words, so it gets its own pass.
    #[test]
    fn many_hands_answer_what_one_hand_answers_counting() {
        let words: Vec<u64> = (0..200_000).map(|i: u64| (i * 7919) % 20_000).collect();
        let mut t = GroupTable::counting(vec![PartKind::Int], 1);
        t.count_ints(&words);
        let mut same = GroupTable::counting(vec![PartKind::Int], 1);
        same.count_ints(&words);
        assert!(t.groups() > SPLIT_ROWS * 2, "wide enough for the split");
        assert_eq!(t.rows(&[false, true], 8), same.rows(&[false, true], 1));
    }

    /// The same check on a table whose keys arrived already in order,
    /// which is the case where a stride through the groups samples an
    /// ordered list rather than a shuffled one.
    #[test]
    fn many_hands_answer_what_one_hand_answers_on_sorted_input() {
        let vals: Vec<i64> = (0..200_000).map(|i| i / 10).collect();
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        count_ints(&mut t, &vals);
        let mut same = GroupTable::new(vec![PartKind::Int], 1);
        same.merge_from(&t).unwrap();
        assert!(t.groups() > SPLIT_ROWS * 2, "wide enough for the split");
        assert_eq!(t.rows(&[false, true], 8), same.rows(&[false, true], 1));
    }

    /// The split has to hand every group to exactly one range, and the
    /// ranges have to ascend, or laying them end to end is not the sort.
    /// It should also split them somewhere near evenly, since a hand
    /// that gets most of the groups is the serial sort back again.
    #[test]
    fn the_split_covers_every_group_once_and_the_ranges_ascend() {
        let vals: Vec<i64> = (0..200_000).map(|i| (i * 7919) % 20_000).collect();
        let mut t = GroupTable::new(vec![PartKind::Int], 1);
        count_ints(&mut t, &vals);
        let groups: Vec<u32> = (0..t.groups as u32).collect();
        let want = groups.len();
        let buckets = t.split_groups(groups, 8);
        let mut seen: Vec<u32> = buckets.iter().flatten().copied().collect();
        assert_eq!(seen.len(), want, "no group dropped or handed out twice");
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), want);
        let mut last = i64::MIN;
        for bucket in &buckets {
            assert!(
                bucket.len() < want / 2,
                "a range with {} of {want} groups is not a split",
                bucket.len()
            );
            let (mut low, mut high) = (i64::MAX, i64::MIN);
            for &g in bucket {
                let key = t.keys[g as usize * t.stride] as i64;
                low = low.min(key);
                high = high.max(key);
            }
            if bucket.is_empty() {
                continue;
            }
            assert!(
                low > last,
                "range starts at {low}, the one before ended {last}"
            );
            last = high;
        }
    }

    /// A date is the one lane stored narrower than the word it rides in,
    /// so a negative one is the case where comparing the raw word and
    /// comparing the value part ways.
    #[test]
    fn dates_before_the_epoch_sort_before_the_ones_after() {
        let lane = TemporalLane::Date;
        let mut t = GroupTable::new(vec![PartKind::Temporal(lane)], 1);
        count_ints(&mut t, &[10, -5, 0, -400]);
        let rows = t.rows(&[false, true], 1);
        let days: Vec<i64> = rows
            .iter()
            .map(|r| match r[0] {
                Value::Temporal(zu_common::Temporal::Date(d)) => i64::from(d),
                _ => panic!("date key"),
            })
            .collect();
        assert_eq!(days, [-400, -5, 0, 10]);
    }
}
