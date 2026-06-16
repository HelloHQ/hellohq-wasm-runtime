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

# P3 round-trip guest (p3-probe world): imports ONLY hellohq:plugin/hostcall,
# exports `run` which forwards through the import. Isolated crate test-guest-p3.
P3_CORE="test-guest-p3/target/wasm32-unknown-unknown/release/p3_probe_guest.wasm"
P3_OUT="tests/fixtures/p3_probe_guest.wasm"

( cd test-guest-p3 && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$P3_CORE" -o "$P3_OUT"

echo "Wrote $P3_OUT"
wasm-tools component wit "$P3_OUT"
