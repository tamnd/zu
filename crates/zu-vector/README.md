# zudb-vector

Typed columnar vectors and the expression kernels that run over them, for the [zu](https://github.com/tamnd/zu) query executor.

Instead of a row of tagged values per cell, a column is one contiguous buffer of one physical type, nulls are a separate bitmap, and a filter produces a list of row indices instead of copying rows. Kernels are plain Rust loops over those buffers, written so the compiler can auto-vectorize them.

## The pieces

| Type | What it is |
| --- | --- |
| `MorselArena` | A bump allocator in 256 KiB blocks. Every buffer for one morsel of work comes from it, and `reset` keeps the blocks, so a warm executor allocates nothing per morsel. |
| `ValueVector` | One column: a `PhysType`, an encoding (`Flat`, `Constant`, or `Dict` codes), the data buffer, and an optional validity `Bitmap`. |
| `PhysType` | `Bool`, `Int64`, `Float64`, `NodeRef`, `RelRef`, `Str`, `List`, `Interval`, `Date`, `Timestamp`. One word per value (bools are bit-packed, strings are 16 bytes). |
| `Bitmap` | One bit per row, used for validity and for predicate results. Null handling is a word-wise AND. |
| `SelVector` | A sorted list of `u16` row indices. Filters refine a selection and never move data. |
| `StrView`, `StrBuffers`, `Dictionary` | Strings as 16-byte views: up to 12 bytes inline, otherwise a 4-byte prefix plus a buffer id and offset. Dictionary-encoded strings stay as integer codes until the result is produced. |
| `DataChunk`, `ChunkSet` | A chunk is a set of vectors with one shared selection, usually `VECTOR_SIZE` (2048) rows. A chunk set is a factorized result: a flat chunk plus list-level chunks whose sizes multiply. |
| `Program`, `ExprOp` | An expression compiled into a short register program. Evaluating it walks the ops, not an expression tree. |
| `kernels` | Comparison, arithmetic, math, string, temporal, hashing, gather, set and aggregate kernels. |

## How data flows

The executor in `zudb-exec` splits a scan into morsels. For each morsel it takes an arena, reads columns from storage into `ValueVector`s, and builds a `DataChunk`. A WHERE clause is a `Program` that ends in a comparison, so `eval_filter` returns a `Bitmap`, which becomes a `SelVector` on the chunk. Projections are programs evaluated with `eval`, which return a new vector. When the morsel is done the results are copied into the sink and the arena is reset for the next one.

Buffers from the arena are not tied to it by a lifetime. The rule is that a vector never outlives its morsel unless it is copied out, and debug builds check that on every access.

## Where it fits

It depends only on `zudb-common`. `zudb-exec` is built on it, and `zudb-query` and `zudb` use it for the vectorized snapshot reads.

## Example

```rust
use zu_vector::{
    BinOp, CmpOp, DataChunk, ExprOp, MorselArena, OwnedValue, PhysType, Program, SelVector,
    ValueVector,
};

fn main() -> zu_common::Result<()> {
    // Every buffer for one morsel comes from this arena.
    let mut arena = MorselArena::new();
    let ages = ValueVector::flat_from(&mut arena, PhysType::Int64, &[17i64, 34, 52, 8]);
    let chunk = DataChunk::new(vec![ages], 4);

    // WHERE age >= 18, compiled once into a register program.
    let filter = Program {
        ops: vec![
            ExprOp::LoadCol { col: 0, dst: 0 },
            ExprOp::LoadConst { v: OwnedValue::Int(18), dst: 1 },
            ExprOp::Compare { op: CmpOp::Ge, l: 0, r: 1, dst: 2 },
        ],
        regs: 3,
    };
    let bits = filter.eval_filter(&chunk, &mut arena)?;
    let sel = SelVector::from_bitmap(&mut arena, &bits);
    assert_eq!(sel.as_slice(), &[1, 2]);

    // RETURN age * 12, evaluated a whole vector at a time.
    let months = Program {
        ops: vec![
            ExprOp::LoadCol { col: 0, dst: 0 },
            ExprOp::LoadConst { v: OwnedValue::Int(12), dst: 1 },
            ExprOp::Binary { op: BinOp::Mul, l: 0, r: 1, dst: 2 },
        ],
        regs: 3,
    };
    let out = months.eval(&chunk, &mut arena)?;
    assert_eq!(out.values::<i64>(), &[204, 408, 624, 96]);

    // The next morsel reuses the same memory.
    arena.reset();
    Ok(())
}
```

## Should you depend on this?

Probably not directly. This crate is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and all the zu crates are released together with exact version pins between them. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-vector` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_vector`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
