#!/usr/bin/env bash
# Fetch the real benchmark datasets into ~/data/zu. Sizes are the downloads;
# LiveJournal expands to about 1.1 GB of edge list.
#
#   soc-LiveJournal1   4.8 M nodes, 69 M edges   B6 ingest, B8 bits/edge
#
# LDBC SNB SF1/SF10 are generated, not downloaded; see zu bench ldbc (M2).
set -euo pipefail

DATA_DIR="${ZU_DATA:-$HOME/data/zu}"
mkdir -p "$DATA_DIR"
cd "$DATA_DIR"

fetch() {
    local url="$1" out="$2"
    if [ -f "$out" ]; then
        echo "have $out"
    else
        echo "fetching $out"
        curl -fsSL --retry 3 -o "$out.part" "$url" && mv "$out.part" "$out"
    fi
}

fetch https://snap.stanford.edu/data/soc-LiveJournal1.txt.gz soc-LiveJournal1.txt.gz
[ -f soc-LiveJournal1.txt ] || gunzip -k soc-LiveJournal1.txt.gz

ls -lh "$DATA_DIR"
