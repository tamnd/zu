//! Fuzzes the pure decoders that sit behind the crc32c walls of the file
//! format: the catalog, the table index, the group directory, segment
//! metas, the encoding cascade, the free list, and the FullZip payload
//! decoder. `zu1_file` cannot
//! reach these with mutated bytes because a flipped bit fails the chain
//! crc first, so they get raw bytes here. The first byte routes to a
//! decoder, the rest is its input. Errors are expected; panics, hangs,
//! and unbounded allocation are findings.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zu_zu1::catalog::{Catalog, TableIndex};
use zu_zu1::file::decode_free_list;
use zu_zu1::fullzip;
use zu_zu1::graph::Directory;
use zu_zu1::segment::SegmentMeta;

/// The ceiling a real caller would pass: the row count its segment meta
/// promised, at most twice the 1024-row chunk size.
const MAX_VALUES: usize = 2048;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, bytes)) = data.split_first() else {
        return;
    };
    match sel % 7 {
        0 => {
            let _ = Catalog::decode(bytes);
        }
        1 => {
            let _ = TableIndex::decode(bytes);
        }
        2 => {
            let _ = Directory::decode(bytes);
        }
        3 => {
            let _ = SegmentMeta::decode(bytes, 0);
        }
        4 => {
            let mut out = Vec::new();
            let _ = zu_encoding::segment::decode_any(bytes, MAX_VALUES, &mut out);
        }
        5 => {
            let Some((count, rest)) = bytes.split_at_checked(8) else {
                return;
            };
            let block_count = u64::from_le_bytes(count.try_into().unwrap());
            let _ = decode_free_list(rest, block_count);
        }
        _ => {
            // The FullZip payload decoder, with the claims a real meta
            // would carry taken from the input so the fuzzer controls
            // them, bounded the way a group directory bounds real rows.
            let Some((claims, rest)) = bytes.split_at_checked(4) else {
                return;
            };
            let value_count = u64::from(u16::from_le_bytes(claims[..2].try_into().unwrap()));
            let uncompressed = u64::from(u16::from_le_bytes(claims[2..4].try_into().unwrap()));
            let mut out = Vec::new();
            let mut ends = Vec::new();
            let _ = fullzip::decode_payload(rest, value_count, uncompressed, &mut out, &mut ends);
        }
    }
});
