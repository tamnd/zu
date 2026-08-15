# 0001. The C ABI is the binding substrate, not the Rust API

Status: accepted, 2026-08-15. Specs: `Spec/2064g/dx/02-c-abi.md`, `Spec/2064g/dx/03-binding-architecture.md`.

## Context

zu is written in Rust and every client we intend to ship is written in something else. There are two ways to get from one to the other. Each binding can link the Rust crate directly and expose it through whatever FFI its own language offers, or every binding can go through one C ABI that the engine exports and that the engine is then obliged to keep stable.

The direct route is genuinely tempting at the start. It skips a layer, it keeps Rust types visible for longer, and the first binding written that way is quicker to get running than the first binding written against a header that does not exist yet.

It stops being tempting at the second binding. Rust has no stable ABI, so every consumer has to be compiled by the same toolchain against the same crate versions, which means the Go, Java, and .NET stories become "build the engine from source with a matching Rust" rather than "link a library". It also means there is no single place where lifetime, threading, and error semantics are written down. Each binding rediscovers them, and they disagree in ways nobody notices until two clients give different answers to the same query.

## Decision

`libzu` exports a C ABI, declared in a generated `zu.h`, and that ABI is the contract every non-Rust client is written against. It is versioned, it is frozen at v1.0, and it is the thing the conformance corpus is ultimately measuring.

The semantics live with the ABI rather than with each binding: who owns a pointer, when a handle may be used from another thread, what happens to a result when its connection closes, and how an error carries its GQLSTATUS condition. A binding implements those semantics; it does not invent them.

This is also the line DuckDB draws, and nine years of them running it is the strongest evidence available that it holds up.

## Consequences

Every client gets the same semantics for free, and a bug in them is fixed once. A prebuilt `libzu` is enough to run any client, so installing a client does not require a Rust toolchain. Tier-3 languages become possible at all, because a community binding needs a header and a test corpus rather than a Rust build.

The costs are real and are accepted. There is one more layer to cross, which is a per-call transition cost that matters most in the languages with the most expensive FFI, and the answer to it is batching at the ABI rather than pretending the cost is not there: iteration pulls a chunk per crossing, column access crosses once per column, and the appender buffers. Every new engine feature has to be given a C shape before any client can reach it, which slows the first client and speeds up the fifth. And the ABI cannot be casually changed after v1.0, which is the point of it.

## Rejected

**Rust-crate-per-binding.** Rejected on ABI instability, on the toolchain burden it puts on every user, and on the semantic drift between bindings that nothing would catch.

**A server protocol instead of an ABI.** An embedded database whose clients talk to it over a socket is not an embedded database. Arrow Flight SQL is offered as an interoperability surface for BI tools (`Spec/2064g/dx/13-integrations.md` §5), and that is a different thing from the binding path.
