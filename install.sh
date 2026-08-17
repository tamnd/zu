#!/bin/sh
# The one-liner of dx/12 section 6:
#
#   curl -fsSL https://raw.githubusercontent.com/tamnd/zu/main/install.sh | sh
#
# It downloads the release archive for this machine, checks it against
# the digests the release published, and unpacks it as an install
# prefix. What lands is the whole of what a release carries for one
# platform, `bin/zu`, `include/zu.h`, both library forms, the pkg-config
# file and the CMake package config, because a user who installs the CLI
# today is a user who compiles against the library next week and a
# second download is a second version to keep straight.
#
# POSIX sh, deliberately: this runs on Alpine's ash, on Debian's dash
# and on macOS's ancient bash, and a bashism here is an installer that
# works everywhere it was tested and nowhere it was not. It also runs
# with its own source on stdin, since that is what `curl | sh` means, so
# nothing here reads from stdin.
#
# Every step that could fail quietly fails loudly instead. A missing
# hasher is an error rather than a skipped check, because an installer
# that verifies a download when it can is an installer that does not
# verify it on the machine where that mattered.

set -eu

# Where a release lives. The two forms differ because GitHub spells the
# newest release one way and a named one another, and an installer that
# built the URL itself would have to know which. ZU_BASE replaces both,
# which is what the tests point at a directory of files.
repo="tamnd/zu"
version="${ZU_VERSION:-latest}"
base="${ZU_BASE:-}"
prefix="${ZU_PREFIX:-$HOME/.zu}"
target="${ZU_TARGET:-}"
modify_path=1
quiet=0

usage() {
    cat <<'EOF'
usage: install.sh [options]

  --version <version>  the release to install, or `latest` (default)
  --prefix <dir>       where to install it (default ~/.zu)
  --target <triple>    the platform to install for, when this is not it
  --no-modify-path     do not offer to put bin/ on PATH
  --quiet              print nothing but errors
  --help               this

environment: ZU_VERSION, ZU_PREFIX, ZU_TARGET, ZU_BASE override the same
things, so the one-liner can be configured without arguments to pass
through `sh -s --`.
EOF
}

say() {
    if [ "$quiet" -eq 0 ]; then
        echo "$@"
    fi
}

die() {
    echo "install.sh: $*" >&2
    exit 1
}

# One of these has to exist to fetch anything, and one of the three
# hashers below has to exist to check it. Both are looked for before
# anything is downloaded, so a machine that cannot finish the install
# says so before it has spent a minute on a 20 MiB archive.
have() {
    command -v "$1" >/dev/null 2>&1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="${2:?--version takes a version}"; shift 2 ;;
        --prefix) prefix="${2:?--prefix takes a directory}"; shift 2 ;;
        --target) target="${2:?--target takes a target triple}"; shift 2 ;;
        --no-modify-path) modify_path=0; shift ;;
        --quiet) quiet=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; die "unknown option $1" ;;
    esac
done

# The target triple, which is the name of the artifact and therefore has
# to be one of the seven in platforms.toml rather than something spelled
# from uname. The linux libc is the part uname cannot answer: the same
# kernel and the same architecture run glibc on Debian and musl on
# Alpine, and the wrong one of those is a binary that dies at its first
# dynamic symbol. `ldd --version` naming musl is the reliable way to
# ask, and the loader on disk is the fallback for a system where ldd is
# a shell script that refuses to run.
detect_target() {
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64) arch=x86_64 ;;
        arm64|aarch64) arch=aarch64 ;;
        *) die "no release for $arch, build from source: cargo install zu-cli" ;;
    esac
    case "$(uname -s)" in
        Darwin) echo "$arch-apple-darwin" ;;
        Linux)
            libc=gnu
            if [ -n "$(ls /lib/ld-musl-* 2>/dev/null)" ]; then
                libc=musl
            elif ldd --version 2>&1 | grep -qi musl; then
                libc=musl
            fi
            echo "$arch-unknown-linux-$libc"
            ;;
        *)
            die "$(uname -s) is not a platform this script installs for, and Windows has install.ps1"
            ;;
    esac
}

# The digest of a file, from whichever of the three tools this machine
# has. They print it three different ways and all three put it first, so
# one cut takes it.
sha256() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then
        openssl dgst -sha256 "$1" | tr ' ' '\n' | tail -1
    else
        die "no sha256sum, shasum or openssl, so the download cannot be checked"
    fi
}

fetch() {
    if have curl; then
        curl -fsSL "$1" -o "$2"
    elif have wget; then
        wget -q "$1" -O "$2"
    else
        die "no curl and no wget"
    fi
}

# The archive is zstd, which tar reads directly on any libarchive built
# this decade and on GNU tar 1.31 and newer. The pipe is for the rest,
# and it is a real fallback rather than a nicety: a container image with
# busybox tar in it is the common case for the musl artifacts.
unpack() {
    archive="$1"
    into="$2"
    if tar --zstd -xf "$archive" -C "$into" 2>/dev/null; then
        return 0
    fi
    if have zstd; then
        zstd -dc "$archive" | tar -xf - -C "$into"
        return 0
    fi
    die "this tar cannot read zstd and there is no zstd on PATH"
}

[ -n "$target" ] || target="$(detect_target)"
have curl || have wget || die "no curl and no wget"

if [ -z "$base" ]; then
    if [ "$version" = latest ]; then
        base="https://github.com/$repo/releases/latest/download"
    else
        base="https://github.com/$repo/releases/download/v$version"
    fi
fi

archive="libzu-$target.tar.zst"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "installing zu for $target"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
    die "no release at $base, check the version"
fetch "$base/$archive" "$tmp/$archive" ||
    die "$target is not a platform this release publishes"

# The digest list is the release's own, and the line for this archive is
# the only one that matters here. A name that is not in the list is a
# release that published an artifact it did not record, which is the one
# case where installing anyway would defeat the whole check.
want="$(grep "  $archive\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || die "$archive is not in the release's SHA256SUMS"
got="$(sha256 "$tmp/$archive")"
if [ "$want" != "$got" ]; then
    die "$archive is not what the release says it is: $got, and SHA256SUMS says $want"
fi

# Unpacked beside the download and moved into place afterwards, so an
# install that fails half way through leaves the previous one alone
# rather than a prefix holding half of each.
mkdir -p "$tmp/unpacked"
unpack "$tmp/$archive" "$tmp/unpacked"
staged="$tmp/unpacked/libzu-$target"
[ -d "$staged" ] || die "$archive does not unpack as libzu-$target"

mkdir -p "$prefix"
# Copied per entry rather than as a directory, because `cp -R dir dest`
# means two different things depending on whether dest exists, and the
# second install on a machine is the one that would find out.
for entry in "$staged"/*; do
    name="$(basename "$entry")"
    rm -rf "$prefix/$name"
    cp -R "$entry" "$prefix/$name"
done
chmod +x "$prefix/bin/zu" 2>/dev/null || true

installed="$("$prefix/bin/zu" version 2>/dev/null | head -1)" ||
    die "$prefix/bin/zu does not run on this machine"
say "installed $installed in $prefix"

# PATH is advice and not an edit. A script that appends to a shell
# profile it guessed at is a script that appends to it again on the next
# install, and the guess is wrong for anybody using fish, nushell or a
# login shell they configured themselves.
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *)
        if [ "$modify_path" -eq 1 ]; then
            say ""
            say "add it to your PATH with:"
            say "    export PATH=\"$prefix/bin:\$PATH\""
        fi
        ;;
esac
