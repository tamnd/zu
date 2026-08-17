#!/bin/sh
# Build libzu and the CLI for one target, measure them against the size
# budgets in platforms.toml, stage the package, check what the shared
# library exports, and build the C smoke test four ways against it.
# Usage: scripts/libzu-build.sh <target> <smoke>
#
# One script for all seven tier 1 platforms, because the musl rows are
# the same work as the rest and only the place it happens differs: every
# other row runs this on the runner, and those two run it through docker
# on an Alpine image, since a musl shared library needs the libc and the
# unwinder of the system it ships to. Two copies of these commands would
# drift the day one of them gained a step.
#
# POSIX sh rather than bash for the same reason: on Alpine bash is a
# package and sh is the shell, and a build script is a poor place to
# find that out.
set -eu

target="${1:?usage: libzu-build.sh <target> <smoke>}"
smoke="${2:?usage: libzu-build.sh <target> <smoke>}"
out="target/$target/release"
stage="dist/libzu-$target"

cargo build --release --target "$target" -p zu-capi -p zu-cli

# The dx/14 section 4 ceilings, from the same table as the matrix. Size
# is a real adoption factor for serverless and mobile targets and it
# only ever drifts upward, so it is a number with a limit rather than a
# graph somebody looks at once a quarter.
#
# The shared library is what is gated, because it is what a user
# installs. The static archive is not: it is object files, and a
# consumer's linker takes the ones their calls reach, so what it weighs
# on disk is not a size anybody pays.
cargo run -q -p xtask -- platforms --measure "$out" --target "$target"

# What a static link needs named after the archive. Asked of rustc
# rather than written down, because the answer differs per target and
# changes with the toolchain, and a pkg-config file whose Libs.private
# is a guess is a `pkg-config --static` line that does not link.
#
# `--color never` because CI sets CARGO_TERM_COLOR=always and a note
# wrapped in escape codes is a note this pattern does not match, which
# is an empty answer rather than a wrong one. An empty answer stops the
# build here: everything downstream would carry on and fail at the far
# end of it, where the message is a page of undefined references to
# pthread_create rather than the one sentence that explains them.
syslibs="$(cargo rustc -q --color never --release --target "$target" -p zu-capi \
    --crate-type staticlib -- --print native-static-libs 2>&1 |
    sed -n 's/^note: native-static-libs: //p' | tail -1)"
if [ -z "$syslibs" ]; then
    echo "static link: rustc did not say what $target needs beside the archive"
    exit 1
fi
echo "static link: $syslibs"

# The archive this row publishes, laid out as a prefix so that the two
# build systems can find things in it (dx/09 C-4). The layout is code
# with tests on it rather than a heredoc that a second platform's job
# would spell differently.
rm -rf "$stage"
cargo run -q -p xtask -- package --stage "$stage" --built "$out" --target "$target" \
    --syslibs "$syslibs"

# From here on it is what the package says rather than what rustc said,
# which are the same list in at most a different spelling: MSVC's
# options arrive from rustc with the slash MSVC documents and are
# staged with a dash, since a leading slash is the start of a path to
# CMake and to the shell git bash runs. Reading it back means the
# static smoke link below is a check on Libs.private itself, and means
# this rule is written once, where there are tests on it.
syslibs="$(sed -n 's/^Libs.private: //p' "$stage/lib/pkgconfig/libzu.pc")"

# What the shared library exports, which dx/09 C-5 says is the header
# and nothing else. `cargo xtask package` proves the header and the
# crate declare and define the same functions; this proves the library
# on disk holds that list and no more, which is the half a check on
# source cannot see. A leaked Rust or libstd symbol here is the
# collision that makes two plugins embedding libzu unusable in one
# process (dx/02 section 9).
case "$target" in
*windows*)
    # A DLL exports what was marked for export, and rustc marks the
    # ABI's functions and nothing else, so there is nothing here to
    # correct and no nm to do it with.
    echo "exports: $target exports what it marks, which is the ABI"
    ;;
*darwin*)
    leaked="$(nm -gU "$out/libzu.dylib" | awk '$3 !~ /^_zu_/ {print $3}')"
    ;;
*)
    leaked="$(nm -gD --defined-only "$out/libzu.so" | awk '$3 !~ /^zu_/ {print $3}')"
    ;;
esac
if [ -n "${leaked:-}" ]; then
    echo "exports: $target leaks symbols that are not part of the ABI:"
    echo "$leaked"
    exit 1
fi

# A cross compiled row is measured and not exercised. Saying so is the
# difference between a matrix that tests seven platforms and one that
# claims to.
if [ "$smoke" != "true" ]; then
    echo "smoke: $target is built where nothing can run it, so this is a size check"
    exit 0
fi

# What a user does: a C file whose only knowledge of zu is the header,
# compiled by a compiler that is not rustc, linked against the package
# this run just staged. The Rust test in the crate calls the same
# functions and proves something else, since it links the rlib the
# compiler had in hand.
#
# Four ways, because a package can be wrong in four places and each one
# fails a different user. The two library forms fail differently: a
# shared library that loads is no evidence that the archive holds every
# symbol the linker will ask it for, and an archive needs the system
# libraries the Rust standard library uses named on the command line
# while the shared one carries them itself. The two build systems fail
# differently again: a `.pc` and a CMake config are each a set of
# relative paths that are either right or a user's first five minutes.
#
# Out of the tree, so that a run leaves the checkout as it found it and
# the artifact step uploads what was built rather than what was tested.
work="$(mktemp -d)"
printf '1 2\n1 3\n2 3\n3 1\n' > "$work/edges.txt"
prefix="$PWD/$stage"
smoke_c="$PWD/crates/zu-capi/tests/smoke.c"

