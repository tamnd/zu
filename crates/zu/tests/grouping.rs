//! The explicit `GROUP BY` clause (ISO 16.15, feature GQ15).
//!
//! A projection holding an aggregate groups by everything else it
//! projects, which is the rule Cypher has and the one this engine
//! started with. `GROUP BY` says the same thing out loud, so what is
//! checked here is that it answers what the implicit grouping answers
//! when the two agree, and that a projection the grouping cannot
//! explain is refused rather than answered with a row per input.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 9;

fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..NODES)
        .flat_map(|i| [(i, (i + 1) % NODES), (i, (i + 3) % NODES)])
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        seeded(&path);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }

    /// The `k` column in the order the statement answered it, which is
    /// the whole of what a sort key decides.
    fn keys(&mut self, source: &str) -> Vec<i64> {
        self.conn
            .query(source)
            .expect("query")
            .iter()
            .map(|row| row.get_by_name::<i64>("k").expect("k"))
            .collect()
    }

    /// The `k` and `n` columns, sorted on the key, since a grouping
    /// answers a row per group and not a row in any order.
    fn groups(&mut self, source: &str) -> Vec<(i64, i64)> {
        let rows = self.conn.query(source).expect("query");
        let mut groups: Vec<(i64, i64)> = rows
            .iter()
            .map(|row| {
                (
                    row.get_by_name::<i64>("k").expect("k"),
                    row.get_by_name::<i64>("n").expect("n"),
                )
            })
            .collect();
        groups.sort_unstable();
        groups
    }
}

/// Written out, the grouping is the one the projection already implied,
/// so the two answer the same rows.
#[test]
fn group_by_says_what_the_projection_implied() {
    let mut fx = Fixture::open("group.zu1");
    let explicit = fx.groups(
        "MATCH (p:person) \
         LET k = p.id % 3 \
         RETURN k AS k, count(*) AS n \
         GROUP BY k",
    );
    assert_eq!(explicit, [(0, 3), (1, 3), (2, 3)]);
    let implicit = fx.groups(
        "MATCH (p:person) \
         LET k = p.id % 3 \
         RETURN k AS k, count(*) AS n",
    );
    assert_eq!(explicit, implicit);
}

/// Two keys are one grouping of the pair rather than two groupings.
#[test]
fn several_keys_group_on_all_of_them() {
    let mut fx = Fixture::open("group-pair.zu1");
    let rows = fx
        .conn
        .query(
            "MATCH (p:person) \
             LET a = p.id % 3, b = p.id % 2 \
             RETURN a AS a, b AS b, count(*) AS n \
             GROUP BY a, b",
        )
        .expect("query");
    let mut seen: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|row| {
            (
                row.get_by_name::<i64>("a").expect("a"),
                row.get_by_name::<i64>("b").expect("b"),
                row.get_by_name::<i64>("n").expect("n"),
            )
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            (0, 0, 2),
            (0, 1, 1),
            (1, 0, 1),
            (1, 1, 2),
            (2, 0, 2),
            (2, 1, 1)
        ]
    );
}

/// A grouping with nothing to aggregate still answers one row per
/// group, because that is what a group is.
#[test]
fn grouping_without_an_aggregate_answers_one_row_per_group() {
    let mut fx = Fixture::open("group-bare.zu1");
    let rows = fx
        .conn
        .query("MATCH (p:person) LET k = p.id % 3 RETURN k AS k GROUP BY k")
        .expect("query");
    let mut keys: Vec<i64> = rows
        .iter()
        .map(|row| row.get_by_name::<i64>("k").expect("k"))
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, [0, 1, 2]);
}

/// An item that is neither a key nor an aggregate has no one value in a
/// group, so it is refused by name rather than answered with whichever
/// row of the group came first.
#[test]
fn an_item_the_grouping_does_not_fix_is_refused() {
    let mut fx = Fixture::open("group-loose.zu1");
    let err = fx
        .conn
        .query(
            "MATCH (p:person) \
             LET k = p.id % 3 \
             RETURN k AS k, p.id AS id, count(*) AS n \
             GROUP BY k",
        )
        .expect_err("p.id differs inside a group");
    assert!(
        err.to_string().contains("read once per group"),
        "{err}, want the reason"
    );
}

/// A key the projection does not carry would group the rows and then
/// leave no column saying which group each row is, so it is refused
/// with what to do about it.
#[test]
fn a_key_that_is_not_projected_is_refused() {
    let mut fx = Fixture::open("group-hidden.zu1");
    let err = fx
        .conn
        .query("MATCH (p:person) LET k = p.id % 3 RETURN count(*) AS n GROUP BY k")
        .expect_err("the key is not a column");
    assert!(
        err.to_string().contains("project it as well"),
        "{err}, want what to do about it"
    );
}

/// The clause belongs to a projection rather than to a query, so a
/// `WITH` takes it the same way a `RETURN` does.
#[test]
fn with_takes_a_grouping_too() {
    let mut fx = Fixture::open("group-with.zu1");
    let groups = fx.groups(
        "MATCH (p:person) \
         LET k = p.id % 3 \
         WITH k AS k, count(*) AS n GROUP BY k \
         FILTER n > 2 \
         RETURN k AS k, n AS n",
    );
    assert_eq!(groups, [(0, 3), (1, 3), (2, 3)]);
}

