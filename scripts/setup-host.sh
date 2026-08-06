#!/usr/bin/env bash
# Provision a bench host: Rust toolchain plus a clone of tamnd/zu, then prove
# the workspace builds and tests there. Idempotent, run it as often as you like.
set -euo pipefail

REPO_URL="https://github.com/tamnd/zu"
REPO_DIR="${ZU_DIR:-$HOME/zu}"

if ! command -v cargo >/dev/null 2>&1; then
    if ! command -v rustup >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain stable
    fi
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
rustup default stable >/dev/null 2>&1 || true

if [ -d "$REPO_DIR/.git" ]; then
    git -C "$REPO_DIR" fetch -q origin && git -C "$REPO_DIR" reset -q --hard origin/main
else
    git clone -q "$REPO_URL" "$REPO_DIR"
fi

cd "$REPO_DIR"
cargo test --workspace --all-features --quiet 2>&1 | tail -3

echo "host: $(hostname) cores: $(nproc) rust: $(rustc --version | cut -d' ' -f2) OK"
