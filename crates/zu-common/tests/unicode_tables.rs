//! Regenerates `src/unicode/generated.rs` from the Unicode Character
//! Database and fails on drift, the same way the GQLSTATUS table test
//! works. Run with `ZU_UPDATE_UNICODE=1 cargo test -p zu-common --test
//! unicode_tables` after the artifacts change.
//!
//! The artifacts are checked in at `artifacts/UnicodeData.txt` and
//! `artifacts/CompositionExclusions.txt` with their SHA-256 next to
//! them, and `make check-artifacts` verifies the hashes. Two guards
//! rather than one, because they catch different mistakes: the hash
//! catches a swapped artifact, the drift test catches a hand-edited
//! table.
//!
//! Three things this generator does that are worth knowing about.
//!
//! It expands the decompositions all the way. `UnicodeData.txt` gives
//! one step, so U+1E09 maps to U+00E7 and an acute, and U+00E7 maps to a
//! c and a cedilla; the table here holds the three characters the string
//! ends at. That is a decision about where the recursion happens, not
//! about what the answer is, and doing it once here means the runtime
//! decomposition is a lookup with no loop around it.
//!
//! It builds the composition table by inverting the canonical mappings
//! of exactly two characters, minus the exclusions. Three kinds of
//! character are excluded from composition and only one of them is in
//! the exclusion file: a singleton mapping is excluded because it is not
//! a pair, and a mapping whose first character is not a starter is
//! excluded because nothing could reach it. Both fall out of the shape
//! of the mapping, so only the script-specific exclusions have to be
//! read.
//!
//! It reads the general category column twice, once for the characters
//! an identifier may start with and once for the ones it may go on
//! with. ISO/IEC 39075:2024 subclause 21.3 writes those two sets as
//! lists of Unicode general category classes and nothing else, so the
//! table is the artifact's own column filtered, and a range written as
//! a First and a Last pair is expanded here rather than at runtime.
//!
//! It leaves Hangul out. The syllable block is eleven thousand
//! characters that decompose and compose by arithmetic, and none of them
//! carries a decomposition mapping in the artifact, so there is nothing
//! to leave out on purpose; the assertion at the end says so, because a
//! future Unicode that wrote them out would silently quadruple the
//! table.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// The two halves of the identifier table: the codes a name may start
/// with and the codes it may go on with, each as `(start, end)` ranges.
type Ident = (Vec<(u32, u32)>, Vec<(u32, u32)>);

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifact(name: &str) -> String {
    std::fs::read_to_string(manifest_dir().join("artifacts").join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// What the database says about the characters that say anything.
struct Database {
    ccc: BTreeMap<u32, u8>,
    canonical: BTreeMap<u32, Vec<u32>>,
    compatibility: BTreeMap<u32, Vec<u32>>,
}

fn parse_unicode_data(text: &str) -> Database {
    let mut db = Database {
        ccc: BTreeMap::new(),
        canonical: BTreeMap::new(),
        compatibility: BTreeMap::new(),
    };
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        assert!(fields.len() >= 6, "short line {line:?}");
        let code = u32::from_str_radix(fields[0], 16).expect("codepoint");
        let class: u8 = fields[3].parse().expect("combining class");
        if class != 0 {
            db.ccc.insert(code, class);
        }
        let mapping = fields[5].trim();
        if mapping.is_empty() {
            continue;
        }
        // A range is written as two lines whose names end in First and
        // Last, and every character between them shares the fields. No
        // range carries a decomposition today; if one ever does, the
        // loop below would record it for the endpoint alone.
        assert!(
            !fields[1].ends_with("First>") && !fields[1].ends_with("Last>"),
            "a range carries a decomposition: {line:?}"
        );
        let (compatibility, body) = match mapping.strip_prefix('<') {
            Some(rest) => (true, rest.split_once('>').expect("compatibility tag").1),
            None => (false, mapping),
        };
        let chars: Vec<u32> = body
            .split_whitespace()
            .map(|c| u32::from_str_radix(c, 16).expect("mapped codepoint"))
            .collect();
        assert!(!chars.is_empty(), "empty mapping in {line:?}");
        if compatibility {
            db.compatibility.insert(code, chars);
        } else {
            assert!(chars.len() <= 2, "a canonical mapping is one or two");
            db.canonical.insert(code, chars);
        }
    }
    db
}

/// The identifier ranges out of the general category column, as
/// `(start, extend)`.
///
/// ISO 21.3: an identifier starts with a character of class Lu, Ll, Lt,
/// Lm, Lo or Nl, and goes on with one of those or of class Mn, Mc, Nd,
/// Pc or Cf. The underscore is a start as well, and it is Pc rather
/// than a letter, so it is added by hand here the same way the standard
/// adds it by hand.
///
/// Adjacent codes of the same kind are merged into one range, which is
/// what makes the table a few hundred entries rather than a hundred and
/// forty thousand.
fn parse_ident(text: &str) -> Ident {
    const START: [&str; 6] = ["Lu", "Ll", "Lt", "Lm", "Lo", "Nl"];
    const MORE: [&str; 5] = ["Mn", "Mc", "Nd", "Pc", "Cf"];

    let mut start = Vec::new();
    let mut extend = Vec::new();
    let mut pending: Option<u32> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        let code = u32::from_str_radix(fields[0], 16).expect("codepoint");
        let category = fields[2];
        // A range is two lines and the fields on them agree, so the
        // First line is remembered and the Last line closes it.
        let (from, to) = if fields[1].ends_with("First>") {
            pending = Some(code);
            continue;
        } else if fields[1].ends_with("Last>") {
            (pending.take().expect("a First before every Last"), code)
        } else {
            (code, code)
        };
        let is_start = START.contains(&category) || from == u32::from('_');
        if is_start {
            push_range(&mut start, from, to);
        }
        if is_start || MORE.contains(&category) {
            push_range(&mut extend, from, to);
        }
    }
    push_range(&mut start, u32::from('_'), u32::from('_'));
    push_range(&mut extend, u32::from('_'), u32::from('_'));
    start.sort_unstable();
    extend.sort_unstable();
    (merge(start), merge(extend))
}

