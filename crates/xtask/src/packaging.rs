//! The package manager manifests, generated from the same two tables
//! the release is.
//!
//! dx/12 section 6 asks for five ways in: `install.sh`, `install.ps1`,
//! Homebrew, Scoop and Docker. The two scripts are hand written, because
//! a script that detects a platform is a script and not a rendering of a
//! table. The two manifests are not: a Homebrew formula is four URLs and
//! four digests, a Scoop manifest is one of each, and every one of those
//! numbers is a fact `platforms.toml` and the release already know. A
//! hand maintained formula is the file that still installs 0.4.2 three
//! days after 0.5.0 shipped.
//!
//! So they are generated here and held to the tables by
//! `cargo xtask packaging --check`, which is how a tier 1 target added
//! to `platforms.toml` reaches the two package managers that can carry
//! it rather than being a platform users cannot install the easy way.
//!
//! The committed copies carry a digest of sixty-four zeros, because the
//! release that will fill them in has not happened when they are
//! written. That is deliberate and it is why the placeholder is a
//! well-formed digest of the wrong value rather than an empty string or
//! the word TODO: a manifest published by mistake fails at the checksum
//! and installs nothing, which is the failure worth having. The release
//! renders them again with `--sums`, over the `SHA256SUMS` it just
//! assembled.

use std::collections::BTreeMap;
use std::path::Path;

/// Where the two generated manifests live.
pub const HOMEBREW: &str = "packaging/homebrew/zu.rb";
pub const SCOOP: &str = "packaging/scoop/zu.json";

/// The other three ways in, which are checked for existing rather than
/// generated, since a target list is the only thing in them a table
/// could be the source of.
pub const INSTALL_SH: &str = "install.sh";
pub const INSTALL_PS1: &str = "install.ps1";
pub const DOCKERFILE: &str = "Dockerfile";

/// The digest of a file that does not exist yet.
pub const UNKNOWN: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One rendered file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub path: &'static str,
    pub text: String,
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A committed manifest is not what the tables render, which is
    /// either a hand edit or a table that moved without it.
    Drift {
        path: &'static str,
        line: usize,
        found: String,
        want: String,
    },
    /// One of the hand-written ways in is missing.
    Absent { path: &'static str },
    /// A tier 1 target no channel installs. The platform is built,
    /// published and unreachable by any of the five one-liners, which is
    /// a promise kept everywhere except where a user meets it.
    Unpackaged { target: String },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::Drift {
                path,
                line,
                found,
                want,
            } => write!(
                f,
                "{path}:{line}: is {found:?} and the tables render {want:?}. \
                 Run `cargo xtask packaging`."
            ),
            Note::Absent { path } => {
                write!(
                    f,
                    "{path}: is not in this tree, so dx/12 section 6 has a hole"
                )
            }
            Note::Unpackaged { target } => write!(
                f,
                "{target} is tier 1 and no install channel carries it, so the only way in is a source build"
            ),
        }
    }
}

/// Which of the five ways in carries a target.
///
/// Homebrew is macOS and glibc Linux, which is what its own bottles are.
/// Scoop is Windows. The musl rows are the container's, since a static
/// musl build is what makes an image work at all and neither package
/// manager has a place for one. The two scripts carry everything, which
/// is why they are not the interesting half of this list: the question
/// this answers is which targets have a channel a user already has
/// installed.
pub fn channels(target: &str) -> Vec<&'static str> {
    let mut out = vec![];
    if target.ends_with("-apple-darwin") || target.ends_with("-unknown-linux-gnu") {
        out.push("homebrew");
    }
    if target.ends_with("-pc-windows-msvc") {
        out.push("scoop");
    }
    if target.ends_with("-unknown-linux-musl") {
        out.push("docker");
    }
    if !target.contains("wasm") {
        out.push(if target.ends_with("-pc-windows-msvc") {
            "install.ps1"
        } else {
            "install.sh"
        });
    }
    out
}

/// Both manifests, for a version and whatever digests are known.
pub fn render(
    targets: &[String],
    version: &str,
    digests: &BTreeMap<String, String>,
) -> Result<Vec<Manifest>, String> {
    Ok(vec![
        Manifest {
            path: HOMEBREW,
            text: homebrew(targets, version, digests)?,
        },
        Manifest {
            path: SCOOP,
            text: scoop(targets, version, digests)?,
        },
    ])
}

/// The archive one target publishes, and the URL it publishes at.
fn archive(target: &str) -> String {
    format!("libzu-{target}.tar.zst")
}

