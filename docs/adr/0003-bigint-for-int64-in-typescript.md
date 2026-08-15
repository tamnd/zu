# 0003. INT64 is `bigint` in TypeScript, by default and always

Status: accepted, 2026-08-15. Spec: `Spec/2064g/dx/05-typescript.md`.

## Context

JavaScript numbers are IEEE doubles. Integers above 2^53 do not survive the trip. zu's INT64 goes to 2^63, node offsets and internal ids live in that range, and `count(*)` over a large graph gets there on its own.

There are three ways a client can handle this. Return `number` and lose precision silently. Return `number` below the safe threshold and `bigint` above it, so the type depends on the data. Or return `bigint` always.

The second option is the one that looks friendly and is the worst of the three. A union type that varies per row means every consumer writes a type guard or gets a runtime error on the day their ids grow, and the code that was tested on a small graph is exactly the code that breaks on a large one.

The first option is what several JavaScript database clients do, and it is why "the id came back wrong" is a recurring bug report in that ecosystem.

## Decision

INT64 is `bigint`. Always, whatever the value, whatever the runtime.

A `{ bigIntMode: "number" }` connection option exists for people who know their data fits and want arithmetic without conversions. It is documented alongside its precision hazard, it is never the default, and there is no automatic or per-value switching.

## Consequences

`count(*)` returns a `bigint`, which surprises a developer once, in the first five minutes, with a clear type error at compile time. That is the trade being made: one surprise up front, in the place where surprises are cheap, instead of a corrupted identifier in production, in the place where they are not.

`bigint` does not mix with `number` in arithmetic and does not serialize through `JSON.stringify`, so the client documents both and ships a `toJSON` helper. Both are ordinary JavaScript facts that anyone using `bigint` for money or ids already knows.

`bigint` allocates where a small `number` would not, so the columnar path exposes `BigInt64Array` for callers who want a typed array rather than a row of boxed values, which is the shape a hot loop should be using anyway.

## Rejected

**`number` by default.** Silent corruption above 2^53. Not a defensible default for a database client.

**A union that varies by value.** Every call site needs a guard, and the failure surfaces only on data the developer did not have when they wrote the code. Worse than either consistent option.

**A string for INT64.** Precise, ordering-safe on the wire, and useless: nobody wants to parse a string to do arithmetic, and it makes the common case worse to protect the rare one.
