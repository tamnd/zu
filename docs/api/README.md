# The API model

`model.json` is the normalized description of the `zu` public API, and it is the one thing every generated artifact is built from: the reference pages, the SDK feature matrix, the `zu.h` header, and the `api-map.toml` completeness check that fails a pull request adding a public symbol nothing maps. It is generated from rustdoc's JSON output by `crates/xtask` and committed, so that an API change shows up in the diff of the pull request that makes it and so that a consumer can read a file rather than needing a nightly toolchain.

## Generating it

```
cargo xtask model                     # rewrite docs/api/model.json
cargo xtask model --check             # fail if the tree is stale
cargo xtask model --out /tmp/m.json   # write somewhere else
cargo xtask model --toolchain nightly-2026-08-14
```

The generator runs `cargo +nightly rustdoc --lib -Z unstable-options --output-format json` for `zu` and for the four crates `zu` re-exports from, and reads what cargo wrote. rustdoc JSON is nightly-only and versioned, so CI pins an exact nightly and the generator refuses a `format_version` it has not been taught rather than half reading a changed format and producing a model that looks fine and is missing a third of the API. The pinned nightly is `nightly-2026-08-14`, which produces format 61.

`--check` regenerates into memory and compares byte for byte, and on a mismatch it prints the identifiers that were added and removed rather than saying the file differs. That comparison is only a real check because the generator is deterministic: entities are collected into a `BTreeMap` keyed by identifier and the JSON writer reorders nothing, so regenerating an unchanged API gives back the same bytes. A bench asserts this over 32 rebuilds.

## What the file holds

```json
{
  "schema": 1,
  "crate": "zu",
  "version": "0.0.1",
  "rustdoc_format_version": 61,
  "entities": [ ... ]
}
```

`schema` moves when the shape of this file changes and not when the API it describes does. `rustdoc_format_version` records what the model was extracted from, so a consumer can tell a model regenerated under a new rustdoc from one that was not.

Every entity is one thing in the API, at the path a user names it by.

| Field | Always | Meaning |
| --- | --- | --- |
| `id` | yes | `zu::session::Session::open`. The join key with `api-map.toml` and with everything else generated. |
| `kind` | yes | One of the kinds below. |
| `name` | yes | The last segment of the id. |
| `of` | no | The entity this one hangs off: a method's type, a variant's enum, a field's struct. |
| `signature` | no | Rust source form. |
| `source` | no | Where the item really lives, when the public path is a re-export. |
| `doc` | no | The doc comment, whole. |
| `deprecated` | no | `true`, or the field is absent. |

Optional fields are absent rather than null, so a consumer checking for a field it needs does not have to distinguish the two.

`signature` is the `fn` line for anything callable, and the declared type for a field, a constant, an alias, or an associated item. A variant carries its payload the way the source declares it: `Conflict(String)`, `Corrupt { what: &str, detail: String }`, or just its own name when it is plain. Structs, enums, traits, and modules have no signature, because their shape is their members. One field rather than two, because every consumer wants the same thing out of it, which is what to write in the target language.

Kinds, and what the current model holds of each:

| Kind | Count | Notes |
| --- | --- | --- |
| `method` | 264 | An inherent method, or an item declared on a trait. |
| `field` | 131 | Named, or a tuple position spelled `0`. |
| `constant` | 105 | Constants and statics both. |
| `variant` | 96 | |
| `function` | 81 | A free function. |
| `struct` | 59 | |
| `module` | 29 | |
| `enum` | 26 | |
| `type-alias` | 10 | |
| `trait` | 5 | |

There is one more kind the generator can emit and the committed model never does. `unresolved` is a public name whose target lives in a crate the generator was not handed, and it is recorded rather than dropped because a gap nobody notices is the worse failure. A test asserts the model contains none, since one means the crate list in `crates/xtask/src/main.rs` is short.

## What it does that rustdoc does not

rustdoc's own output cannot play this part. It is nightly-only, it changes shape between releases, it is over half a megabyte for a crate this size, and it describes one crate at a time while `zu` re-exports part of its surface from four others.

