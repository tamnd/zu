//! The map reader and the two directions of the completeness check.
//!
//! Fixtures here are small enough to read in full, so a test that
//! fails says which behaviour changed rather than which of eight
//! hundred entities moved. What the check does against the real
//! surface is in `tests/ledger.rs`, which reads the committed files.

use xtask::apimap::{self, Map, Tier};

/// A ledger with two groups and one exception, which is enough shape
/// for every question about precedence and coverage.
fn ledger() -> Map {
    Map::parse(
        r#"
schema = 1
target = "rust"

[[group]]
prefix = "zu::engine"
tier = 3
reason = "storage internals"

[[group]]
prefix = "zu::engine::VERSION"
tier = 2

[[group]]
prefix = "zu::session"
tier = 1

[[entity]]
id = "zu::engine::Block"
tier = 2
"#,
    )
    .expect("the ledger parses")
}

fn ids(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn messages(problems: &[apimap::Problem]) -> Vec<String> {
    problems.iter().map(ToString::to_string).collect()
}

#[test]
fn a_group_covers_what_is_under_it_and_not_what_merely_starts_the_same() {
    let map = ledger();
    assert_eq!(map.tier_of("zu::session"), Some(Tier::Bound));
    assert_eq!(map.tier_of("zu::session::Session::run"), Some(Tier::Bound));
    // `zu::sessions` is a different path that happens to share nine
    // characters with this one.
    assert_eq!(map.tier_of("zu::sessions"), None);
    assert_eq!(map.tier_of("zu::session_pool"), None);
}

#[test]
fn the_narrower_group_wins_wherever_it_reaches() {
    let map = ledger();
    assert_eq!(map.tier_of("zu::engine::file"), Some(Tier::Internal));
    assert_eq!(map.tier_of("zu::engine::VERSION"), Some(Tier::Optional));
}

#[test]
fn an_entity_beats_every_group_over_it() {
    let map = ledger();
    assert_eq!(map.tier_of("zu::engine::Block"), Some(Tier::Optional));
    // And only the entity named, not its members.
    assert_eq!(map.tier_of("zu::engine::Block::len"), Some(Tier::Internal));
}

#[test]
fn a_public_name_nothing_covers_is_the_failure_this_exists_for() {
    let problems = apimap::check_surface(
        &ledger(),
        &ids(&[
            "zu::brandnew::Thing",
            "zu::engine::Block",
            "zu::engine::VERSION",
            "zu::engine::file::Header",
            "zu::session::Session",
        ]),
    );
    assert_eq!(problems.len(), 1, "{:#?}", messages(&problems));
    assert!(
        problems[0]
            .message
            .starts_with("zu::brandnew::Thing is public and nothing maps it"),
        "{}",
        problems[0]
    );
}

#[test]
fn a_group_that_stopped_matching_anything_is_a_decision_about_code_that_is_gone() {
    let problems = apimap::check_surface(&ledger(), &ids(&["zu::session::Session"]));
    let messages = messages(&problems);
    assert!(
        messages
            .iter()
            .any(|m| m.contains("nothing public is under zu::engine, so this group is dead")),
        "{messages:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("zu::engine::Block is not in the model")),
        "{messages:#?}"
    );
}

#[test]
fn a_ledger_that_covers_the_surface_exactly_has_nothing_to_say() {
    let problems = apimap::check_surface(
        &ledger(),
        &ids(&[
            "zu::engine::Block",
            "zu::engine::VERSION",
            "zu::engine::file::Header",
            "zu::session::Session",
        ]),
    );
    assert_eq!(messages(&problems), Vec::<String>::new());
}

#[test]
fn a_problem_that_points_at_a_line_says_which_one() {
    let problems = apimap::check_surface(&ledger(), &ids(&["zu::session::Session"]));
    assert!(
        problems.iter().all(|p| p.line.is_some()),
        "{:#?}",
        messages(&problems)
    );
    assert!(messages(&problems)[0].starts_with("line "));
}

#[test]
fn a_binding_owes_every_tier_one_entity_a_name() {
    let binding = Map::parse(
        r#"
schema = 1
target = "python"

[[entity]]
id = "zu::session::Session"
name = "zudb.Connection"
"#,
    )
    .expect("the binding map parses");
    let ids = ids(&["zu::session::Session", "zu::session::Session::run"]);
    let problems = apimap::check_binding(&binding, &ledger(), &ids);
    assert_eq!(
        messages(&problems),
        vec!["zu::session::Session::run is tier 1 and python does not name it"]
    );
}

#[test]
fn a_binding_owes_nothing_for_the_tiers_below_one() {
    let binding = Map::parse("schema = 1\ntarget = \"go\"\n").expect("parses");
    let ids = ids(&["zu::engine::Block", "zu::engine::file::Header"]);
    assert_eq!(
        messages(&apimap::check_binding(&binding, &ledger(), &ids)),
        Vec::<String>::new()
    );
}

#[test]
fn a_binding_naming_something_the_ledger_says_nobody_binds_is_a_disagreement() {
    let binding = Map::parse(
        r#"
schema = 1
target = "node"

[[entity]]
id = "zu::engine::file::Header"
name = "Header"

[[entity]]
id = "zu::gone::Away"
name = "Away"
"#,
    )
    .expect("parses");
    let ids = ids(&["zu::engine::file::Header"]);
    let messages = messages(&apimap::check_binding(&binding, &ledger(), &ids));
    assert!(
        messages[0].contains("zu::engine::file::Header is tier 3, and node names it"),
        "{messages:#?}"
    );
    assert!(
        messages[1].contains("zu::gone::Away is not in the model"),
        "{messages:#?}"
    );
}

#[test]
fn a_module_is_not_something_a_binding_can_be_asked_to_name() {
    let model = zu_json::parse(
        r#"{"entities": [
             {"id": "zu::session", "kind": "module", "name": "session"},
             {"id": "zu::session::Session", "kind": "struct", "name": "Session"}
           ]}"#,
    )
    .expect("the model parses");
    assert_eq!(
        apimap::mappable_ids(&model).expect("ids"),
        ids(&["zu::session::Session"])
    );
}

