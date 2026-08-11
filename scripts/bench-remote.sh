#!/usr/bin/env bash
# Run the performance gate on a bench host and print the numbers.
# Usage: scripts/bench-remote.sh server3 [git-ref]
# The script is piped over stdin so quoting survives the cmd.exe to WSL
# hop on gamingpc; the Linux hosts run it natively.
set -euo pipefail

HOST="${1:?usage: bench-remote.sh <host> [ref]}"
REF="${2:-origin/main}"

# The B7 open bench builds a 10 GB file under ZU_DATA and the B6 phase
# loads the 117 M edge com-Orkut graph; only gamingpc has the disk and
# RAM for them, the small servers skip both.
B7=0
B6=0
if [ "$HOST" = gamingpc ]; then
    B7=1
    B6=1
fi

REMOTE=$(
    cat <<EOF
set -e
cd \$HOME/zu
git fetch -q origin
git reset -q --hard "$REF"
. \$HOME/.cargo/env 2>/dev/null || true
echo "host: \$(hostname), \$(nproc) cores, \$(rustc --version | cut -d' ' -f1-2)"
ZU_GATE=1 ZU_DATA=\$HOME/data/zu cargo bench -q -p zu-encoding --features zstd --bench decode 2>/dev/null
ZU_GATE=1 ZU_DATA=\$HOME/data/zu ZU_B6=$B6 cargo bench -q -p zu-zu1 --bench ingest 2>/dev/null
ZU_GATE=1 ZU_DATA=\$HOME/data/zu cargo bench -q -p zu-zu1 --bench blob 2>/dev/null
ZU_GATE=1 ZU_DATA=\$HOME/data/zu ZU_B7=$B7 cargo bench -q -p zu-zu1 --bench open 2>/dev/null
ZU_GATE=1 ZU_DATA=\$HOME/data/zu cargo bench -q -p zu --bench ldbc 2>/dev/null
ZU_GATE=1 cargo bench -q -p zu --bench groupby 2>/dev/null
ZU_GATE=1 cargo bench -q -p zu --bench topn 2>/dev/null
EOF
)

if [ "$HOST" = gamingpc ]; then
    printf '%s\n' "$REMOTE" | ssh "$HOST" "wsl -d Ubuntu -- bash -s"
else
    printf '%s\n' "$REMOTE" | ssh "$HOST" "bash -s"
fi
