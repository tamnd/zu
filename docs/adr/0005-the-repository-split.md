# 0005. The ABI is the repository boundary

Status: accepted, 2026-08-15. Spec: `Spec/2064g/dx/18-repository-topology.md`.

## Context

Nine deliverables need a home: the engine, the Rust SDK, the CLI, the C ABI, seven language clients, the tier-3 binding kit, the conformance corpus, and the documentation site. A monorepo makes cross-cutting changes atomic and makes every client's CI run on every engine commit. Separate repositories give each client its own release cadence, its own issue tracker, and its own contributors, and in Go's case the module path is the repository path, so it has no choice.

Choosing by taste produces a boundary nobody can apply to the next question. What is needed is a rule.

## Decision

Code that compiles against the engine's internals stays in `tamnd/zu`. Code that compiles against the frozen C ABI lives in its own repository.

Three things follow that a naive first-party-versus-third-party split would get wrong:

The **Rust SDK stays here**. DuckDB's `duckdb-rs` is out of tree because Rust is a foreign language to a C++ engine. For us Rust is the engine's own language and `zudb` is a direct dependent of the engine crates, not an ABI consumer. Splitting it would make the reference SDK the one SDK that cannot be changed atomically with the API it wraps.

**`zu.h` stays here**, because it is generated from the API model and is therefore an output of this build. `tamnd/zu-c` is the C and C++ developer kit: examples, the header-only C++ wrapper, CMake and vcpkg and Conan packaging, the sanitizer suites. Its README says so in its first paragraph, because a repository named `zu-c` that does not contain `zu.h` owes the reader that sentence immediately.

**The conformance corpus stays here**, because it is versioned with the engine and gates engine releases. It ships as a release artifact every binding repository consumes. `tamnd/zu-kit` holds the runners, not the cases.

The resulting nine: `zu`, `zu-web`, `zu-c`, `zu-python`, `zu-node`, `zu-go`, `zu-java`, `zu-dotnet`, `zu-kit`.

## Consequences

Each client releases on its own schedule for its own fixes, and on the train's schedule for an engine version. The single version number across all nine is preserved by a conductor workflow: one tag here publishes the artifacts, dispatches to each client repository, collects their results, runs the corpus, and publishes in a fixed order with the Go tag last because it is the only irreversible step.

The cost that is real: a contributor fixing a typo in a doc comment files against `zu`, and one fixing a typo in a guide files against `zu-web`. Every page carries an "Edit this page" link that resolves to whichever repository actually owns it, which is the mitigation.

The cost that is dangerous: engine bugs accumulating in client trackers where engine maintainers never see them. This is the most common failure of a multi-repository client ecosystem, so every client repository's bug template asks first whether the problem reproduces through the `zu` CLI and routes it here if it does.

Configuration is applied identically at creation rather than per repository: public, Apache-2.0, protected `main`, seeded labels, wiki and projects off, one security policy pointing here.

## Rejected

**Everything in one repository.** Atomic cross-cutting changes are worth a lot, and Go alone makes it impossible: the module path is the repository path, so `github.com/tamnd/zu` would have to be a Go module at its root. It also means every Python or Node contributor clones the engine and its test datasets to fix a type stub.

**Splitting by first-party versus third-party.** Puts the C ABI out and the Rust SDK's location undecided, and gives no answer at all for the corpus or the header. Not a rule, just a list.

**Documentation in `tamnd/zu`.** `Spec/2064g/docs/02-platform-and-toolchain.md` originally argued this, on the grounds that docs-as-code dies when a doc fix becomes a second pull request in a second repository. That reasoning is wrong, and `duckdb-web` is the counterexample: a separate repository, and the most frequently pushed one in the DuckDB organisation. What kills docs-as-code is documentation that is not generated from the source of truth and therefore drifts silently. Distance in the filesystem is not the mechanism, absence of a gate is, so the gates move with the content: the completeness check runs inside the engine pull request against that pull request's own `model.json`, and fails on any documented symbol that disappeared.
