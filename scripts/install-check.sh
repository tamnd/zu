#!/bin/sh
# Install zu the way a reader of the README installs it, and then do the
# first thing they came to do with it.
# Usage: scripts/install-check.sh <target>
#
# The done-when of DX1 is that `curl | sh` installs a CLI that loads a
# Parquet file and prints `zu stat`, on macOS, linux gnu, linux musl and
# Windows. Nothing short of running it proves that: install.sh has unit
# tests, but they hand it a fake release whose `bin/zu` is a shell script
# that echoes a version, which proves the download and the digest check
# and says nothing about whether what lands is a working program.
#
# So this runs the real installer over a real release of this platform,
# built by the job that calls this, and then converts an edge list to
# Parquet, loads it and prints the statistics. Four steps because that is
# the shortest path from a fresh machine to a database, and every one of
# them is somebody's first five minutes.
#
# POSIX sh for the same reason install.sh is: this runs on Alpine's ash
# inside the musl rows. Windows runs install.ps1 from a step of its own,
# in PowerShell, since a PowerShell installer driven from bash is a test
# of neither.

set -eu

target="${1:?usage: install-check.sh <target>}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# A release of one platform, from the same table and the same packer the
# real one uses, so that what gets installed here is what a user would
# have downloaded rather than a tarball this script rolled itself. The
# archive is the whole prefix and SHA256SUMS is over it, which is what
# makes the digest check below a check and not a formality.
#
# `--fast` because this release is installed once and deleted. The level
# a release is packed at is there so that nine repositories download
# less, and paying two minutes of a core for it here would buy a shorter
# copy from one directory to another.
release="$work/release"
cargo run -q -p xtask -- artifacts --assemble "$release" \
    --built dist --target "$target" --fast

# `curl | sh`, actually piped, because that is the failure mode a
# one-liner has and no other invocation reproduces: a script read from
# stdin cannot read from stdin, and an installer that prompted or that
# consumed a line of itself works perfectly when run as a file.
#
# file:// rather than a server, since curl reads it and the release is on
# this disk. What is under test is the installer, and the two bytes of
# difference between a file transfer and an HTTP one are curl's.
#
# The target is not passed. The row already knows which platform this is,
# so letting the script work it out and then fetching the archive by the
# name it decided is a check on detect_target: get it wrong and the
# release has no such archive, which is the error a user on that machine
# would have seen.
prefix="$work/prefix"
ZU_BASE="file://$release" ZU_PREFIX="$prefix" sh -s -- --no-modify-path < install.sh

# The whole prefix and not just the CLI, because dx/12 section 6 installs
# a package: a user who takes the one-liner today compiles against the
# header next week, and an installer that unpacked bin/ alone would pass
# every test that only ran a program.
for file in bin/zu include/zu.h lib/pkgconfig/libzu.pc; do
    [ -f "$prefix/$file" ] || {
        echo "install: $file is not in $prefix, so what landed is not a package"
        exit 1
    }
done

zu="$prefix/bin/zu"

# What the promise says, in the order a user meets it. The edge list is
# text because that is what the world has, Parquet is what it becomes,
# and the conversion is done by the installed binary rather than by the
# build, so a CLI shipped without the arrow feature fails here rather
# than in a bug report.
printf '1 2\n1 3\n2 3\n3 1\n' > "$work/edges.txt"
"$zu" convert "$work/edges.txt" "$work/edges.parquet"
"$zu" copy "$work/edges.parquet" "$work/graph.zu1"
"$zu" stat "$work/graph.zu1"

# That the statistics are of the graph that went in, since `zu stat` on a
# database it failed to load would print a shape rather than fail, and
# four edges over four nodes is the answer the four lines above have.
stat="$("$zu" stat "$work/graph.zu1")"
for want in "node (4 rows)" "edge (4 edges, node to node)"; do
    case "$stat" in
    *"$want"*) ;;
    *)
        echo "install: zu stat does not say \"$want\":"
        echo "$stat"
        exit 1
        ;;
    esac
done

echo "install: $target installs from a release, reads Parquet and answers"
