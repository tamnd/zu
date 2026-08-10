#!/bin/sh
# Disassembly gate for the top vector kernels (zu#74, perf/11 tier 1).
#
# The kernels bench exports one no_mangle anchor per hot kernel. This
# script disassembles each anchor out of the built bench binary and
# asserts that the kernels relying on auto-vectorization actually
# vectorized: a toolchain bump or an innocent-looking refactor that
# drops a loop back to scalar shows up here, not three releases later
# in a bench regression.
#
# Usage: cargo bench -p zu-vector --no-run && bench/check_asm.sh
set -eu

cd "$(dirname "$0")/.."

bin=$(ls -t target/release/deps/kernels-* 2>/dev/null | grep -v '\.d$' | head -1 || true)
if [ -z "$bin" ]; then
    echo "check_asm: no kernels bench binary; run cargo bench -p zu-vector --no-run first" >&2
    exit 1
fi

arch=$(uname -m)
case "$arch" in
    # The arrangement suffix rides the register (GNU: v0.2d) or the
    # mnemonic (Apple: add.2d v0); match the suffix itself.
    arm64|aarch64) simd_pat='\.(2d|4s|8h|16b)\b' ;;
    x86_64|amd64)  simd_pat='%[xy]mm[0-9]+' ;;
    *) echo "check_asm: unknown arch $arch" >&2; exit 1 ;;
esac

disasm() {
    # llvm objdump (macOS) and binutils (Linux) spell the flag
    # differently; try both, with and without the underscore prefix.
    for name in "$1" "_$1"; do
        out=$(objdump --disassemble-symbols="$name" "$bin" 2>/dev/null || true)
        [ -n "$(printf '%s' "$out" | grep -E '^\s*[0-9a-f]+:')" ] && { printf '%s' "$out"; return 0; }
        out=$(objdump --disassemble="$name" "$bin" 2>/dev/null || true)
        [ -n "$(printf '%s' "$out" | grep -E '^\s*[0-9a-f]+:')" ] && { printf '%s' "$out"; return 0; }
    done
    return 1
}

# anchor:pattern:expectation. simd means packed operations must appear;
# scalar kernels are cursor or bit chains where SIMD is not the design.
# The pattern is a mangled-path fragment that finds the kernel's own
# symbols in the symbol table. Depending on inlining the loop body sits
# either inside the anchor or behind a call the disassembler may not
# name (a GOT jump on GNU objdump), so the union of anchor, named
# callees, and pattern matches is what always holds it.
checks="
zu_asm_cmp_i64_const:3cmp7compare:simd
zu_asm_cmp_i64_pair:3cmp7compare:simd
zu_asm_cmp_dict_const:3cmp7compare:simd
zu_asm_sum_i64:3agg7sum_i64:simd
zu_asm_sum_f64:3agg7sum_f64:scalar
zu_asm_min_i64:3agg7min_i64:simd
zu_asm_intersect:6setops:scalar
zu_asm_bitmap_to_sel:9SelVector11from_bitmap:scalar
zu_asm_hash:4hash10hash_slice:scalar
zu_asm_gather:7kernels6gather:scalar
"

# Anchors are thin wrappers; the kernel loops may sit one call down as
# shared monomorphizations. Pull in the direct callees so the SIMD
# check sees the actual loop bodies.
with_callees() {
    top=$(disasm "$1") || return 1
    printf '%s\n' "$top"
    printf '%s\n' "$top" \
        | grep -oE '<[^>+]+>' | tr -d '<>' | sort -u \
        | while read -r callee; do
            case "$callee" in *panic*|*unwind*|"$1"|_"$1") continue ;; esac
            disasm "$callee" || true
        done
}

symtab=$(objdump -t "$bin" 2>/dev/null | awk '{print $NF}' | sort -u)

failed=0
for entry in $checks; do
    sym=${entry%%:*}
    rest=${entry#*:}
    pat=${rest%%:*}
    want=${rest##*:}
    if ! asm=$(with_callees "$sym"); then
        echo "FAIL $sym: symbol not found in $bin"
        failed=1
        continue
    fi
    for s in $(printf '%s\n' "$symtab" | grep -- "$pat" || true); do
        asm="$asm
$(disasm "$s" || true)"
    done
    insns=$(printf '%s\n' "$asm" | grep -cE '^\s*[0-9a-f]+:' || true)
    simd=$(printf '%s\n' "$asm" | grep -cE "$simd_pat" || true)
    if [ "$want" = simd ] && [ "$simd" -eq 0 ]; then
        echo "FAIL $sym: expected vector instructions, found none in $insns insns"
        failed=1
    else
        echo "ok   $sym: $insns insns, $simd vector"
    fi
done

# The bit unpack loop sits behind a width dispatcher as private
# const-width monomorphizations, so no anchor reaches it one call down.
# Find the transposed bodies in the symbol table by name and require the
# aggregate to contain vector code.
unpack_syms=$(objdump -t "$bin" 2>/dev/null | grep -oE '[^ ]*unpack_transposed[^ ]*' | sort -u)
if [ -z "$unpack_syms" ]; then
    echo "FAIL unpack_transposed: no symbols found in $bin"
    failed=1
else
    insns=0
    simd=0
    for s in $unpack_syms; do
        asm=$(disasm "$s") || continue
        insns=$((insns + $(printf '%s\n' "$asm" | grep -cE '^\s*[0-9a-f]+:' || true)))
        simd=$((simd + $(printf '%s\n' "$asm" | grep -cE "$simd_pat" || true)))
    done
    if [ "$simd" -eq 0 ]; then
        echo "FAIL unpack_transposed: expected vector instructions, found none in $insns insns"
        failed=1
    else
        echo "ok   unpack_transposed: $insns insns, $simd vector (all widths)"
    fi
fi

exit $failed
