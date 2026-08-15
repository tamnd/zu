//! `zu stat` is where the store's size breakdown is published, and the
//! JSON form of it is read by the gql-compat harness rather than by a
//! person. That makes its field names part of a contract with another
//! repository, so they are pinned here.

use std::process::Command;

fn seeded(path: &std::path::Path) {
    let mut db = zu::zu1::file::Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
    edges.sort_unstable();
    edges.dedup();
    zu::zu1::graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
}

fn stat(path: &std::path::Path, format: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("stat")
        .arg(path)
        .args(format)
        .output()
        .expect("run zu stat");
    assert!(
        out.status.success(),
        "zu stat failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

/// The value of `key` in a flat JSON object of numbers.
fn number(json: &str, key: &str) -> u64 {
    let at = json
        .find(&format!("\"{key}\":"))
        .unwrap_or_else(|| panic!("no {key} in {json}"))
        + key.len()
        + 3;
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|e| panic!("{key} is not a number in {json}: {e}"))
}

#[test]
fn the_text_form_splits_the_store_into_schema_free_and_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stat.zu1");
    seeded(&path);
    let text = stat(&path, &[]);
    for line in ["size:", "  schema:", "  free:", "  data:"] {
        assert!(text.contains(line), "no {line} line in\n{text}");
    }
    // The high-water mark and the block count differ by one, which is
    // block zero, and the two used to sit on adjacent lines under names
    // that said they were the same thing.
    assert!(text.contains("high water:"), "{text}");
    // The catalog objects a file holds, which for a bulk loaded file is
    // the root directory and the home graph its two tables are in.
    assert!(text.contains("directory:       /"), "{text}");
    assert!(
        text.contains("graph:           home in / (any, 2 tables)"),
        "{text}"
    );
}

#[test]
fn the_json_form_carries_the_fields_the_harness_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stat.zu1");
    seeded(&path);
    let json = stat(&path, &["--format", "json"]);
    assert!(json.starts_with('{'), "{json}");

    // The three parts of the store account for all of it. A harness
    // subtracting schema_bytes from a measured store size is relying on
    // exactly this, so a change that made the parts overlap or leave a
    // remainder would silently move every bits-per-edge figure.
    assert_eq!(
        number(&json, "bytes"),
        number(&json, "schema_bytes") + number(&json, "free_bytes") + number(&json, "data_bytes"),
        "{json}"
    );
    assert_eq!(
        number(&json, "bytes"),
        number(&json, "blocks") * number(&json, "block_size"),
        "{json}"
    );
    assert!(number(&json, "schema_bytes") > 0, "{json}");
    assert!(
        number(&json, "schema_bytes") < number(&json, "bytes"),
        "the whole store is schema: {json}"
    );

    // The catalog rides along because a caller dividing by the graph
    // needs the graph, and asking twice over two commands invites the
    // two answers to come from different epochs.
    assert!(
        json.contains("\"node_tables\":[{\"name\":\"person\""),
        "{json}"
    );
    assert!(
        json.contains("\"rel_tables\":[{\"name\":\"follows\""),
        "{json}"
    );
    assert!(json.contains("\"edges\":"), "{json}");
}

#[test]
fn an_unknown_format_is_a_usage_error_not_a_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stat.zu1");
    seeded(&path);
    let out = Command::new(env!("CARGO_BIN_EXE_zu"))
        .arg("stat")
        .arg(&path)
        .args(["--format", "yaml"])
        .output()
        .expect("run");
    assert!(!out.status.success(), "an unknown format was accepted");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage:"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
