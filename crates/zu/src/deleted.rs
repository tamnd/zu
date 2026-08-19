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
//! says whether any table has one, and a table with no chain answers
//! every question with a map miss.
//!
//! The set is kept in two halves, sealed and fresh, and never put
//! together into one. Sealed is what the chains say and only changes
//! under a fold; fresh is what the commits since then took away and
//! grows by one on every delete. Merging them would cost the length of
//! the whole set on every statement, which is a delete that gets
//! slower the more the file has deleted, so the two ascending runs are
//! carried side by side and read that way.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::zu1::catalog::TableIndex;
use crate::zu1::file::Zu1File;
use crate::zu1::fold::{TOMBSTONE_KEY, decode_tombstones};
use crate::zu1::meta;
use zu_common::{IdMap, Result};

/// The rows the commits since the last fold took away, by table, each
/// list ascending. This is what a delete leaves behind when it is not
/// folded, and it goes over the chains the same way a cell patch goes
/// over a column.
pub(crate) type Tombstones = IdMap<u32, Arc<[u64]>>;

/// The part of one half's list for `table` that falls in
/// `base..base + rows`, which is ascending because the list is.
fn cut(half: &BTreeMap<u32, Arc<[u64]>>, table: u32, base: u64, rows: u64) -> &[u64] {
    let Some(all) = half.get(&table) else {
        return &[];
    };
    let from = all.partition_point(|&o| o < base);
    let to = all.partition_point(|&o| o < base + rows);
    &all[from..to]
}

/// Every table's deleted rows, sorted, the chains and the commits over
/// them kept apart. See the module note for why they stay apart.
#[derive(Debug, Clone, Default)]
pub(crate) struct Deleted {
    sealed: BTreeMap<u32, Arc<[u64]>>,
    fresh: BTreeMap<u32, Arc<[u64]>>,
}

impl Deleted {
    /// Reads the tombstone chain of every table that has one. A file
    /// where nothing was ever deleted comes back empty after a single
    /// table index read.
    pub(crate) fn load(db: &mut Zu1File) -> Result<Self> {
        let entries: Vec<(u32, u64)> = TableIndex::load(db)?.entries().to_vec();
        let mut sealed = BTreeMap::new();
        for (id, root) in entries {
            if id & TOMBSTONE_KEY == 0 {
                continue;
            }
            let offsets = decode_tombstones(&meta::read_chain(db, root)?)?;
            if offsets.is_empty() {
                continue;
            }
            sealed.insert(id & !TOMBSTONE_KEY, offsets.into());
        }
        Ok(Deleted {
            sealed,
            fresh: BTreeMap::new(),
        })
    }

    /// The same read with the rows a commit took away and nobody has
    /// folded yet put over it, which is what every reader wants: a row
    /// a delete removed is gone whether or not the tombstone has
    /// reached the chain it will end up in.
    pub(crate) fn load_with(db: &mut Zu1File, patch: &Tombstones) -> Result<Self> {
        let mut gone = Deleted::load(db)?;
        gone.overlay(patch);
        Ok(gone)
    }

