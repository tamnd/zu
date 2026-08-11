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
//! Probing is the memory-bound part, one dependent random access per
//! row, so the probe loop warms the slot line eight rows ahead. That is
//! what makes the vector unit of work pay: eight probes are in flight at
//! once instead of one, and the same trick works on the accumulator
//! updates, which the caller runs as its own loop over group indices.
//!
//! Ordering is not the table's job. Groups come out in insertion order
//! and the sink sorts them at the end, which is where the old engine's
//! ascending-key output is reproduced.

use zu_query::exec::Value;
use zu_vector::kernels::hash64;

use crate::compile::AggSpec;
use crate::sink::Acc;

/// What one key column holds, which decides how many words it occupies
/// and how it compares.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartKind {
    /// An integer or a dense row id: one word.
    Int,
    /// A node: table id and row offset, two words.
    Node,
    /// A string: byte offset and length into the batch's or the table's
    /// byte buffer, two words.
    Str,
}

impl PartKind {
    pub(crate) fn words(self) -> usize {
        match self {
            PartKind::Int => 1,
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
}

impl KeyBatch {
    /// Sizes the batch for `rows` keys of `stride` words and zeroes it.
    pub(crate) fn reset(&mut self, stride: usize, rows: usize) {
        self.words.clear();
        self.words.resize(stride * rows, 0);
        self.bytes.clear();
        self.stride = stride;
        self.rows = rows;
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
    /// fill row by row, since the bytes only pack in order.
    pub(crate) fn set_str(&mut self, row: usize, off: usize, s: &[u8]) {
        let at = row * self.stride + off;
        self.words[at] = self.bytes.len() as u64;
        self.words[at + 1] = s.len() as u64;
        self.bytes.extend_from_slice(s);
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
    slots: Vec<[u64; 2]>,
    mask: usize,
    groups: usize,
    /// One hash per row of the batch in flight.
    hashes: Vec<u64>,
}

/// Slots start here and double past three quarters full. Seven eighths
/// halves the table and keeps four slots to a cache line, which sounds
/// like the better trade, and it measured 25 percent slower on the
/// hundred thousand group bench: the probe chains lengthen faster than
/// the smaller footprint pays back. Sixty-four costs a kilobyte and
/// covers the many-groups-of-one queries that never grow past a handful.
const INIT_SLOTS: usize = 64;

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
            hashes: Vec::new(),
        }
    }

    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    #[cfg(test)]
    fn groups(&self) -> usize {
        self.groups
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
        for row in 0..batch.rows {
            if row + AHEAD < batch.rows {
                let i = hashes[row + AHEAD] as usize & self.mask;
                warm(self.slots[i]);
            }
            out[row] = self.find_or_insert(hashes[row], batch, row, specs) as u32;
        }
        self.hashes = hashes;
    }

    fn find_or_insert(&mut self, h: u64, batch: &KeyBatch, row: usize, specs: &[AggSpec]) -> usize {
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
                if self.simple || self.key_eq(g, batch, row) {
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
                PartKind::Int => self.keys.push(words[w]),
                PartKind::Node => {
                    self.keys.push(words[w]);
                    self.keys.push(words[w + 1]);
                }
                PartKind::Str => {
                    let (off, len) = (words[w] as usize, words[w + 1] as usize);
                    self.keys.push(self.heap.len() as u64);
                    self.keys.push(len as u64);
                    self.heap.extend_from_slice(&batch.bytes[off..off + len]);
                }
            }
            w += part.words();
        }
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
                PartKind::Int | PartKind::Node => {
                    if stored[w..w + part.words()] != words[w..w + part.words()] {
                        return false;
                    }
                }
                PartKind::Str => {
                    let len = stored[w + 1] as usize;
                    if len != words[w + 1] as usize {
                        return false;
                    }
                    let a = stored[w] as usize;
                    let b = words[w] as usize;
                    if self.heap[a..a + len] != batch.bytes[b..b + len] {
                        return false;
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
                PartKind::Int => hash64(h ^ words[w]),
                PartKind::Node => hash64(hash64(h ^ words[w]) ^ words[w + 1]),
                PartKind::Str => {
                    let (off, len) = (words[w] as usize, words[w + 1] as usize);
                    hash64(h ^ hash_bytes(&bytes[off..off + len]))
                }
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
                PartKind::Int => hash64(h ^ words[w]),
                PartKind::Node => hash64(hash64(h ^ words[w]) ^ words[w + 1]),
                PartKind::Str => {
                    let (off, len) = (words[w] as usize, words[w + 1] as usize);
                    hash64(h ^ hash_bytes(&self.heap[off..off + len]))
                }
            };
            w += part.words();
        }
        h
    }

    fn grow(&mut self) {
        let n = self.slots.len() * 2;
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
        let mut batch = KeyBatch::default();
        batch.reset(self.stride, 1);
        for g in 0..other.groups {
            other.read_into(g, &mut batch);
            // The hash reads key content, never the offsets, so the
            // other table's stored hash is this table's probe hash.
            let target = self.find_or_insert(other.hash_stored(g), &batch, 0, &[]);
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
        for &part in &self.parts {
            if part == PartKind::Str {
                let off = self.keys[base + w] as usize;
                let len = self.keys[base + w + 1] as usize;
                batch.set_str(0, w, &self.heap[off..off + len]);
            }
            w += part.words();
        }
    }

    /// Every group as its key values and its states, insertion ordered.
    pub(crate) fn drain(self) -> Vec<(Vec<Value>, Vec<Acc>)> {
        let mut out = Vec::with_capacity(self.groups);
        let mut accs = self.accs.into_iter();
        for g in 0..self.groups {
            let base = g * self.stride;
            let mut vals = Vec::with_capacity(self.parts.len());
            let mut w = 0;
            for &part in &self.parts {
                vals.push(match part {
                    PartKind::Int => Value::Int(self.keys[base + w] as i64),
                    PartKind::Node => Value::Node {
                        table: self.keys[base + w] as u32,
                        offset: self.keys[base + w + 1],
                    },
                    PartKind::Str => {
                        let off = self.keys[base + w] as usize;
                        let len = self.keys[base + w + 1] as usize;
                        // The caller checked these bytes for UTF-8 on
                        // the way in, so the lossy read never replaces
                        // anything.
                        Value::Str(String::from_utf8_lossy(&self.heap[off..off + len]).into_owned())
                    }
                });
                w += part.words();
            }
            out.push((vals, accs.by_ref().take(self.n_aggs).collect()));
        }
        out
    }
}

/// Hash of a string key part: eight bytes at a time through the same
/// finalizer the integer keys use, length folded in so that "ab" and
/// "ab\0" cannot land on the same value.
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = b.len() as u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = hash64(h ^ u64::from_le_bytes(c.try_into().expect("eight bytes")));
    }
    let mut tail = 0u64;
    for (i, &c) in chunks.remainder().iter().enumerate() {
        tail |= u64::from(c) << (i * 8);
    }
    hash64(h ^ tail)
}

#[cfg(test)]
mod tests {
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
        let strs = ["a", "bb", "a", "a longer string past the inline limit", "bb"];
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
}
