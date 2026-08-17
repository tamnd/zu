//! `install.sh` against a release that is a directory of files.
//!
//! The one-liner of dx/12 section 6 is the first thing a user runs and
//! the one piece of this repository that cannot be exercised by a unit
//! test: it is a shell script whose job is to talk to a release. So the
//! release is faked here, as a directory holding an archive shaped
//! exactly like the one `cargo xtask artifacts --assemble` writes and
//! the `SHA256SUMS` beside it, and `ZU_BASE` points the script at it
//! over `file://`. Everything after the download is then the real code
//! path, digest check included.
//!
//! The interesting case is the one with no `--target`: the archive is
//! named for the triple this build is for, so a script that detected
//! the platform differently asks for a file that is not there and the
//! test fails. That is the only way to check `uname` mapping without
//! reimplementing it in the assertion, and it is why CI running this on
//! four platforms is worth more than four times as many cases here.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use xtask::{sha256, tarball};

/// The triple this test binary was built for, which is the triple the
/// script has to work out for itself.
fn target() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => {
            let libc = if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            };
            format!("{arch}-unknown-linux-{libc}")
        }
        other => panic!("this test does not know what {other} calls itself"),
    }
}

/// A directory nothing else is writing to. The same argument the CLI's
/// corpus command makes: `tempfile` is not a dependency of this crate
/// and one process id and one counter are enough.
fn tempdir(what: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("zu-install-{what}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a temp dir");
    path
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

/// A release directory: one platform archive laid out as a prefix, and
/// the digest list over it.
///
/// The `zu` inside is a shell script rather than a build of the real
/// one, because what is under test is the installing and not the
/// program: a real binary would make this test cost a release build and
/// tell it nothing it does not already learn from four lines of sh.
fn release(target: &str) -> PathBuf {
    let dir = tempdir("release");
    let archive = format!("libzu-{target}.tar.zst");
    let files = vec![
        (
            format!("libzu-{target}/bin/zu"),
            b"#!/bin/sh\necho \"zu 9.9.9\"\n".to_vec(),
        ),
        (
            format!("libzu-{target}/include/zu.h"),
            b"/* the C ABI */\n".to_vec(),
        ),
        (
            format!("libzu-{target}/lib/pkgconfig/zu.pc"),
            b"Name: zu\n".to_vec(),
        ),
        (format!("libzu-{target}/LICENSE"), b"Apache-2.0\n".to_vec()),
    ];
    let packed = tarball::compress(&tarball::tar(&files).expect("a tar")).expect("zstd");
    std::fs::write(dir.join(&archive), &packed).expect("write");
    std::fs::write(
        dir.join("SHA256SUMS"),
        format!("{}  {archive}\n", sha256::hex(&packed)),
    )
    .expect("write");
    dir
}

/// Runs the script against a release directory, and returns what it
/// did.
fn install(release: &Path, prefix: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(root().join("install.sh"))
        .args(["--prefix", &prefix.display().to_string()])
        .arg("--no-modify-path")
        .args(args)
        .env("ZU_BASE", format!("file://{}", release.display()))
        .output()
        .expect("sh runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// curl is what reads a `file://` URL here. wget refuses the scheme, so
/// a machine with only that one cannot run these cases, and skipping is
/// honest where failing would be a bug report about the test.
fn has_curl() -> bool {
    Command::new("sh")
        .args(["-c", "command -v curl"])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_platform_it_works_out_for_itself_is_the_one_this_build_is_for() {
    if !has_curl() {
        return;
    }
    let target = target();
    let release = release(&target);
    let prefix = tempdir("prefix");

    // No --target, so the archive is found only if `uname` was read the
    // way platforms.toml names the same machine.
    let out = install(&release, &prefix, &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains(&target), "{said}");
    assert!(
        said.contains("installed zu 9.9.9"),
        "the version it prints is the one it ran: {said}"
    );

    // The whole prefix and not just the program, because a user who
    // installs the CLI today compiles against the library next week.
    for path in ["bin/zu", "include/zu.h", "lib/pkgconfig/zu.pc", "LICENSE"] {
        assert!(prefix.join(path).exists(), "{path} did not land");
    }
    let program = Command::new(prefix.join("bin/zu"))
        .output()
        .expect("the installed program runs");
    assert!(program.status.success(), "it unpacked unrunnable");
}

/// The reason the digest list is fetched first. A one-liner that pipes
/// an unverified download into a shell is the install story every audit
/// stops at, so a tampered archive has to stop the install rather than
/// be noticed afterwards.
#[test]
fn an_archive_that_is_not_what_the_release_says_installs_nothing() {
    if !has_curl() {
        return;
    }
    let target = target();
    let release = release(&target);
    let prefix = tempdir("prefix");
    std::fs::write(
        release.join("SHA256SUMS"),
        format!("{}  libzu-{target}.tar.zst\n", "f".repeat(64)),
    )
    .expect("write");

    let out = install(&release, &prefix, &[]);
    assert!(
        !out.status.success(),
        "it installed a file it could not check"
    );
    assert!(
        stderr(&out).contains("is not what the release says it is"),
        "{}",
        stderr(&out)
    );
    assert!(
        !prefix.join("bin/zu").exists(),
        "nothing lands from an archive that failed its digest"
    );
}

/// A release that published an artifact it did not record is the one
/// case where installing anyway would defeat the check entirely, so it
/// is refused rather than treated as an unchecked download.
#[test]
fn an_archive_with_no_line_in_the_digest_list_is_refused() {
    if !has_curl() {
        return;
    }
    let release = release(&target());
    let prefix = tempdir("prefix");
    std::fs::write(release.join("SHA256SUMS"), "").expect("write");

    let out = install(&release, &prefix, &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("SHA256SUMS"), "{}", stderr(&out));
}

/// A platform the release does not carry gets the sentence that says
/// so, rather than the fetcher's opinion of a 404.
#[test]
fn a_platform_the_release_does_not_publish_says_which_one() {
    if !has_curl() {
        return;
    }
    let release = release(&target());
    let prefix = tempdir("prefix");

    let out = install(
        &release,
        &prefix,
        &["--target", "sparc64-unknown-linux-gnu"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("sparc64-unknown-linux-gnu is not a platform this release publishes"),
        "{}",
        stderr(&out)
    );
}

/// A release that is not there at all, which is what a mistyped version
/// looks like, and the two failures are told apart on purpose.
#[test]
fn a_version_that_was_never_released_says_to_check_the_version() {
    if !has_curl() {
        return;
    }
    let prefix = tempdir("prefix");
    let out = install(&tempdir("empty"), &prefix, &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("check the version"),
        "{}",
        stderr(&out)
    );
}

/// The script is its own documentation, and a wrong option is a usage
/// error rather than a download.
#[test]
fn it_explains_itself_and_refuses_an_option_it_does_not_have() {
    let out = Command::new("sh")
        .arg(root().join("install.sh"))
        .arg("--help")
        .output()
        .expect("sh runs");
    assert!(out.status.success());
    let said = String::from_utf8_lossy(&out.stdout);
    for flag in ["--version", "--prefix", "--target", "--no-modify-path"] {
        assert!(said.contains(flag), "{flag} is not in the usage: {said}");
    }

    let out = Command::new("sh")
        .arg(root().join("install.sh"))
        .arg("--no-such-flag")
        .output()
        .expect("sh runs");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unknown option"), "{}", stderr(&out));
}

/// Both scripts read the same file, so the format the digest list is
/// written in is a promise to two readers. This holds the shell one to
/// what the release writes, since the pair of spaces between the two
/// fields is what `sha256sum -c` wants and what the PowerShell script's
/// pattern matches.
#[test]
fn the_two_scripts_read_the_digest_list_the_same_way() {
    let root = root();
    let sh = std::fs::read_to_string(root.join("install.sh")).expect("install.sh");
    let ps1 = std::fs::read_to_string(root.join("install.ps1")).expect("install.ps1");
    assert!(
        sh.contains("SHA256SUMS"),
        "install.sh reads the digest list"
    );
    assert!(ps1.contains("SHA256SUMS"), "install.ps1 reads it too");
    for name in ["ZU_VERSION", "ZU_PREFIX", "ZU_TARGET", "ZU_BASE"] {
        assert!(sh.contains(name), "install.sh honours {name}");
        assert!(ps1.contains(name), "install.ps1 honours {name}");
    }
}
