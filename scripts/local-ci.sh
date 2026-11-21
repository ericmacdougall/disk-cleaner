#!/usr/bin/env bash
# Local replacement for the GitHub-hosted disk-cleaner workflows (ci.yml + site.yml).
# Run from the repository root. Requires a stable Rust toolchain and python3.
#
# This covers fmt, clippy, the workspace tests, the cargo-deny license check and
# the site unit tests that used GitHub-hosted runners. It does NOT cover
# everything the old workflows did; see docs/LOCAL-CI.md ("Not covered") for the
# dropped checks (mount-test incl. mount_alias/foreign_owner, the Linux/Windows
# matrix, the binaries job, and site.yml's pages-gate/pages-rehearsal).
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

echo "==> cargo deny check licenses"
if cargo deny --version >/dev/null 2>&1; then
  cargo deny check licenses
elif [ "${LOCAL_CI_ALLOW_NO_DENY:-0}" = "1" ]; then
  echo "!!! WARNING: cargo-deny is not installed; the license check was SKIPPED (LOCAL_CI_ALLOW_NO_DENY=1)." >&2
  echo "!!! This run does NOT prove license compliance. Install with: cargo install --locked cargo-deny" >&2
else
  echo "cargo-deny not found; install with 'cargo install --locked cargo-deny'" >&2
  echo "(or set LOCAL_CI_ALLOW_NO_DENY=1 to skip the license check with a loud warning)" >&2
  exit 127
fi

echo "==> python3 -m unittest discover -s site/tests -v"
python3 -m unittest discover -s site/tests -v

echo "local-ci: all checks passed on $(uname -s) $(uname -m) at $(date)"
