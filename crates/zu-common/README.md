# zudb-common

The shared vocabulary of [zu](https://github.com/tamnd/zu), an embedded property-graph database: ids, the error type, the type model, and the value types (decimals, dates and times, byte strings) that every other crate in the workspace needs to agree on.

It is the bottom of the dependency graph. It depends only on `thiserror`, and every other zu crate depends on it.

## What is in it

| Item | What it is |
| --- | --- |
| `NodeId`, `RelId`, `TableId`, `NodeGroupId`, `NodeOffset`, `Epoch` | Graph identifiers. `NodeId` packs a table, a node group and a row into one `u64`. |
| `ZuError`, `Result` | The error every zu crate returns. Query errors carry a GQLSTATUS diagnostic. |
| `gqlstatus` | The ISO/IEC 39075 condition codes, generated from the standard's own table, and the `DiagnosticRecord` that carries one back to a caller. |
| `types` | The two layer type model: `LogicalType` is what the language talks about, `PhysicalType` is what a vector holds. |
| `Decimal` | Exact decimals as an `i128` and a scale, up to 38 digits. |
| `temporal` | Dates, times, datetimes and durations, stored as plain counts (days, nanoseconds, months). |
| `bytes`, `unicode`, `keywords` | Byte string literals, Unicode normalization (NFC, NFD, NFKC, NFKD), and the GQL reserved word lists. |
| `Interrupt` | A handle a caller keeps on a running statement to stop it or read its progress. |
| `IdMap`, `IdSet` | Hash maps and sets with a cheap hasher for keys that come from the catalog, not from users. |

## Ids

`NodeId` is the one layout that is written to disk by every storage engine, so it does not change between versions:

```text
bits 63..50  table_id     (14 bits, up to 16 384 tables)
bits 49..28  node_group   (22 bits, up to 4 194 304 groups per table)
bits 27..11  row          (17 bits, GROUP_ROWS = 131 072 rows per group)
bits 10..0   reserved     (must be zero)
```

A node group is the unit storage works in: a table is split into groups of `GROUP_ROWS` rows, and each group's columns are stored and compressed together. The full data model is in `docs/03-data-model.md`.

## Values

The value types are built so that comparing two values is comparing two integers. A date is a day count since 1970-01-01, a local time is nanoseconds since midnight, and a duration is either a month count or a nanosecond count depending on its kind. Calendar arithmetic only happens when text is parsed or printed. `Decimal` works the same way: `1.20` and `1.2` compare equal, and each prints back the way it was written.

## Errors

`ZuError` has a variant for I/O, corrupt data, unsupported format ids, invalid arguments, write conflicts and interrupts. Errors raised by a query are `ZuError::Gql` and carry a GQLSTATUS code, a severity, and optionally a position in the statement text, so a client can point at the token that failed instead of parsing the message.

## Example

```rust
use zu_common::gqlstatus::codes;
use zu_common::{Decimal, LogicalType, NodeId, Temporal, ZuError};

fn main() {
    // A node id packs the table, the node group and the row into one u64.
    let id = NodeId::new(3, 7, 42);
    assert_eq!((id.table(), id.group(), id.row()), (3, 7, 42));

    // Decimals are exact: an i128 and a scale, never a float.
    let price = Decimal::parse("19.99", 2).unwrap();
    let tax = Decimal::parse("1.60", 2).unwrap();
    println!("{}", price.add(&tax).unwrap()); // 21.59

    // A date is a day count, so comparing two dates compares two integers.
    let day = Temporal::parse(&LogicalType::Date, "2026-10-02").unwrap();
    println!("{day}");

    // Errors carry a GQLSTATUS code from ISO/IEC 39075.
    let err = ZuError::gql(codes::C22012, "x / 0");
    assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22012"));
    println!("{err}");
}
```

## Should you depend on this?

Probably not directly. This crate is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and all the zu crates are released together with exact version pins between them. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-common` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_common`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
