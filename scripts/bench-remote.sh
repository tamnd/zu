#!/usr/bin/env bash
# Run the performance gate on a bench host and print the numbers.
# Usage: scripts/bench-remote.sh server3 [git-ref]
# The script is piped over stdin so quoting survives the cmd.exe to WSL
# hop on gamingpc; the Linux hosts run it natively.
set -euo pipefail

HOST="${1:?usage: bench-remote.sh <host> [ref]}"
REF="${2:-origin/main}"

REMOTE=$(
    cat <<EOF
set -e
cd \$HOME/zu
git fetch -q origin
git reset -q --hard "$REF"
. \$HOME/.cargo/env 2>/dev/null || true
echo "host: \$(hostname), \$(nproc) cores, \$(rustc --version | cut -d' ' -f1-2)"
ZU_GATE=1 ZU_DATA=\$HOME/data/zu cargo bench -q -p zu-encoding --bench decode 2>/dev/null
EOF
)

if [ "$HOST" = gamingpc ]; then
    printf '%s\n' "$REMOTE" | ssh "$HOST" "wsl -d Ubuntu -- bash -s"
else
    printf '%s\n' "$REMOTE" | ssh "$HOST" "bash -s"
fi
