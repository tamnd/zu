//! Writes seed corpora for the fuzz targets: a real two-table database
//! for `zu1_file`, the chain payloads and encoded containers it contains
//! for `zu1_decode`, and encoder output for the per-encoding targets.
//! Mutation from valid bytes reaches format logic that random input
//! never would, since every chain and segment sits behind a crc32c.
//! Run with `cargo run --bin seeds` from the fuzz directory.

use std::fs;
use std::path::Path;

use zu_zu1::file::Zu1File;
use zu_zu1::{graph, meta, segment};

fn write_seed(dir: &Path, name: &str, bytes: &[u8]) {
    fs::create_dir_all(dir).expect("corpus dir");
    fs::write(dir.join(name), bytes).expect("seed file");
}

/// Prefixes the router byte the `zu1_decode` target strips.
fn routed(selector: u8, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + bytes.len());
    out.push(selector);
    out.extend_from_slice(bytes);
    out
}

fn main() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("seed.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    let mut follows: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    follows.sort_unstable();
    follows.dedup();
    // A key index on one of the two tables so the directory seed covers
    // the has_keys branch; 13 is coprime to 97, so the keys are unique.
    let keys: Vec<u64> = (0..97u64).map(|i| (i * 13 + 5) % 97).collect();
    graph::bulk_load_keyed(&mut db, "person", "follows", 97, &follows, Some(&keys))
        .expect("load follows");
    let mut likes: Vec<(u32, u32)> = (0..200u32).map(|i| (i % 61, (i * 13 + 5) % 97)).collect();
    likes.sort_unstable();
    likes.dedup();
    graph::bulk_load_as(&mut db, "person", "likes", 97, &likes).expect("load likes");

    let catalog_root = db.db_header().catalog_root;
    let index_root = db.db_header().table_index_root;
    let free_root = db.db_header().free_list_root;
    let catalog_bytes = meta::read_chain(&mut db, catalog_root).expect("catalog");
    let index_bytes = meta::read_chain(&mut db, index_root).expect("index");
    let index = zu_zu1::catalog::TableIndex::load(&mut db).expect("index load");
    let directory_root = index.entries()[0].1;
    let directory_bytes = meta::read_chain(&mut db, directory_root).expect("directory");
    let free_bytes = meta::read_chain(&mut db, free_root).expect("free list");
    drop(db);

    let file_dir = corpus.join("zu1_file");
    let file_bytes = fs::read(&path).expect("read seed db");
    write_seed(&file_dir, "two-tables.zu1", &file_bytes);
    write_seed(&file_dir, "headers-only.zu1", &file_bytes[..12288]);

    let decode_dir = corpus.join("zu1_decode");
    write_seed(&decode_dir, "catalog", &routed(0, &catalog_bytes));
    write_seed(&decode_dir, "table-index", &routed(1, &index_bytes));
    write_seed(&decode_dir, "directory", &routed(2, &directory_bytes));
    let meta = segment::SegmentMeta {
        value_count: 5000,
        payload_len: 40_000,
        uncompressed_bytes: 40_000,
        min: 7,
        max: 40_000,
        crc: 0xC0FFEE,
        blocks: vec![3],
    };
    let mut meta_bytes = Vec::new();
    meta.encode(&mut meta_bytes);
    write_seed(&decode_dir, "segment-meta", &routed(3, &meta_bytes));
    let mut free_seed = 64u64.to_le_bytes().to_vec();
    free_seed.extend_from_slice(&free_bytes);
    write_seed(&decode_dir, "free-list", &routed(5, &free_seed));

    // Encoded containers: one input per shape the cascade picks between.
    let shapes: &[(&str, Vec<u64>)] = &[
        ("sorted", (0..3000u64).map(|i| 1000 + i * 3).collect()),
        ("constant", vec![42; 2000]),
        ("runs", (0..2000u64).map(|i| i / 250).collect()),
        ("dictish", (0..2000u64).map(|i| (i * 7) % 16).collect()),
        ("binary", (0..2000u64).map(|i| (i * i / 7) & 1).collect()),
        (
            "dominant",
            (0..2000u64)
                .map(|i| if i % 9 == 4 { i * 40_009 } else { 777 })
                .collect(),
        ),
        ("adjacency", {
            let mut v = Vec::new();
            for list in 0..150u64 {
                let anchor = (list * 40_009) % 4_000_000;
                v.push(anchor % 1000);
                for k in 0..13 {
                    v.push(anchor + k * 17 + 1);
                }
            }
            v
        }),
    ];
    for (name, values) in shapes {
        let mut buf = Vec::new();
        zu_encoding::segment::encode_auto(values, &mut buf);
        write_seed(&decode_dir, &format!("cascade-{name}"), &routed(4, &buf));
        write_seed(&corpus.join("decode_segment"), name, &buf);
        let mut fb = Vec::new();
        zu_encoding::for_bitpack::encode(values, &mut fb);
        write_seed(&corpus.join("decode_for_bitpack"), name, &fb);
        let mut d = Vec::new();
        zu_encoding::delta::encode(values, &mut d);
        write_seed(&corpus.join("decode_delta"), name, &d);
        let mut dp = Vec::new();
        zu_encoding::delta_patch::encode(values, &mut dp);
        write_seed(&corpus.join("decode_delta_patch"), name, &dp);
    }

    // The typed encoders only accept their own shapes, so they seed from
    // the matching entry instead of every shape in the loop.
    let binary = &shapes.iter().find(|(n, _)| *n == "binary").unwrap().1;
    let mut bb = Vec::new();
    zu_encoding::bool_bitpack::encode(binary, &mut bb);
    write_seed(&corpus.join("decode_bool_bitpack"), "binary", &bb);
    let dominant = &shapes.iter().find(|(n, _)| *n == "dominant").unwrap().1;
    let mut fq = Vec::new();
    zu_encoding::frequency::encode(dominant, &mut fq);
    write_seed(&corpus.join("decode_frequency"), "dominant", &fq);
    let mut text = Vec::new();
    for &(s, d) in &follows {
        text.extend_from_slice(format!("{s}\t{d}\n").as_bytes());
    }
    let mut fs = Vec::new();
    zu_encoding::fsst::encode(&text, &mut fs);
    write_seed(&corpus.join("decode_fsst"), "edge-text", &fs);
    println!("seeds written under {}", corpus.display());
}
