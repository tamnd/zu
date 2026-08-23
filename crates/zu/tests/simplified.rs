//! The simplified path pattern of ISO 16.12, features G039 and G080 to
//! G082: a stretch of edges written between one pair of slashes, with
//! the nodes between them left unwritten.
//!
//! It is a way of writing a pattern rather than a second kind of
//! pattern, so what these tests ask is that it answers what the long
//! form answers: `-/ knows knows /->` and `-[:knows]->()-[:knows]->` are
//! one question, and so are the seven arrows around the slashes and the
//! seven an ordinary step writes. The rest is what the slashes may hold,
//! which is read against the one fact that an edge here is kept under
//! exactly one type: the bar, the ampersand and the exclamation mark are
//! a set of types and the complement of one, and what those two cannot
//! hold is refused by name.

use zu::Database;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;

const NODES: u32 = 5;

/// A line with a shortcut: 0 to 1 to 2 to 3 to 4, and 0 straight to 2,
/// so paths of two hops and paths of one hop reach some of the same
/// nodes and a count that read a length wrongly shows up as a count.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = vec![(0, 1), (1, 2), (2, 3), (3, 4), (0, 2)];
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", NODES.into(), &edges).expect("load");
}

/// [`seeded`] with a second edge type over the same nodes, so that a
/// step which excludes one type has another one left to walk.
fn seeded_with_a_second_type(path: &std::path::Path) {
    seeded(path);
    let mut db = Zu1File::open(path).expect("open");
    bulk_load_as(&mut db, "person", "likes", NODES.into(), &[(0, 4)]).expect("load");
}

struct Fixture {
    _dir: tempfile::TempDir,
    conn: zu::Connection,
}

impl Fixture {
    fn open(name: &str) -> Fixture {
        Fixture::seeded_by(name, seeded)
    }

    fn seeded_by(name: &str, seed: fn(&std::path::Path)) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        seed(&path);
        let db = Database::open(&path).expect("open");
        let conn = db.connect().expect("connect");
        Fixture { _dir: dir, conn }
    }

    /// How many paths the stretch matches between two person nodes.
    fn walks(&mut self, stretch: &str) -> i64 {
        let source = format!("MATCH (a:person){stretch}(b:person) RETURN count(*) AS n");
        let rows = self.conn.query(&source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("n").expect("n"))
            .next()
            .expect("a count answers one row")
    }

    /// The ids the far end was bound to, one entry per path, so a
    /// stretch that reaches a node twice says so.
    fn reached(&mut self, stretch: &str) -> Vec<i64> {
        let source =
            format!("MATCH (a:person {{id: 0}}){stretch}(b:person) RETURN b.id AS id ORDER BY id");
        let rows = self.conn.query(&source).expect("query");
        rows.iter()
            .map(|row| row.get_by_name::<i64>("id").expect("id"))
            .collect()
    }

    fn refused(&mut self, stretch: &str) -> String {
        let source = format!("MATCH (a:person){stretch}(b:person) RETURN count(*) AS n");
        self.conn
            .query(&source)
            .expect_err("this one does not run")
            .to_string()
    }
}

/// The short form against the long form of the same walk. A label is a
/// step, two labels written against each other are two steps with a
/// node nobody named between them, and brackets group.
#[test]
fn a_simplified_stretch_answers_what_the_long_form_answers() {
    let mut fx = Fixture::open("simplified-parity.zu1");
    for (short, long) in [
        ("-/ knows /->", "-[:knows]->"),
        ("-/ (knows) /->", "-[:knows]->"),
        ("-/ knows knows /->", "-[:knows]->()-[:knows]->"),
        ("-/ knows{2} /->", "-[:knows]->{2}"),
        ("-/ knows+ /->", "-[:knows]->+"),
        ("-/ knows{1,2} /->", "-[:knows]->{1,2}"),
        ("-/ knows? /->", "(()-[:knows]->())?"),
    ] {
        let one = fx.walks(short);
        let other = fx.walks(long);
        assert_eq!(one, other, "{short} against {long}");
        assert!(one > 0, "{short} answered nothing");
    }
}

/// What those counts are, so the parity above is parity with something
/// right rather than with the same mistake twice.
#[test]
fn a_simplified_stretch_counts_the_paths_it_walks() {
    let mut fx = Fixture::open("simplified-counts.zu1");
    assert_eq!(fx.walks("-/ knows /->"), 5);
    assert_eq!(fx.walks("-/ knows knows /->"), 4);
    // Five walks of one hop, four of two, three of three and one of
    // four, the trail mode leaving nothing longer.
    assert_eq!(fx.walks("-/ knows+ /->"), 13);
    // Five nodes standing on themselves, then the five one hop walks.
    assert_eq!(fx.walks("-/ knows? /->"), 10);
    assert_eq!(fx.reached("-/ knows knows /->"), [2, 3]);
}

/// The arrow around the slashes says which way every step of the
/// stretch goes, and there are the seven an ordinary arrow writes.
#[test]
fn the_seven_arrows_say_what_the_seven_ordinary_ones_say() {
    let mut fx = Fixture::open("simplified-arrows.zu1");
    for (short, long) in [
        ("-/ knows /->", "-[:knows]->"),
        ("<-/ knows /-", "<-[:knows]-"),
        ("<-/ knows /->", "<-[:knows]->"),
        ("-/ knows /-", "-[:knows]-"),
        ("~/ knows /~", "~[:knows]~"),
        ("<~/ knows /~", "<~[:knows]~"),
        ("~/ knows /~>", "~[:knows]~>"),
    ] {
        assert_eq!(fx.walks(short), fx.walks(long), "{short} against {long}");
    }
}

