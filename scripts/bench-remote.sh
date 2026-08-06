#!/usr/bin/env bash
# Run the performance gate on a bench host and print the numbers.
# Usage: scripts/bench-remote.sh server3 [git-ref]
# gamingpc runs inside WSL2 Ubuntu; the Linux hosts run natively.
set -euo pipefail

HOST="${1:?usage: bench-remote.sh <host> [ref]}"
REF="${2:-origin/main}"

RUN='cd ~/zu && git fetch -q origin && git checkout -q '"$REF"' 2>/dev/null || git reset -q --hard '"$REF"'
. "$HOME/.cargo/env" 2>/dev/null || true
echo "host: $(hostname), $(nproc) cores, $(rustc --version | cut -d" " -f1-2)"
ZU_GATE=1 ZU_DATA="$HOME/data/zu" cargo bench -q -p zu-encoding --bench decode 2>/dev/null'

if [ "$HOST" = gamingpc ]; then
    # shellcheck disable=SC2029
    ssh "$HOST" "wsl -d Ubuntu -- bash -lc '$RUN'"
else
    # shellcheck disable=SC2029
    ssh "$HOST" "bash -lc '$RUN'"
fi
