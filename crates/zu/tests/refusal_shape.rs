//! What a refused declaration looks like, pinned word for word.
//!
//! `refusals.rs` next door asks whether a refusal carries a condition.
//! This asks the other half of schema/05 §4: whether the sentence under
//! the condition is one a person can act on. Those are different
//! questions and a test on the first passes while the second is broken,
//! because a GQLSTATUS is comparable across engines and a message is
//! what the user actually reads.
//!
//! §4 asks a refusal to say what was asked, say what is true, and say
//! where. The first two are prose and the third is a pair of numbers,
//! and the pair is what this file is mostly about: a graph type naming
//! thirty properties refuses the same way whichever one is wrong, so a
//! message with no position hands the reader a search over their own
//! statement.
//!
//! Not every refusal below has one yet, and the ones that do not are in
//! the snapshot saying so rather than left out of it. They divide
//! cleanly: a refusal the parser raises has the text and the offset in
//! hand, and a refusal the catalog raises has neither, because
//! `ElementTypeDef` and `PropertyDef` in `zu-query`'s ast carry no
//! spans. Giving the second group a position means putting spans on the
//! ast, which is a change to a shared type and its own piece of work.
//! The count at the bottom is what makes that debt a number.
//!
//! `ZU_UPDATE_SNAPSHOTS=1 cargo test --release -p zu --test refusal_shape`
//! rewrites the file, and the diff on the way into the commit is the
//! review.

use std::path::PathBuf;

use zu::query::run;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A file this test wrote itself, so every refusal below is about the
/// statement rather than about the file.
fn graph(dir: &std::path::Path) -> Zu1File {
    let mut zu = Zu1File::create(&dir.join("refusal_shape.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 2, &[(0, 1)]).unwrap();
    zu
}

/// Every way a declaration can be refused, one statement each.
///
/// The order groups them by what raises: the parser first, which has
/// the text, then the catalog, which does not. Each is legal enough to
/// reach the stage that refuses it, so none of these is a stand-in for
/// a different error.
const REFUSALS: &[(&str, &str)] = &[
    (
        "a type name nothing spells",
        "CREATE GRAPH TYPE t1 { (:Probe {v :: NOSUCHTYPE}) }",
    ),
    (
        "a type name nothing spells, late in a long declaration",
        "CREATE GRAPH TYPE t2 { (:Probe {a :: INT32, b :: STRING, c :: DATE, d :: NOSUCHTYPE}) }",
    ),
    (
        "a signed word in front of a type that has no sign",
        "CREATE GRAPH TYPE t3 { (:Probe {v :: UNSIGNED STRING}) }",
    ),
    (
        "arguments a type does not take",
        "CREATE GRAPH TYPE t4 { (:Probe {v :: DATE(3)}) }",
    ),
    (
        "a list maximum written in parentheses",
        "CREATE GRAPH TYPE t5 { (:Probe {v :: LIST<INT32>(2)}) }",
    ),
    (
        "a type this file cannot write",
        "CREATE GRAPH TYPE t6 { (:Probe {total :: DECIMAL(38,2)}) }",
    ),
    (
        "one property declared twice",
        "CREATE GRAPH TYPE t7 { (:Probe {v :: INT32, v :: STRING}) }",
    ),
    (
        "one element type declared twice",
        "CREATE GRAPH TYPE t8 { (a :Probe {v :: INT32}), (a :Other {w :: INT32}) }",
    ),
    (
        "a key label set written and left empty",
        "CREATE GRAPH TYPE t9 { ( => :Probe {v :: INT32}) }",
    ),
];

/// The refusal rendered as schema/05 §4 describes it: the code a
/// harness matches on, the words the standard gives that code, the
/// severity letter a binding switches on, the sentence, and the place.
///
/// The caret is not part of the record. It is drawn here because what
/// this file is reviewing is whether the numbers land on the right
/// token, and a column count is not something a reader can check by
/// eye.
fn render(asks: &str, source: &str, err: &zu_common::ZuError) -> String {
    let mut out = format!("# {asks}\n  statement {source}\n");
    let Some(record) = err.diagnostic() else {
        out.push_str("  (no diagnostic record)\n\n");
        return out;
    };
    let condition = record.status.condition();
    out.push_str(&format!("  gqlstatus {}\n", condition.code));
    out.push_str(&format!(
        "  condition {}{}\n",
        condition.class,
        condition
            .subclass
            .map(|s| format!(" / {s}"))
            .unwrap_or_default()
    ));
    out.push_str(&format!("  severity  {}\n", record.severity().letter()));
    out.push_str(&format!("  message   {}\n", record.detail));
    match record.position {
        Some(at) => out.push_str(&format!(
            "  at        line {} column {} offset {}\n",
            at.line, at.column, at.offset
        )),
        None => out.push_str("  at        nowhere: the raise site has no span\n"),
    }
    if let (Some(excerpt), Some(at)) = (record.excerpt.as_deref(), record.position) {
        out.push_str(&format!("  excerpt   {excerpt}\n"));
        let pad = " ".repeat(at.column.saturating_sub(1) as usize);
        out.push_str(&format!("            {pad}^\n"));
    }
    out.push('\n');
    out
}

#[test]
fn a_refused_declaration_says_what_and_where() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let mut out = String::new();
    let mut placeless = 0;
    for (asks, source) in REFUSALS {
        let err = run(source, &mut db, &[]).expect_err(asks);
        out.push_str(&render(asks, source, &err));
        if err.position().is_none() {
            placeless += 1;
        }
    }
    snapshot("declaration-refusals.txt", &out);
    // Three of the nine, and they are the three the catalog raises: the
    // unstorable type, the property declared twice, and the element
    // type declared twice. Every one of them is about a def in the ast,
    // and no def carries an offset, so this number comes down when the
    // ast gets spans and not before.
    assert_eq!(
        placeless, 3,
        "how many refusals still say nothing about where"
    );
}

/// Compares against the committed file, or rewrites it when asked.
///
/// Hand-rolled to match `zu-cli`'s snapshot suite, which is the one
/// this borrows its shape from, so a reader who knows that one already
/// knows this.
fn snapshot(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed == actual {
        return;
    }
    if std::env::var_os("ZU_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a writable tests dir");
        std::fs::write(&path, actual).expect("a writable snapshot");
        return;
    }
    panic!(
        "{} is not what a refused declaration says. Read the difference, and if the new \
         wording is the intended one, `ZU_UPDATE_SNAPSHOTS=1 cargo test --release -p zu \
         --test refusal_shape` writes it.\n\n--- committed\n{committed}\n--- printed\n{actual}",
        path.display()
    );
}
