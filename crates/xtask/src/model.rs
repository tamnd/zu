//! rustdoc's JSON, normalized into `model.json`.
//!
//! The model is the one description of the API that every other
//! generated thing is built from: the reference pages, the SDK feature
//! matrix, `zu.h`, and the `api-map.toml` completeness check that fails
//! a pull request adding a public symbol nothing maps. rustdoc's own
//! output cannot play that part. It is nightly-only, it changes shape
//! between releases, it is a third of a megabyte for a crate this size,
//! and it describes one crate at a time while `zu` re-exports a third
//! of its surface from four others.
//!
//! So this module does three things rustdoc does not. It follows
//! `pub use` into the crate the item really lives in and files the
//! result under the path a user actually types, because `zu::GqlStatus`
//! is the name a binding has to map and `zu_common::gqlstatus::
//! GqlStatus` is not. It flattens inherent impls into methods hanging
//! off their type, since an impl block is a Rust spelling and no other
//! language has one. And it emits entities sorted by identifier with a
//! writer that reorders nothing, so regenerating an unchanged API gives
//! back the same bytes and the CI check is a real check rather than a
//! coin toss.
//!
//! What is deliberately left out: trait implementations, blanket and
//! synthetic impls, and anything private. A binding binds inherent
//! methods and the traits this crate defines; `impl Borrow<T> for U`
//! is a fact about Rust's type system that no other language has a
//! word for.

use std::collections::{BTreeMap, HashMap, HashSet};

use zu_json::Json;

use crate::rustdoc::CrateDoc;

/// The schema version of `model.json` itself, which moves when the
/// shape of the file changes and not when the API it describes does.
pub const SCHEMA: i64 = 1;

/// One thing in the API, at the path a user names it by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entity {
    /// `zu::session::Session::run`. The join key with `api-map.toml`.
    pub id: String,
    pub kind: &'static str,
    pub name: String,
    /// The entity this one hangs off: a method's type, a variant's
    /// enum, a field's struct.
    pub of: Option<String>,
    /// Rust source form: the `fn` line for anything callable, the
    /// declared type for a field, a constant, or an alias. One field
    /// rather than two, because every consumer wants the same thing out
    /// of it, which is what to write in the target language.
    pub signature: Option<String>,
    /// The doc comment, whole. Trimming it to a summary here would
    /// throw away the text the reference pages need and force a second
    /// extraction that could disagree with this one.
    pub doc: Option<String>,
    /// Where the item really lives, when the public path is a re-export.
    pub source: Option<String>,
    pub deprecated: bool,
}

/// The whole model, ready to be written.
#[derive(Debug)]
pub struct Model {
    pub crate_name: String,
    pub crate_version: String,
    pub format_version: u64,
    /// Sorted by id, which is what makes the output stable.
    pub entities: Vec<Entity>,
}

impl Model {
    pub fn to_json(&self) -> Json {
        let entities = self
            .entities
            .iter()
            .map(|e| {
                let mut fields = vec![
                    ("id".to_string(), Json::Str(e.id.clone())),
                    ("kind".to_string(), Json::Str(e.kind.to_string())),
                    ("name".to_string(), Json::Str(e.name.clone())),
                ];
                // Absent rather than null: a consumer checking for a
                // field it needs should not have to distinguish the two,
                // and the file is smaller for it.
                for (key, value) in [
                    ("of", &e.of),
                    ("signature", &e.signature),
                    ("source", &e.source),
                    ("doc", &e.doc),
                ] {
                    if let Some(v) = value {
                        fields.push((key.to_string(), Json::Str(v.clone())));
                    }
                }
                if e.deprecated {
                    fields.push(("deprecated".to_string(), Json::Bool(true)));
                }
                Json::Obj(fields)
            })
            .collect();
        Json::Obj(vec![
            ("schema".to_string(), Json::Int(SCHEMA)),
            ("crate".to_string(), Json::Str(self.crate_name.clone())),
            ("version".to_string(), Json::Str(self.crate_version.clone())),
            (
                "rustdoc_format_version".to_string(),
                Json::Int(self.format_version as i64),
            ),
            ("entities".to_string(), Json::Arr(entities)),
        ])
    }
}

