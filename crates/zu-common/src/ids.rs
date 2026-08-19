//! A hash for maps whose key is an id.
//!
//! The default hash is SipHash, which is there so a map fed keys by a
//! stranger cannot be made to collide on purpose. Nothing in the engine
//! is fed keys by a stranger: a table id, a column id, a block offset
//! and a chunk number all come from the catalog or the file, and the
//! maps they key are small and read once or twice a statement. What
//! SipHash costs on those is a round of mixing per lookup for a
//! guarantee nobody needs, and it shows: on a point write the two
//! lookups of the props map alone were near a tenth of the processor
//! time the statement spent outside the file.
//!
//! So ids get their own hash, one multiply and one shift. The multiply
//! is by an odd constant, which is a bijection on 64 bits and puts the
//! entropy of the whole key in the top bits; the shift folds those back
//! down, because a hash table reads the top bits and the bottom bits for
//! two different things and both have to be mixed. Sequential ids, which
//! is what these keys mostly are, come out spread rather than adjacent.
//!
//! This is not a hash to key anything on that arrives from outside. Use
//! it for ids and use the default for the rest.

use std::hash::{BuildHasherDefault, Hasher};

/// The odd constant, 2^64 over the golden ratio.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// Spreads one key over the whole word, top and bottom.
const fn mix(key: u64) -> u64 {
    let spread = key.wrapping_mul(GOLDEN);
    spread ^ (spread >> 32)
}

/// A [`Hasher`] for keys that are one integer.
///
/// A key written as several pieces, or as bytes, still hashes, just
/// without the one-multiply shape: each piece mixes into what came
/// before. That is here so a misuse is slow rather than wrong.
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = mix(self.0 ^ u64::from(byte));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }

    fn write_u16(&mut self, n: u16) {
        self.write_u64(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = mix(self.0.rotate_left(5) ^ n);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_i8(&mut self, n: i8) {
        self.write_u64(n as u64);
    }

    fn write_i16(&mut self, n: i16) {
        self.write_u64(n as u64);
    }

    fn write_i32(&mut self, n: i32) {
        self.write_u64(n as u64);
    }

    fn write_i64(&mut self, n: i64) {
        self.write_u64(n as u64);
    }

    fn write_isize(&mut self, n: isize) {
        self.write_u64(n as u64);
    }
}

/// The [`BuildHasher`](std::hash::BuildHasher) of [`IdHasher`], which
/// is what the map types below carry.
pub type BuildIdHasher = BuildHasherDefault<IdHasher>;

/// A [`HashMap`](std::collections::HashMap) keyed by an id.
pub type IdMap<K, V> = std::collections::HashMap<K, V, BuildIdHasher>;

/// A [`HashSet`](std::collections::HashSet) of ids.
pub type IdSet<K> = std::collections::HashSet<K, BuildIdHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::hash::Hash;

    fn hash<K: Hash>(key: K) -> u64 {
        let mut h = IdHasher::default();
        key.hash(&mut h);
        h.finish()
    }

    #[test]
    fn a_map_keyed_by_an_id_reads_back_what_was_put_in_it() {
        let mut m: IdMap<u32, &str> = IdMap::default();
        for i in 0..1000u32 {
            m.insert(i, "row");
        }
        assert_eq!(m.len(), 1000);
        assert_eq!(m.get(&999), Some(&"row"));
        assert_eq!(m.get(&1000), None);
    }

    #[test]
    fn the_ids_a_catalog_hands_out_do_not_land_on_one_another() {
        let seen: HashSet<u64> = (0..4096u32).map(hash).collect();
        assert_eq!(seen.len(), 4096, "sequential ids all differ");
    }

    /// What a hash table reads: the top bits pick the control byte and
    /// the bottom bits pick the bucket, so neither end may be the key
    /// itself.
    #[test]
    fn both_ends_of_the_word_move_with_the_key() {
        for shift in [0u32, 7, 25, 57] {
            let ends = |h: u64| (h >> 57, h & 0x7f);
            let (top, bottom) = ends(hash(1u64 << shift));
            let (next_top, next_bottom) = ends(hash((1u64 << shift) + 1));
            assert_ne!(top, next_top, "top bits at shift {shift}");
            assert_ne!(bottom, next_bottom, "bottom bits at shift {shift}");
        }
    }

    #[test]
    fn a_key_written_as_bytes_still_separates() {
        let seen: HashSet<u64> = ["age", "name", "since", "id", ""]
            .map(hash)
            .into_iter()
            .collect();
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn two_ids_in_a_pair_are_not_the_same_pair_swapped() {
        assert_ne!(hash((1u32, 2u32)), hash((2u32, 1u32)));
    }
}
