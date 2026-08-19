#!/bin/sh
# What a reader gets, built on a machine that has nothing else on it.
#
#     scripts/crate-check.sh <repository url> <revision>
#
# Every other job in this repository builds inside the workspace, with a
# lockfile, a target directory somebody else warmed, and a hosted image
# carrying a decade of development packages. None of that is true of the
# person who read the README and typed cargo add. What they have is a
# toolchain, a network, and a crate of their own, and the failures that
# only they see are the ones nothing else looks for: a dependency that
# needs pkg-config and a system library, a build script that shells out
# to a tool the image happened to have, a feature that resolves one way
# inside this workspace and another way outside it, a rev nobody can
# fetch because it was never pushed.
#
# So this makes a crate outside the workspace, points it at this
# revision, and builds and runs the README's own sixty seconds program
# with nothing else on the machine. Standard shell, no cargo plugins,
# no checkout: the container mounts one file, which is the snippet a
# test already holds to the README character for character.
#
# What it checks is already checked in tests/readme.rs, which holds the
# same two lines of output and the same database file. That is on
# purpose and must stay that way: the interesting variable here is the
# machine, not the claim, and a second program asking a different
# question would not tell anybody whether the first one still works
# outside the workspace.
#
# The dependency is written in the git form rather than as a version,
# because zu is publish = false and there is nothing on crates.io yet.
# The day there is, this is the line that changes and the rest of the
# program stays as it is.

set -eu

repository=${1:?the repository url to depend on}
revision=${2:?the revision to depend on}
program=${PROGRAM:-/sixty-seconds.rs}

# What the image is claimed to be, checked rather than believed. A
# toolchain image has to carry a compiler and a linker, so those are not
# on the list and cannot be. Neither is perl, which debian-slim carries
# as part of the base system rather than as a build tool. Everything
# else is: the day a base image starts shipping pkg-config is the day a
# dependency can start needing it without anybody here finding out, and
# the day it ships git is the day this stops saying anything about cargo
# fetching a revision on its own.
for tool in git python3 make cmake pkg-config node; do
	if command -v "$tool" >/dev/null 2>&1; then
		echo "this image has $tool on it, so it is not the machine this job is about"
		exit 1
	fi
done

# Somewhere that is not the checkout and not the current directory,
# because a crate that only builds beside the engine's own workspace is
# a crate nobody outside it can build. /tmp is outside every workspace
# root cargo would walk up to from here.
crate=$(mktemp -d)/sixty-seconds
mkdir -p "$crate/src"
cp "$program" "$crate/src/main.rs"

cat >"$crate/Cargo.toml" <<EOF
[package]
name = "sixty-seconds"
version = "0.0.0"
edition = "2024"

[dependencies]
zudb = { package = "zu", git = "$repository", rev = "$revision" }
EOF

cd "$crate"

# Debug rather than release, because this is about the machine and not
# about the optimiser, and because debug is what cargo run does and so
# is what the reader of the README actually types first.
cargo run --quiet >output.txt

# The program prints two people, in the order the engine returned them,
# and a job that only checked the exit code would pass on a program that
# printed nothing at all.
cat >expected.txt <<'EOF'
ada 1
grace 2
EOF

if ! diff -u expected.txt output.txt; then
	echo "the sixty seconds program built and ran and printed something else"
	exit 1
fi

# And the file it was told to make, since a database that lives in a
# page cache and never on a disk prints all of the above.
test -s social.zu1 || { echo "the program made no database file"; exit 1; }

echo "zudb at $revision, on $(rustc --version): it works"
