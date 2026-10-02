# zudb-encoding

The lightweight compression schemes that [zu](https://github.com/tamnd/zu) uses for columns on disk, and the cascade that picks one for each segment.

Each scheme is a pair of plain functions, `encode(values, &mut out)` and `decode(bytes, max_values, &mut out)`, over byte buffers. There is no I/O and no state. The goal for every decoder is at least 1 GB/s per core with no more than 64 KiB of scratch memory, which is why these are lightweight encodings and not general-purpose compression.

## Encodings

Every encoding has a stable one-byte `EncodingId`. It is written into the file in front of each segment, so the numbers never change once released.

| Id | Encoding | Module | Used for |
| --- | --- | --- | --- |
| 0 | Plain | `segment` | Fallback when nothing else is smaller |
| 1 | Constant | `segment` | A segment where every value is the same |
| 2 | RLE | `rle` | Long runs of repeated values |
| 3 | Dict | `dict` | Few distinct values (up to 8192), sorted so range filters stay cheap |
| 4 | Frame of reference + bit packing | `for_bitpack` | Values in a narrow range |
| 5 | Delta + bit packing | `delta` | Sorted or slowly growing values, like ids |
| 6 | ALP | `alp` | Doubles that are really decimals, like prices or coordinates |
| 7 | ALP_RD | `alp_rd` | Doubles that use their full mantissa |
| 8 | FSST | `fsst` | Strings, using a trained table of common substrings |
| 9 | Bool bit packing | `bool_bitpack` | 0 and 1 values, one bit each |
| 10 | Frequency | `frequency` | One dominant value and scattered exceptions |
| 11 | Zstd | `zstd_leaf` | Cold string segments where ratio matters more than speed |
| 12 | Delta + patched bit packing | `delta_patch` | Adjacency lists, where a rare large gap would otherwise widen every value |

Underneath them are a few building blocks: `bitpack` packs 1024 values at a fixed width in a FastLanes-style interleaved layout that the compiler auto-vectorizes, `patch` stores outliers as exceptions so they do not set the width of a whole chunk, and `validity` stores null bitmaps separately from values.

## The cascade

`segment::encode_auto` is what the storage engine calls for integer columns. It reads the input once, works out the encoded size of every candidate from what it saw (runs, distinct values, the dominant value, the min and width per 1024-value chunk), and only encodes with the winner. If the result ends up larger than Plain, it writes Plain instead. The id byte goes first, so `segment::decode_any` needs nothing else to read it back. Floats and strings go through `alp`, `alp_rd`, `fsst` and `zstd_leaf` directly.

Every decoder takes a `max_values` (or `max_bytes`) ceiling from the caller and checks every count in the payload against it before allocating, so a corrupt or hostile file is an error and not a huge allocation.

## Where it fits

`zudb-zu1`, the native file format, is the main user. The ids and layouts are specified in `docs/04-storage-zu1-format.md`. The `zstd` feature links libzstd for compression; without it, zstd segments still decode through the pure Rust `ruzstd`, so the default build can read every file.

## Example

```rust
use zu_encoding::segment::{decode_any, encode_auto};
use zu_encoding::{EncodingId, alp, fsst};

fn main() -> zu_common::Result<()> {
    // Integers: the cascade prices every candidate and keeps the cheapest.
    let ids: Vec<u64> = (1_000..3_048).collect();
    let mut seg = Vec::new();
    let chosen = encode_auto(&ids, &mut seg);
    println!("{chosen:?}: {} values in {} bytes", ids.len(), seg.len());

    let mut back = Vec::new();
    decode_any(&seg, ids.len(), &mut back)?;
    assert_eq!(back, ids);
    assert_eq!(EncodingId::try_from(seg[0])?, chosen);

    // Doubles that are really decimals, like prices.
    let prices = [19.99, 5.25, 100.0, 0.5];
    let mut buf = Vec::new();
    alp::encode(&prices, &mut buf);
    let mut out = Vec::new();
    alp::decode(&buf, prices.len(), &mut out)?;
    assert_eq!(out, prices);

    // Strings: FSST replaces common substrings with one-byte codes.
    let text = b"https://example.com/a https://example.com/b https://example.com/c";
    let mut packed = Vec::new();
    fsst::encode(text, &mut packed);
    let mut plain = Vec::new();
    fsst::decode(&packed, text.len(), &mut plain)?;
    assert_eq!(plain, text);
    Ok(())
}
```

On that input the cascade picks `DeltaBitPack` and stores 2048 ids in 287 bytes.

## Should you depend on this?

Probably not directly. This crate is published so that [`zudb`](https://crates.io/crates/zudb) can depend on it. Its API changes whenever the engine needs it to, with no deprecation period, and all the zu crates are released together with exact version pins between them. If you want to use zu from Rust, add `zudb` and use what it exports.

The package is named `zudb-encoding` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_encoding`.

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification.

## License

Apache-2.0
