//! Primary-key index: original node ids surviving `REORDER`, the sealed
//! level of `docs/04-storage-zu1-format.md` §7.
//!
//! A relabeled graph stores rows under new ids, so without this index a
//! caller must carry the mapping out of band. The index stores the pairs
//! `(key, row)` sorted by key as two ordinary MiniBlock segments: the
//! keys, which delta-pack tightly because they are sorted, and the rows
//! in key order. A lookup fence-searches the key segment for the one
//! chunk that could hold the key, decodes it, and on a hit reads the
//! single matching row value, so it costs two chunk decodes however many
//! keys exist. Misses outside the key zone map cost nothing at all.
//!
//! The mutable hash overlay of §7 arrives with the updatable CSR; sealed
//! bulk loads only need the static level.

use std::sync::Arc;

use zu_common::{Result, ZuError};

use crate::file::Zu1File;
use crate::segment::{
    ChunkCache, ChunkDirectory, SegmentMeta, find_in_sorted_cached, load_chunk_directory_pooled,
    read_one_cached, read_segment, write_segment,
};

/// Locations of the two index segments, stored in the group directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyIndex {
    pub keys: SegmentMeta,
    pub rows: SegmentMeta,
}

/// Builds the sealed index from `key_by_row`, where `key_by_row[row]` is
/// the key of row `row`. Keys must be unique.
pub fn write_key_index(db: &mut Zu1File, key_by_row: &[u64]) -> Result<KeyIndex> {
    write_key_index_live(db, key_by_row, &[])
}

/// The same over the rows `dead` does not name, which are sorted row
/// offsets a DELETE took away.
///
/// A row nothing holds any more has no original id, and keeping its key
/// would be worse than pointless: the key is the one thing a later
/// INSERT can reasonably ask for back, and an index that still lists it
/// refuses the reuse as a duplicate of a row that is not there. So the
/// index covers the live rows, and a lookup of a key a DELETE took away
/// misses the way a lookup of a key nobody ever held does.
pub fn write_key_index_live(
    db: &mut Zu1File,
    key_by_row: &[u64],
    dead: &[u64],
) -> Result<KeyIndex> {
    let mut pairs: Vec<(u64, u64)> = key_by_row
        .iter()
        .enumerate()
        .filter(|(row, _)| dead.binary_search(&(*row as u64)).is_err())
        .map(|(row, &key)| (key, row as u64))
        .collect();
    pairs.sort_unstable();
    if let Some(w) = pairs.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(ZuError::InvalidArgument(format!(
            "duplicate key {} at rows {} and {}",
            w[0].0, w[0].1, w[1].1
        )));
    }
    let keys: Vec<u64> = pairs.iter().map(|&(k, _)| k).collect();
    let rows: Vec<u64> = pairs.iter().map(|&(_, r)| r).collect();
    Ok(KeyIndex {
        keys: write_segment(db, &keys)?,
        rows: write_segment(db, &rows)?,
    })
}

/// Read access to a sealed index: holds both segments' chunk
/// directories so every lookup goes straight to its one chunk, and a
/// decoded-chunk cache per segment so warm lookups that repeat a chunk
/// skip the read and the decode. That cache is the B2 budget: a warm
/// point lookup is a fence partition point and a binary search.
#[derive(Debug)]
pub struct KeyReader {
    index: KeyIndex,
    key_chunks: Arc<ChunkDirectory>,
    row_chunks: Arc<ChunkDirectory>,
    key_cache: ChunkCache,
    row_cache: ChunkCache,
}

impl KeyReader {
    pub fn load(db: &mut Zu1File, index: KeyIndex) -> Result<Self> {
        let pools = db.pools();
        let key_chunks = load_chunk_directory_pooled(db, &pools.fences, &index.keys)?;
        let row_chunks = load_chunk_directory_pooled(db, &pools.fences, &index.rows)?;
        Ok(Self {
            index,
            key_chunks,
            row_chunks,
            key_cache: ChunkCache::default(),
            row_cache: ChunkCache::default(),
        })
    }

