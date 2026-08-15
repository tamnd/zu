#!/bin/sh
# Build libzu and the CLI for one target, measure them against the size
# budgets in platforms.toml, and run the C smoke test.
# Usage: scripts/libzu-build.sh <target> <smoke>
#
# One script for all seven tier 1 platforms, because the musl rows are
# the same work as the rest and only the place it happens differs: every
# other row runs this on the runner, and those two run it through docker
# on an Alpine image, since a musl shared library needs the libc and the
# unwinder of the system it ships to. Two copies of these six commands
# would drift the day one of them gained a step.
#
# POSIX sh rather than bash for the same reason: on Alpine bash is a
# package and sh is the shell, and a build script is a poor place to
# find that out.
set -eu

target="${1:?usage: libzu-build.sh <target> <smoke>}"
smoke="${2:?usage: libzu-build.sh <target> <smoke>}"
out="target/$target/release"

cargo build --release --target "$target" -p zu-capi -p zu-cli

# The dx/14 section 4 ceilings, from the same table as the matrix. Size
# is a real adoption factor for serverless and mobile targets and it
# only ever drifts upward, so it is a number with a limit rather than a
# graph somebody looks at once a quarter.
cargo run -q -p xtask -- platforms --measure "$out" --target "$target"

# A cross compiled row is measured and not exercised. Saying so is the
# difference between a matrix that tests seven platforms and one that
# claims to.
if [ "$smoke" != "true" ]; then
    echo "smoke: $target is built where nothing can run it, so this is a size check"
    exit 0
fi

# What a user does: a C file whose only knowledge of zu is the header,
# compiled by a compiler that is not rustc, linked against the shared
# library this run just built. The Rust test in the crate calls the same
# functions and proves something else, since it links the rlib the
# compiler had in hand.
#
# Out of the tree, so that a run leaves the checkout as it found it and
# the artifact step uploads what was built rather than what was tested.
work="$(mktemp -d)"
printf '1 2\n1 3\n2 3\n3 1\n' > "$work/edges.txt"

case "$target" in
*windows*)
    # The two things that differ here are the compiler and what a caller
    # links against: the import library beside the DLL rather than the
    # DLL itself, and the DLL beside the executable rather than an rpath.
    "$out/zu.exe" copy "$work/edges.txt" "$work/smoke.zu1"
    clang -O2 -Icrates/zu-capi/include crates/zu-capi/tests/smoke.c \
        -o "$work/smoke.exe" "$out/zu.dll.lib"
    cp "$out/zu.dll" "$work/"
    "$work/smoke.exe" "$work/smoke.zu1"
    ;;
*)
    "$out/zu" copy "$work/edges.txt" "$work/smoke.zu1"
    cc -O2 -Icrates/zu-capi/include crates/zu-capi/tests/smoke.c \
        -o "$work/smoke" -L"$out" -lzu -Wl,-rpath,"$PWD/$out"
    "$work/smoke" "$work/smoke.zu1"
    ;;
esac