fn url(version: &str, target: &str) -> String {
    format!(
        "https://github.com/tamnd/zu/releases/download/v{version}/{}",
        archive(target)
    )
}

fn digest(digests: &BTreeMap<String, String>, target: &str) -> String {
    digests
        .get(&archive(target))
        .cloned()
        .unwrap_or_else(|| UNKNOWN.to_string())
}

/// The one tier 1 target for an operating system and a cpu, or the
/// error that says the tables cannot render this manifest.
fn pick<'a>(targets: &'a [String], os: &str, cpu: &str) -> Result<&'a str, String> {
    targets
        .iter()
        .map(String::as_str)
        .find(|t| t.ends_with(os) && t.starts_with(cpu))
        .ok_or_else(|| format!("no tier 1 {cpu}{os} target, which {HOMEBREW} needs"))
}

/// The Homebrew formula: four platforms, each a URL and a digest.
///
/// It installs the unpacked prefix piece by piece rather than whole,
/// because Homebrew only links what lands in `bin`, `lib` and `include`,
/// and a formula that put the prefix under the cellar unlinked would
/// install a `zu` that is not on anybody's PATH.
fn homebrew(
    targets: &[String],
    version: &str,
    digests: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(&format!(
        "# Generated by `cargo xtask packaging` from platforms.toml. Edit that, not this.\n\
         #\n\
         # The digests are the release's own, copied from the SHA256SUMS it\n\
         # published, so `brew install` checks exactly what the install script\n\
         # checks. A digest of sixty-four zeros is a formula rendered before the\n\
         # release it names, which fails at the checksum rather than installing\n\
         # something nobody signed off on.\n\
         class Zu < Formula\n  \
           desc \"Embedded property graph database with a GQL engine\"\n  \
           homepage \"https://github.com/tamnd/zu\"\n  \
           version \"{version}\"\n  \
           license \"Apache-2.0\"\n"
    ));
    for (os, block, arm, intel) in [
        ("-apple-darwin", "on_macos", "aarch64", "x86_64"),
        ("-unknown-linux-gnu", "on_linux", "aarch64", "x86_64"),
    ] {
        out.push_str(&format!("\n  {block} do\n"));
        for (shape, cpu) in [("on_arm", arm), ("on_intel", intel)] {
            let target = pick(targets, os, cpu)?;
            out.push_str(&format!(
                "    {shape} do\n      url \"{}\"\n      sha256 \"{}\"\n    end\n",
                url(version, target),
                digest(digests, target)
            ));
        }
        out.push_str("  end\n");
    }
    out.push_str(
        "\n  def install\n    \
           bin.install \"bin/zu\"\n    \
           lib.install Dir[\"lib/*\"]\n    \
           include.install Dir[\"include/*\"]\n    \
           prefix.install \"LICENSE\"\n  \
         end\n\
         \n  test do\n    \
           assert_match version.to_s, shell_output(\"#{bin}/zu version\")\n    \
           assert_match \"usage: zu <command>\", shell_output(\"#{bin}/zu --help\")\n  \
         end\n\
         end\n",
    );
    Ok(out)
}

/// The Scoop manifest: one architecture, because Windows on Arm is tier
/// 2 and a manifest that offered it would be offering a build the
/// release does not block on.
///
/// `extract_dir` is the directory inside the archive, which is the
/// install prefix, so what Scoop shims is `bin\zu.exe` under it. The
/// `depends` is zstd, since Scoop's own extractor reaches for it and an
/// install that fails on a missing decompressor is an install one-liner
/// with a footnote.
fn scoop(
    targets: &[String],
    version: &str,
    digests: &BTreeMap<String, String>,
) -> Result<String, String> {
    let target = targets
        .iter()
        .map(String::as_str)
        .find(|t| *t == "x86_64-pc-windows-msvc")
        .ok_or_else(|| format!("no tier 1 x86_64-pc-windows-msvc target, which {SCOOP} needs"))?;
    Ok(format!(
        "{{\n    \
           \"##\": \"Generated by `cargo xtask packaging` from platforms.toml. Edit that, not this.\",\n    \
           \"version\": \"{version}\",\n    \
           \"description\": \"Embedded property graph database with a GQL engine\",\n    \
           \"homepage\": \"https://github.com/tamnd/zu\",\n    \
           \"license\": \"Apache-2.0\",\n    \
           \"depends\": \"zstd\",\n    \
           \"architecture\": {{\n        \
               \"64bit\": {{\n            \
                   \"url\": \"{}\",\n            \
                   \"hash\": \"{}\",\n            \
                   \"extract_dir\": \"libzu-{target}\"\n        \
               }}\n    \
           }},\n    \
           \"bin\": \"bin\\\\zu.exe\",\n    \
           \"checkver\": {{\n        \
               \"github\": \"https://github.com/tamnd/zu\"\n    \
           }},\n    \
           \"autoupdate\": {{\n        \
               \"architecture\": {{\n            \
                   \"64bit\": {{\n                \
                       \"url\": \"https://github.com/tamnd/zu/releases/download/v$version/libzu-{target}.tar.zst\",\n                \
                       \"extract_dir\": \"libzu-{target}\"\n            \
                   }}\n        \
               }},\n        \
               \"hash\": {{\n            \
                   \"url\": \"https://github.com/tamnd/zu/releases/download/v$version/SHA256SUMS\"\n        \
               }}\n    \
           }}\n\
         }}\n",
        url(version, target),
        digest(digests, target),
    ))
}