/// A step may write a direction of its own, in front of its label or
/// behind it, and it wins over the arrow around the slashes. That is
/// what makes the arrow a default rather than a rule.
#[test]
fn a_step_may_override_the_direction_the_arrow_set() {
    let mut fx = Fixture::open("simplified-override.zu1");
    // The arrow walks either way, and the override narrows it.
    assert_eq!(fx.walks("-/ knows /-"), 10);
    assert_eq!(fx.walks("-/ knows> /-"), 5);
    assert_eq!(fx.walks("-/ <knows /-"), 5);
    // A group takes the override wherever its steps wrote none.
    assert_eq!(fx.walks("-/ (knows knows)> /-"), 4);
}

/// A bar inside the slashes is either label on the one step, which is
/// the label union an ordinary step already writes, so a label written
/// on both sides of it is the one label and not two walks of it.
#[test]
fn a_bar_inside_the_slashes_is_either_label_on_one_step() {
    let mut fx = Fixture::open("simplified-bar.zu1");
    assert_eq!(fx.walks("-/ knows|knows /->"), fx.walks("-[:knows]->"));
}

/// An ampersand asks the one label of the step to be several things at
/// once. An edge is kept under exactly one type here, so the only
/// conjunction with an answer is the one that names the same type
/// twice, and that answer is the type.
#[test]
fn an_ampersand_inside_the_slashes_asks_the_one_label_twice() {
    let mut fx = Fixture::open("simplified-conjunction.zu1");
    assert_eq!(fx.walks("-/ knows & knows /->"), fx.walks("-[:knows]->"));
    assert_eq!(fx.walks("-/ (knows|knows) & knows /->"), 5);
}

/// An exclamation mark says the step is not of some type, and an edge
/// kept under one type is of every other type instead, so the step
/// walks all the types the mark did not name.
#[test]
fn an_exclamation_mark_walks_every_other_type() {
    let mut fx = Fixture::seeded_by("simplified-negation.zu1", seeded_with_a_second_type);
    assert_eq!(fx.walks("-/ !knows /->"), fx.walks("-[:likes]->"));
    assert_eq!(fx.walks("-/ !likes /->"), fx.walks("-[:knows]->"));
    // One edge of the second type, from 0 to 4, so a step that keeps
    // out the first one reaches the far end of the line in a hop.
    assert_eq!(fx.reached("-/ !knows /->"), [4]);
    // The mark and the bar read together: neither type is nothing.
    assert_eq!(fx.walks("-/ !knows & !likes /->"), 0);
}

/// A dash in front of a step is the seventh override: it says the step
/// may go either way, whatever the arrow around the slashes said.
#[test]
fn a_dash_in_front_of_a_step_says_it_goes_either_way() {
    let mut fx = Fixture::open("simplified-any-direction.zu1");
    assert_eq!(fx.walks("-/ -knows /->"), fx.walks("-[:knows]-"));
    assert_eq!(fx.walks("-/ -knows /->"), 10);
    // The other steps of the stretch keep what the arrow gave them, so
    // only the step the dash stands in front of goes either way.
    assert_eq!(
        fx.walks("-/ -knows knows /->"),
        fx.walks("-[:knows]-()-[:knows]->")
    );
}

/// An edge is kept under exactly one type here, so the parts of the
/// standard's label expression that ask about more than that are
/// refused by name and pointed at what to write instead.
#[test]
fn what_a_simplified_stretch_refuses() {
    let mut fx = Fixture::open("simplified-refusals.zu1");
    for stretch in [
        "-/ % /->",
        "-/ knows&likes /->",
        "-/ knows & !knows /->",
        "-/ !(knows|!knows) /->",
    ] {
        let err = fx.refused(stretch);
        assert!(err.contains("stored under one type"), "{stretch}: {err}");
    }
    // Two types on the one step name themselves in the message; a type
    // asked for and excluded at once is the other way to ask for
    // nothing, and it says so in its own words.
    assert!(
        fx.refused("-/ knows&likes /->")
            .contains("this step names 2")
    );
    let err = fx.refused("-/ knows & !knows /->");
    assert!(err.contains("excludes in the same breath"), "{err}");
    // An ampersand joins the labels of one step, so a walk on either
    // side of it is not something it can join.
    let err = fx.refused("-/ (knows knows) & knows /->");
    assert!(err.contains("a label and not a walk"), "{err}");
    // The dash and the arrowhead answer the same question twice.
    let err = fx.refused("-/ -knows> /->");
    assert!(
        err.contains("a second answer to the same question"),
        "{err}"
    );
    // A bar between walks of different shapes is an alternation of
    // paths, which the bar between two whole patterns already says.
    let err = fx.refused("-/ knows knows | knows /->");
    assert!(err.contains("either label on the one step"), "{err}");
    let err = fx.refused("-/ knows |+| knows /->");
    assert!(err.contains("multiset alternation"), "{err}");
    // A count with no ceiling over more than one step is lengths with
    // no end to the list of them, the same refusal a quantifier behind
    // brackets gets.
    for stretch in ["-/ (knows knows)+ /->", "-/ knows* /->"] {
        let err = fx.refused(stretch);
        assert!(
            err.contains("write a ceiling on the count"),
            "{stretch}: {err}"
        );
    }
    // Both ends say the same thing about the edge or neither does.
    let err = fx.refused("-/ knows /~>");
    assert!(err.contains("undirected at both ends"), "{err}");
}