/// One crate's rustdoc output, turned into the two lookups the walk
/// needs: item by id, and id by fully qualified path.
struct Reader<'a> {
    name: &'a str,
    /// rustdoc's `index`, which holds the items it documented.
    index: HashMap<&'a str, &'a Json>,
    /// rustdoc's `paths`, which holds a path for every item it can name,
    /// including ones in other crates that it did not document.
    paths: HashMap<&'a str, Vec<&'a str>>,
    /// The crate each id in `paths` belongs to, spelled as Rust does.
    owners: HashMap<&'a str, &'a str>,
    /// The reverse of `paths` for this crate's own items, so a
    /// re-export naming `zu_common::gqlstatus::GqlStatus` can be
    /// resolved in the crate that defines it.
    by_path: HashMap<String, &'a str>,
    version: &'a str,
}

impl<'a> Reader<'a> {
    fn new(doc: &'a CrateDoc) -> Result<Self, String> {
        let j = &doc.doc;
        let index = obj_map(j.get("index").ok_or("no index")?);
        let raw_paths = j.get("paths").ok_or("no paths")?;
        // crate_id 0 is always the crate rustdoc was run on; the rest
        // are named in external_crates.
        let mut crates: HashMap<i64, &str> = HashMap::new();
        crates.insert(0, doc.name.as_str());
        if let Some(fields) = j.get("external_crates").and_then(Json::as_obj) {
            for (id, entry) in fields {
                if let (Ok(n), Some(name)) =
                    (id.parse::<i64>(), entry.get("name").and_then(Json::as_str))
                {
                    crates.insert(n, name);
                }
            }
        }
        let mut paths = HashMap::new();
        let mut owners = HashMap::new();
        let mut by_path = HashMap::new();
        for (id, entry) in raw_paths.as_obj().ok_or("paths is not an object")? {
            let segments: Vec<&str> = entry
                .get("path")
                .and_then(Json::as_arr)
                .map(|a| a.iter().filter_map(Json::as_str).collect())
                .unwrap_or_default();
            let owner = entry
                .get("crate_id")
                .and_then(Json::as_i64)
                .and_then(|c| crates.get(&c).copied())
                .unwrap_or("");
            if owner == doc.name {
                by_path.insert(segments.join("::"), id.as_str());
            }
            paths.insert(id.as_str(), segments);
            owners.insert(id.as_str(), owner);
        }
        Ok(Reader {
            name: &doc.name,
            index,
            paths,
            owners,
            by_path,
            version: j.get("crate_version").and_then(Json::as_str).unwrap_or(""),
        })
    }

    fn item(&self, id: &str) -> Option<&'a Json> {
        self.index.get(id).copied()
    }
}

/// rustdoc spells ids as JSON numbers in some places and object keys in
/// others, so every lookup goes through one spelling.
fn id_of(value: &Json) -> Option<String> {
    value.as_i64().map(|i| i.to_string())
}

fn obj_map(value: &Json) -> HashMap<&str, &Json> {
    value
        .as_obj()
        .map(|fields| fields.iter().map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default()
}

/// Builds the model for `root`, resolving re-exports into whichever of
/// `docs` defines them.
pub fn build(docs: &[CrateDoc], root: &str) -> Result<Model, String> {
    let readers: Vec<Reader> = docs.iter().map(Reader::new).collect::<Result<_, _>>()?;
    let home = readers
        .iter()
        .position(|r| r.name == root)
        .ok_or_else(|| format!("no rustdoc output for {root}"))?;
    let root_id = docs[home]
        .doc
        .get("root")
        .and_then(Json::as_i64)
        .map(|i| i.to_string())
        .ok_or("no root item")?;
    let format_version = docs[home]
        .doc
        .get("format_version")
        .and_then(Json::as_u64)
        .unwrap_or(0);

    let mut walk = Walk {
        readers: &readers,
        out: BTreeMap::new(),
        seen: HashSet::new(),
    };
    walk.module(home, &root_id, &[root.to_string()], None);

    Ok(Model {
        crate_name: root.to_string(),
        crate_version: readers[home].version.to_string(),
        format_version,
        entities: walk.out.into_values().collect(),
    })
}

struct Walk<'a> {
    readers: &'a [Reader<'a>],
    /// Keyed by id so the sort is free and a re-export reached twice by
    /// two paths lands once.
    out: BTreeMap<String, Entity>,
    /// (crate index, item id, public path) triples already expanded.
    /// The path is in the key on purpose. An item re-exported at two
    /// public paths is two names a binding has to map, and keying on
    /// the item alone would drop whichever the walk reached second;
    /// `zu::GqlStatus` went missing exactly that way. A real cycle
    /// grows the path every time round, so MAX_DEPTH still ends it.
    seen: HashSet<(usize, String, String)>,
}