/// The three group sizes every sort key below is about: four people in
/// group 0, three in group 1 and two in group 2, so the count orders
/// the groups one way and the key itself orders them the other.
const SIZED: &str = "MATCH (p:person) \
     LET k = CASE WHEN p.id < 4 THEN 0 WHEN p.id < 7 THEN 1 ELSE 2 END ";

/// GF20. A sort key of ISO 14.9 is an expression, so a set function may
/// stand in one, and what it means there is what it means in an item:
/// the clause groups by everything else it projects and the key is read
/// once per group. Nothing carries the count here, so the order is the
/// only sign it was worked out at all.
#[test]
fn a_sort_key_may_be_a_count_no_column_carries() {
    let mut fx = Fixture::open("order-by-count.zu1");
    let down = fx.keys(&format!("{SIZED} RETURN k AS k ORDER BY count(*) DESC"));
    assert_eq!(down, [0, 1, 2]);
    let up = fx.keys(&format!("{SIZED} RETURN k AS k ORDER BY count(*) ASC"));
    assert_eq!(up, [2, 1, 0]);
}

/// The key is the whole expression and not a call standing alone, so an
/// aggregate inside one is hoisted out of wherever it was written and
/// the arithmetic around it runs on the value the group answered.
#[test]
fn a_sort_key_may_do_arithmetic_on_an_aggregate() {
    let mut fx = Fixture::open("order-by-count-plus.zu1");
    let keys = fx.keys(&format!(
        "{SIZED} RETURN k AS k, count(*) AS n ORDER BY count(*) * -1 ASC"
    ));
    assert_eq!(keys, [0, 1, 2]);
}

/// A set function the projection already carries is accumulated once
/// and read twice, so the key becomes the column rather than a second
/// accumulator over the same rows.
#[test]
fn a_count_a_column_carries_is_accumulated_once() {
    let mut fx = Fixture::open("order-by-projected-count.zu1");
    let plan = fx
        .conn
        .explain(&format!(
            "{SIZED} RETURN k AS k, count(*) AS n ORDER BY count(*) DESC"
        ))
        .expect("plan");
    assert_eq!(plan.matches("count(*)").count(), 1, "{plan}");
    assert!(plan.contains("Sort n DESC"), "{plan}");
    let keys = fx.keys(&format!(
        "{SIZED} RETURN k AS k, count(*) AS n ORDER BY count(*) DESC"
    ));
    assert_eq!(keys, [0, 1, 2]);
}

/// The other way round: an aggregate no column carries is worked out
/// under a name of its own, which the listing gives both where the
/// grouping fills it and where the sort reads it.
#[test]
fn a_count_no_column_carries_is_named_in_the_plan() {
    let mut fx = Fixture::open("order-by-hidden-count.zu1");
    let plan = fx
        .conn
        .explain(&format!("{SIZED} RETURN k AS k ORDER BY count(*) DESC"))
        .expect("plan");
    let named = plan
        .lines()
        .find_map(|line| line.trim().strip_prefix("Aggregate count(*) AS "))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the grouping names what it accumulates");
    assert!(
        plan.contains(&format!("Sort {named} DESC")),
        "{plan}, want the sort reading {named}"
    );
}

/// A scalar function over a set function is a projection over the
/// groups, which is refused as an item and works as a sort key, because
/// a key is read after the grouping closes rather than being one of the
/// columns the grouping answers.
#[test]
fn a_sort_key_may_wrap_a_set_function_in_a_scalar_one() {
    let mut fx = Fixture::open("order-by-size-of-collect.zu1");
    let keys = fx.keys(&format!(
        "{SIZED} RETURN k AS k ORDER BY size(collect_list(p.id)) DESC"
    ));
    assert_eq!(keys, [0, 1, 2]);
}

/// Sorting by a set function makes the clause a grouping even where no
/// item is one, so a key that reads a name the grouping does not fix is
/// refused for the same reason an item would be. Without this the
/// answer would be sorted on whichever row of the group the engine
/// happened to be holding.
#[test]
fn a_name_the_grouping_does_not_fix_is_refused_in_a_sort_key() {
    let mut fx = Fixture::open("order-by-loose.zu1");
    let err = fx
        .conn
        .query(&format!(
            "{SIZED} RETURN k AS k ORDER BY count(*) DESC, p.id ASC"
        ))
        .expect_err("p.id differs inside a group");
    assert!(
        err.to_string().contains("read once per group"),
        "{err}, want the reason"
    );
}

/// The key belongs to a projection rather than to a query, so a `WITH`
/// sorts by an aggregate the way a `RETURN` does, and the clauses
/// behind it read the rows in that order.
#[test]
fn a_with_sorts_by_an_aggregate_too() {
    let mut fx = Fixture::open("order-by-count-with.zu1");
    let keys = fx.keys(&format!(
        "{SIZED} WITH k AS k ORDER BY count(*) DESC LIMIT 2 RETURN k AS k"
    ));
    assert_eq!(keys, [0, 1]);
}
