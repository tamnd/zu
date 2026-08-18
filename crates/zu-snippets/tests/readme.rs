//! The README against the programs it prints.
//!
//! Two checks, and the second is the one with teeth. The text in the
//! fenced block has to be the text of the example, character for
//! character, so a snippet cannot be edited on the page into something
//! that was never compiled. Then the example runs, in a directory of
//! its own, and has to print what the page says it prints, so a
//! snippet cannot go on compiling after the statements in it stopped
//! meaning what they meant.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root, two levels above this package.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the package sits two levels under the root")
        .to_path_buf()
}

/// Every fenced block of one language in a markdown file, in order.
fn blocks(markdown: &str, language: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() != format!("```{language}") {
            continue;
        }
        let mut block = String::new();
        for line in lines.by_ref() {
            if line.trim_end() == "```" {
                break;
            }
            block.push_str(line);
            block.push('\n');
        }
        found.push(block);
    }
    found
}

/// Where the example binary is, building it if this test was run in a
/// way that did not. `cargo test` builds examples and `cargo test
/// --test readme` does not, and a test that only passes under one of
/// them is a test somebody will disbelieve.
fn example(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps, and an example of
    // the same build is in target/<profile>/examples.
    let exe = std::env::current_exe().expect("this test has a path");
    let built = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>/deps/<test>")
        .join("examples")
        .join(name);
    let built = built.with_extension(std::env::consts::EXE_EXTENSION);
    if built.exists() {
        return built;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "zu-snippets", "--example", name])
        .current_dir(root())
        .status()
        .expect("cargo runs");
    assert!(status.success(), "building the {name} example");
    assert!(built.exists(), "no {name} example at {}", built.display());
    built
}

#[test]
fn the_readme_prints_the_program_this_repository_compiles() {
    let readme = std::fs::read_to_string(root().join("README.md")).expect("a README");
    let printed = blocks(&readme, "rust");
    assert_eq!(
        printed.len(),
        zu_snippets::SNIPPETS.len(),
        "the README prints {} Rust blocks and this package holds {} snippets",
        printed.len(),
        zu_snippets::SNIPPETS.len()
    );
    for (block, name) in printed.iter().zip(zu_snippets::SNIPPETS) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join(format!("{name}.rs"));
        let source = std::fs::read_to_string(&path).expect("the example is there");
        assert_eq!(
            block,
            &source,
            "the README block and {} have drifted apart",
            path.display()
        );
    }
}

#[test]
fn the_sixty_second_program_runs_and_prints_what_the_readme_says() {
    let dir = tempfile::tempdir().expect("a directory of its own");
    let run = Command::new(example("sixty-seconds"))
        .current_dir(dir.path())
        .output()
        .expect("the example runs");
    assert!(
        run.status.success(),
        "the example failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "ada 1\ngrace 2\n");
    // The reader's copy writes into the directory they ran it from,
    // which is the part of the story a compile check cannot see.
    assert!(dir.path().join("social.zu1").is_file());
}