/// A path deeper than this is a re-export cycle rustdoc let through.
const MAX_DEPTH: usize = 32;

impl<'a> Walk<'a> {
    fn push(&mut self, entity: Entity) {
        self.out.entry(entity.id.clone()).or_insert(entity);
    }

    fn module(&mut self, home: usize, id: &str, path: &[String], source: Option<String>) {
        if path.len() > MAX_DEPTH || !self.seen.insert((home, id.to_string(), path.join("::"))) {
            return;
        }
        let reader = &self.readers[home];
        let Some(item) = reader.item(id) else { return };
        // The crate root is a module too, and it is the one module that
        // is the model's subject rather than a member of it.
        if path.len() > 1 {
            self.push(entity(path, "module", item, None, source.clone()));
        }
        let members = item
            .get("inner")
            .and_then(|i| i.get("module"))
            .and_then(|m| m.get("items"))
            .and_then(Json::as_arr)
            .unwrap_or_default()
            .to_vec();
        for member in &members {
            if let Some(mid) = id_of(member) {
                self.member(home, &mid, path);
            }
        }
    }

    /// One item inside a module, dispatched on what rustdoc says it is.
    fn member(&mut self, home: usize, id: &str, parent: &[String]) {
        let reader = &self.readers[home];
        let Some(item) = reader.item(id) else { return };
        let kind = inner_kind(item);
        // A `use` carries the visibility that matters; everything else
        // has to say public itself, since rustdoc indexes private items
        // reached through a public re-export.
        if kind != "use" && item.get("visibility").and_then(Json::as_str) != Some("public") {
            return;
        }
        let name = item.get("name").and_then(Json::as_str).unwrap_or("");
        let here = |n: &str| {
            let mut p = parent.to_vec();
            p.push(n.to_string());
            p
        };
        match kind {
            "use" => self.reexport(home, item, parent),
            "module" => self.module(home, id, &here(name), None),
            "struct" | "enum" | "trait" => self.type_like(home, id, &here(name), None),
            "function" => {
                let sig = signature(item, reader.name);
                self.push(entity(&here(name), "function", item, sig, None));
            }
            "type_alias" => {
                let ty = declared_type(item, reader.name);
                self.push(entity(&here(name), "type-alias", item, ty, None));
            }
            "constant" | "static" => {
                let ty = declared_type(item, reader.name);
                self.push(entity(&here(name), "constant", item, ty, None));
            }
            "macro" | "proc_macro" => self.push(entity(&here(name), "macro", item, None, None)),
            _ => {}
        }
    }

