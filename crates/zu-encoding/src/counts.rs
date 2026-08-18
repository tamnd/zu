//! Counting distinct values, which two encodings need and the cascade
//! asks about before it picks one.
//!
//! Dict needs to know how many distinct values a chunk holds, because
//! that is its code width and the format caps it; Frequency needs the
//! value that dominates and how often. Both used to build a `HashMap`,
//! and a `HashMap` of `u64` keys under the default hasher is SipHash per
//! value, which measured as the single largest cost of writing a column.
//! This is the same count over an open addressed table with a multiply
//! shift hash, reused between calls so a chunk costs no allocation.
//!
//! Past `cap` distinct values the table stops taking new keys and says
//! so. That is what the callers want either way: Dict is illegal past
//! the cap, and a value that dominates a chunk is in the table long
//! before the tail of scattered ones fills it.

use std::cell::RefCell;

/// Knuth's multiplicative constant, the odd 64-bit golden ratio.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// What a pass over a chunk found.
pub(crate) struct Counts {
    /// How many distinct values, or None when there were more than the
    /// caller's cap and counting stopped.
    pub distinct: Option<usize>,
    /// The most common value the table held, and how often it appeared.
    /// Ties go to the one seen first.
    pub top: u64,
    pub top_count: usize,
}

#[derive(Default)]
struct Table {
    /// Value and count per slot, a count of zero meaning empty.
    slots: Vec<(u64, u32)>,
    /// The slots this pass wrote, so the next one clears those and not
    /// the whole table.
    used: Vec<u32>,
}

thread_local! {
    static TABLE: RefCell<Table> = RefCell::new(Table::default());
}

/// Counts `values`, giving up on distinctness past `cap` keys.
pub(crate) fn count(values: &[u64], cap: usize) -> Counts {
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let want = (cap.min(values.len()) * 2).next_power_of_two().max(64);
        if table.slots.len() < want {
            table.slots = vec![(0, 0); want];
            table.used = Vec::with_capacity(want / 2);
        }
        let Table { slots, used } = &mut *table;
        for &slot in used.iter() {
            slots[slot as usize] = (0, 0);
        }
        used.clear();
        let mask = table.slots.len() - 1;
        let cap = cap.min(values.len());
        let mut full = false;
        let mut top = values.first().copied().unwrap_or(0);
        let mut top_count = 0usize;
        for &v in values {
            let mut at = (v.wrapping_mul(GOLDEN) >> 32) as usize & mask;
            loop {
                let (key, n) = table.slots[at];
                if n == 0 {
                    if table.used.len() == cap {
                        full = true;
                        break;
                    }
                    table.slots[at] = (v, 1);
                    table.used.push(at as u32);
                    if top_count == 0 {
                        top = v;
                        top_count = 1;
                    }
                    break;
                }
                if key == v {
                    let n = n + 1;
                    table.slots[at].1 = n;
                    if n as usize > top_count {
                        top = v;
                        top_count = n as usize;
                    }
                    break;
                }
                at = (at + 1) & mask;
            }
        }
        Counts {
            distinct: (!full).then_some(table.used.len()),
            top,
            top_count,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_distinct_and_top() {
        let values = [7u64, 3, 7, 9, 7, 3];
        let counts = count(&values, 64);
        assert_eq!(counts.distinct, Some(3));
        assert_eq!((counts.top, counts.top_count), (7, 3));
    }

    #[test]
    fn gives_up_past_the_cap_and_keeps_counting_what_it_holds() {
        // Two values that dominate, then a long tail past the cap. The
        // count of distinct values is gone, the dominant value is not.
        let mut values = vec![5u64; 100];
        values.extend(1000..2000u64);
        let counts = count(&values, 8);
        assert_eq!(counts.distinct, None);
        assert_eq!((counts.top, counts.top_count), (5, 100));
    }

    #[test]
    fn the_table_comes_back_clean() {
        assert_eq!(count(&[1, 1, 2], 64).distinct, Some(2));
        let counts = count(&[9, 9, 9], 64);
        assert_eq!(counts.distinct, Some(1));
        assert_eq!((counts.top, counts.top_count), (9, 3));
    }

    #[test]
    fn empty_input() {
        let counts = count(&[], 64);
        assert_eq!(counts.distinct, Some(0));
        assert_eq!(counts.top_count, 0);
    }

    #[test]
    fn a_wide_run_of_keys_probes_without_losing_one() {
        let values: Vec<u64> = (0..2000u64).map(|i| i.wrapping_mul(GOLDEN)).collect();
        let counts = count(&values, 4096);
        assert_eq!(counts.distinct, Some(2000));
        assert_eq!(counts.top_count, 1);
    }
}
