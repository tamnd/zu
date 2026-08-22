//! What the normalizer does to rustdoc's output, checked against
//! documents small enough to read.
//!
//! Each fixture here is the smallest rustdoc-shaped document that shows
//! one behaviour: a re-export crossing a crate boundary, a glob, an
//! item public at two paths, a blanket impl that must not appear. The
//! real thing is 325 KB and proves nothing to a reader.

use xtask::model;
use xtask::rustdoc::{CrateDoc, FORMAT_VERSION};
use xtask::scratch::Scratch;

/// Builds a rustdoc document out of an id-to-item map and a path table,
/// so a fixture below is just the items it is about.
fn doc(name: &str, root: i64, index: &str, paths: &str, externals: &str) -> CrateDoc {
    let text = format!(
        r#"{{"root":{root},"crate_version":"0.0.1","format_version":{FORMAT_VERSION},
            "index":{{{index}}},"paths":{{{paths}}},"external_crates":{{{externals}}}}}"#
    );
    CrateDoc {
        name: name.to_string(),
        doc: zu_json::parse(&text).unwrap_or_else(|e| panic!("fixture {name}: {e}")),
    }
}

fn item(id: i64, name: &str, visibility: &str, inner: &str) -> String {
    let name = if name.is_empty() {
        "null".to_string()
    } else {
        format!("\"{name}\"")
    };
    format!(
        r#""{id}":{{"id":{id},"crate_id":0,"name":{name},"visibility":"{visibility}",
           "docs":null,"deprecation":null,"inner":{inner}}}"#
    )
}

fn module(items: &[i64]) -> String {
    let ids: Vec<String> = items.iter().map(i64::to_string).collect();
    format!(
        r#"{{"module":{{"is_crate":false,"items":[{}]}}}}"#,
        ids.join(",")
    )
}

fn strukt(impls: &[i64]) -> String {
    let ids: Vec<String> = impls.iter().map(i64::to_string).collect();
    format!(
        r#"{{"struct":{{"kind":{{"plain":{{"fields":[]}}}},"impls":[{}]}}}}"#,
        ids.join(",")
    )
}

fn path(id: i64, crate_id: i64, segments: &[&str], kind: &str) -> String {
    let quoted: Vec<String> = segments.iter().map(|s| format!("\"{s}\"")).collect();
    format!(
        r#""{id}":{{"crate_id":{crate_id},"path":[{}],"kind":"{kind}"}}"#,
        quoted.join(",")
    )
}

fn ids(model: &model::Model) -> Vec<&str> {
    model.entities.iter().map(|e| e.id.as_str()).collect()
}

#[test]
fn a_module_of_types_comes_out_as_types_under_their_module() {
    let index = [
        item(1, "root", "public", &module(&[2])),
        item(2, "inner", "public", &module(&[3])),
        item(3, "Thing", "public", &strukt(&[])),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["root"], "module"),
        path(3, 0, &["root", "inner", "Thing"], "struct"),
    ]
    .join(",");
    let built = model::build(&[doc("root", 1, &index, &paths, "")], "root").expect("builds");
    assert_eq!(ids(&built), ["root::inner", "root::inner::Thing"]);
    // The crate root is what the model is about, not a member of it.
    assert!(!ids(&built).contains(&"root"));
}

#[test]
fn a_private_item_next_to_a_public_one_stays_out() {
    let index = [
        item(1, "root", "public", &module(&[2, 3])),
        item(2, "Public", "public", &strukt(&[])),
        item(3, "Private", "default", &strukt(&[])),
    ]
    .join(",");
    let built = model::build(
        &[doc("root", 1, &index, &path(1, 0, &["root"], "module"), "")],
        "root",
    )
    .expect("builds");
    assert_eq!(ids(&built), ["root::Public"]);
}