    /// A struct, enum, or trait, plus the members that hang off it.
    fn type_like(&mut self, home: usize, id: &str, path: &[String], source: Option<String>) {
        if path.len() > MAX_DEPTH || !self.seen.insert((home, id.to_string(), path.join("::"))) {
            return;
        }
        let reader = &self.readers[home];
        let Some(item) = reader.item(id) else { return };
        let kind = inner_kind(item);
        let model_kind = match kind {
            "struct" => "struct",
            "enum" => "enum",
            "trait" => "trait",
            _ => return,
        };
        self.push(entity(path, model_kind, item, None, source));
        let owner = path.join("::");
        let inner = item.get("inner").and_then(|i| i.get(kind));

        // Fields, variants, and trait items are all children rustdoc
        // lists by id under a key that differs per kind.
        let children = match kind {
            // A struct is plain, a tuple, or a unit. A tuple's fields
            // are named 0 and 1 and are as public as any other, so they
            // belong in the model; rustdoc writes null in the positions
            // that are private.
            "struct" => {
                let shape = inner.and_then(|s| s.get("kind"));
                shape
                    .and_then(|k| k.get("plain"))
                    .and_then(|p| p.get("fields"))
                    .or_else(|| shape.and_then(|k| k.get("tuple")))
                    .and_then(Json::as_arr)
                    .unwrap_or_default()
                    .to_vec()
            }
            "enum" => inner
                .and_then(|e| e.get("variants"))
                .and_then(Json::as_arr)
                .unwrap_or_default()
                .to_vec(),
            _ => inner
                .and_then(|t| t.get("items"))
                .and_then(Json::as_arr)
                .unwrap_or_default()
                .to_vec(),
        };
        for child in &children {
            let Some(cid) = id_of(child) else { continue };
            let Some(citem) = reader.item(&cid) else {
                continue;
            };
            let cname = citem.get("name").and_then(Json::as_str).unwrap_or("");
            let mut cpath = path.to_vec();
            cpath.push(cname.to_string());
            let ckind = match inner_kind(citem) {
                "struct_field" => "field",
                "variant" => "variant",
                "function" => "method",
                "assoc_const" => "assoc-const",
                "assoc_type" => "assoc-type",
                _ => continue,
            };
            let sig = match ckind {
                "method" => signature(citem, reader.name),
                "variant" => variant_shape(citem, reader, reader.name),
                _ => declared_type(citem, reader.name),
            };
            let mut e = entity(&cpath, ckind, citem, sig, None);
            e.of = Some(owner.clone());
            self.push(e);
        }

        // Methods live in impl blocks. Inherent ones are the API;
        // blanket and synthetic impls are Rust facts about Rust.
        let impls = inner
            .and_then(|i| i.get("impls"))
            .and_then(Json::as_arr)
            .unwrap_or_default()
            .to_vec();
        for imp in &impls {
            let Some(iid) = id_of(imp) else { continue };
            let Some(iitem) = reader.item(&iid) else {
                continue;
            };
            let Some(block) = iitem.get("inner").and_then(|i| i.get("impl")) else {
                continue;
            };
            let is_inherent = matches!(block.get("trait"), None | Some(Json::Null));
            let is_synthetic = block.get("is_synthetic").and_then(Json::as_bool) == Some(true);
            let is_blanket = !matches!(block.get("blanket_impl"), None | Some(Json::Null));
            if !is_inherent || is_synthetic || is_blanket {
                continue;
            }
            for m in block
                .get("items")
                .and_then(Json::as_arr)
                .unwrap_or_default()
            {
                let Some(mid) = id_of(m) else { continue };
                let Some(mitem) = reader.item(&mid) else {
                    continue;
                };
                if mitem.get("visibility").and_then(Json::as_str) != Some("public") {
                    continue;
                }
                let mname = mitem.get("name").and_then(Json::as_str).unwrap_or("");
                let mut mpath = path.to_vec();
                mpath.push(mname.to_string());
                let mkind = match inner_kind(mitem) {
                    "function" => "method",
                    "assoc_const" => "assoc-const",
                    "assoc_type" => "assoc-type",
                    _ => continue,
                };
                let sig = if mkind == "method" {
                    signature(mitem, reader.name)
                } else {
                    declared_type(mitem, reader.name)
                };
                let mut e = entity(&mpath, mkind, mitem, sig, None);
                e.of = Some(owner.clone());
                self.push(e);
            }
        }
    }

    /// `pub use`, which is where the public path and the defining path
    /// part company. The model files the item under the public path and
    /// records the defining one, because a binding maps what a user
    /// types and a maintainer greps for where it lives.
    fn reexport(&mut self, home: usize, item: &Json, parent: &[String]) {
        let Some(u) = item.get("inner").and_then(|i| i.get("use")) else {
            return;
        };
        let name = u.get("name").and_then(Json::as_str).unwrap_or("");
        let is_glob = u.get("is_glob").and_then(Json::as_bool) == Some(true);
        let Some(target) = u.get("id").and_then(id_of) else {
            return;
        };
        // A glob splices the target module's members in at this level
        // rather than adding one, since `pub use m::*` is not a module.
        let path = if is_glob {
            parent.to_vec()
        } else {
            let mut p = parent.to_vec();
            p.push(name.to_string());
            p
        };

        let reader = &self.readers[home];
        let source = reader
            .paths
            .get(target.as_str())
            .map(|segments| segments.join("::"));

        // Same crate: rustdoc documented it, so walk it in place.
        if reader.item(&target).is_some() {
            if is_glob {
                self.module(home, &target, &path, source);
            } else {
                self.dispatch(home, &target, &path, source);
            }
            return;
        }

        // Another crate: find its rustdoc output and the item inside it
        // by the path this crate knows it by.
        let Some(owner) = reader.owners.get(target.as_str()).copied() else {
            return;
        };
        let Some(there) = self.readers.iter().position(|r| r.name == owner) else {
            // Not a crate we were given. Record the entity so the model
            // does not silently lose a public name, and say where it
            // went, which is more useful than a gap nobody notices.
            if let Some(src) = source {
                let kind = reader
                    .paths
                    .get(target.as_str())
                    .map(|_| "unresolved")
                    .unwrap_or("unresolved");
                self.push(Entity {
                    id: path.join("::"),
                    kind,
                    name: name.to_string(),
                    of: None,
                    signature: None,
                    doc: None,
                    source: Some(src),
                    deprecated: false,
                });
            }
            return;
        };
        let Some(segments) = reader.paths.get(target.as_str()) else {
            return;
        };
        let key = segments.join("::");
        let Some(&elsewhere) = self.readers[there].by_path.get(&key) else {
            return;
        };
        let elsewhere = elsewhere.to_string();
        if is_glob {
            self.module(there, &elsewhere, &path, Some(key));
        } else {
            self.dispatch(there, &elsewhere, &path, Some(key));
        }
    }

