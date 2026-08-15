//! Repository automation, run as `cargo xtask <command>`.
//!
//! The one command so far is `model`, which regenerates
//! `docs/api/model.json` from the public Rust surface of the `zu`
//! crate. The model is committed rather than built on demand for two
//! reasons. A reviewer sees the API change in the diff of the pull
//! request that makes it, which is the only place anyone will look at
//! it. And every consumer of the model, from the reference pages to
//! the `api-map.toml` check, can read a file instead of needing a
//! nightly toolchain.
//!
//! Regenerating it needs nightly, because rustdoc's JSON output is
//! nightly-only. `--check` needs nightly too and is what CI runs.

use xtask::{model, rustdoc};

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
cargo xtask model [--out PATH] [--check] [--toolchain NAME]

  --out PATH        where to write the model (default docs/api/model.json)
  --check           regenerate and compare, writing nothing; nonzero on drift
  --toolchain NAME  the nightly to run rustdoc with (default nightly)
";

/// The crates whose public items reach the surface of `zu` through a
/// `pub use`. rustdoc documents one crate at a time, so each of these
/// has to be generated as well or a third of the API is a name with
/// nothing behind it.
const REEXPORTED: [&str; 4] = ["zu-common", "zu-storage", "zu-zu1", "zu-query"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("model") => match model_command(&args[1..]) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("xtask: {e}");
                ExitCode::FAILURE
            }
        },
        Some("--help" | "-h") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("xtask: no command {other:?}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn model_command(args: &[String]) -> Result<ExitCode, String> {
    let mut out = PathBuf::from("docs/api/model.json");
    let mut check = false;
    let mut toolchain = "nightly".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--out" => {
                out = PathBuf::from(args.get(i + 1).ok_or("--out wants a path")?);
                i += 1;
            }
            "--toolchain" => {
                toolchain = args.get(i + 1).ok_or("--toolchain wants a name")?.clone();
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }

    let mut docs = Vec::with_capacity(REEXPORTED.len() + 1);
    for package in std::iter::once("zu").chain(REEXPORTED) {
        docs.push(rustdoc::generate(package, &toolchain)?);
    }
    let model = model::build(&docs, "zu")?;
    let text = model.to_json().to_pretty();

    if check {
        let found = std::fs::read_to_string(&out)
            .map_err(|e| format!("reading {}: {e}. Run `cargo xtask model`.", out.display()))?;
        if found == text {
            println!(
                "{} is current, {} entities",
                out.display(),
                model.entities.len()
            );
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!(
            "{} is out of date. Run `cargo xtask model`.\n",
            out.display()
        );
        report_drift(&found, &text);
        return Ok(ExitCode::FAILURE);
    }

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(&out, &text).map_err(|e| format!("writing {}: {e}", out.display()))?;
    println!("{}: {} entities", out.display(), model.entities.len());
    Ok(ExitCode::SUCCESS)
}

/// Says which entities appeared and which went, because "the file
/// differs" sends a reader to a thousand-line diff to learn something
/// the check already knows.
fn report_drift(found: &str, want: &str) {
    let ids = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("\"id\": \""))
            .filter_map(|l| l.strip_suffix("\","))
            .map(str::to_string)
            .collect()
    };
    let before = ids(found);
    let after = ids(want);
    let mut quiet = true;
    for id in &after {
        if !before.contains(id) {
            eprintln!("  added   {id}");
            quiet = false;
        }
    }
    for id in &before {
        if !after.contains(id) {
            eprintln!("  removed {id}");
            quiet = false;
        }
    }
    if quiet {
        eprintln!("  the same entities, with a changed signature or doc comment");
    }
}
