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

# ai:inference streaming guest (inference-guest world in wit/world.wit): imports
# ONLY hellohq:plugin/{inference,types}, exports `run` which calls
# inference.complete([{role:"user",content:"hello"}], {max-tokens:64}), drains
# the returned stream<string> concatenating the token deltas, and returns the
# concatenation as bytes. Isolated crate test-guest-inference. Drives the
# streaming inference test (tests/inference_complete.rs).
INFER_CORE="test-guest-inference/target/wasm32-unknown-unknown/release/inference_guest.wasm"
INFER_OUT="tests/fixtures/inference_guest.wasm"

( cd test-guest-inference && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$INFER_CORE" -o "$INFER_OUT"

echo "Wrote $INFER_OUT"
wasm-tools component wit "$INFER_OUT"

# SYNCHRONOUS storage + events guest (storage-events-guest world in
# wit/probe.wit): imports ONLY hellohq:plugin/{storage,events,types}, exports a
# plain (non-async) `run` which runs a storage round-trip (set/get/list-keys/
# delete) plus events.emit({kind:"ready",payload:"ok"}) and returns a compact
# summary "<get-bytes>|<c1>|<c2>". Isolated crate test-guest-storage-events.
# Drives the storage/events test (tests/storage_events.rs).
SE_CORE="test-guest-storage-events/target/wasm32-unknown-unknown/release/storage_events_guest.wasm"
SE_OUT="tests/fixtures/storage_events_guest.wasm"

( cd test-guest-storage-events && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$SE_CORE" -o "$SE_OUT"

echo "Wrote $SE_OUT"
wasm-tools component wit "$SE_OUT"
