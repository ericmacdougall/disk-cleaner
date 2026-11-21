#!/usr/bin/env bash
# Local replacement for the GitHub-hosted disk-cleaner workflows (ci.yml + site.yml).
# Run from the repository root. Requires a stable Rust toolchain and python3.
#
# This covers the fmt, clippy, unit/integration tests and site tests that used
# GitHub-hosted runners. It does not run the Linux-only tmpfs mount test or the
# Windows matrix job; those stay available behind workflow_dispatch if minutes
# are ever allocated again.
set -euo pipefail

if [ -f "$(dirname "$0")/../Cargo.toml" ]; then
  root="$(cd "$(dirname "$0")/.." && pwd)"
elif [ -f ./Cargo.toml ]; then
  root="$(pwd)"
else
  echo "run this from the disk-cleaner repository root" >&2
  exit 1
fi
cd "$root"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; install a stable Rust toolchain and put cargo on PATH" >&2
  exit 127
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 not found" >&2
  exit 127
fi

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> cargo clippy --workspace --all-targets --locked -- -D warnings"
cargo clippy --workspace --all-targets --locked -- -D warnings

echo "==> cargo test --workspace --locked --no-fail-fast"
cargo test --workspace --locked --no-fail-fast

echo "==> python3 -m unittest discover -s site/tests -v"
python3 -m unittest discover -s site/tests -v

echo "local-ci: all checks passed on $(uname -s) $(uname -m) at $(date)"
