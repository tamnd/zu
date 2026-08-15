//! `conformance-<version>.tar.zst`, the corpus as a release artifact.
//!
//! The cases live in this repository because they are versioned with
//! the engine and gate its releases (ADR 0005). Eight other repositories
//! have to run them, and none of them can reach into this one: a client
//! pins an engine version and needs the cases that shipped with it, not
//! the cases on this repository's main branch, which are the cases for
//! a version it has not adopted. So the corpus is published the same
//! way the engine is, as one file per release, named for the version it
//! belongs to.
//!
//! The archive is deliberately dull. A tar of the case files exactly as
//! they are written here, their README beside them so an unpacked
//! artifact explains itself, and a manifest that says what should be in
//! it. Every language has a tar reader and a zstd reader, which is the
//! whole reason for the format; nothing in here needs a zu client to
//! open.
//!
//! Two properties are worth more than they cost. The archive is
//! reproducible, which is the shared tar writer's doing and the reason
//! a mirror can be compared against a release rather than trusted. And
//! the packer parses every case before it ships one, so a corpus that
//! does not load cannot become an artifact that eight repositories fail
//! on.

use std::path::Path;

use zu_json::Json;

use crate::tarball;

/// The manifest's schema version, which moves when the shape of the
/// archive changes and not when the cases do. A client that unpacks an
/// artifact it does not understand should say so rather than run half
/// of it.
pub const SCHEMA: i64 = 1;

/// One case file in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub cases: usize,
    pub bytes: usize,
    /// A CRC32C of the file, which catches a truncated download and a
    /// corrupted unpack. It is not a signature and does not pretend to
    /// be one: what a release is signed with is the release's business,
    /// and a checksum inside the archive could only ever agree with an
    /// archive that was rewritten wholesale.
    pub crc: u32,
}

/// A packed corpus, and what went into it.
#[derive(Debug, Clone)]
pub struct Packed {
    /// The `.tar.zst` bytes.
    pub archive: Vec<u8>,
    /// The uncompressed tar, which is what the reproducibility test
    /// compares and what the compression ratio is measured against.
    pub tar: Vec<u8>,
    pub manifest: String,
    pub entries: Vec<Entry>,
    /// The directory every path in the archive is under, which is the
    /// artifact's own name without its extensions.
    pub prefix: String,
}

impl Packed {
    /// The total number of cases, which is the number a release note
    /// cites and the number a client checks it ran.
    pub fn cases(&self) -> usize {
        self.entries.iter().map(|e| e.cases).sum()
    }
}

/// Packs the corpus at `dir` for `version`, with `readme` beside the
/// cases if it is there.
///
/// Every case file is parsed on the way in. That is the whole of the
/// validation and it is deliberately the runner's own reader rather
/// than a second one: an artifact that the reader in this repository
/// cannot read is one the eight readers written against it have no
/// chance with.
pub fn pack(dir: &Path, readme: Option<&Path>, version: &str) -> Result<Packed, String> {
    if version.is_empty() || version.contains(['/', '\\', ' ']) {
        return Err(format!("{version:?} is not a version"));
    }
    let suites = zu_corpus::load(dir)?;
    let prefix = format!("conformance-{version}");

    let mut entries = Vec::with_capacity(suites.len());
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(suites.len() + 2);
    for suite in &suites {
        let path = dir.join(format!("{}.{}", suite.name, zu_corpus::EXTENSION));
        let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let in_archive = format!("cases/{}.{}", suite.name, zu_corpus::EXTENSION);
        entries.push(Entry {
            name: suite.name.clone(),
            path: in_archive.clone(),
            cases: suite.cases.len(),
            bytes: bytes.len(),
            crc: crc32c::crc32c(&bytes),
        });
        files.push((in_archive, bytes));
    }

    let manifest = manifest(version, &entries);
    // The manifest goes first so that a reader streaming the archive
    // knows what it is holding before it holds any of it.
    files.insert(
        0,
        ("manifest.json".to_string(), manifest.clone().into_bytes()),
    );
    if let Some(readme) = readme {
        let bytes =
            std::fs::read(readme).map_err(|e| format!("reading {}: {e}", readme.display()))?;
        files.insert(1, ("README.md".to_string(), bytes));
    }

    // Everything in the archive is under one directory, so that
    // unpacking it in a directory that already has things in it puts the
    // corpus somewhere rather than everywhere.
    let under: Vec<(String, Vec<u8>)> = files
        .into_iter()
        .map(|(name, bytes)| (format!("{prefix}/{name}"), bytes))
        .collect();
    let tar = tarball::tar(&under)?;
    let archive = tarball::compress(&tar)?;
    Ok(Packed {
        archive,
        tar,
        manifest,
        entries,
        prefix,
    })
}

