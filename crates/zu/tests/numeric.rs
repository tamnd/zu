//! GF01 over a stored graph, answered by both engines and compared.
//!
//! The five numeric functions that keep an exact argument exact are the
//! first of the library to become vector kernels, and the claim a kernel
//! makes is not that it is fast but that it answers what the row engine
//! answered. Nothing in a statement says which engine takes it, so the
//! two answering differently would be worse than either answer on its
//! own. Every case below is run twice, once with the pipeline engine and
//! once pinned to the row engine, and the two are compared.
//!
//! It is one test on purpose. Which engine runs is read from the
//! environment, the tests of a binary share one, and a second test
//! toggling it beside this one would be reading whichever value the
//! other had set.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 6;

/// The answers a statement gives, as the words a reader compares, and
/// the condition it raised where it has none.
fn answers(path: &std::path::Path, source: &str) -> String {
    let db = Database::open(path).expect("open");
    let mut conn = db.connect().expect("connect");
    match conn.query(source) {
        Err(err) => format!("raised {err}"),
        Ok(result) => result
            .iter()
            .map(|row| format!("{:?}", row.values()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[test]
fn the_numeric_functions_answer_the_same_on_both_engines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("numeric.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    let edges: Vec<(u32, u32)> = (0..NODES).map(|i| (i, (i + 1) % NODES)).collect();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
    drop(db);

    let cases = [
        // The exact argument, kept exact through all five.
        "MATCH (p:person) RETURN abs(p.id - 3) AS a ORDER BY a",
        "MATCH (p:person) RETURN sign(p.id - 3) AS a ORDER BY a",
        "MATCH (p:person) RETURN ceil(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN floor(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN round(p.id) AS a ORDER BY a",
        // The same functions over an approximate argument, where the
        // answers are approximate too, and the sign is not.
        "MATCH (p:person) RETURN abs(p.id * 1.5 - 4.0) AS a ORDER BY a",
        "MATCH (p:person) RETURN sign(p.id * 1.5 - 4.0) AS a ORDER BY a",
        "MATCH (p:person) RETURN ceil(p.id * 1.5) AS a ORDER BY a",
        "MATCH (p:person) RETURN floor(p.id * 1.5) AS a ORDER BY a",
        // Rounding to a written number of digits, inside the fraction
        // and left of the point.
        "MATCH (p:person) RETURN round(p.id * 1.5, 1) AS a ORDER BY a",
        "MATCH (p:person) RETURN round(p.id * 10, -2) AS a ORDER BY a",
        // In a filter, which is where a call used to take a whole scan
        // back to the row engine.
        "MATCH (p:person) WHERE abs(p.id - 3) < 2 RETURN p.id AS id ORDER BY id",
        "MATCH (p:person) WHERE sign(p.id - 3) > 0 RETURN p.id AS id ORDER BY id",
        // Behind a guard, which is the shape the row engine reads in
        // the order it was written.
        "MATCH (p:person) WHERE p.id > 3 AND abs(p.id - 3) > 1 RETURN p.id AS id ORDER BY id",
        // Nested in arithmetic and nested in each other.
        "MATCH (p:person) RETURN abs(p.id - 3) + floor(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN sign(abs(p.id - 3)) AS a ORDER BY a",
        // Grouped, where the call stands in the key.
        "MATCH (p:person) RETURN sign(p.id - 3) AS s, count(*) AS c ORDER BY s",
        // A condition, which both engines have to raise rather than
        // answer: the distance of the bottom integer from nought is one
        // past the top of one.
        "MATCH (p:person) RETURN abs(-9223372036854775807 - p.id) AS a ORDER BY a",
        // The approximate half, whose answers are floats whatever
        // arrived. A whole number in and a float out is the one shape
        // where the answer is wider than the argument.
        "MATCH (p:person) RETURN sqrt(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN exp(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN ln(p.id + 1) AS a ORDER BY a",
        "MATCH (p:person) RETURN log10(p.id + 1) AS a ORDER BY a",
        "MATCH (p:person) RETURN sin(p.id) AS a, cos(p.id) AS b ORDER BY a",
        "MATCH (p:person) RETURN tan(p.id * 0.5) AS a ORDER BY a",
        "MATCH (p:person) RETURN cot(p.id + 1) AS a ORDER BY a",
        "MATCH (p:person) RETURN asin(p.id * 0.2) AS a, acos(p.id * 0.2) AS b ORDER BY a",
        "MATCH (p:person) RETURN atan(p.id - 3) AS a ORDER BY a",
        "MATCH (p:person) RETURN degrees(p.id * 1.5) AS a, radians(p.id * 30) AS b ORDER BY a",
        // In a filter and behind a guard, which are the shapes the
        // functions are written in and the reason they are kernels.
        "MATCH (p:person) WHERE sqrt(p.id) > 1.5 RETURN p.id AS id ORDER BY id",
        "MATCH (p:person) WHERE p.id > 0 AND ln(p.id) < 1 RETURN p.id AS id ORDER BY id",
        // Nested, and the sum of a call, which is the shape a report
        // is written in.
        "MATCH (p:person) RETURN sqrt(abs(p.id - 3)) AS a ORDER BY a",
        "MATCH (p:person) RETURN sum(sqrt(p.id)) AS s",
        // The conditions of the approximate half, one of each kind: a
        // root below nought, a logarithm of nought, an inverse sine
        // outside minus one to one, and an exponential past the top of
        // a float.
        "MATCH (p:person) RETURN sqrt(p.id - 3) AS a ORDER BY a",
        "MATCH (p:person) RETURN ln(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN log10(p.id - 1) AS a ORDER BY a",
        "MATCH (p:person) RETURN asin(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN acos(p.id) AS a ORDER BY a",
        "MATCH (p:person) RETURN exp(p.id * 1000) AS a ORDER BY a",
        "MATCH (p:person) RETURN cot(p.id) AS a ORDER BY a",
    ];

    for source in cases {
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let pipeline = answers(&path, source);
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let rows = answers(&path, source);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        assert_eq!(pipeline, rows, "the two engines differ on {source}");
        assert!(!pipeline.is_empty(), "{source} answered nothing at all");
    }
}