    /// Returns the row stored under `key`, or `None`.
    pub fn lookup(&mut self, db: &mut Zu1File, key: u64) -> Result<Option<u64>> {
        let Some(pos) = find_in_sorted_cached(
            db,
            &self.index.keys,
            &self.key_chunks,
            &mut self.key_cache,
            key,
        )?
        else {
            return Ok(None);
        };
        read_one_cached(
            db,
            &self.index.rows,
            &self.row_chunks,
            &mut self.row_cache,
            pos,
        )
        .map(Some)
    }
}

/// Reads the index back in the shape [`write_key_index`] took it,
/// `key_by_row[row]` being the key of that row over the whole row
/// domain, and `None` where the index holds no key for the row.
///
/// A kernel whose answer is stated in original ids rather than in rows
/// needs every key, and asking for them one at a time costs a fence
/// search each to learn what two sequential segment scans already say.
///
/// The gaps are the rows a DELETE took away, which the index stops
/// covering the moment the fold rebuilds it. They are `None` rather
/// than any particular number because every number is a key some row
/// could legitimately hold.
pub fn key_by_row(db: &mut Zu1File, index: &KeyIndex, node_count: u64) -> Result<Vec<Option<u64>>> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "key index",
        detail,
    };
    let mut keys = Vec::with_capacity(index.keys.value_count as usize);
    read_segment(db, &index.keys, &mut keys)?;
    let mut rows = Vec::with_capacity(index.rows.value_count as usize);
    read_segment(db, &index.rows, &mut rows)?;
    if keys.len() != rows.len() {
        return Err(corrupt(format!(
            "index holds {} keys against {} rows",
            keys.len(),
            rows.len()
        )));
    }
    let mut by_row = vec![None; node_count as usize];
    for (&key, &row) in keys.iter().zip(&rows) {
        let slot = by_row
            .get_mut(row as usize)
            .ok_or_else(|| corrupt(format!("row {row} out of 0..{node_count}")))?;
        *slot = Some(key);
    }
    Ok(by_row)
}