fn push_range(out: &mut Vec<(u32, u32)>, from: u32, to: u32) {
    out.push((from, to));
}

/// Joins ranges that touch or overlap, over a list already in order.
fn merge(ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1.saturating_add(1) => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

fn parse_exclusions(text: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().expect("split yields one").trim();
            (!line.is_empty()).then(|| u32::from_str_radix(line, 16).expect("excluded codepoint"))
        })
        .collect()
}

/// The version from the artifact's own first line, which is where the
/// database writes it: `# CompositionExclusions-16.0.0.txt`.
fn parse_version(exclusions: &str) -> String {
    let first = exclusions.lines().next().expect("a first line");
    first
        .trim_start_matches("# CompositionExclusions-")
        .trim_end_matches(".txt")
        .to_string()
}

/// The decomposition of a character all the way down, canonical only or
/// canonical and compatibility together.
fn expand(db: &Database, code: u32, compatibility: bool, out: &mut Vec<u32>) {
    let mapping = db
        .canonical
        .get(&code)
        .or_else(|| compatibility.then(|| db.compatibility.get(&code)).flatten());
    match mapping {
        Some(chars) => {
            for &c in chars {
                expand(db, c, compatibility, out);
            }
        }
        None => out.push(code),
    }
}

/// A pool of decomposed characters and the slices into it, where two
/// characters that decompose to the same string share one slice.
#[derive(Default)]
struct Pool {
    chars: Vec<u32>,
    at: HashMap<Vec<u32>, u32>,
}

impl Pool {
    fn intern(&mut self, chars: Vec<u32>) -> (u32, u8) {
        let len = u8::try_from(chars.len()).expect("a decomposition under 256 characters");
        if let Some(&at) = self.at.get(&chars) {
            return (at, len);
        }
        let at = u32::try_from(self.chars.len()).expect("a pool under four billion");
        self.chars.extend_from_slice(&chars);
        self.at.insert(chars, at);
        (at, len)
    }
}

struct Tables {
    version: String,
    ccc: Vec<(u32, u8)>,
    canonical: Vec<(u32, u32, u8)>,
    compatibility: Vec<(u32, u32, u8)>,
    decomposed: Vec<u32>,
    composition: Vec<(u32, u32, u32)>,
    ident_start: Vec<(u32, u32)>,
    ident_extend: Vec<(u32, u32)>,
}

