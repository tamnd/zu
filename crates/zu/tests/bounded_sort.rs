//! ORDER BY under a LIMIT, over enough rows to cross a chunk.
//!
//! A bounded sort is allowed to throw rows away, so the only thing that
//! makes it correct is that what it keeps is what the unbounded sort
//! would have handed back first. Every case here is that comparison,
//! taken over a table long enough that the sink settles many chunks
//! rather than one, because a buffer that judges a chunk against
//! nothing but itself passes a single chunk and fails everything else.
//!
//! One integer key with nothing missing from it has its own path
//! through the driver, which reads the column as `i64` and never builds
//! a key at all, so the columns below are shaped to work it: one that
//! climbs with the scan, which under a descending key makes every row
//! the new best, one that falls, which makes every row a loser after
//! the first, and one with seven values in five thousand rows, where
//! nearly every compare is a tie. A nullable column is here to prove
//! the general path still answers, since a null orders outside the
//! direction and the fast path declines it.

use zu::query::run;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropInput, PropValues, store_props_nullable};
use zu_query::exec::Value;

const NODES: u64 = 5_000;

struct Fixture {
    _dir: tempfile::TempDir,
    db: Zu1File,
}

fn seeded() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bounded-sort.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "N", "E", NODES, &[]).expect("load");
    let n = NODES as i64;
    // The column is stored as words and read back as signed integers,
    // so `few` runs through zero: a raw integer compare that forgot its
    // sign would order the negatives after everything.
    let up: Vec<u64> = (0..n).map(|i| i as u64).collect();
    let down: Vec<u64> = (0..n).map(|i| (n - 1 - i) as u64).collect();
    let few: Vec<u64> = (0..n).map(|i| (i % 7 - 3) as u64).collect();
    let maybe: Vec<u64> = (0..n).map(|i| (i / 3) as u64).collect();
    // Every third row holds nothing, which is often enough that a null
    // lands in any window a LIMIT here could take.
    let mut mask = vec![0u64; (NODES as usize).div_ceil(64)];
    for row in 0..NODES as usize {
        if row % 3 != 0 {
            mask[row / 64] |= 1u64 << (row % 64);
        }
    }
    store_props_nullable(
        &mut db,
        "N",
        &[
            PropInput::dense("up", PropValues::Int(&up)),
            PropInput::dense("down", PropValues::Int(&down)),
            PropInput::dense("few", PropValues::Int(&few)),
            PropInput {
                name: "maybe",
                values: PropValues::Int(&maybe),
                validity: Some(&mask),
                declared: None,
            },
        ],
    )
    .expect("props");
    Fixture { _dir: dir, db }
}

impl Fixture {
    /// The single column of a query, nulls included, in the order the
    /// query answered it.
    fn col(&mut self, source: &str) -> Vec<Option<i64>> {
        run(source, &mut self.db, &[])
            .expect("query")
            .rows
            .into_iter()
            .map(|row| match row.into_iter().next() {
                Some(Value::Int(n)) => Some(n),
                Some(Value::Null) => None,
                other => panic!("not an integer column: {other:?}"),
            })
            .collect()
    }
}

/// What a bounded sort keeps is the prefix of what the unbounded one
/// hands back, whatever the column looks like and whichever way the
/// key runs.
#[test]
fn a_limit_over_a_sort_takes_the_orders_prefix() {
    let mut fx = seeded();
    for col in ["up", "down", "few", "maybe"] {
        for dir in ["ASC", "DESC"] {
            let full = fx.col(&format!(
                "MATCH (n:N) RETURN n.{col} AS v ORDER BY n.{col} {dir}"
            ));
            assert_eq!(full.len(), NODES as usize, "{col} {dir} lost rows");
            for k in [1usize, 3, 50, 1000] {
                let got = fx.col(&format!(
                    "MATCH (n:N) RETURN n.{col} AS v ORDER BY n.{col} {dir} LIMIT {k}"
                ));
                assert_eq!(got, full[..k], "{col} {dir} LIMIT {k}");
            }
        }
    }
}

/// A SKIP under a LIMIT is still a window on the same order, and it is
/// the case the bound has to be carried through rather than dropped:
/// the buffer has to hold skip plus limit rows to answer it.
#[test]
fn a_skip_under_a_limit_takes_the_same_window() {
    let mut fx = seeded();
    for col in ["up", "few", "maybe"] {
        let full = fx.col(&format!(
            "MATCH (n:N) RETURN n.{col} AS v ORDER BY n.{col} DESC"
        ));
        for (skip, take) in [(0usize, 4usize), (1, 3), (17, 5), (2_500, 10)] {
            let got = fx.col(&format!(
                "MATCH (n:N) RETURN n.{col} AS v ORDER BY n.{col} DESC SKIP {skip} LIMIT {take}"
            ));
            assert_eq!(
                got,
                full[skip..skip + take],
                "{col} SKIP {skip} LIMIT {take}"
            );
        }
    }
}

/// The key the sort reads and the column the query returns need not be
/// the same thing. This is the commonest bounded shape there is, and
/// the one where the projection sits between the sort and the answer.
#[test]
fn the_key_need_not_be_a_column_the_answer_carries() {
    let mut fx = seeded();
    let full = fx.col("MATCH (n:N) RETURN n.up AS v ORDER BY n.down DESC");
    let got = fx.col("MATCH (n:N) RETURN n.up AS v ORDER BY n.down DESC LIMIT 3");
    assert_eq!(got, full[..3]);
    // Spelled through the alias, which reaches the same column by a
    // different name and must reach the same answer.
    let alias = fx.col("MATCH (n:N) RETURN n.up AS v ORDER BY v DESC LIMIT 3");
    assert_eq!(alias, vec![Some(4_999), Some(4_998), Some(4_997)]);
}

/// Ties break on scan position, so a column with seven values in five
/// thousand rows answers with the rows that came first and not with an
/// arbitrary seven hundred of them.
#[test]
fn a_tie_breaks_on_the_row_that_came_first() {
    let mut fx = seeded();
    let rows = run(
        "MATCH (n:N) RETURN n.few AS v, n.up AS seen ORDER BY n.few DESC LIMIT 4",
        &mut fx.db,
        &[],
    )
    .expect("query")
    .rows;
    let seen: Vec<i64> = rows
        .iter()
        .map(|row| match row[1] {
            Value::Int(n) => n,
            _ => panic!("not an integer column"),
        })
        .collect();
    // few is row % 7 less three, so its threes are rows 6, 13, 20, 27.
    assert_eq!(seen, [6, 13, 20, 27]);
}