#[test]
fn the_tiers_are_counted_so_a_passing_check_still_says_something() {
    let census = ledger().census(&ids(&[
        "zu::session::Session",
        "zu::engine::Block",
        "zu::engine::VERSION",
        "zu::engine::file::Header",
    ]));
    assert_eq!(census.get(&Tier::Bound), Some(&1));
    assert_eq!(census.get(&Tier::Optional), Some(&2));
    assert_eq!(census.get(&Tier::Internal), Some(&1));
}

#[test]
fn a_tier_three_with_no_reason_is_the_one_entry_that_teaches_nobody_anything() {
    let err = Map::parse(
        "schema = 1\ntarget = \"rust\"\n\n[[group]]\nprefix = \"zu::engine\"\ntier = 3\n",
    )
    .expect_err("refused");
    assert!(err.contains("tier 3 with no `reason`"), "{err}");

    let err = Map::parse("schema = 1\ntarget = \"rust\"\n\n[[entity]]\nid = \"zu::X\"\ntier = 3\n")
        .expect_err("refused");
    assert!(err.contains("tier 3 with no `reason`"), "{err}");
}

#[test]
fn a_file_out_of_order_is_refused_because_nobody_reviews_a_diff_they_cannot_follow() {
    let err = Map::parse(
        "schema = 1\ntarget = \"rust\"\n\n[[group]]\nprefix = \"zu::b\"\ntier = 1\n\n[[group]]\nprefix = \"zu::a\"\ntier = 1\n",
    )
    .expect_err("refused");
    assert!(err.starts_with("line 8"), "{err}");
    assert!(err.contains("sorts before"), "{err}");
}

#[test]
fn the_same_name_twice_is_refused_rather_than_one_of_them_winning() {
    let err = Map::parse(
        "schema = 1\ntarget = \"rust\"\n\n[[entity]]\nid = \"zu::X\"\ntier = 1\n\n[[entity]]\nid = \"zu::X\"\ntier = 2\n",
    )
    .expect_err("refused");
    assert!(err.contains("zu::X appears twice"), "{err}");
}

#[test]
fn the_two_jobs_the_schema_does_are_kept_apart() {
    for (text, want) in [
        // A tier in a binding map would let two files disagree about
        // what a binding owes.
        (
            "schema = 1\ntarget = \"go\"\n\n[[entity]]\nid = \"zu::X\"\nname = \"X\"\ntier = 1\n",
            "`tier` only in the rust map",
        ),
        // A name in the ledger has nothing to name.
        (
            "schema = 1\ntarget = \"rust\"\n\n[[entity]]\nid = \"zu::X\"\ntier = 1\nname = \"X\"\n",
            "`name` only in a binding map",
        ),
        // A group in a binding map would be a name derived from a
        // path prefix, which is not a thing.
        (
            "schema = 1\ntarget = \"java\"\n\n[[group]]\nprefix = \"zu\"\ntier = 1\n",
            "`[[group]]` only in the rust map",
        ),
        (
            "schema = 1\ntarget = \"java\"\n\n[[entity]]\nid = \"zu::X\"\n",
            "`[[entity]]` with no `name`",
        ),
    ] {
        let err = Map::parse(text).expect_err("refused");
        assert!(err.contains(want), "wanted {want:?}, got {err:?}");
    }
}

#[test]
fn a_map_whose_shape_is_wrong_is_refused_before_anything_joins_against_it() {
    for (text, want) in [
        ("target = \"rust\"\n", "no `schema`"),
        ("schema = 2\ntarget = \"rust\"\n", "schema 2"),
        ("schema = 1\n", "no `target`"),
        ("schema = 1\ntarget = \"cobol\"\n", "is not one of"),
        (
            "schema = 1\ntarget = \"rust\"\nextra = 1\n",
            "no key \"extra\"",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[group]]\nprefix = \"zu\"\ntier = 4\nreason = \"x\"\n",
            "tier 4, and the tiers are 1, 2, 3",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[group]]\ntier = 1\n",
            "`[[group]]` with no `prefix`",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[entity]]\ntier = 1\n",
            "`[[entity]]` with no `id`",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[entity]]\nid = \"zu::X\"\n",
            "`[[entity]]` with no `tier`",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[entity]]\nid = \"zu::X\"\ntier = 1\nresaon = \"typo\"\n",
            "no key \"resaon\"",
        ),
        (
            "schema = 1\ntarget = \"rust\"\n\n[[entities]]\nid = \"zu::X\"\n",
            "no `[[entities]]` table",
        ),
    ] {
        let err = Map::parse(text).expect_err(&format!("{text:?} is refused"));
        assert!(err.contains(want), "wanted {want:?}, got {err:?}");
    }
}
