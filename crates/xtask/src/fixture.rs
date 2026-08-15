//! Rustdoc-shaped documents built to order, for the tests and the
//! bench.
//!
//! The precise behaviour tests read hand-written JSON, because a test
//! that asserts what happens to a glob re-export should show the reader
//! the glob. This module is for the other half: a document of a chosen
//! size, so the bench has a deterministic input and the tests can check
//! that a thousand types come out as a thousand types.

use zu_json::Json;

use crate::rustdoc::{CrateDoc, FORMAT_VERSION};

/// A crate with `modules` public modules, each holding `types` structs,
/// each with `methods` inherent methods. Ids are allocated in one
/// sequence so the document is laid out the way rustdoc lays one out.
pub fn crate_doc(name: &str, modules: usize, types: usize, methods: usize) -> CrateDoc {
    let mut index: Vec<(String, Json)> = Vec::new();
    let mut paths: Vec<(String, Json)> = Vec::new();
    let mut next = 0i64;
    let mut id = || {
        next += 1;
        next
    };

    let root = id();
    let mut root_items = Vec::new();
    for m in 0..modules {
        let module = id();
        let module_name = format!("module{m}");
        let mut module_items = Vec::new();
        for t in 0..types {
            let ty = id();
            let ty_name = format!("Thing{t}");
            let block = id();
            let mut block_items = Vec::new();
            for k in 0..methods {
                let method = id();
                block_items.push(Json::Int(method));
                index.push((
                    method.to_string(),
                    item(
                        method,
                        Some(&format!("call{k}")),
                        "public",
                        Some("What this method does, in a sentence of the length a real one runs to."),
                        Json::Obj(vec![("function".into(), function())]),
                    ),
                ));
            }
            index.push((
                block.to_string(),
                item(
                    block,
                    None,
                    "default",
                    None,
                    Json::Obj(vec![(
                        "impl".into(),
                        Json::Obj(vec![
                            ("trait".into(), Json::Null),
                            ("is_synthetic".into(), Json::Bool(false)),
                            ("blanket_impl".into(), Json::Null),
                            ("items".into(), Json::Arr(block_items)),
                        ]),
                    )]),
                ),
            ));
            index.push((
                ty.to_string(),
                item(
                    ty,
                    Some(&ty_name),
                    "public",
                    Some("A type in the public surface."),
                    Json::Obj(vec![(
                        "struct".into(),
                        Json::Obj(vec![
                            (
                                "kind".into(),
                                Json::Obj(vec![(
                                    "plain".into(),
                                    Json::Obj(vec![("fields".into(), Json::Arr(vec![]))]),
                                )]),
                            ),
                            ("impls".into(), Json::Arr(vec![Json::Int(block)])),
                        ]),
                    )]),
                ),
            ));
            paths.push((
                ty.to_string(),
                path_entry(&[name, &module_name, &ty_name], "struct"),
            ));
            module_items.push(Json::Int(ty));
        }
        index.push((
            module.to_string(),
            item(
                module,
                Some(&module_name),
                "public",
                None,
                Json::Obj(vec![(
                    "module".into(),
                    Json::Obj(vec![("items".into(), Json::Arr(module_items))]),
                )]),
            ),
        ));
        paths.push((
            module.to_string(),
            path_entry(&[name, &module_name], "module"),
        ));
        root_items.push(Json::Int(module));
    }
    index.push((
        root.to_string(),
        item(
            root,
            Some(name),
            "public",
            None,
            Json::Obj(vec![(
                "module".into(),
                Json::Obj(vec![
                    ("is_crate".into(), Json::Bool(true)),
                    ("items".into(), Json::Arr(root_items)),
                ]),
            )]),
        ),
    ));
    paths.push((root.to_string(), path_entry(&[name], "module")));

    CrateDoc {
        name: name.to_string(),
        doc: Json::Obj(vec![
            ("root".into(), Json::Int(root)),
            ("crate_version".into(), Json::Str("0.0.1".into())),
            ("format_version".into(), Json::Int(FORMAT_VERSION as i64)),
            ("index".into(), Json::Obj(index)),
            ("paths".into(), Json::Obj(paths)),
            ("external_crates".into(), Json::Obj(vec![])),
        ]),
    }
}

fn item(id: i64, name: Option<&str>, visibility: &str, docs: Option<&str>, inner: Json) -> Json {
    Json::Obj(vec![
        ("id".into(), Json::Int(id)),
        ("crate_id".into(), Json::Int(0)),
        (
            "name".into(),
            name.map_or(Json::Null, |n| Json::Str(n.into())),
        ),
        ("visibility".into(), Json::Str(visibility.into())),
        (
            "docs".into(),
            docs.map_or(Json::Null, |d| Json::Str(d.into())),
        ),
        ("deprecation".into(), Json::Null),
        ("inner".into(), inner),
    ])
}

fn path_entry(segments: &[&str], kind: &str) -> Json {
    Json::Obj(vec![
        ("crate_id".into(), Json::Int(0)),
        (
            "path".into(),
            Json::Arr(segments.iter().map(|s| Json::Str((*s).into())).collect()),
        ),
        ("kind".into(), Json::Str(kind.into())),
    ])
}

/// `fn call(&self, source: &str, limit: u64) -> Result<Rows>`, which is
/// enough shape to exercise the renderer's common paths.
fn function() -> Json {
    let borrowed = |ty: Json, mutable: bool| {
        Json::Obj(vec![(
            "borrowed_ref".into(),
            Json::Obj(vec![
                ("lifetime".into(), Json::Null),
                ("is_mutable".into(), Json::Bool(mutable)),
                ("type".into(), ty),
            ]),
        )])
    };
    let primitive = |n: &str| Json::Obj(vec![("primitive".into(), Json::Str(n.into()))]);
    Json::Obj(vec![
        (
            "sig".into(),
            Json::Obj(vec![
                (
                    "inputs".into(),
                    Json::Arr(vec![
                        Json::Arr(vec![
                            Json::Str("self".into()),
                            borrowed(
                                Json::Obj(vec![("generic".into(), Json::Str("Self".into()))]),
                                false,
                            ),
                        ]),
                        Json::Arr(vec![
                            Json::Str("source".into()),
                            borrowed(primitive("str"), false),
                        ]),
                        Json::Arr(vec![Json::Str("limit".into()), primitive("u64")]),
                    ]),
                ),
                (
                    "output".into(),
                    Json::Obj(vec![(
                        "resolved_path".into(),
                        Json::Obj(vec![
                            ("path".into(), Json::Str("Result".into())),
                            ("args".into(), Json::Null),
                        ]),
                    )]),
                ),
                ("is_c_variadic".into(), Json::Bool(false)),
            ]),
        ),
        (
            "generics".into(),
            Json::Obj(vec![("params".into(), Json::Arr(vec![]))]),
        ),
        (
            "header".into(),
            Json::Obj(vec![
                ("is_const".into(), Json::Bool(false)),
                ("is_unsafe".into(), Json::Bool(false)),
                ("is_async".into(), Json::Bool(false)),
                ("abi".into(), Json::Str("Rust".into())),
            ]),
        ),
    ])
}
