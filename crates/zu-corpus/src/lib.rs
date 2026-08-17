//! The cross-client conformance corpus: the cases, and the runner
//! that is the first of nine to run them.
//!
//! zu ships nine clients. They are written in seven languages, two of
//! them go through the C ABI by a different mechanism than the rest
//! (ADR 0002), and every one of them decodes values with code nobody
//! else's client shares. The question the corpus answers is the one
//! that follows from that: does a value put in through one of them and
//! taken out through another mean the same thing.
//!
//! It is a set of YAML files, each a statement and the rows it owes,
//! versioned with the engine and published as a release artifact every
//! client repository consumes (ADR 0005). The cases live here because
//! they gate engine releases; `tamnd/zu-kit` holds the other runners.
//!
//! Five pieces, one per module. [`yaml`] reads the subset of YAML the
//! files are written in. [`value`] is the `{type, value}` encoding a
//! case writes values in, whose entire job is to survive nine readers
//! in seven languages without any of them rounding anything. [`case`]
//! is what a case is. [`load`] is the data a suite puts in through its
//! client's bulk load path before its cases read it back, which is the
//! half of the question an expression cannot ask. [`runner`] runs them
//! against this engine.

pub mod case;
pub mod load;
pub mod runner;
pub mod value;
pub mod yaml;

use std::path::Path;

pub use case::{Case, Expect, Suite};
pub use load::Load;
pub use runner::{Outcome, Ran, Report, run};

/// The extension a case file has, which is also what says a file in
/// the corpus directory is one.
pub const EXTENSION: &str = "yaml";

/// Every suite in a directory, in name order, or the first file that
/// is not one.
///
/// The order is the directory's, sorted, rather than the order the
/// filesystem hands back, so that two machines running the corpus
/// produce reports that can be compared line by line.
pub fn load(dir: &Path) -> Result<Vec<Suite>, String> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == EXTENSION))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no .{EXTENSION} files in {}", dir.display()));
    }

    let mut suites = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let suite = Suite::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        // The name in the file and the name of the file are two places
        // to write one thing, so they are checked against each other
        // rather than one of them being ignored.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if suite.name != stem {
            return Err(format!(
                "{}: the suite calls itself {:?} and the file calls it {stem:?}",
                path.display(),
                suite.name
            ));
        }
        suites.push(suite);
    }
    Ok(suites)
}
