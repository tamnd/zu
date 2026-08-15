# Architecture decision records

An ADR here records a decision that is expensive to reverse and that somebody will otherwise re-open in six months without the context that settled it. The specifications in `docs/` say what the system does; an ADR says why one of the forks in the road was taken and what it cost.

The bar for writing one is narrow on purpose. A decision earns an ADR when reversing it would break a published interface, a file format, or somebody else's repository. Everything smaller belongs in a doc comment or in the specification chapter it affects.

The format is the same every time: context, the decision, what it costs, and what was rejected. Status is one of proposed, accepted, or superseded, and a superseded record says which ADR replaced it and stays in place, because a decision log that deletes its own history is a decision log nobody trusts.

Numbers are allocated in order and never reused.

| # | Decision | Status |
|---|---|---|
| [0001](0001-c-abi-over-rust-only-bindings.md) | The C ABI is the binding substrate, not the Rust API | accepted |
| [0002](0002-pyo3-and-napi-rs-as-the-two-exceptions.md) | Python and JavaScript bind through PyO3 and napi-rs, not the C ABI | accepted |
| [0003](0003-bigint-for-int64-in-typescript.md) | INT64 is `bigint` in TypeScript, by default and always | accepted |
| [0004](0004-panama-over-jni.md) | The JVM binds through Panama, with JNI as a fallback provider | accepted |
| [0005](0005-the-repository-split.md) | The ABI is the repository boundary | accepted |