    /// A re-export target, whatever kind it turned out to be.
    fn dispatch(&mut self, home: usize, id: &str, path: &[String], source: Option<String>) {
        let reader = &self.readers[home];
        let Some(item) = reader.item(id) else { return };
        let name = path.last().cloned().unwrap_or_default();
        match inner_kind(item) {
            "module" => self.module(home, id, path, source),
            "struct" | "enum" | "trait" => self.type_like(home, id, path, source),
            "function" => {
                let mut e = entity(path, "function", item, signature(item, reader.name), source);
                e.name = name;
                self.push(e);
            }
            "type_alias" => {
                let ty = declared_type(item, reader.name);
                self.push(entity(path, "type-alias", item, ty, source));
            }
            "constant" | "static" => {
                let ty = declared_type(item, reader.name);
                self.push(entity(path, "constant", item, ty, source));
            }
            "macro" | "proc_macro" => self.push(entity(path, "macro", item, None, source)),
            _ => {}
        }
    }
}

fn inner_kind(item: &Json) -> &str {
    item.get("inner")
        .and_then(Json::as_obj)
        .and_then(|fields| fields.first())
        .map(|(k, _)| k.as_str())
        .unwrap_or("")
}

fn entity(
    path: &[String],
    kind: &'static str,
    item: &Json,
    signature: Option<String>,
    source: Option<String>,
) -> Entity {
    Entity {
        id: path.join("::"),
        kind,
        name: path.last().cloned().unwrap_or_default(),
        of: None,
        signature,
        doc: item
            .get("docs")
            .and_then(Json::as_str)
            .map(str::to_string)
            .filter(|d| !d.is_empty()),
        source,
        deprecated: !matches!(item.get("deprecation"), None | Some(Json::Null)),
    }
}

/// The declared type of the things that have one instead of a `fn`
/// line: fields, constants, statics, aliases, and the associated
/// members of a trait. A field with a name and no type is not something
/// a header generator can emit, so this is not optional detail.
fn declared_type(item: &Json, home: &str) -> Option<String> {
    let inner = item.get("inner")?.as_obj()?.first()?;
    let ty = match inner.0.as_str() {
        // A tuple struct's field is the type, with nothing wrapped
        // round it.
        "struct_field" => &inner.1,
        "constant" | "static" | "type_alias" | "assoc_const" | "assoc_type" => {
            inner.1.get("type")?
        }
        _ => return None,
    };
    Some(absolute(&render_type(ty), home))
}

