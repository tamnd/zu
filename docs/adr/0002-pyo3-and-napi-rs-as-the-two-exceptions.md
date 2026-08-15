# 0002. Python and JavaScript bind through PyO3 and napi-rs, not the C ABI

Status: accepted, 2026-08-15. Supplements ADR 0001. Specs: `Spec/2064g/dx/06-python.md`, `Spec/2064g/dx/05-typescript.md`.

## Context

ADR 0001 says every non-Rust client goes through the C ABI. Two languages are worth an exception, and it is better to name them and say why than to let the rule quietly erode later.

Python and JavaScript are the two languages where the runtime, not the FFI, is where the hard problems are. In Python those problems are the GIL, the buffer protocol, exception chaining, the `__init_subclass__`-shaped edges of the object model, free-threaded builds, and the abi3 stable-ABI wheel story. In JavaScript they are the event loop, the threadpool, N-API's version negotiation, `AbortSignal`, and the fact that a synchronous native call in a server is a production incident.

Going through the C ABI in these two would mean writing that runtime layer by hand in C, twice, and then keeping it correct across CPython 3.11 to 3.15 and across Node, Bun, and Deno. PyO3 and napi-rs are each many years of exactly that work, and both are Rust-to-runtime rather than Rust-to-C-to-runtime, so binding through `libzu`'s C ABI would mean paying the transition cost and still writing the runtime layer.

## Decision

`zu-python` binds with PyO3 and maturin. `zu-node` binds with napi-rs. Both link the Rust crates directly rather than `libzu`'s C ABI. Every other client, including Go, Java, .NET, and every tier-3 language, goes through the C ABI as ADR 0001 says.

The exception is bounded in one specific way: these two bindings still implement the semantics the ABI defines, and they still run the same conformance corpus as every other client. The exception is about the mechanism, not the contract. A behavioural difference between `zu-python` and `zu-go` is a bug in one of them, and the corpus is what says which.

## Consequences

Python gets zero-copy Arrow, a released GIL around every engine call, real exception chaining, and wheels that work with no compiler on the user's machine. JavaScript gets calls that never block the event loop, N-API's ABI stability across Node and Electron versions, and prebuilt binaries with no `node-gyp`.

Both bindings must be built by the same Rust toolchain as the engine, which is fine because both ship prebuilt artifacts and neither expects a user to compile anything. Both are more exposed to engine-internal changes than an ABI consumer, so both are built and tested in the release train against the engine commit they will ship with.

The rule is now a rule with two named exceptions, which is a thing that decays if nobody guards it. The guard is this record: a third exception needs its own ADR arguing the same case, and "the ABI was inconvenient" is not that case.

## Rejected

**cffi or ctypes for Python.** Works, and is what the tier-3 kit offers. It is not competitive for a tier-1 client: no zero-copy buffer protocol without hand-written glue, a per-call cost that shows up in a row loop, and no clean story for releasing the GIL.

**A hand-written C extension for Python, or a hand-written N-API addon.** This is the honest alternative and it is what several database clients do. It means owning several thousand lines of C whose failure mode is memory corruption in someone else's process, maintained across five CPython versions and three JS runtimes, to avoid a dependency that exists specifically to do that. Not worth it.