fn build(db: &Database, exclusions: &[u32], version: String, ident: Ident) -> Tables {
    let mut pool = Pool::default();
    let mut canonical = Vec::new();
    for &code in db.canonical.keys() {
        let mut chars = Vec::new();
        expand(db, code, false, &mut chars);
        let (at, len) = pool.intern(chars);
        canonical.push((code, at, len));
    }
    // Every character with any mapping needs a compatibility entry, not
    // only the ones the artifact tagged: a canonical mapping whose parts
    // have compatibility mappings decomposes further under NFKD than
    // under NFD, and the runtime reads one table per form.
    let mut compatibility = Vec::new();
    for &code in db.canonical.keys().chain(db.compatibility.keys()) {
        let mut chars = Vec::new();
        expand(db, code, true, &mut chars);
        let (at, len) = pool.intern(chars);
        compatibility.push((code, at, len));
    }
    compatibility.sort_unstable();

    let excluded: std::collections::HashSet<u32> = exclusions.iter().copied().collect();
    let mut composition = Vec::new();
    for (&code, mapping) in &db.canonical {
        if mapping.len() != 2 || excluded.contains(&code) {
            continue;
        }
        if db.ccc.contains_key(&mapping[0]) {
            continue;
        }
        composition.push((mapping[0], mapping[1], code));
    }
    composition.sort_unstable();

    Tables {
        version,
        ccc: db.ccc.iter().map(|(&c, &k)| (c, k)).collect(),
        canonical,
        compatibility,
        decomposed: pool.chars,
        composition,
        ident_start: ident.0,
        ident_extend: ident.1,
    }
}

fn ch(code: u32) -> String {
    assert!(
        char::from_u32(code).is_some(),
        "{code:#x} is not a character"
    );
    format!("'\\u{{{code:x}}}'")
}