/// Every digest a `SHA256SUMS` records, by the name it records it for.
///
/// The format is `sha256sum`'s own, two spaces between the digest and
/// the name, which is what the release writes and what both install
/// scripts read.
pub fn digests(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((digest, name)) = line.split_once("  ") {
            out.insert(name.trim().to_string(), digest.trim().to_string());
        }
    }
    out
}

/// The committed manifests against what the tables render, and every
/// tier 1 target against the channels that carry it.
pub fn check(root: &Path, targets: &[String], version: &str) -> Result<Vec<Note>, String> {
    let mut notes = Vec::new();
    for manifest in render(targets, version, &BTreeMap::new())? {
        let path = root.join(manifest.path);
        let found = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                notes.push(Note::Absent {
                    path: manifest.path,
                });
                continue;
            }
        };
        if let Some(note) = drift(manifest.path, &found, &manifest.text) {
            notes.push(note);
        }
    }
    for path in [INSTALL_SH, INSTALL_PS1, DOCKERFILE] {
        if !root.join(path).exists() {
            notes.push(Note::Absent { path });
        }
    }
    for target in targets {
        if channels(target).is_empty() {
            notes.push(Note::Unpackaged {
                target: target.clone(),
            });
        }
    }
    Ok(notes)
}

/// The first line the two texts disagree on, which is the line a person
/// wants rather than the whole file twice.
fn drift(path: &'static str, found: &str, want: &str) -> Option<Note> {
    if found == want {
        return None;
    }
    let mut theirs = found.lines();
    let mut ours = want.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (theirs.next(), ours.next()) {
            (None, None) => return None,
            (a, b) if a == b => continue,
            (a, b) => {
                return Some(Note::Drift {
                    path,
                    line,
                    found: a.unwrap_or("<end of file>").to_string(),
                    want: b.unwrap_or("<end of file>").to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIER1: [&str; 7] = [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ];

    fn targets() -> Vec<String> {
        TIER1.iter().map(|t| (*t).to_string()).collect()
    }

    /// The formula names four platforms and no others, since the three
    /// it leaves out are the two musl rows, which are the container's,
    /// and Windows, which is Scoop's.
    #[test]
    fn the_formula_carries_the_four_platforms_homebrew_installs_on() {
        let text = homebrew(&targets(), "1.2.3", &BTreeMap::new()).expect("renders");
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            assert!(text.contains(&archive(target)), "{target} is not in it");
        }
        for target in [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(!text.contains(&archive(target)), "{target} is in it");
        }
        assert_eq!(text.matches("sha256 \"").count(), 4);
        assert_eq!(text.matches("url \"").count(), 4);
        assert!(text.contains("version \"1.2.3\""));
    }

    /// A target the formula needs and the table does not have is an
    /// error and not a formula with a gap in it, because a formula with
    /// three platforms in it installs on the fourth by failing.
    #[test]
    fn a_platform_the_formula_needs_and_the_table_lacks_is_an_error() {
        let without: Vec<String> = targets()
            .into_iter()
            .filter(|t| t != "x86_64-apple-darwin")
            .collect();
        let e = homebrew(&without, "1.2.3", &BTreeMap::new()).expect_err("no formula");
        assert!(e.contains("x86_64-apple-darwin"), "{e}");
        let e = scoop(&[], "1.2.3", &BTreeMap::new()).expect_err("no manifest");
        assert!(e.contains("windows"), "{e}");
    }

    /// The digests are the release's, read out of the file the release
    /// writes, and a name with no line in it renders as the placeholder
    /// rather than as nothing.
    #[test]
    fn the_digests_come_from_the_release_and_what_is_missing_is_visibly_missing() {
        let sums = format!(
            "{}  libzu-aarch64-apple-darwin.tar.zst\n{}  libzu-x86_64-pc-windows-msvc.tar.zst\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let known = digests(&sums);
        assert_eq!(known.len(), 2);

        let text = homebrew(&targets(), "1.2.3", &known).expect("renders");
        assert!(text.contains(&format!("sha256 \"{}\"", "a".repeat(64))));
        assert_eq!(
            text.matches(UNKNOWN).count(),
            3,
            "the three not in the list"
        );

        let text = scoop(&targets(), "1.2.3", &known).expect("renders");
        assert!(text.contains(&format!("\"hash\": \"{}\"", "b".repeat(64))));
    }

    /// The placeholder is the shape of a digest and the value of no
    /// file, so a manifest that reaches a user before its release fails
    /// at the checksum instead of installing whatever is at the URL.
    #[test]
    fn the_placeholder_is_a_digest_no_file_has() {
        assert_eq!(UNKNOWN.len(), 64);
        assert!(UNKNOWN.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(UNKNOWN, crate::sha256::hex(b""));
    }

    /// The manifest Scoop reads has to be JSON, and the autoupdate block
    /// is what keeps the manifest current without a person, so both are
    /// checked rather than assumed.
    #[test]
    fn the_scoop_manifest_is_json_scoop_can_read() {
        let text = scoop(&targets(), "1.2.3", &BTreeMap::new()).expect("renders");
        let doc = zu_json::parse(&text).expect("json");
        assert_eq!(
            doc.get("version").and_then(zu_json::Json::as_str),
            Some("1.2.3")
        );
        assert_eq!(
            doc.get("bin").and_then(zu_json::Json::as_str),
            Some("bin\\zu.exe")
        );
        let arch = doc
            .get("architecture")
            .and_then(|a| a.get("64bit"))
            .expect("an architecture");
        assert_eq!(
            arch.get("extract_dir").and_then(zu_json::Json::as_str),
            Some("libzu-x86_64-pc-windows-msvc")
        );
        assert!(
            doc.get("autoupdate")
                .and_then(|a| a.get("hash"))
                .and_then(|h| h.get("url"))
                .and_then(zu_json::Json::as_str)
                .is_some_and(|u| u.ends_with("SHA256SUMS")),
            "autoupdate reads the release's own digests"
        );
    }

    /// Every tier 1 target reaches a user through something. This is the
    /// check that makes the eighth platform somebody adds a platform
    /// with a way in rather than a build.
    #[test]
    fn every_tier_one_target_has_a_channel_that_carries_it() {
        for target in TIER1 {
            assert!(!channels(target).is_empty(), "{target}");
        }
        assert_eq!(channels("aarch64-apple-darwin"), ["homebrew", "install.sh"]);
        assert_eq!(
            channels("x86_64-pc-windows-msvc"),
            ["scoop", "install.ps1"],
            "Windows is Scoop's and the PowerShell script's, and neither of the other two"
        );
        assert_eq!(
            channels("x86_64-unknown-linux-musl"),
            ["docker", "install.sh"]
        );
        assert!(channels("wasm32-unknown-unknown").is_empty());
    }

    /// The drift note names the first line that differs, because a
    /// generated file that says "these two files differ" has told a
    /// person to run a diff themselves.
    #[test]
    fn drift_names_the_first_line_that_differs() {
        assert_eq!(drift(SCOOP, "a\nb\n", "a\nb\n"), None);
        assert_eq!(
            drift(SCOOP, "a\nb\n", "a\nc\n"),
            Some(Note::Drift {
                path: SCOOP,
                line: 2,
                found: "b".to_string(),
                want: "c".to_string(),
            })
        );
        assert_eq!(
            drift(SCOOP, "a\n", "a\nb\n"),
            Some(Note::Drift {
                path: SCOOP,
                line: 2,
                found: "<end of file>".to_string(),
                want: "b".to_string(),
            })
        );
    }

    /// The tree's own manifests, which is the check CI runs, and it is
    /// here as well so that a table edit fails the test suite rather
    /// than the release.
    #[test]
    fn the_committed_manifests_are_what_this_tree_renders() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the workspace root");
        let targets = crate::artifacts::tier1(root).expect("the tier 1 targets");
        let notes = check(root, &targets, env!("CARGO_PKG_VERSION")).expect("the check runs");
        assert!(notes.is_empty(), "{notes:?}");
    }
}