    /// Puts the unfolded offsets over the chains. The lists themselves
    /// are shared, so this costs a reference count per table that has
    /// lost a row since the last fold and nothing per row.
    pub(crate) fn overlay(&mut self, patch: &Tombstones) {
        self.fresh = patch
            .iter()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(&table, rows)| (table, Arc::clone(rows)))
            .collect();
    }

    /// Whether one row is gone, which it is when either half names it.
    /// A file that has only ever been written to answers with two map
    /// misses.
    pub(crate) fn holds(&self, table: u32, offset: u64) -> bool {
        let names = |half: &BTreeMap<u32, Arc<[u64]>>| {
            half.get(&table)
                .is_some_and(|rows| rows.binary_search(&offset).is_ok())
        };
        names(&self.sealed) || names(&self.fresh)
    }

    /// The deleted rows that fall in `base..base + rows`, one ascending
    /// run per half, which is what a chunk filter walks alongside its
    /// own rows. A row in both halves, which is what a delete of a row
    /// an earlier fold already tombstoned leaves, appears in both.
    pub(crate) fn span(&self, table: u32, base: u64, rows: u64) -> (&[u64], &[u64]) {
        (
            cut(&self.sealed, table, base, rows),
            cut(&self.fresh, table, base, rows),
        )
    }

    /// Every table's set, to hand to a reader that keeps its own copy.
    /// The sets themselves are shared, so this costs a reference count
    /// per table that has lost a row and nothing per row.
    pub(crate) fn rows(&self) -> zu_query::exec::DeletedRows {
        zu_query::exec::DeletedRows::of(self.sealed.clone(), self.fresh.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deleted(pairs: &[(u32, &[u64])]) -> Deleted {
        Deleted {
            sealed: pairs
                .iter()
                .map(|&(table, rows)| (table, rows.into()))
                .collect(),
            fresh: BTreeMap::new(),
        }
    }

    /// The two runs of a span, put together, which is what the callers
    /// see once their cursor has walked both.
    fn span(d: &Deleted, table: u32, base: u64, rows: u64) -> Vec<u64> {
        let (sealed, fresh) = d.span(table, base, rows);
        let mut all: Vec<u64> = sealed.iter().chain(fresh).copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    #[test]
    fn a_file_that_never_deleted_a_row_holds_nothing() {
        let d = Deleted::default();
        assert!(!d.holds(0, 0));
        assert!(span(&d, 0, 0, 1024).is_empty());
    }

    #[test]
    fn a_deleted_row_is_held_and_its_neighbours_are_not() {
        let d = deleted(&[(3, &[1, 4, 9])]);
        assert!(d.holds(3, 4));
        assert!(!d.holds(3, 5));
        assert!(!d.holds(4, 4), "another table keeps its rows");
    }

    #[test]
    fn an_unfolded_delete_joins_the_chain_it_will_end_up_in() {
        let mut d = deleted(&[(1, &[2, 6]), (2, &[0])]);
        let patch: Tombstones = [
            (1u32, [1u64, 2, 9].as_slice().into()),
            (3, [4u64].as_slice().into()),
        ]
        .into_iter()
        .collect();
        d.overlay(&patch);
        assert_eq!(span(&d, 1, 0, 16), &[1, 2, 6, 9], "both runs, pair once");
        assert_eq!(
            span(&d, 2, 0, 16),
            &[0],
            "a table the patch says nothing of"
        );
        assert_eq!(span(&d, 3, 0, 16), &[4], "a table only the patch knows of");
        assert!(d.holds(1, 9));
        assert!(d.holds(1, 6));
        assert_eq!(span(&d, 1, 0, 3), &[1, 2]);
    }

    #[test]
    fn a_second_patch_replaces_the_first_rather_than_adding_to_it() {
        let mut d = deleted(&[(1, &[2])]);
        d.overlay(&[(1u32, [5u64].as_slice().into())].into_iter().collect());
        d.overlay(&[(1u32, [5u64, 7].as_slice().into())].into_iter().collect());
        assert_eq!(
            span(&d, 1, 0, 16),
            &[2, 5, 7],
            "the patch is the whole patch"
        );
        d.overlay(&Tombstones::default());
        assert_eq!(span(&d, 1, 0, 16), &[2], "a fold takes the patch away");
    }

    #[test]
    fn a_chunk_sees_only_the_rows_that_fall_in_it() {
        let d = deleted(&[(1, &[0, 5, 2048, 2050, 5000])]);
        assert_eq!(span(&d, 1, 0, 2048), &[0, 5]);
        assert_eq!(span(&d, 1, 2048, 2048), &[2048, 2050]);
        assert_eq!(span(&d, 1, 4096, 2048), &[5000]);
        assert!(span(&d, 1, 6144, 2048).is_empty());
    }
}