#[test]
fn inherent_methods_hang_off_their_type_and_blanket_impls_do_not() {
    // Three impls on one struct: the inherent block a binding has to
    // bind, a trait impl, and a blanket one. Only the first is API.
    let inherent =
        r#"{"impl":{"trait":null,"is_synthetic":false,"blanket_impl":null,"items":[5]}}"#;
    let traited = r#"{"impl":{"trait":{"path":"Display"},"is_synthetic":false,"blanket_impl":null,"items":[6]}}"#;
    let blanket = r#"{"impl":{"trait":{"path":"Into"},"is_synthetic":false,"blanket_impl":{"generic":"T"},"items":[7]}}"#;
    let sig = r#"{"function":{"sig":{"inputs":[["self",{"borrowed_ref":{"lifetime":null,"is_mutable":false,"type":{"generic":"Self"}}}]],"output":null,"is_c_variadic":false},"generics":{"params":[]},"header":{"is_const":false,"is_unsafe":false,"is_async":false,"abi":"Rust"}}}"#;
    let index = [
        item(1, "root", "public", &module(&[2])),
        item(2, "Thing", "public", &strukt(&[3, 4, 8])),
        item(3, "", "default", inherent),
        item(4, "", "default", traited),
        item(8, "", "default", blanket),
        item(5, "mine", "public", sig),
        item(6, "fmt", "public", sig),
        item(7, "into", "public", sig),
    ]
    .join(",");
    let built = model::build(
        &[doc("root", 1, &index, &path(1, 0, &["root"], "module"), "")],
        "root",
    )
    .expect("builds");
    assert_eq!(ids(&built), ["root::Thing", "root::Thing::mine"]);
    let method = &built.entities[1];
    assert_eq!(method.of.as_deref(), Some("root::Thing"));
    assert_eq!(method.signature.as_deref(), Some("fn mine(&self)"));
}

