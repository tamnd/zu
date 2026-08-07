#!/usr/bin/env bash
# Preprocess the LDBC SNB BI SF1 snapshot into the plain text files the
# ldbc bench reads. Input is the extracted composite-merged-fk dataset
# (set ZU_LDBC to the directory holding bi-sf1-composite-merged-fk);
# output is two files under ZU_DATA:
#
#   ldbc-sf1-person-keys.txt   one person id per line
#   ldbc-sf1-knows.txt         person1Id person2Id per line
#
# The snapshot stores pipe-delimited gzipped parts with a header per
# part and a creationDate first column; this strips both and keeps the
# id columns only. Sizes are small: SF1 has about 10 K persons and
# 173 K knows edges.
set -euo pipefail

LDBC_DIR="${ZU_LDBC:-$HOME/ldbc/sf1-bi}"
DATA_DIR="${ZU_DATA:-$HOME/data/zu}"
SNAP="$LDBC_DIR/bi-sf1-composite-merged-fk/graphs/csv/bi/composite-merged-fk/initial_snapshot/dynamic"

[ -d "$SNAP" ] || { echo "no snapshot under $SNAP" >&2; exit 1; }
mkdir -p "$DATA_DIR"

out="$DATA_DIR/ldbc-sf1-person-keys.txt"
: > "$out.part"
for f in "$SNAP"/Person/part-*.csv.gz; do
    gunzip -c "$f" | tail -n +2 | awk -F'|' '{print $2}' >> "$out.part"
done
mv "$out.part" "$out"
echo "$(wc -l < "$out" | tr -d ' ') person keys -> $out"

out="$DATA_DIR/ldbc-sf1-knows.txt"
: > "$out.part"
for f in "$SNAP"/Person_knows_Person/part-*.csv.gz; do
    gunzip -c "$f" | tail -n +2 | awk -F'|' '{print $2, $3}' >> "$out.part"
done
mv "$out.part" "$out"
echo "$(wc -l < "$out" | tr -d ' ') knows edges -> $out"
