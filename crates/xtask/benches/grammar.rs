//! What holding three highlighters to one word list costs.
//!
//! The number that matters is not the total, which is a few
//! milliseconds over a file of a hundred and fifty words. It is the
//! shape. Every check here is a word against a file: is this word in
//! the shell's table, is this keyword one the list knows, is the
//! generated query the one the list writes. The obvious way to write
//! any of them is a scan of the file per word, which is a cost that
//! grows with the product and therefore a word list that stops growing.
//! zuQL will have more words than it has today, because the standard
//! has more than the engine parses yet, so what this measures is the
//! cost per word as the list goes from the hundred and fifty it holds
//! now to ten times that.
//!
//! Run: cargo bench -p xtask --bench grammar

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use xtask::grammar::{self, Kind, Vocabulary};

fn main() {
    println!(
        "{:>9}  {:>9}  {:>9}  {:>11}",
        "words", "ms", "µs/word", "generated"
    );
    let mut first: Option<f64> = None;
    for groups in [10usize, 40, 160, 640] {
        let text = generated(groups);
        let vocabulary = Vocabulary::parse(&text).expect("the generated list parses");
        let words = vocabulary.all().len();
        let js = grammar_js(&vocabulary);
        let rust = highlight_rs(&vocabulary);
        let mut bytes = 0;
        let ms = best(|| {
            let vocabulary = Vocabulary::parse(black_box(&text)).expect("parses");
            let spelled = grammar::spelled(black_box(&js));
            let table = grammar::shell_table(black_box(&rust)).expect("a table");
            assert_eq!(table.len(), vocabulary.shell_table().len());
            assert_eq!(spelled.len(), words);
            bytes = vocabulary
                .generated(&js)
                .iter()
                .map(|file| black_box(file.text.len()))
                .sum();
        });
        let per = ms * 1e3 / words as f64;
        println!("{words:9}  {ms:9.3}  {per:9.3}  {:9} B", bytes);

        // A word costs a hash lookup and its own bytes. Three times the
        // cost per word at sixty four times the list is a scan of the
        // file per word wearing a hash table.
        let base = *first.get_or_insert(per);
        assert!(
            per < base * 3.0,
            "{words} words cost {per:.3} µs each against {base:.3} µs at the smallest list, \
             which is not flat"
        );
    }

    // And the committed files, which is what CI runs: the word list, the
    // two generated files, the shell's table and the grammar's keywords,
    // all read off the disk.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let vocabulary = Vocabulary::load(&root.join(grammar::PATH)).expect("the word list loads");
    let ms = best(|| {
        black_box(vocabulary.check(black_box(&root)).expect("the check runs"));
    });
    let notes = vocabulary.check(&root).expect("the check runs");
    println!(
        "\ncommitted tree: {} words, {} keywords the grammar spells, {} in the shell's table, \
         {ms:.2} ms, {} to fix",
        vocabulary.all().len(),
        grammar::spelled(&read(&root.join(grammar::GRAMMAR))).len(),
        vocabulary.shell_table().len(),
        notes.len(),
    );
}

/// A word list of `groups` groups of ten words each, spread over the
/// five kinds the way the real one is.
fn generated(groups: usize) -> String {
    let kinds = [
        Kind::Keyword,
        Kind::Keyword,
        Kind::Type,
        Kind::Function,
        Kind::Algorithm,
    ];
    let mut text =
        String::from("schema = 1\ndoc = \"The words the bench uses.\"\naudited = \"2026-08-18\"\n");
    for group in 0..groups {
        let words: Vec<String> = (0..10).map(|n| format!("\"WORD_{group}_{n}\"")).collect();
        text.push_str(&format!(
            "\n[[group]]\nkind = \"{}\"\ndoc = \"A group the bench generated.\"\nwords = [{}]\n",
            kinds[group % kinds.len()].name(),
            words.join(", ")
        ));
    }
    text
}

/// A grammar that spells every word, which is the file the keyword
/// check reads.
fn grammar_js(vocabulary: &Vocabulary) -> String {
    let mut text = String::from("module.exports = grammar({ name: \"gql\", rules: {\n");
    for (word, _) in vocabulary.all() {
        text.push_str(&format!(
            "    rule_{word}: ($) => seq(kw(\"{word}\"), $.thing),\n"
        ));
    }
    text.push_str("} });\n");
    text
}

/// The shell's table, as the shell writes it.
fn highlight_rs(vocabulary: &Vocabulary) -> String {
    let mut text = String::from("pub(crate) const KEYWORDS: &[&str] = &[\n");
    for word in vocabulary.shell_table() {
        text.push_str(&format!("    \"{word}\",\n"));
    }
    text.push_str("];\n\nfn colour() {}\n");
    text
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The best of seven, in milliseconds. The best rather than the mean
/// because the thing being measured is the work, and every sample above
/// the floor is the machine doing something else.
fn best(mut body: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    best
}
