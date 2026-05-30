#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   ./ci-check.sh        # check only (same as CI)
#   ./ci-check.sh --fix  # auto-fix formatting + clippy, then check

FIX=false
if [[ "${1:-}" == "--fix" ]]; then
    FIX=true
fi

if $FIX; then
    echo "==> Fixing formatting..."
    cargo fmt --all

    echo "==> Fixing clippy warnings..."
    cargo clippy --all-targets --all-features --fix --allow-dirty
else
    echo "==> Checking formatting..."
    cargo fmt --all -- --check

    echo "==> Running clippy..."
    cargo clippy --all-targets --all-features -- -D warnings
fi

echo "==> Building..."
cargo build --all-features

echo "==> Running tests..."
if command -v cargo-nextest >/dev/null 2>&1; then
    cargo nextest run --all-features --profile ci
else
    echo "    cargo-nextest not installed; falling back to 'cargo test'."
    echo "    Install with: cargo install cargo-nextest --locked"
    cargo test --all-features
fi

echo "==> All checks passed."