/// What the archive says is in it.
fn manifest(version: &str, entries: &[Entry]) -> String {
    let suites = entries
        .iter()
        .map(|e| {
            Json::Obj(vec![
                ("name".to_string(), Json::Str(e.name.clone())),
                ("path".to_string(), Json::Str(e.path.clone())),
                ("cases".to_string(), Json::Int(e.cases as i64)),
                ("bytes".to_string(), Json::Int(e.bytes as i64)),
                ("crc32c".to_string(), Json::Str(format!("{:08x}", e.crc))),
            ])
        })
        .collect();
    let total: usize = entries.iter().map(|e| e.cases).sum();
    let doc = Json::Obj(vec![
        ("schema".to_string(), Json::Int(SCHEMA)),
        ("version".to_string(), Json::Str(version.to_string())),
        (
            "case_schema".to_string(),
            Json::Int(zu_corpus::case::SCHEMA),
        ),
        ("cases".to_string(), Json::Int(total as i64)),
        ("suites".to_string(), Json::Arr(suites)),
    ]);
    let mut text = doc.to_pretty();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// A directory of cases, written where the test can pack it.
    fn corpus(name: &str, suites: &[(&str, usize)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zu-pack-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
        for (suite, cases) in suites {
            let mut text = format!(
                "schema: 1\nsuite: {suite}\ndoc: A suite the packing test wrote, which exists so \
                 the packer has something to pack.\n\ncases:\n"
            );
            for n in 0..*cases {
                text.push_str(&format!(
                    "  - name: case-{n}\n    doc: A case the packing test wrote, which is here so \
                     the count in the manifest has something to count.\n    query: RETURN {n} AS \
                     n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: \
                     INT64\n            value: \"{n}\"\n"
                ));
            }
            std::fs::write(dir.join(format!("{suite}.yaml")), text).expect("writes");
        }
        dir
    }

    use tarball::entries;

    #[test]
    fn the_archive_holds_the_cases_byte_for_byte() {
        let dir = corpus("bytes", &[("alpha", 2), ("beta", 1)]);
        let packed = pack(&dir, None, "1.2.3").expect("packs");
        let files = entries(&packed.tar);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "conformance-1.2.3/manifest.json",
                "conformance-1.2.3/cases/alpha.yaml",
                "conformance-1.2.3/cases/beta.yaml",
            ]
        );
        let on_disk = std::fs::read(dir.join("alpha.yaml")).expect("reads");
        assert_eq!(files[1].1, on_disk, "the case file ships as it is written");
    }

    #[test]
    fn packing_twice_gives_the_same_bytes() {
        let dir = corpus("reproducible", &[("alpha", 3)]);
        let first = pack(&dir, None, "1.2.3").expect("packs");
        let second = pack(&dir, None, "1.2.3").expect("packs");
        assert_eq!(
            first.archive, second.archive,
            "the same cases pack to the same artifact"
        );
    }

    #[test]
    fn the_manifest_counts_every_suite_and_every_case() {
        let dir = corpus("manifest", &[("alpha", 2), ("beta", 5)]);
        let packed = pack(&dir, None, "0.1.0").expect("packs");
        assert_eq!(packed.cases(), 7);
        let doc = zu_json::parse(&packed.manifest).expect("the manifest is JSON");
        assert_eq!(doc.get("schema").and_then(Json::as_i64), Some(SCHEMA));
        assert_eq!(doc.get("version").and_then(Json::as_str), Some("0.1.0"));
        assert_eq!(doc.get("cases").and_then(Json::as_i64), Some(7));
        let suites = doc.get("suites").and_then(Json::as_arr).expect("suites");
        assert_eq!(suites.len(), 2);
        assert_eq!(suites[1].get("name").and_then(Json::as_str), Some("beta"));
        assert_eq!(suites[1].get("cases").and_then(Json::as_i64), Some(5));
    }

    #[test]
    fn the_manifest_checksum_is_of_the_file_as_shipped() {
        let dir = corpus("crc", &[("alpha", 1)]);
        let packed = pack(&dir, None, "0.1.0").expect("packs");
        let shipped = &entries(&packed.tar)[1].1;
        assert_eq!(packed.entries[0].crc, crc32c::crc32c(shipped));
        assert_eq!(packed.entries[0].bytes, shipped.len());
    }

    #[test]
    fn a_readme_travels_with_the_cases() {
        let dir = corpus("readme", &[("alpha", 1)]);
        let readme = dir.join("../zu-pack-readme.md");
        std::fs::write(&readme, "what this is\n").expect("writes");
        let packed = pack(&dir, Some(&readme), "0.1.0").expect("packs");
        let files = entries(&packed.tar);
        assert_eq!(files[1].0, "conformance-0.1.0/README.md");
        assert_eq!(files[1].1, b"what this is\n");
    }

    #[test]
    fn a_case_that_does_not_parse_stops_the_pack() {
        let dir = corpus("broken", &[("alpha", 1)]);
        std::fs::write(dir.join("beta.yaml"), "schema: 1\nsuite: beta\n").expect("writes");
        let err = pack(&dir, None, "0.1.0").expect_err("refused");
        assert!(err.contains("beta"), "{err}");
    }

    #[test]
    fn the_archive_decompresses_to_the_tar_it_was_made_from() {
        let dir = corpus("roundtrip", &[("alpha", 4)]);
        let packed = pack(&dir, None, "9.9.9").expect("packs");
        let out = zstd::bulk::decompress(&packed.archive, packed.tar.len() * 2).expect("unpacks");
        assert_eq!(out, packed.tar);
        assert!(
            packed.archive.len() < packed.tar.len(),
            "the compression is worth doing at {} against {}",
            packed.archive.len(),
            packed.tar.len()
        );
    }

    #[test]
    fn a_version_that_would_become_a_path_is_refused() {
        let dir = corpus("version", &[("alpha", 1)]);
        for version in ["", "../etc", "1.0 rc1"] {
            assert!(pack(&dir, None, version).is_err(), "{version:?}");
        }
    }
}