/// Full check for `zu verify`: both segments decode clean (crc and zone
/// included via the scan path), the keys ascend strictly, and the rows
/// are exactly the live ones, which are the row domain less the sorted
/// offsets `dead` names.
pub fn verify_key_index(
    db: &mut Zu1File,
    index: &KeyIndex,
    node_count: u64,
    dead: &[u64],
) -> Result<()> {
    let corrupt = |detail: String| ZuError::Corrupt {
        what: "key index",
        detail,
    };
    let mut keys = Vec::with_capacity(index.keys.value_count as usize);
    read_segment(db, &index.keys, &mut keys)?;
    let mut rows = Vec::with_capacity(index.rows.value_count as usize);
    read_segment(db, &index.rows, &mut rows)?;
    let live = node_count - dead.len() as u64;
    if keys.len() as u64 != live || rows.len() as u64 != live {
        return Err(corrupt(format!(
            "index holds {} keys and {} rows over {live} live of {node_count} nodes",
            keys.len(),
            rows.len()
        )));
    }
    if let Some(w) = keys.windows(2).find(|w| w[0] >= w[1]) {
        return Err(corrupt(format!("keys not strictly ascending at {}", w[1])));
    }
    let mut seen = vec![false; node_count as usize];
    for &row in &rows {
        let slot = seen
            .get_mut(row as usize)
            .ok_or_else(|| corrupt(format!("row {row} out of 0..{node_count}")))?;
        if *slot {
            return Err(corrupt(format!("row {row} listed twice")));
        }
        if dead.binary_search(&row).is_ok() {
            return Err(corrupt(format!("row {row} is deleted and still keyed")));
        }
        *slot = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_finds_its_row_and_misses_stay_misses() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("k.zu1")).unwrap();
        // A permutation-shaped key set: multiply by an odd constant mod a
        // power of two, so keys are unique, unsorted by row, and spread
        // across many chunks.
        let n = 10_000u64;
        let key_of = |row: u64| (row.wrapping_mul(0x9E37_79B9) % (1 << 20)) | (1 << 40);
        let key_by_row: Vec<u64> = (0..n).map(key_of).collect();
        let index = write_key_index(&mut db, &key_by_row).unwrap();
        verify_key_index(&mut db, &index, n, &[]).unwrap();
        let mut reader = KeyReader::load(&mut db, index).unwrap();
        for row in (0..n).step_by(97) {
            assert_eq!(
                reader.lookup(&mut db, key_of(row)).unwrap(),
                Some(row),
                "row {row}"
            );
        }
        // Misses: below the zone, above it, and in-zone gaps.
        assert_eq!(reader.lookup(&mut db, 0).unwrap(), None);
        assert_eq!(reader.lookup(&mut db, u64::MAX).unwrap(), None);
        let mut sorted = key_by_row.clone();
        sorted.sort_unstable();
        for w in sorted.windows(2).take(50) {
            if w[1] > w[0] + 1 {
                assert_eq!(reader.lookup(&mut db, w[0] + 1).unwrap(), None);
            }
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_at_build() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("k.zu1")).unwrap();
        let err = write_key_index(&mut db, &[5, 9, 5]).unwrap_err();
        assert!(format!("{err}").contains("duplicate key 5"));
    }

    #[test]
    fn a_deleted_row_keeps_neither_its_key_nor_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("k.zu1")).unwrap();
        // Row 1 is gone, so its key is free for another row to take:
        // the index over the four rows holds three keys and answers 30
        // with a miss.
        let index = write_key_index_live(&mut db, &[10, 30, 20, 30], &[1]).unwrap();
        verify_key_index(&mut db, &index, 4, &[1]).unwrap();
        assert_eq!(
            key_by_row(&mut db, &index, 4).unwrap(),
            vec![Some(10), None, Some(20), Some(30)]
        );
        let mut reader = KeyReader::load(&mut db, index).unwrap();
        assert_eq!(reader.lookup(&mut db, 30).unwrap(), Some(3));
        assert_eq!(reader.lookup(&mut db, 10).unwrap(), Some(0));
        // An index that still holds a deleted row's key is a file that
        // will refuse the reuse, so verify says so rather than waiting
        // for the INSERT to fail.
        let stale = write_key_index(&mut db, &[10, 30, 20]).unwrap();
        let err = verify_key_index(&mut db, &stale, 3, &[1]).unwrap_err();
        assert!(format!("{err}").contains("3 keys and 3 rows over 2 live"));
        // The right number of keys, over the wrong rows: one of them is
        // the deleted one and the live row 3 has none.
        let stale = KeyIndex {
            keys: write_segment(&mut db, &[10, 20, 30]).unwrap(),
            rows: write_segment(&mut db, &[0, 1, 2]).unwrap(),
        };
        let err = verify_key_index(&mut db, &stale, 4, &[1]).unwrap_err();
        assert!(format!("{err}").contains("row 1 is deleted and still keyed"));
    }

    #[test]
    fn verify_catches_a_short_or_shuffled_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("k.zu1")).unwrap();
        let index = write_key_index(&mut db, &[10, 30, 20]).unwrap();
        verify_key_index(&mut db, &index, 3, &[]).unwrap();
        // Count mismatch against a larger claimed domain.
        let err = verify_key_index(&mut db, &index, 4, &[]).unwrap_err();
        assert!(format!("{err}").contains("3 keys"));
        // A rows segment that repeats a row: build a broken index by hand
        // from two valid segments.
        let broken = KeyIndex {
            keys: write_segment(&mut db, &[1, 2, 3]).unwrap(),
            rows: write_segment(&mut db, &[0, 1, 1]).unwrap(),
        };
        let err = verify_key_index(&mut db, &broken, 3, &[]).unwrap_err();
        assert!(format!("{err}").contains("listed twice"));
        let broken = KeyIndex {
            keys: write_segment(&mut db, &[1, 2, 3]).unwrap(),
            rows: write_segment(&mut db, &[0, 1, 7]).unwrap(),
        };
        let err = verify_key_index(&mut db, &broken, 3, &[]).unwrap_err();
        assert!(format!("{err}").contains("out of"));
    }
}
