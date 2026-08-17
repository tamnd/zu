//! The rows a `DELETE` took away, as a reader sees them.
//!
//! A delete does not compact: offsets have to stay stable, because
//! every edge in the file names its endpoints by offset, so the row a
//! delete removes keeps its place and the fold writes the offset into
//! the table's tombstone chain instead. The reader is what makes the
//! row gone, and this is the set it filters by.
//!
//! Nothing here costs anything on a file that has never deleted a row.
//! The chains hang off reserved table index keys, so one index read
//! says whether any table has one, and a table with no chain has an
//! empty set that every caller checks before it looks a row up.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::zu1::catalog::TableIndex;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{TOMBSTONE_KEY, decode_tombstones};
use crate::zu1::meta;
use zu_common::Result;

/// Every table's deleted rows, sorted, read out of the sealed file.
#[derive(Debug, Clone, Default)]
pub(crate) struct Deleted {
    tables: BTreeMap<u32, Arc<[u64]>>,
}

impl Deleted {
    /// Reads the tombstone chain of every table that has one. A file
    /// where nothing was ever deleted comes back empty after a single
    /// table index read.
    pub(crate) fn load(db: &mut Zu1File) -> Result<Self> {
        let entries: Vec<(u32, u64)> = TableIndex::load(db)?.entries().to_vec();
        let mut tables = BTreeMap::new();
        for (id, root) in entries {
            if id & TOMBSTONE_KEY == 0 {
                continue;
            }
            let offsets = decode_tombstones(&meta::read_chain(db, root)?)?;
            if offsets.is_empty() {
                continue;
            }
            tables.insert(id & !TOMBSTONE_KEY, offsets.into());
        }
        Ok(Deleted { tables })
    }

    /// Whether any table in the file has lost a row. False is the
    /// answer on every file that has only ever been written to, and it
    /// is what the read paths test before they do any work at all.
    pub(crate) fn none(&self) -> bool {
        self.tables.is_empty()
    }

    /// The deleted rows of one table, ascending.
    pub(crate) fn rows(&self, table: u32) -> &[u64] {
        self.tables.get(&table).map_or(&[], |rows| rows)
    }

    /// Whether one row is gone.
    pub(crate) fn holds(&self, table: u32, offset: u64) -> bool {
        !self.none() && self.rows(table).binary_search(&offset).is_ok()
    }

    /// The deleted rows that fall in `base..base + rows`, ascending,
    /// which is the slice a chunk filter walks alongside its own rows.
    pub(crate) fn span(&self, table: u32, base: u64, rows: u64) -> &[u64] {
        let all = self.rows(table);
        let from = all.partition_point(|&o| o < base);
        let to = all.partition_point(|&o| o < base + rows);
        &all[from..to]
    }

    /// Every table's set, to hand to a reader that keeps its own copy.
    /// The sets themselves are shared, so this costs a reference count
    /// per table that has lost a row and nothing per row.
    pub(crate) fn tables(&self) -> &BTreeMap<u32, Arc<[u64]>> {
        &self.tables
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deleted(pairs: &[(u32, &[u64])]) -> Deleted {
        Deleted {
            tables: pairs
                .iter()
                .map(|&(table, rows)| (table, rows.into()))
                .collect(),
        }
    }

    #[test]
    fn a_file_that_never_deleted_a_row_holds_nothing() {
        let d = Deleted::default();
        assert!(d.none());
        assert!(d.rows(0).is_empty());
        assert!(!d.holds(0, 0));
        assert!(d.span(0, 0, 1024).is_empty());
    }

    #[test]
    fn a_deleted_row_is_held_and_its_neighbours_are_not() {
        let d = deleted(&[(3, &[1, 4, 9])]);
        assert!(d.holds(3, 4));
        assert!(!d.holds(3, 5));
        assert!(!d.holds(4, 4), "another table keeps its rows");
    }

    #[test]
    fn a_chunk_sees_only_the_rows_that_fall_in_it() {
        let d = deleted(&[(1, &[0, 5, 2048, 2050, 5000])]);
        assert_eq!(d.span(1, 0, 2048), &[0, 5]);
        assert_eq!(d.span(1, 2048, 2048), &[2048, 2050]);
        assert_eq!(d.span(1, 4096, 2048), &[5000]);
        assert!(d.span(1, 6144, 2048).is_empty());
    }
}
