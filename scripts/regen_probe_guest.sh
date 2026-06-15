#!/usr/bin/env bash
# Regenerate tests/fixtures/workspace_probe_guest.wasm — the P2 Option A guest
# component (see tests/fixtures/README.md). Run from anywhere; paths are repo-root
# relative.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CORE="test-guest/target/wasm32-unknown-unknown/release/workspace_probe_guest.wasm"
OUT="tests/fixtures/workspace_probe_guest.wasm"

( cd test-guest && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$CORE" -o "$OUT"

echo "Wrote $OUT"
wasm-tools component wit "$OUT"
