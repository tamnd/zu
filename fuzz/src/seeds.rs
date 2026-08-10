//! Writes seed corpora for the fuzz targets: a real two-table database
//! for `zu1_file`, the chain payloads and encoded containers it contains
//! for `zu1_decode`, and encoder output for the per-encoding targets.
//! Mutation from valid bytes reaches format logic that random input
//! never would, since every chain and segment sits behind a crc32c.
//! Run with `cargo run --bin seeds` from the fuzz directory.

use std::fs;
use std::path::Path;

use zu_zu1::file::Zu1File;
use zu_zu1::{fullzip, graph, meta, segment};

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
        structural: segment::Structural::MiniBlock,
        sorted: true,
        blocks: vec![3],
    };
    let mut meta_bytes = Vec::new();
    meta.encode(&mut meta_bytes);
    write_seed(&decode_dir, "segment-meta", &routed(3, &meta_bytes));
    let mut free_seed = 64u64.to_le_bytes().to_vec();
    free_seed.extend_from_slice(&free_bytes);
    write_seed(&decode_dir, "free-list", &routed(5, &free_seed));

    // A valid FullZip payload behind the claims prefix the target reads:
    // value count then total value bytes, both u16.
    let strings: Vec<Vec<u8>> = (0..1200u64)
        .map(|i| format!("https://example.com/user/{}/posts?page={}", i * 37, i % 12).into_bytes())
        .collect();
    let string_refs: Vec<&[u8]> = strings.iter().map(|v| v.as_slice()).collect();
    let mut fullzip_payload = Vec::new();
    let value_bytes =
        fullzip::encode_payload(&string_refs, &mut fullzip_payload).expect("fullzip payload");
    let mut fullzip_seed = (strings.len() as u16).to_le_bytes().to_vec();
    fullzip_seed.extend_from_slice(&(value_bytes as u16).to_le_bytes());
    fullzip_seed.extend_from_slice(&fullzip_payload);
    write_seed(&decode_dir, "fullzip-payload", &routed(6, &fullzip_seed));

    // The roundtrip target parses u16-length-prefixed rows.
    let roundtrip_dir = corpus.join("fullzip_roundtrip");
    let mut roundtrip_seed = Vec::new();
    for row in string_refs.iter().take(40) {
        roundtrip_seed.extend_from_slice(&(row.len() as u16).to_le_bytes());
        roundtrip_seed.extend_from_slice(row);
    }
    write_seed(&roundtrip_dir, "urls", &roundtrip_seed);

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
    let mut zs = Vec::new();
    zu_encoding::zstd_leaf::encode(&text, &mut zs);
    write_seed(&corpus.join("decode_zstd"), "edge-text", &zs);
    let coords: Vec<f64> = follows
        .iter()
        .map(|&(s, d)| f64::from(s) + f64::from(d) / 1e5)
        .collect();
    let mut al = Vec::new();
    zu_encoding::alp::encode(&coords, &mut al);
    write_seed(&corpus.join("decode_alp"), "coords", &al);
    let radians: Vec<f64> = coords.iter().map(|d| d.to_radians()).collect();
    let mut rd = Vec::new();
    zu_encoding::alp_rd::encode(&radians, &mut rd);
    write_seed(&corpus.join("decode_alp_rd"), "radians", &rd);

    // Real query shapes for the parser target, LDBC interactive style,
    // so mutation starts from text that reaches deep into the grammar.
    let queries: &[(&str, &str)] = &[
        (
            "is1-profile",
            "MATCH (n:Person {id: $personId})-[:IS_LOCATED_IN]->(p:Place) \
             RETURN n.firstName AS firstName, n.lastName AS lastName, p.id AS cityId",
        ),
        (
            "ic1-friends",
            "MATCH (p:Person {id: $personId})-[:KNOWS*1..3]-(friend:Person {firstName: $firstName}) \
             WHERE friend.id <> $personId \
             RETURN DISTINCT friend.id AS friendId, friend.lastName AS lastName \
             ORDER BY friendId ASC LIMIT 20",
        ),
        (
            "aggregate",
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS friends WHERE friends > 5 \
             RETURN a.name, friends ORDER BY friends DESC, a.name SKIP 2 LIMIT 10",
        ),
        (
            "unwind-expr",
            "UNWIND [1, 2.5, 'x', NULL, $p] AS v \
             MATCH (n) WHERE NOT n.age + 1 * 2 >= 10 AND n.name STARTS WITH 'A' \
             OR n.id IN [1, 2] AND n.bio IS NOT NULL RETURN v, n LIMIT 1",
        ),
        (
            "paths",
            "MATCH q = (a)<-[r:LIKES|FOLLOWS {since: 2020}]-(b), (b)-[*..4]-(c:`odd label`) \
             OPTIONAL MATCH (c)--(d) RETURN *",
        ),
    ];
    for (name, text) in queries {
        write_seed(&corpus.join("zuql_parse"), name, text.as_bytes());
    }
    println!("seeds written under {}", corpus.display());
}
