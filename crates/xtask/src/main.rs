//! Repository automation, run as `cargo xtask <command>`.
//!
//! `model` regenerates `docs/api/model.json` from the public Rust
//! surface of the `zu` crate. The model is committed rather than built
//! on demand for two reasons. A reviewer sees the API change in the
//! diff of the pull request that makes it, which is the only place
//! anyone will look at it. And every consumer of the model, from the
//! reference pages to the `api-map.toml` check, can read a file
//! instead of needing a nightly toolchain.
//!
//! `api-map` joins the model against a map and reports what neither
//! file can see on its own: a public symbol nobody classified, a
//! classification for code that is gone, a tier-1 entity a binding
//! stopped naming.
//!
//! Regenerating the model needs nightly, because rustdoc's JSON output
//! is nightly-only, and `model --check` needs it too. The map check
//! reads two committed files and needs nothing.

use xtask::{apimap, model, rustdoc};

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
cargo xtask model [--out PATH] [--check] [--toolchain NAME]

  --out PATH        where to write the model (default docs/api/model.json)
  --check           regenerate and compare, writing nothing; nonzero on drift
  --toolchain NAME  the nightly to run rustdoc with (default nightly)

cargo xtask api-map [--map PATH] [--model PATH] [--ledger PATH] [--list]

  --map PATH        the map to check (default docs/api/api-map.toml)
  --model PATH      the API model to check it against (default docs/api/model.json)
  --ledger PATH     the rust map a binding map is checked against
  --list            print `tier<TAB>id` for every entity, and check nothing
";

/// The crates whose public items reach the surface of `zu` through a
/// `pub use`. rustdoc documents one crate at a time, so each of these
/// has to be generated as well or a third of the API is a name with
/// nothing behind it.
const REEXPORTED: [&str; 4] = ["zu-common", "zu-storage", "zu-zu1", "zu-query"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("model") => run(model_command(&args[1..])),
        Some("api-map") => run(api_map_command(&args[1..])),
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

fn run(result: Result<ExitCode, String>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xtask: {e}");
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

fn api_map_command(args: &[String]) -> Result<ExitCode, String> {
    const DEFAULT_MAP: &str = "docs/api/api-map.toml";
    let mut map_path = PathBuf::from(DEFAULT_MAP);
    let mut model_path = PathBuf::from("docs/api/model.json");
    let mut ledger_path: Option<PathBuf> = None;
    let mut list = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--map" => {
                map_path = PathBuf::from(args.get(i + 1).ok_or("--map wants a path")?);
                i += 1;
            }
            "--model" => {
                model_path = PathBuf::from(args.get(i + 1).ok_or("--model wants a path")?);
                i += 1;
            }
            "--ledger" => {
                ledger_path = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--ledger wants a path")?,
                ));
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }

    let model = read(&model_path)?;
    let model = zu_json::parse(&model).map_err(|e| format!("{}: {e}", model_path.display()))?;
    let ids = apimap::mappable_ids(&model)?;
    let map = read_map(&map_path)?;

    if list {
        for id in &ids {
            match map.tier_of(id) {
                Some(tier) => println!("{}\t{id}", tier.number()),
                None => println!("-\t{id}"),
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Which check runs is the map's own business, because a map that
    // classifies and a map that names are the same schema doing two
    // jobs and only the file knows which one it is. Each one also says
    // what it counted, so that a check that passes is still something
    // a reader can notice a change in.
    let (problems, summary) = if map.target == apimap::LEDGER {
        let census = map.census(&ids);
        let counts: Vec<String> = census
            .iter()
            .map(|(tier, n)| format!("{n} tier {}", tier.number()))
            .collect();
        (
            apimap::check_surface(&map, &ids),
            format!(
                "{} entities classified for {} ({})",
                ids.len(),
                map.target,
                counts.join(", ")
            ),
        )
    } else {
        let path = ledger_path.unwrap_or_else(|| PathBuf::from(DEFAULT_MAP));
        let ledger = read_map(&path)?;
        if ledger.target != apimap::LEDGER {
            return Err(format!(
                "{} is a {} map, and a binding map is checked against the {} one",
                path.display(),
                ledger.target,
                apimap::LEDGER
            ));
        }
        let owed = ledger.census(&ids).get(&apimap::Tier::Bound).copied();
        (
            apimap::check_binding(&map, &ledger, &ids),
            format!(
                "{} names {} entities, covering the {} owed at tier 1",
                map.target,
                map.entries.len(),
                owed.unwrap_or(0)
            ),
        )
    };

    if !problems.is_empty() {
        eprintln!(
            "{}: {} problem(s) against {}\n",
            map_path.display(),
            problems.len(),
            model_path.display()
        );
        // A binding map that was never written reports every tier-1
        // entity at once, and several hundred lines of the same
        // sentence teach a reader nothing the count did not. The rest
        // are one command away and the number of them is printed, so
        // this is a shortened list and not a quiet one.
        const SHOWN: usize = 20;
        for problem in problems.iter().take(SHOWN) {
            eprintln!("  {problem}");
        }
        if problems.len() > SHOWN {
            eprintln!(
                "  and {} more. Rerun with --list to see how every entity is classified.",
                problems.len() - SHOWN
            );
        }
        return Ok(ExitCode::FAILURE);
    }

    println!("{}: {summary}", map_path.display());
    Ok(ExitCode::SUCCESS)
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn read_map(path: &Path) -> Result<apimap::Map, String> {
    apimap::Map::parse(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))
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
