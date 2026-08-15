//! The committed `api-map.toml` against the committed `model.json`.
//!
//! This is the gate the milestone asks for: a pull request that adds a
//! public symbol and does not say what a binding owes it fails here.
//! It is a test rather than only a CI job because it reads two
//! committed files and needs no toolchain, so it runs on every
//! platform in the existing test matrix and, more to the point, on the
//! machine of whoever added the symbol.
//!
//! Adding a symbol takes two steps in this order, and the failures say
//! so: `cargo xtask model` to put it in the model, then an entry in
//! the map to say what it is for.

use std::collections::BTreeSet;

use xtask::apimap::{self, Map, Tier};

fn ledger() -> Map {
    let text = include_str!("../../../docs/api/api-map.toml");
    Map::parse(text).expect("docs/api/api-map.toml parses")
}

fn ids() -> Vec<String> {
    let text = include_str!("../../../docs/api/model.json");
    let model = zu_json::parse(text).expect("docs/api/model.json parses");
    apimap::mappable_ids(&model).expect("the model has entities with identifiers")
}

#[test]
fn every_public_name_is_classified_and_every_classification_still_names_something() {
    let problems = apimap::check_surface(&ledger(), &ids());
    assert!(
        problems.is_empty(),
        "docs/api/api-map.toml and docs/api/model.json disagree:\n{}",
        problems
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_ledger_is_the_one_that_classifies() {
    assert_eq!(ledger().target, apimap::LEDGER);
}

#[test]
fn nothing_is_at_tier_three_without_saying_why() {
    // `Map::parse` refuses this, so what this test really asserts is
    // that the rule is still the rule. It is cheap and it is the one
    // property of the file a reviewer is most likely to want to trust
    // without rereading the parser.
    let ledger = ledger();
    for group in &ledger.groups {
        assert!(
            group.tier != Tier::Internal || group.reason.is_some(),
            "{} is tier 3 with no reason",
            group.prefix
        );
    }
    for entry in &ledger.entries {
        assert!(
            entry.tier != Some(Tier::Internal) || entry.reason.is_some(),
            "{} is tier 3 with no reason",
            entry.id
        );
    }
}

#[test]
fn the_surface_a_binding_owes_is_the_one_a_user_touches() {
    let ledger = ledger();
    let bound: BTreeSet<String> = ids()
        .into_iter()
        .filter(|id| ledger.tier_of(id) == Some(Tier::Bound))
        .collect();

    // A session, values in and out, results, and the error model. If
    // any of these stopped being tier 1, a binding could drop it and
    // the release would still pass, which is the failure this whole
    // file exists to prevent.
    for id in [
        "zu::session::Session",
        "zu::session::Session::open",
        "zu::session::Session::run",
        "zu::session::Session::prepare",
        "zu::session::Session::execute",
        "zu::query::Value",
        "zu::query::QueryResult",
        "zu::query::QueryResult::rows",
        "zu::ZuError",
        "zu::GqlStatus",
        "zu::Severity",
        "zu::DiagnosticRecord",
        "zu::NodeId",
    ] {
        assert!(bound.contains(id), "{id} is no longer tier 1");
    }

    // And the engine's own machinery is not. A tier 1 here would put
    // the zu1 block layout in six languages.
    for id in [
        "zu::zu1::file::Zu1File",
        "zu::zu1::segment::Segment",
        "zu::snapshot::Zu1Snapshot",
        "zu::GraphStore",
    ] {
        assert!(!bound.contains(id), "{id} became tier 1");
    }
}

#[test]
fn a_binding_map_written_against_this_ledger_is_checked_the_other_way() {
    // The nine repositories have no maps yet, so what this asserts is
    // that the release direction works against the real ledger and not
    // only against a fixture: a map naming one tier-1 entity is short
    // by the rest, and naming a tier-3 entity is a disagreement.
    let binding = Map::parse(
        "schema = 1\ntarget = \"python\"\n\n\
         [[entity]]\nid = \"zu::session::Session\"\nname = \"zudb.Connection\"\n\n\
         [[entity]]\nid = \"zu::zu1::file::Zu1File\"\nname = \"zudb.File\"\n",
    )
    .expect("the binding map parses");
    let problems = apimap::check_binding(&binding, &ledger(), &ids());
    let text: Vec<String> = problems.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|p| p.contains("zu::zu1::file::Zu1File is tier 3, and python names it")),
        "{text:#?}"
    );
    assert!(
        text.iter()
            .any(|p| p == "zu::session::Session::run is tier 1 and python does not name it"),
        "{text:#?}"
    );
    assert!(
        !text
            .iter()
            .any(|p| p.contains("zu::session::Session is tier 1")),
        "the one entity it did name is reported anyway:\n{text:#?}"
    );
}