/// What a variant carries, written the way the source declares it:
/// `Temporal(TemporalValue)`, `Unary { op: UnaryOp, expr: Expr }`, or
/// nothing at all for a plain one. A tagged union is the shape every
/// binding has the most trouble with, and the payload is the part of it
/// that cannot be guessed from the name.
fn variant_shape(item: &Json, reader: &Reader, home: &str) -> Option<String> {
    let name = item.get("name").and_then(Json::as_str)?;
    let kind = item.get("inner")?.get("variant")?.get("kind")?;
    let field = |value: &Json| -> Option<(String, String)> {
        let f = reader.item(&id_of(value)?)?;
        Some((
            f.get("name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            declared_type(f, home)?,
        ))
    };
    if let Some(fields) = kind.get("tuple").and_then(Json::as_arr) {
        let parts: Vec<String> = fields.iter().filter_map(|f| Some(field(f)?.1)).collect();
        return Some(format!("{name}({})", parts.join(", ")));
    }
    if let Some(fields) = kind
        .get("struct")
        .and_then(|s| s.get("fields"))
        .and_then(Json::as_arr)
    {
        let parts: Vec<String> = fields
            .iter()
            .filter_map(|f| field(f).map(|(n, t)| format!("{n}: {t}")))
            .collect();
        return Some(format!("{name} {{ {} }}", parts.join(", ")));
    }
    Some(name.to_string())
}

/// Renders a function's Rust source form. This is what a header
/// generator and a reference page both need, and neither can recover it
/// from a name.
fn signature(item: &Json, home: &str) -> Option<String> {
    let f = item.get("inner")?.get("function")?;
    let header = f.get("header");
    let flag = |k: &str| header.and_then(|h| h.get(k)).and_then(Json::as_bool) == Some(true);
    let mut out = String::new();
    if flag("is_const") {
        out.push_str("const ");
    }
    if flag("is_async") {
        out.push_str("async ");
    }
    if flag("is_unsafe") {
        out.push_str("unsafe ");
    }
    if let Some(abi) = header.and_then(|h| h.get("abi")).and_then(Json::as_str)
        && abi != "Rust"
    {
        out.push_str(&format!("extern {abi:?} "));
    }
    out.push_str("fn ");
    out.push_str(item.get("name").and_then(Json::as_str).unwrap_or(""));
    if let Some(params) = f
        .get("generics")
        .and_then(|g| g.get("params"))
        .and_then(Json::as_arr)
    {
        let named: Vec<String> = params
            .iter()
            // An `impl Trait` argument is also a generic parameter, one
            // rustdoc synthesizes and names `impl Into<String>`.
            // Printing it in the angle brackets as well as in the
            // argument list would render `fn new<impl Into<String>>`,
            // which is not something anyone wrote or could compile.
            .filter(|p| {
                p.get("kind")
                    .and_then(|k| k.get("type"))
                    .and_then(|t| t.get("is_synthetic"))
                    .and_then(Json::as_bool)
                    != Some(true)
            })
            .filter_map(|p| p.get("name").and_then(Json::as_str))
            // A lifetime tells a binding nothing and clutters every
            // signature it appears in.
            .filter(|n| !n.starts_with('\''))
            .map(str::to_string)
            .collect();
        if !named.is_empty() {
            out.push('<');
            out.push_str(&named.join(", "));
            out.push('>');
        }
    }
    out.push('(');
    let sig = f.get("sig");
    if let Some(inputs) = sig.and_then(|s| s.get("inputs")).and_then(Json::as_arr) {
        let rendered: Vec<String> = inputs
            .iter()
            .filter_map(|pair| {
                let items = pair.as_arr()?;
                let name = items.first()?.as_str()?;
                let ty = absolute(&render_type(items.get(1)?), home);
                // `self` prints as the receiver alone, the way it is
                // written, not as `self: &mut Self`.
                Some(if name == "self" {
                    match ty.as_str() {
                        "&Self" => "&self".to_string(),
                        "&mut Self" => "&mut self".to_string(),
                        "Self" => "self".to_string(),
                        other => format!("self: {other}"),
                    }
                } else {
                    format!("{name}: {ty}")
                })
            })
            .collect();
        out.push_str(&rendered.join(", "));
        if sig
            .and_then(|s| s.get("is_c_variadic"))
            .and_then(Json::as_bool)
            == Some(true)
        {
            if !rendered.is_empty() {
                out.push_str(", ");
            }
            out.push_str("...");
        }
    }
    out.push(')');
    match sig.and_then(|s| s.get("output")) {
        None | Some(Json::Null) => {}
        Some(ty) => {
            out.push_str(" -> ");
            out.push_str(&absolute(&render_type(ty), home));
        }
    }
    Some(out)
}

/// Rewrites `crate::` to the crate it means. rustdoc renders a path the
/// way the source wrote it, and the source is inside the crate while
/// every reader of the model is outside it, where `crate::` names
/// nothing.
fn absolute(rendered: &str, home: &str) -> String {
    if rendered.contains("crate::") {
        rendered.replace("crate::", &format!("{home}::"))
    } else {
        rendered.to_string()
    }
}

/// Renders one rustdoc type. An unrecognised shape becomes `?tag`
/// rather than silently disappearing, and a test asserts the real model
/// contains none, so a format change shows up as a diff instead of a
/// quietly wrong signature.
fn render_type(ty: &Json) -> String {
    if let Some(word) = ty.as_str() {
        // `"infer"` and the like arrive as bare strings.
        return if word == "infer" {
            "_".into()
        } else {
            word.into()
        };
    }
    let Some((tag, body)) = ty.as_obj().and_then(<[(String, Json)]>::first) else {
        return "?".into();
    };
    match tag.as_str() {
        "primitive" | "generic" => body.as_str().unwrap_or("?").to_string(),
        "resolved_path" => {
            let name = body.get("path").and_then(Json::as_str).unwrap_or("?");
            format!("{name}{}", render_args(body.get("args")))
        }
        "borrowed_ref" => {
            let m = if body.get("is_mutable").and_then(Json::as_bool) == Some(true) {
                "mut "
            } else {
                ""
            };
            let inner = body
                .get("type")
                .map(render_type)
                .unwrap_or_else(|| "?".into());
            format!("&{m}{inner}")
        }
        "raw_pointer" => {
            let m = if body.get("is_mutable").and_then(Json::as_bool) == Some(true) {
                "mut"
            } else {
                "const"
            };
            let inner = body
                .get("type")
                .map(render_type)
                .unwrap_or_else(|| "?".into());
            format!("*{m} {inner}")
        }
        "tuple" => {
            let parts: Vec<String> = body
                .as_arr()
                .unwrap_or_default()
                .iter()
                .map(render_type)
                .collect();
            // A one-element tuple keeps its comma or it reads as parens.
            if parts.len() == 1 {
                format!("({},)", parts[0])
            } else {
                format!("({})", parts.join(", "))
            }
        }
        "slice" => format!("[{}]", render_type(body)),
        "array" => {
            let inner = body
                .get("type")
                .map(render_type)
                .unwrap_or_else(|| "?".into());
            let len = body.get("len").and_then(Json::as_str).unwrap_or("_");
            format!("[{inner}; {len}]")
        }
        "impl_trait" => format!("impl {}", render_bounds(body)),
        "dyn_trait" => {
            let traits: Vec<String> = body
                .get("traits")
                .and_then(Json::as_arr)
                .unwrap_or_default()
                .iter()
                .map(|t| match t.get("trait") {
                    Some(p) => format!(
                        "{}{}",
                        p.get("path").and_then(Json::as_str).unwrap_or("?"),
                        render_args(p.get("args"))
                    ),
                    None => "?".to_string(),
                })
                .collect();
            format!("dyn {}", traits.join(" + "))
        }
        "qualified_path" => {
            let base = body
                .get("self_type")
                .map(render_type)
                .unwrap_or_else(|| "?".into());
            let name = body.get("name").and_then(Json::as_str).unwrap_or("?");
            format!("{base}::{name}")
        }
        "function_pointer" => "fn(..)".to_string(),
        other => format!("?{other}"),
    }
}

fn render_args(args: Option<&Json>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    let Some(angle) = args.get("angle_bracketed") else {
        return String::new();
    };
    let parts: Vec<String> = angle
        .get("args")
        .and_then(Json::as_arr)
        .unwrap_or_default()
        .iter()
        .filter_map(|a| {
            if let Some(t) = a.get("type") {
                Some(render_type(t))
            } else if let Some(c) = a.get("const") {
                c.get("expr").and_then(Json::as_str).map(str::to_string)
            } else {
                // A lifetime argument, which carries no meaning outside
                // Rust and would only add noise to every signature.
                None
            }
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("<{}>", parts.join(", "))
    }
}

fn render_bounds(bounds: &Json) -> String {
    let parts: Vec<String> = bounds
        .as_arr()
        .unwrap_or_default()
        .iter()
        .filter_map(|b| {
            let t = b.get("trait_bound")?.get("trait")?;
            // The arguments are the bound. `impl Into` says nothing that
            // `impl Into<String>` does not say precisely.
            let name = t.get("path").and_then(Json::as_str)?;
            Some(format!("{name}{}", render_args(t.get("args"))))
        })
        .collect();
    parts.join(" + ")
}