fn render(tables: &Tables) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated from `artifacts/UnicodeData.txt` and\n\
         //! `artifacts/CompositionExclusions.txt`, the Unicode Character\n\
         //! Database. Do not edit by hand: run\n\
         //! `ZU_UPDATE_UNICODE=1 cargo test -p zu-common --test unicode_tables`.\n\
         //!\n\
         //! The decompositions are fully expanded and the composition\n\
         //! pairs already have the exclusions taken out of them, so every\n\
         //! table here is read once and read directly.\n\
         \n",
    );
    out.push_str(&format!(
        "/// The version of the database these tables were written from.\n\
         pub static UNICODE_VERSION: &str = \"{}\";\n\n",
        tables.version
    ));

    out.push_str(
        "/// Characters with a nonzero canonical combining class, in code\n\
         /// order. Everything absent is a starter.\n\
         #[rustfmt::skip]\n\
         pub(super) static CCC: &[(char, u8)] = &[\n",
    );
    rows(
        &mut out,
        8,
        tables
            .ccc
            .iter()
            .map(|(c, k)| format!("({}, {k}),", ch(*c))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// Canonical decompositions, in code order, as a character and a\n\
         /// slice of `DECOMPOSED` given by its start and its length.\n\
         #[rustfmt::skip]\n\
         pub(super) static CANONICAL: &[(char, u32, u8)] = &[\n",
    );
    rows(
        &mut out,
        6,
        tables
            .canonical
            .iter()
            .map(|(c, at, len)| format!("({}, {at}, {len}),", ch(*c))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// Compatibility decompositions, in code order, read the same\n\
         /// way. Every character with any decomposition is here, because\n\
         /// a canonical mapping can expand further under a compatibility\n\
         /// form than under a canonical one.\n\
         #[rustfmt::skip]\n\
         pub(super) static COMPATIBILITY: &[(char, u32, u8)] = &[\n",
    );
    rows(
        &mut out,
        6,
        tables
            .compatibility
            .iter()
            .map(|(c, at, len)| format!("({}, {at}, {len}),", ch(*c))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// The characters both decomposition tables point into. Two\n\
         /// characters that decompose to the same string share a slice.\n\
         #[rustfmt::skip]\n\
         pub(super) static DECOMPOSED: &[char] = &[\n",
    );
    rows(
        &mut out,
        12,
        tables.decomposed.iter().map(|c| format!("{},", ch(*c))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// Primary composites, sorted by the pair that makes them. A\n\
         /// pair that is here composes; one that is not does not, which\n\
         /// includes every pair the standard excludes.\n\
         #[rustfmt::skip]\n\
         pub(super) static COMPOSITION: &[(char, char, char)] = &[\n",
    );
    rows(
        &mut out,
        4,
        tables
            .composition
            .iter()
            .map(|(a, b, c)| format!("({}, {}, {}),", ch(*a), ch(*b), ch(*c))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// The characters an identifier may start with, as inclusive\n\
         /// ranges in code order: general category Lu, Ll, Lt, Lm, Lo or\n\
         /// Nl, and the underscore (ISO 21.3).\n\
         #[rustfmt::skip]\n\
         pub(super) static IDENT_START: &[(char, char)] = &[\n",
    );
    rows(
        &mut out,
        4,
        tables
            .ident_start
            .iter()
            .map(|(a, b)| format!("({}, {}),", ch(*a), ch(*b))),
    );
    out.push_str("];\n\n");

    out.push_str(
        "/// The characters an identifier may go on with, read the same\n\
         /// way: every start, and general category Mn, Mc, Nd, Pc or Cf.\n\
         #[rustfmt::skip]\n\
         pub(super) static IDENT_EXTEND: &[(char, char)] = &[\n",
    );
    rows(
        &mut out,
        4,
        tables
            .ident_extend
            .iter()
            .map(|(a, b)| format!("({}, {}),", ch(*a), ch(*b))),
    );
    out.push_str("];\n");
    out
}

/// Writes the entries of one table out with a fixed number to a line,
/// which is what the `#[rustfmt::skip]` on each of them is for. Fifteen
/// thousand entries one to a line is a file nobody opens twice, and
/// rustfmt has no setting for how wide a table of data should be, so the
/// generator says it and rustfmt is told to leave it alone.
fn rows(out: &mut String, per_line: usize, entries: impl IntoIterator<Item = String>) {
    for (i, entry) in entries.into_iter().enumerate() {
        out.push_str(if i.is_multiple_of(per_line) {
            "    "
        } else {
            " "
        });
        out.push_str(&entry);
        if (i + 1).is_multiple_of(per_line) {
            out.push('\n');
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Runs the rendered tables through rustfmt so the checked-in file is
/// formatted like every other file in the tree and `cargo fmt --check`
/// has nothing to say about it, the same reason the GQLSTATUS generator
/// does it.
fn rustfmt(source: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("rustfmt on PATH; it is in rust-toolchain.toml components");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(source.as_bytes())
        .expect("write to rustfmt");
    let out = child.wait_with_output().expect("rustfmt output");
    assert!(
        out.status.success(),
        "rustfmt rejected the generated tables"
    );
    String::from_utf8(out.stdout).expect("rustfmt emits utf-8")
}

fn tables() -> Tables {
    let data = artifact("UnicodeData.txt");
    let exclusions = artifact("CompositionExclusions.txt");
    let db = parse_unicode_data(&data);
    build(
        &db,
        &parse_exclusions(&exclusions),
        parse_version(&exclusions),
        parse_ident(&data),
    )
}

#[test]
fn generated_tables_match_the_artifacts() {
    let rendered = rustfmt(&render(&tables()));
    let path = manifest_dir().join("src/unicode/generated.rs");
    if std::env::var_os("ZU_UPDATE_UNICODE").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &rendered).expect("write generated tables");
        return;
    }
    // Checkout on Windows rewrites the line endings, so compare the
    // content rather than the bytes, the same way the GQLSTATUS test
    // does.
    let on_disk = std::fs::read_to_string(&path).expect("generated tables");
    assert_eq!(
        on_disk.replace("\r\n", "\n"),
        rendered,
        "src/unicode/generated.rs is stale; run ZU_UPDATE_UNICODE=1 cargo test -p zu-common --test unicode_tables"
    );
}

#[test]
fn hangul_is_arithmetic_and_stays_out_of_the_tables() {
    let tables = tables();
    let syllables = 0xAC00..0xD7A4;
    assert!(
        !tables
            .canonical
            .iter()
            .any(|(code, _, _)| syllables.contains(code)),
        "the artifact now writes Hangul syllable decompositions out"
    );
    assert!(
        !tables
            .composition
            .iter()
            .any(|(_, _, code)| syllables.contains(code)),
        "the artifact now writes Hangul syllable compositions out"
    );
}

#[test]
fn a_canonical_decomposition_is_one_or_two_characters_before_it_is_expanded() {
    // The guarantee the composition table rests on: inverting the
    // mappings is only well defined because no canonical mapping is
    // longer than a pair.
    let db = parse_unicode_data(&artifact("UnicodeData.txt"));
    assert!(db.canonical.values().all(|m| m.len() <= 2));
    assert!(db.canonical.values().all(|m| !m.is_empty()));
}
