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

# STAGE 3 wasi:http guest (http-guest world in wit-wasi): imports ONLY
# wasi:http/{types,handler}, exports `run` which builds a GET request, calls
# handler.handle, and returns the response status + body. Isolated crate
# test-guest-http. Drives the STAGE 3 end-to-end test (tests/http_handle.rs).
HTTP_CORE="test-guest-http/target/wasm32-unknown-unknown/release/http_guest.wasm"
HTTP_OUT="tests/fixtures/http_guest.wasm"

( cd test-guest-http && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$HTTP_CORE" -o "$HTTP_OUT"

echo "Wrote $HTTP_OUT"
wasm-tools component wit "$HTTP_OUT"

# Streaming-REQUEST-body (POST) guest (http-guest-post world in wit-wasi):
# imports ONLY wasi:http/{types,handler}, exports `run` which builds a POST
# request carrying a body stream ("req-body-123"), calls handler.handle, and
# returns the response status + body. Isolated crate test-guest-http-post.
# Drives the request-body streaming test (tests/http_handle.rs).
POST_CORE="test-guest-http-post/target/wasm32-unknown-unknown/release/http_guest_post.wasm"
POST_OUT="tests/fixtures/http_guest_post.wasm"

( cd test-guest-http-post && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$POST_CORE" -o "$POST_OUT"

echo "Wrote $POST_OUT"
wasm-tools component wit "$POST_OUT"
