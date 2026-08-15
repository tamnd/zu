//! Getting rustdoc's JSON out of cargo and into a `Json`.
//!
//! The output format is nightly-only and versioned, and it changes. The
//! generator therefore records the `format_version` it read in the
//! model and refuses a version it has not been taught, because a silent
//! partial read of a changed format produces a model that looks fine
//! and is missing half the API.

use std::path::{Path, PathBuf};
use std::process::Command;

use zu_json::Json;

/// The rustdoc JSON format this generator understands. Bump it together
/// with whatever `model.rs` had to learn, never on its own.
pub const FORMAT_VERSION: u64 = 61;

/// One crate's rustdoc output, with the crate's own name alongside it
/// because the JSON spells the name only for the crates it depends on.
#[derive(Debug)]
pub struct CrateDoc {
    /// The name as Rust spells it, underscores and all: `zu_common`.
    pub name: String,
    pub doc: Json,
}

/// Runs `cargo rustdoc` for one package and reads what it wrote.
///
/// `toolchain` is passed to cargo as `+name`. The caller supplies it
/// rather than this function hard-coding `+nightly`, so CI can pin the
/// exact nightly from the toolchain table and get the same bytes on
/// every run, which is the whole point of committing the model.
pub fn generate(package: &str, toolchain: &str) -> Result<CrateDoc, String> {
    let target = target_dir()?;
    let mut cmd = Command::new("cargo");
    cmd.arg(format!("+{toolchain}"))
        .args(["rustdoc", "-p", package, "--lib"])
        .args(["-Z", "unstable-options", "--output-format", "json"])
        // Document the crate as its users see it. Private items would
        // put things in the model that no binding can reach.
        .env("RUSTDOCFLAGS", "-Z unstable-options");
    let out = cmd
        .output()
        .map_err(|e| format!("running cargo {toolchain} rustdoc for {package}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo +{toolchain} rustdoc -p {package} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let name = package.replace('-', "_");
    let path = target.join("doc").join(format!("{name}.json"));
    read(&path, &name)
}

/// Reads one rustdoc JSON file that is already on disk.
pub fn read(path: &Path, name: &str) -> Result<CrateDoc, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let doc = zu_json::parse(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let found = doc
        .get("format_version")
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("{} has no format_version", path.display()))?;
    if found != FORMAT_VERSION {
        return Err(format!(
            "{} is rustdoc format {found}, and this generator reads {FORMAT_VERSION}. \
             Read the format's changelog, update crates/xtask/src/model.rs, \
             then bump FORMAT_VERSION.",
            path.display()
        ));
    }
    Ok(CrateDoc {
        name: name.to_string(),
        doc,
    })
}

/// The workspace target directory, asked of cargo rather than assumed,
/// because `CARGO_TARGET_DIR` and `build.target-dir` both move it and a
/// generator that guessed would fail on somebody else's machine only.
fn target_dir() -> Result<PathBuf, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("running cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text =
        String::from_utf8(out.stdout).map_err(|e| format!("cargo metadata is not utf-8: {e}"))?;
    let meta = zu_json::parse(&text).map_err(|e| format!("parsing cargo metadata: {e}"))?;
    meta.get("target_directory")
        .and_then(Json::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata has no target_directory".to_string())
}
