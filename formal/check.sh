#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if ! cargo kani --version >/dev/null 2>&1; then
  echo "cargo-kani is required; install it with:" >&2
  echo "  cargo install --locked kani-verifier && cargo kani setup" >&2
  exit 1
fi

echo "==> Kani: production lifecycle reducer"
cargo kani -p wisp-lifecycle

echo "==> Shuttle: session interleavings"
cargo test -p wisp-lifecycle --test shuttle_lifecycle

echo "==> all implementation verification passed"