So the generator follows `pub use` into the crate the item really lives in and files the result under the path a user types, because `zu::GqlStatus` is the name a binding has to map and `zu_common::gqlstatus::GqlStatus` is not. A glob re-export splices the target module's members in at the same level rather than adding one, since `pub use m::*` is not a module. An item public at two paths appears under both, because those are two names a binding has to map.

It flattens inherent impls into methods hanging off their type, since an impl block is a Rust spelling and no other language has one. Trait implementations, blanket impls, and synthetic impls are left out on purpose: a binding binds inherent methods and the traits this crate defines, and `impl Borrow<T> for U` is a fact about Rust's type system that no other language has a word for.

It rewrites `crate::` to the crate it means, because rustdoc renders a path the way the source wrote it, the source is inside the crate, and every reader of this file is outside it, where `crate::` names nothing. It drops lifetimes from signatures, since they carry nothing outside Rust. It writes `impl Into<String>` once, as the argument, and not also as the synthetic generic parameter rustdoc reports alongside it.

A rustdoc type tag the renderer does not know becomes `?tag` rather than disappearing, so a format change shows up as a diff instead of as a quietly wrong signature. A test asserts the committed model contains none.

## The map

`model.json` says what the API is. `api-map.toml` says what a binding owes it. One map lives here, beside the model, and classifies the whole surface; one lives in each binding repository and gives every classified entity the name that binding calls it by. They are the same schema doing two jobs, and `target` says which.

```
cargo xtask api-map                            # the map here against the model here
cargo xtask api-map --map ../zu-python/api-map.toml
cargo xtask api-map --list                     # `tier<TAB>id` for every entity
```

The check runs in two directions, because either alone leaves a way for the surface and the bindings to drift apart quietly. Every entity in the model has to be classified here, so a public symbol nobody decided about fails CI in this repository rather than shipping unbound in six languages. And every entity classified tier 1 has to be named in each binding's map, so a binding that quietly stopped exposing something fails the release rather than the user's build. Backwards from both: a group prefix or an entry naming code that is gone is reported too, since a decision about something that no longer exists sits in the file looking like coverage.

The tiers:

| Tier | Meaning |
| --- | --- |
| 1 | Every tier-1 SDK exposes it, and a release where one does not is a release that fails. |
| 2 | A binding may expose it, and nothing is gated either way. |
| 3 | No binding exposes it, and a `reason` is required. |

The reason on a tier 3 is the point of that tier. A public symbol nobody binds is a decision, and a decision with no reason written down is one nobody can revisit, so the reader refuses the entry rather than leaving it to a reviewer to notice.

`[[group]]` covers a path prefix and everything under it, longest prefix winning, so a narrower group is an exception to a wider one and the order of the file carries no meaning. `[[entity]]` names one identifier exactly and beats every group. Groups are what make the file reviewable: `zu::zu1` is five hundred entities and one decision, and writing that decision out five hundred times would hide the six entries that are not it. A binding map has no groups, because a name cannot be derived from a path prefix, and no tiers, because a tier is declared once so that two files cannot disagree about what a binding owes.

```toml
schema = 1
target = "python"

[[entity]]
id = "zu::session::Session"
name = "zudb.Connection"
```

Modules are not mapped. A module is a namespace, and namespaces are the one part of a Rust surface no binding reproduces: Python flattens them, C prefixes them, Go packages them by repository. Every entity carries its full path already.

Adding a public symbol therefore takes two steps, in this order, and the failures say so: `cargo xtask model` to put it in the model, then an entry in the map to say what a binding owes it.

Where the surface stands today: 780 mappable entities, 73 at tier 1, 111 at tier 2, 596 at tier 3. Tier 1 is the session, values in and out, results, and the error model, which is what a binding is. Tier 3 is almost all of `zu::zu1`, the storage engine a session talks to and a caller does not.

## Consumers

Nothing generates from the model yet. It lands first because everything in DX0 that does generate from it needs a stable input to be written against, and because committing it means the first pull request that changes the public API shows that change in its own diff. The map is the first thing to read it.