case "$target" in
*windows*)
    "$prefix/bin/zu.exe" copy "$work/edges.txt" "$work/smoke.zu1"
    ;;
*)
    "$prefix/bin/zu" copy "$work/edges.txt" "$work/smoke.zu1"
    ;;
esac

case "$target" in
*windows*)
    # The two things that differ here are the compiler and what a caller
    # links against: the import library in lib/ rather than the DLL
    # itself, and the DLL beside the executable rather than an rpath.
    clang -O2 "-I$prefix/include" "$smoke_c" -o "$work/shared.exe" "$prefix/lib/zu.dll.lib"
    cp "$prefix/bin/zu.dll" "$work/"
    "$work/shared.exe" "$work/smoke.zu1"
    # The list rustc printed is linker input rather than files a driver
    # opens: `kernel32.lib` is a name the linker resolves and not a path
    # clang can find, and `-defaultlib:msvcrt` is an option. `-Wl,`
    # hands each one straight to the linker, which is also what the
    # package tells a consumer's build system to do with them.
    linkargs=""
    for lib in $syslibs; do
        linkargs="$linkargs -Wl,$lib"
    done
    # shellcheck disable=SC2086
    clang -O2 "-I$prefix/include" "$smoke_c" -o "$work/static.exe" "$prefix/lib/zu.lib" $linkargs
    "$work/static.exe" "$work/smoke.zu1"
    ;;
*)
    cc -O2 "-I$prefix/include" "$smoke_c" -o "$work/shared" \
        -L"$prefix/lib" -lzu -Wl,-rpath,"$prefix/lib"
    "$work/shared" "$work/smoke.zu1"
    # shellcheck disable=SC2086
    cc -O2 "-I$prefix/include" "$smoke_c" -o "$work/static" \
        "$prefix/lib/$(basename "$prefix/lib/libzu.a")" $syslibs
    "$work/static" "$work/smoke.zu1"
    ;;
esac

# pkg-config, which is how a Makefile and a meson.build both find this.
# `--static` is asked for as well, since Libs.private is the field a
# generated file is most likely to be quietly wrong in and nothing else
# here reads it.
#
# Asked to run rather than looked for on the PATH, because the Windows
# runner has Strawberry Perl's `pkg-config` on it and that one cannot
# start: it dies in its own BEGIN block looking for a module the
# installation does not have. A tool that is present and cannot answer
# is the same situation as no tool at all, and finding that out by
# running it is the only way to tell them apart.
if pkg-config --version > /dev/null 2>&1; then
    export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
    pkg-config --exists libzu
    echo "pkg-config: $(pkg-config --modversion libzu), $(pkg-config --cflags --libs libzu)"
    # That every one of them survives the trip, which is the failure
    # where a token is spelled in a way pkgconf drops rather than
    # passes on. It cannot be checked by linking: with both forms in
    # one directory a linker takes the shared one, and naming the
    # archive by path instead is not what the field says.
    for lib in $syslibs; do
        case " $(pkg-config --libs --static libzu) " in
        *" $lib "*) ;;
        *)
            echo "pkg-config: --static does not carry $lib, which a static link needs"
            exit 1
            ;;
        esac
    done
    case "$target" in
    *windows*) ;;
    *)
        # shellcheck disable=SC2046
        cc -O2 "$smoke_c" -o "$work/pc" $(pkg-config --cflags --libs libzu) \
            -Wl,-rpath,"$prefix/lib"
        "$work/pc" "$work/smoke.zu1"
        ;;
    esac
else
    echo "pkg-config: none that runs on this machine, so the .pc is staged and not exercised"
fi

# find_package(zu), which is how the other half of the C world finds it.
# A generated config that CMake accepts and then cannot link is the
# failure this catches, so the project is built and run rather than
# configured.
if command -v cmake > /dev/null 2>&1; then
    mkdir -p "$work/cmake"
    cp "$smoke_c" "$work/cmake/smoke.c"
    cat > "$work/cmake/CMakeLists.txt" << 'EOF'
cmake_minimum_required(VERSION 3.20)
project(zu_smoke C)
find_package(zu 0.0 REQUIRED)
add_executable(shared smoke.c)
target_link_libraries(shared PRIVATE zu::zu)
add_executable(static smoke.c)
target_link_libraries(static PRIVATE zu::zu_static)
EOF
    cmake -S "$work/cmake" -B "$work/cmake/build" -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_PREFIX_PATH="$prefix" > /dev/null
    cmake --build "$work/cmake/build" --config Release > /dev/null
    # Both imported targets, because both library forms ship and a
    # consumer choosing between them is the reason they do. Where a
    # generator put the executable differs, so each is found rather
    # than assumed, and not finding one is a failure rather than a
    # loop that runs zero times.
    for name in shared static; do
        exe="$(find "$work/cmake/build" -type f \( -name "$name" -o -name "$name.exe" \) | head -1)"
        if [ -z "$exe" ]; then
            echo "cmake: the $name target built nothing"
            exit 1
        fi
        # Windows looks for a DLL beside the program that needs it and
        # then on the PATH, and neither of those is a directory CMake
        # put anything in. Copying it is what a consumer's install step
        # does, and doing it here rather than adding to the PATH keeps
        # the two executables independent of the order they run in.
        case "$target" in
        *windows*) cp "$prefix/bin/zu.dll" "$(dirname "$exe")/" ;;
        esac
        "$exe" "$work/smoke.zu1" > /dev/null
        echo "cmake: $name linked through find_package(zu) and ran"
    done
else
    echo "cmake: not on this machine, so the package config is staged and not exercised"
fi
