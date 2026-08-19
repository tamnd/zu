//! The Unicode Consortium's own conformance test for normalization,
//! run against `zu_common::unicode`.
//!
//! `NormalizationTest.txt` is checked in at `artifacts/` with its
//! SHA-256 beside it. Every line is five spellings of one string, the
//! source and its four normal forms, and the file states the invariants
//! that have to hold between them. There are about nineteen thousand
//! lines and they are the reason a hand-written normalizer is a
//! reasonable thing to own: the algorithm is small, and the question of
//! whether it is right is answered by the body that defines it rather
//! than by the tests its author thought to write.
//!
//! Part 1 of the file is one line per character that is not its own
//! normalization. That makes the rest of the character space a test too,
//! and the second test here runs it: every character Part 1 does not
//! mention is its own NFC, NFD, NFKC and NFKD, which is a million and a
//! bit assertions and the only way to catch a table entry that should
//! not be there.

use std::collections::HashSet;
use std::path::PathBuf;

use zu_common::unicode::{NormalForm, normalize};

/// One line of the file: the source and its four normal forms.
struct Case {
    line: usize,
    part: u8,
    source: String,
    nfc: String,
    nfd: String,
    nfkc: String,
    nfkd: String,
}

fn cases() -> Vec<Case> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/NormalizationTest.txt");
    let text = std::fs::read_to_string(&path).expect("normalization test artifact");
    let mut part = 0u8;
    let mut cases = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        if let Some(rest) = raw.strip_prefix("@Part") {
            part = rest
                .split_whitespace()
                .next()
                .expect("a part number")
                .parse()
                .expect("a part number");
            continue;
        }
        let line = raw.split('#').next().expect("split yields one").trim();
        if line.is_empty() {
            continue;
        }
        let columns: Vec<String> = line.split(';').map(string).collect();
        assert!(columns.len() >= 5, "line {} is short: {raw:?}", i + 1);
        cases.push(Case {
            line: i + 1,
            part,
            source: columns[0].clone(),
            nfc: columns[1].clone(),
            nfd: columns[2].clone(),
            nfkc: columns[3].clone(),
            nfkd: columns[4].clone(),
        });
    }
    assert!(cases.len() > 18_000, "the artifact lost most of its lines");
    cases
}

/// A column is codepoints in hex, separated by spaces.
fn string(column: &str) -> String {
    column
        .split_whitespace()
        .map(|c| {
            char::from_u32(u32::from_str_radix(c, 16).expect("a hex codepoint"))
                .expect("a codepoint that is a character")
        })
        .collect()
}

#[test]
fn every_line_of_the_conformance_file_holds() {
    for case in cases() {
        let at = case.line;
        // The file states these as c2 == NFC(c1) == NFC(c2) == NFC(c3)
        // and so on for each form. Written out, each form has one answer
        // and the five columns agree about which of them it is.
        for source in [&case.source, &case.nfc, &case.nfd] {
            assert_eq!(
                normalize(source, NormalForm::Nfc),
                case.nfc,
                "NFC, line {at}"
            );
            assert_eq!(
                normalize(source, NormalForm::Nfd),
                case.nfd,
                "NFD, line {at}"
            );
        }
        for source in [&case.nfkc, &case.nfkd] {
            assert_eq!(
                normalize(source, NormalForm::Nfc),
                case.nfkc,
                "NFC of a compatibility form, line {at}"
            );
            assert_eq!(
                normalize(source, NormalForm::Nfd),
                case.nfkd,
                "NFD of a compatibility form, line {at}"
            );
        }
        for source in [&case.source, &case.nfc, &case.nfd, &case.nfkc, &case.nfkd] {
            assert_eq!(
                normalize(source, NormalForm::Nfkc),
                case.nfkc,
                "NFKC, line {at}"
            );
            assert_eq!(
                normalize(source, NormalForm::Nfkd),
                case.nfkd,
                "NFKD, line {at}"
            );
        }
    }
}

#[test]
fn every_character_part_one_does_not_name_is_its_own_normalization() {
    let named: HashSet<char> = cases()
        .iter()
        .filter(|case| case.part == 1)
        .map(|case| {
            let mut chars = case.source.chars();
            let c = chars.next().expect("a character");
            assert!(chars.next().is_none(), "part 1 is one character to a line");
            c
        })
        .collect();
    for code in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(code) else {
            continue;
        };
        if named.contains(&c) {
            continue;
        }
        let s = c.to_string();
        for form in [
            NormalForm::Nfc,
            NormalForm::Nfd,
            NormalForm::Nfkc,
            NormalForm::Nfkd,
        ] {
            assert_eq!(
                normalize(&s, form),
                s,
                "{code:#x} is not its own {}",
                form.name()
            );
        }
    }
}