#[test]
fn everything_that_has_a_type_carries_it() {
    // A field, a constant, or an alias with a name and no type is not
    // something a header generator can emit, so none of them are
    // allowed to come out bare.
    let string = r#"{"resolved_path":{"path":"String","id":9,"args":null}}"#;
    let index = [
        item(1, "root", "public", &module(&[2, 5, 6, 7])),
        item(
            2,
            "Thing",
            "public",
            r#"{"struct":{"kind":{"plain":{"fields":[3,4]}},"impls":[]}}"#,
        ),
        item(
            3,
            "name",
            "public",
            &format!(r#"{{"struct_field":{string}}}"#),
        ),
        item(
            4,
            "size",
            "public",
            r#"{"struct_field":{"primitive":"u64"}}"#,
        ),
        item(
            5,
            "LIMIT",
            "public",
            r#"{"constant":{"type":{"primitive":"u32"},"const":{"expr":"64"}}}"#,
        ),
        item(
            6,
            "Name",
            "public",
            &format!(r#"{{"type_alias":{{"type":{string},"generics":{{"params":[]}}}}}}"#),
        ),
        item(
            7,
            "Pair",
            "public",
            r#"{"struct":{"kind":{"tuple":[9,null]},"impls":[]}}"#,
        ),
        item(9, "0", "public", r#"{"struct_field":{"primitive":"i64"}}"#),
    ]
    .join(",");
    let built = model::build(
        &[doc("root", 1, &index, &path(1, 0, &["root"], "module"), "")],
        "root",
    )
    .expect("builds");
    let by_id: Vec<(&str, Option<&str>)> = built
        .entities
        .iter()
        .map(|e| (e.id.as_str(), e.signature.as_deref()))
        .collect();
    assert_eq!(
        by_id,
        [
            ("root::LIMIT", Some("u32")),
            ("root::Name", Some("String")),
            // A tuple struct's position is as public as a named field
            // and rustdoc writes null where one is private.
            ("root::Pair", None),
            ("root::Pair::0", Some("i64")),
            ("root::Thing", None),
            ("root::Thing::name", Some("String")),
            ("root::Thing::size", Some("u64")),
        ]
    );
}

#[test]
fn a_variant_says_what_it_carries() {
    // The tagged union is the shape every binding has the most trouble
    // with, and the payload is the part of it a name cannot carry.
    let index = [
        item(1, "root", "public", &module(&[2])),
        item(2, "E", "public", r#"{"enum":{"variants":[3,4,5],"impls":[]}}"#),
        item(3, "Empty", "public", r#"{"variant":{"kind":"plain","discriminant":null}}"#),
        item(4, "One", "public", r#"{"variant":{"kind":{"tuple":[6]},"discriminant":null}}"#),
        item(
            5,
            "Named",
            "public",
            r#"{"variant":{"kind":{"struct":{"fields":[7,8],"has_stripped_fields":false}},"discriminant":null}}"#,
        ),
        item(6, "0", "public", r#"{"struct_field":{"primitive":"u32"}}"#),
        item(7, "what", "public", r#"{"struct_field":{"borrowed_ref":{"lifetime":"'a","is_mutable":false,"type":{"primitive":"str"}}}}"#),
        item(8, "code", "public", r#"{"struct_field":{"primitive":"i32"}}"#),
    ]
    .join(",");
    let built = model::build(
        &[doc("root", 1, &index, &path(1, 0, &["root"], "module"), "")],
        "root",
    )
    .expect("builds");
    let shapes: Vec<Option<&str>> = built
        .entities
        .iter()
        .filter(|e| e.kind == "variant")
        .map(|e| e.signature.as_deref())
        .collect();
    assert_eq!(
        shapes,
        [
            Some("Empty"),
            Some("Named { what: &str, code: i32 }"),
            Some("One(u32)"),
        ]
    );
}

#[test]
fn an_impl_trait_argument_is_written_once_and_keeps_its_arguments() {
    // rustdoc reports `detail: impl Into<String>` twice: once as the
    // argument and once as a synthetic generic parameter it names
    // `impl Into<String>`. Printing both gives
    // `fn new<impl Into<String>>(detail: impl Into)`, which is neither
    // what anyone wrote nor anything that compiles.
    let bound = r#"{"trait_bound":{"trait":{"path":"Into","id":21,"args":{"angle_bracketed":{"args":[{"type":{"resolved_path":{"path":"String","id":9,"args":null}}}],"constraints":[]}}},"generic_params":[],"modifier":"none"}}"#;
    let func = format!(
        r#"{{"function":{{"sig":{{"inputs":[["detail",{{"impl_trait":[{bound}]}}]],"output":{{"generic":"Self"}},"is_c_variadic":false}},
           "generics":{{"params":[{{"name":"impl Into<String>","kind":{{"type":{{"bounds":[{bound}],"default":null,"is_synthetic":true}}}}}},
                                  {{"name":"T","kind":{{"type":{{"bounds":[],"default":null,"is_synthetic":false}}}}}}]}},
           "header":{{"is_const":false,"is_unsafe":false,"is_async":false,"abi":"Rust"}}}}}}"#
    );
    let index = [
        item(1, "root", "public", &module(&[2])),
        item(2, "make", "public", &func),
    ]
    .join(",");
    let built = model::build(
        &[doc("root", 1, &index, &path(1, 0, &["root"], "module"), "")],
        "root",
    )
    .expect("builds");
    assert_eq!(
        built.entities[0].signature.as_deref(),
        Some("fn make<T>(detail: impl Into<String>) -> Self")
    );
}

/// The fixture pair used by the re-export tests: a crate `home` that
/// re-exports out of a crate `away`.
fn two_crates() -> [CrateDoc; 2] {
    let away_index = [
        item(1, "away", "public", &module(&[2])),
        item(2, "deep", "public", &module(&[3, 4])),
        item(3, "Thing", "public", &strukt(&[])),
        item(4, "Other", "public", &strukt(&[])),
    ]
    .join(",");
    let away_paths = [
        path(1, 0, &["away"], "module"),
        path(2, 0, &["away", "deep"], "module"),
        path(3, 0, &["away", "deep", "Thing"], "struct"),
        path(4, 0, &["away", "deep", "Other"], "struct"),
    ]
    .join(",");
    [
        doc("home", 1, "", "", ""),
        doc("away", 1, &away_index, &away_paths, ""),
    ]
}

#[test]
fn a_reexport_is_filed_under_the_path_a_user_types() {
    // `pub use away::deep::Thing;` in crate home. rustdoc leaves the
    // target out of home's index, because it is not home's item.
    let index = [
        item(1, "home", "public", &module(&[2])),
        item(
            2,
            "",
            "public",
            r#"{"use":{"source":"away::deep::Thing","name":"Thing","id":90,"is_glob":false}}"#,
        ),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["home"], "module"),
        path(90, 7, &["away", "deep", "Thing"], "struct"),
    ]
    .join(",");
    let [_, away] = two_crates();
    let built = model::build(
        &[
            doc("home", 1, &index, &paths, r#""7":{"name":"away"}"#),
            away,
        ],
        "home",
    )
    .expect("builds");
    assert_eq!(ids(&built), ["home::Thing"]);
    // The public name is the id; where it lives is recorded, not used
    // as the name, because a binding maps what a user writes.
    assert_eq!(
        built.entities[0].source.as_deref(),
        Some("away::deep::Thing")
    );
    assert_eq!(built.entities[0].kind, "struct");
}

#[test]
fn a_glob_reexport_splices_the_module_in_rather_than_adding_a_level() {
    let index = [
        item(1, "home", "public", &module(&[2])),
        item(
            2,
            "",
            "public",
            r#"{"use":{"source":"away::deep","name":"deep","id":91,"is_glob":true}}"#,
        ),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["home"], "module"),
        path(91, 7, &["away", "deep"], "module"),
    ]
    .join(",");
    let [_, away] = two_crates();
    let built = model::build(
        &[
            doc("home", 1, &index, &paths, r#""7":{"name":"away"}"#),
            away,
        ],
        "home",
    )
    .expect("builds");
    // `pub use away::deep::*` puts Thing at home::Thing, not
    // home::deep::Thing.
    assert_eq!(ids(&built), ["home::Other", "home::Thing"]);
}

#[test]
fn an_item_public_at_two_paths_appears_at_both() {
    // The regression this exists for: the walk used to remember items
    // rather than paths, so whichever public name it reached second
    // vanished from the model and no binding could have mapped it.
    let index = [
        item(1, "home", "public", &module(&[2, 3])),
        item(
            2,
            "",
            "public",
            r#"{"use":{"source":"away::deep","name":"deep","id":91,"is_glob":false}}"#,
        ),
        item(
            3,
            "",
            "public",
            r#"{"use":{"source":"away::deep::Thing","name":"Thing","id":90,"is_glob":false}}"#,
        ),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["home"], "module"),
        path(90, 7, &["away", "deep", "Thing"], "struct"),
        path(91, 7, &["away", "deep"], "module"),
    ]
    .join(",");
    let [_, away] = two_crates();
    let built = model::build(
        &[
            doc("home", 1, &index, &paths, r#""7":{"name":"away"}"#),
            away,
        ],
        "home",
    )
    .expect("builds");
    assert_eq!(
        ids(&built),
        [
            "home::Thing",
            "home::deep",
            "home::deep::Other",
            "home::deep::Thing",
        ]
    );
}

#[test]
fn a_reexport_from_a_crate_we_were_not_given_is_recorded_not_dropped() {
    let index = [
        item(1, "home", "public", &module(&[2])),
        item(
            2,
            "",
            "public",
            r#"{"use":{"source":"stranger::Thing","name":"Thing","id":90,"is_glob":false}}"#,
        ),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["home"], "module"),
        path(90, 7, &["stranger", "Thing"], "struct"),
    ]
    .join(",");
    let built = model::build(
        &[doc("home", 1, &index, &paths, r#""7":{"name":"stranger"}"#)],
        "home",
    )
    .expect("builds");
    // A public name with nothing behind it is a bug in the generator's
    // crate list, and a gap nobody notices is the worse failure.
    assert_eq!(ids(&built), ["home::Thing"]);
    assert_eq!(built.entities[0].kind, "unresolved");
    assert_eq!(built.entities[0].source.as_deref(), Some("stranger::Thing"));
}

#[test]
fn a_reexport_cycle_terminates() {
    // `pub use self::inner` inside `inner`, which rustdoc will happily
    // emit and which used to be why the walk needed a visited set.
    let index = [
        item(1, "home", "public", &module(&[2])),
        item(2, "inner", "public", &module(&[3])),
        item(
            3,
            "",
            "public",
            r#"{"use":{"source":"home::inner","name":"inner","id":2,"is_glob":false}}"#,
        ),
    ]
    .join(",");
    let paths = [
        path(1, 0, &["home"], "module"),
        path(2, 0, &["home", "inner"], "module"),
    ]
    .join(",");
    let built = model::build(&[doc("home", 1, &index, &paths, "")], "home").expect("builds");
    assert!(
        built.entities.len() < 64,
        "the walk did not terminate: {} entities",
        built.entities.len()
    );
    assert!(ids(&built).contains(&"home::inner"));
}

#[test]
fn entities_come_out_sorted_and_the_bytes_do_not_move() {
    let docs = [xtask::fixture::crate_doc("zu", 3, 3, 3)];
    let built = model::build(&docs, "zu").expect("builds");
    let sorted: Vec<&str> = {
        let mut v = ids(&built);
        v.sort_unstable();
        v
    };
    assert_eq!(ids(&built), sorted, "entities are not in identifier order");
    let once = built.to_json().to_pretty();
    for _ in 0..4 {
        let again = model::build(&docs, "zu")
            .expect("builds")
            .to_json()
            .to_pretty();
        assert_eq!(again, once, "two runs, two files");
    }
    assert!(
        once.ends_with("}\n"),
        "the artifact has no trailing newline"
    );
}

#[test]
fn a_rustdoc_format_we_do_not_know_is_refused_rather_than_half_read() {
    let dir = Scratch::new("xtask-format");
    let path = dir.join("future.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"root":1,"format_version":{},"index":{{}},"paths":{{}}}}"#,
            FORMAT_VERSION + 1
        ),
    )
    .expect("write");
    let err = xtask::rustdoc::read(&path, "future").expect_err("a newer format is not readable");
    assert!(err.contains("FORMAT_VERSION"), "unhelpful message: {err}");
    std::fs::remove_file(&path).ok();
}
