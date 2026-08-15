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

use xtask::{apimap, artifacts, corpus, model, pins, platforms, repos, rustdoc, terms};

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

cargo xtask corpus-pack [--cases DIR] [--readme PATH] [--out PATH] [--version V] [--check]

  --cases DIR       the case files (default conformance/cases)
  --readme PATH     the README to ship beside them (default conformance/README.md)
  --out PATH        where to write the archive (default target/conformance-<version>.tar.zst)
  --version V       the version the artifact is named for (default this workspace's)
  --check           pack and report, writing nothing

cargo xtask pins [--table PATH] [--list]

  --table PATH      the toolchain table (default toolchains.toml)
  --list            print `component<TAB>pinned<TAB>floor<TAB>repos` for every row, and check nothing

cargo xtask platforms [--table PATH] [--list] [--measure DIR --target TARGET]

  --table PATH      the platform table (default platforms.toml)
  --list            print `tier<TAB>target<TAB>runner<TAB>lib` for every row, and check nothing
  --measure DIR     weigh what a build put in DIR against the size budgets
  --target TARGET   the target that build was for, which says what the files are called

cargo xtask artifacts [--table PATH] [--list] [--assemble DIR] [--verify DIR] [--built DIR] [--version V]

  --table PATH      the artifact contract (default artifacts.toml)
  --list            print `made<TAB>name<TAB>consumers` for every row, and check nothing
  --assemble DIR    gather a release into DIR, from this tree and the platform builds
  --verify DIR      read a release directory back against the contract
  --built DIR       where the platform jobs' artifacts were downloaded (default built)
  --version V       the version being released (default this workspace's)

cargo xtask repos [--table PATH] [--list]

  --table PATH      the repository table (default repos.toml)
  --list            print `role<TAB>name<TAB>tier<TAB>reports` for every row, and check nothing

cargo xtask terms [--table PATH] [--list] [PATH ...]

  --table PATH      the terminology table (default zu-web/style/zu/terms.yml, here or one level up)
  --list            print `group<TAB>term<TAB>definition` for every term, and check nothing
  PATH ...          what to check (default docs, crates, README.md, conformance/README.md)
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
        Some("corpus-pack") => run(corpus_pack_command(&args[1..])),
        Some("pins") => run(pins_command(&args[1..])),
        Some("platforms") => run(platforms_command(&args[1..])),
        Some("artifacts") => run(artifacts_command(&args[1..])),
        Some("repos") => run(repos_command(&args[1..])),
        Some("terms") => run(terms_command(&args[1..])),
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

fn corpus_pack_command(args: &[String]) -> Result<ExitCode, String> {
    let mut cases = PathBuf::from("conformance/cases");
    let mut readme = PathBuf::from("conformance/README.md");
    let mut out: Option<PathBuf> = None;
    // The workspace version, which is the engine version the cases are
    // the contract for. Taking it from the build rather than from an
    // argument is what keeps an artifact from being named for a
    // version it does not hold.
    let mut version = env!("CARGO_PKG_VERSION").to_string();
    let mut check = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--cases" => {
                cases = PathBuf::from(args.get(i + 1).ok_or("--cases wants a path")?);
                i += 1;
            }
            "--readme" => {
                readme = PathBuf::from(args.get(i + 1).ok_or("--readme wants a path")?);
                i += 1;
            }
            "--out" => {
                out = Some(PathBuf::from(args.get(i + 1).ok_or("--out wants a path")?));
                i += 1;
            }
            "--version" => {
                version = args.get(i + 1).ok_or("--version wants a version")?.clone();
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }

    let readme = readme.exists().then_some(readme);
    let packed = corpus::pack(&cases, readme.as_deref(), &version)?;
    let ratio = packed.tar.len() as f64 / packed.archive.len() as f64;
    let summary = format!(
        "{}.tar.zst: {} suites, {} cases, {} KiB packed from {} KiB ({ratio:.1}x)",
        packed.prefix,
        packed.entries.len(),
        packed.cases(),
        packed.archive.len().div_ceil(1024),
        packed.tar.len().div_ceil(1024),
    );
    if check {
        println!("{summary}, written nowhere");
        return Ok(ExitCode::SUCCESS);
    }

    let out = out.unwrap_or_else(|| PathBuf::from(format!("target/{}.tar.zst", packed.prefix)));
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(&out, &packed.archive).map_err(|e| format!("writing {}: {e}", out.display()))?;
    println!("{}: {summary}", out.display());
    Ok(ExitCode::SUCCESS)
}

fn pins_command(args: &[String]) -> Result<ExitCode, String> {
    let mut path = PathBuf::from(pins::PATH);
    let mut list = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--table" => {
                path = PathBuf::from(args.get(i + 1).ok_or("--table wants a path")?);
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }
    let table = pins::Table::load(&path)?;

    if list {
        for component in &table.components {
            println!(
                "{}\t{}\t{}\t{}",
                component.name,
                component.pinned,
                component.floor.as_deref().unwrap_or(""),
                component.repos.join(" ")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // The table is at the root of the tree it describes, so the tree is
    // wherever the table was found and not wherever this was run.
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let notes = table.check(&root)?;
    if notes.is_empty() {
        println!(
            "{} components, {} sites, audited {}, nothing has drifted",
            table.components.len(),
            table.sites.len(),
            table.audited
        );
        return Ok(ExitCode::SUCCESS);
    }
    for note in &notes {
        eprintln!("{note}");
    }
    eprintln!(
        "\n{} to fix. Bumping a version is a change to {} and to every site of it.",
        notes.len(),
        path.display()
    );
    Ok(ExitCode::FAILURE)
}

fn platforms_command(args: &[String]) -> Result<ExitCode, String> {
    let mut path = PathBuf::from(platforms::PATH);
    let mut list = false;
    let mut measure: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--table" => {
                path = PathBuf::from(args.get(i + 1).ok_or("--table wants a path")?);
                i += 1;
            }
            "--measure" => {
                measure = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--measure wants a path")?,
                ));
                i += 1;
            }
            "--target" => {
                target = Some(args.get(i + 1).ok_or("--target wants a target")?.clone());
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }
    let table = platforms::Table::load(&path)?;

    if let Some(dir) = measure {
        // The files are named differently on every platform, so the
        // target is what says which names to look for, and asking for
        // one without the other is a question with no answer.
        let target = target.ok_or("--measure wants the --target it was built for")?;
        let weights = table.weigh(&target, &dir)?;
        let mut over = 0;
        for weight in &weights {
            println!("{weight}");
            if weight.over() {
                over += 1;
            }
        }
        if over == 0 {
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!(
            "\n{over} of {} over budget on {target}. The ceilings are in {}, and they are \
             ceilings because size only ever drifts upward.",
            weights.len(),
            path.display()
        );
        return Ok(ExitCode::FAILURE);
    }
    if target.is_some() {
        return Err("--target is what --measure is read with".to_string());
    }

    if list {
        for platform in &table.platforms {
            println!(
                "{}\t{}\t{}\t{}",
                platform.tier,
                platform.target,
                platform.runner.as_deref().unwrap_or(""),
                platform.lib
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Same rule as the toolchain table above: the table sits at the root
    // of the tree it describes.
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let notes = table.check(&root)?;
    if notes.is_empty() {
        let tier1 = table
            .platforms
            .iter()
            .filter(|p| p.tier == platforms::TIER1)
            .count();
        println!(
            "{} platforms, {tier1} of them tier 1 and every one of those built, audited {}",
            table.platforms.len(),
            table.audited
        );
        return Ok(ExitCode::SUCCESS);
    }
    for note in &notes {
        eprintln!("{note}");
    }
    eprintln!(
        "\n{} to fix. Adding a platform is a change to {} and to the matrix that builds it.",
        notes.len(),
        path.display()
    );
    Ok(ExitCode::FAILURE)
}

fn artifacts_command(args: &[String]) -> Result<ExitCode, String> {
    let mut path = PathBuf::from(artifacts::PATH);
    let mut list = false;
    let mut assemble: Option<PathBuf> = None;
    let mut verify: Option<PathBuf> = None;
    let mut built = PathBuf::from("built");
    // Same rule the corpus packer follows: the version comes from the
    // build rather than from a habit, so an artifact cannot be named
    // for a version it does not hold.
    let mut version = env!("CARGO_PKG_VERSION").to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--table" => {
                path = PathBuf::from(args.get(i + 1).ok_or("--table wants a path")?);
                i += 1;
            }
            "--assemble" => {
                assemble = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--assemble wants a path")?,
                ));
                i += 1;
            }
            "--verify" => {
                verify = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--verify wants a path")?,
                ));
                i += 1;
            }
            "--built" => {
                built = PathBuf::from(args.get(i + 1).ok_or("--built wants a path")?);
                i += 1;
            }
            "--version" => {
                version = args.get(i + 1).ok_or("--version wants a version")?.clone();
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }
    let table = artifacts::Table::load(&path)?;

    if list {
        for artifact in &table.artifacts {
            println!(
                "{}\t{}\t{}",
                artifact.made.kind(),
                artifact.name,
                artifact.consumers.join(" ")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // The table sits at the root of the tree it describes, the same as
    // the two tables beside it.
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let targets = artifacts::tier1(&root)?;

    if let Some(out) = &assemble {
        let made = table.assemble(&root, &built, out, &version, &targets)?;
        for one in &made {
            println!("{one}");
        }
        let later: Vec<&str> = table
            .artifacts
            .iter()
            .filter(|a| !a.published())
            .map(|a| a.name.as_str())
            .collect();
        // A release of six files against a contract of seven rows is a
        // sentence worth printing, because the alternative is a reader
        // counting and wondering.
        println!(
            "{}: {} files for {version}, and {} the contract names that nothing makes yet ({})",
            out.display(),
            made.len(),
            later.len(),
            later.join(", ")
        );
    }

    if let Some(dir) = &verify {
        let (shipped, faults) = table.verify(dir, &version, &targets)?;
        for one in &shipped {
            println!("{one}");
        }
        if !faults.is_empty() {
            for fault in &faults {
                eprintln!("{fault}");
            }
            eprintln!(
                "\n{} to fix in {}. What a release publishes is {}, in both directions.",
                faults.len(),
                dir.display(),
                path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        println!(
            "{}: {} artifacts for {version}, every one of them in {}",
            dir.display(),
            shipped.len(),
            path.display()
        );
    }

    if assemble.is_none() && verify.is_none() {
        let notes = table.check(&root)?;
        if !notes.is_empty() {
            for note in &notes {
                eprintln!("{note}");
            }
            eprintln!(
                "\n{} to fix. What a release publishes is {}, and the release workflow assembles \
                 from it rather than from a list of its own.",
                notes.len(),
                path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        let published = table.artifacts.iter().filter(|a| a.published()).count();
        println!(
            "{} artifacts, {published} of them published today across {} platforms, audited {}",
            table.artifacts.len(),
            targets.len(),
            table.audited
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn repos_command(args: &[String]) -> Result<ExitCode, String> {
    let mut path = PathBuf::from(repos::PATH);
    let mut list = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--table" => {
                path = PathBuf::from(args.get(i + 1).ok_or("--table wants a path")?);
                i += 1;
            }
            other => return Err(format!("no option {other:?}\n\n{USAGE}")),
        }
        i += 1;
    }
    let table = repos::Table::load(&path)?;

    if list {
        for repo in &table.repos {
            println!(
                "{}\t{}\t{}\t{}",
                repo.role.name(),
                repo.name,
                repo.tier.map(|t| t.to_string()).unwrap_or_default(),
                repo.reports.join(" ")
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // The table sits at the root of the tree it describes, the same as
    // the three tables beside it.
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let notes = table.check(&root)?;
    if notes.is_empty() {
        let reports: usize = table.dispatched().map(|r| r.reports.len()).sum();
        println!(
            "{} repositories, {} of them driven by the train and reporting {reports} things back, \
             audited {}",
            table.repos.len(),
            table.dispatched().count(),
            table.audited
        );
        return Ok(ExitCode::SUCCESS);
    }
    for note in &notes {
        eprintln!("{note}");
    }
    eprintln!(
        "\n{} to fix. The repositories of this project are {}, and the conductor, the README and \
         the artifact contract all read that list.",
        notes.len(),
        path.display()
    );
    Ok(ExitCode::FAILURE)
}

/// The prose this repository publishes: the documentation, and the doc
/// comments that become reference pages. The engine's own source is in
/// here because a doc comment is a reference page, not because a
/// comment is prose; nothing outside `//!` and `///` is read.
const PROSE: [&str; 4] = ["docs", "crates", "README.md", "conformance/README.md"];

fn terms_command(args: &[String]) -> Result<ExitCode, String> {
    let mut table: Option<PathBuf> = None;
    let mut list = false;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => list = true,
            "--table" => {
                table = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--table wants a path")?,
                ));
                i += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("no option {other:?}\n\n{USAGE}"));
            }
            path => roots.push(PathBuf::from(path)),
        }
        i += 1;
    }

    let path = match table {
        Some(path) => path,
        None => terms::beside().ok_or_else(|| {
            format!(
                "no {} here or one level up. Clone tamnd/zu-web, or pass --table",
                terms::PATH
            )
        })?,
    };
    let table = terms::Table::load(&path)?;

    if list {
        for term in &table.terms {
            println!("{}\t{}\t{}", term.group, term.term, term.doc);
        }
        return Ok(ExitCode::SUCCESS);
    }

    if roots.is_empty() {
        roots = PROSE.iter().map(PathBuf::from).collect();
    }
    let mut files = Vec::new();
    for root in &roots {
        walk(root, &mut files)?;
    }
    files.sort();

    let mut notes = Vec::new();
    let mut checked = 0;
    for file in &files {
        let Some(kind) = terms::Kind::of(file) else {
            continue;
        };
        checked += 1;
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("reading {}: {e}", file.display()))?;
        notes.extend(table.check(&file.display().to_string(), &text, kind));
    }

    if notes.is_empty() {
        println!(
            "{checked} files, {} terms, {} forms, nothing to fix",
            table.terms.len(),
            table.forms()
        );
        return Ok(ExitCode::SUCCESS);
    }
    for note in &notes {
        eprintln!("{note}");
    }
    let mut files: Vec<&str> = notes.iter().map(|n| n.file()).collect();
    files.sort_unstable();
    files.dedup();
    eprintln!(
        "\n{} to fix in {} of {checked} files. The table is {}.",
        notes.len(),
        files.len(),
        path.display()
    );
    Ok(ExitCode::FAILURE)
}

/// Every file under `root`, or `root` itself if it is one.
fn walk(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if root.is_file() {
        out.push(root.to_path_buf());
        return Ok(());
    }
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("reading {}: {e}", root.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("reading {}: {e}", root.display()))?
            .path();
        // `target` is a build, not a source, and it holds a copy of
        // every dependency's documentation.
        if path.file_name().is_some_and(|n| n == "target") {
            continue;
        }
        match path.is_dir() {
            true => walk(&path, out)?,
            false => out.push(path),
        }
    }
    Ok(())
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
