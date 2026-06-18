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

# REQUEST-trailers guest (http-guest-req-trailers world in wit-wasi): imports
# ONLY wasi:http/{types,handler}, exports `run` which builds a GET request whose
# request trailers future resolves (concurrently, via a spawned writer) to
# Ok(Some(fields)) carrying x-trace="req-trailer-1", calls handler.handle, and
# returns the response status. Isolated crate test-guest-http-req-trailers.
# Drives the request-trailers surfacing test (tests/http_handle.rs): the host
# drains the trailers future and emits it OUT as the reserved
# x-hellohq-request-trailers head line.
REQTR_CORE="test-guest-http-req-trailers/target/wasm32-unknown-unknown/release/http_guest_req_trailers.wasm"
REQTR_OUT="tests/fixtures/http_guest_req_trailers.wasm"

( cd test-guest-http-req-trailers && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$REQTR_CORE" -o "$REQTR_OUT"

echo "Wrote $REQTR_OUT"
wasm-tools component wit "$REQTR_OUT"

# wasi:http@0.2 END-TO-END gate guest (http02-guest world in wit-wasi-02):
# imports ONLY wasi:http/{types,outgoing-handler}@0.2.10 (+ wasi:io/poll@0.2.10),
# exports `run(authority, use-https) -> result<u16, u8>` which builds an outgoing
# GET request, calls outgoing-handler.handle, blocks on the response future, and
# returns the status (Ok) or a denial/error marker (Err). Isolated crate
# test-guest-http-02. Drives the gate end-to-end test (tests/http02_guest.rs),
# where the real guest observes the GatedHttpHooks allow (200) vs deny
# (HttpRequestDenied -> Err(2)) decision.
HTTP02_CORE="test-guest-http-02/target/wasm32-unknown-unknown/release/http02_guest.wasm"
HTTP02_OUT="tests/fixtures/http02_guest.component.wasm"

( cd test-guest-http-02 && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new "$HTTP02_CORE" -o "$HTTP02_OUT"

echo "Wrote $HTTP02_OUT"
wasm-tools component wit "$HTTP02_OUT"

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

# CAPSTONE fixture (capstone_plugin.component.wasm): the real SDK-authored
# quickstart plugin (plugin-sdk/examples/component-quickstart), NOT a hand-rolled
# probe. Built with hellohq-plugin-sdk against the canonical hellohq:plugin WIT;
# tree-shakes down to workspace/storage/events/log (+ types) and exports `guest`.
# Its own build.sh does the two build steps; we just copy the result in.
# Drives the end-to-end capstone test (tests/capstone.rs).
QUICKSTART_DIR="../plugin-sdk/examples/component-quickstart"
CAPSTONE_BUILT="$QUICKSTART_DIR/component_quickstart.component.wasm"
CAPSTONE_OUT="tests/fixtures/capstone_plugin.component.wasm"

if [ -d "$QUICKSTART_DIR" ]; then
  ( cd "$QUICKSTART_DIR" && bash build.sh )
  cp "$CAPSTONE_BUILT" "$CAPSTONE_OUT"
  echo "Wrote $CAPSTONE_OUT"
  wasm-tools component wit "$CAPSTONE_OUT"
else
  echo "SKIP: $QUICKSTART_DIR not found; capstone fixture not regenerated" >&2
fi

# GO GUEST fixture (go_guest.component.wasm): the real TinyGo-built Go SDK
# quickstart (plugin-sdk/examples/component-quickstart-go). It embeds the TinyGo
# language runtime, so on top of the four hellohq:plugin/* capabilities it also
# imports the wasi:0.2 surface (cli/io/clocks/filesystem/random). Built with
# `tinygo build -target=wasip2` (a Component Model component directly — no
# separate `wasm-tools component new` step). Its own build.sh does the build; we
# just copy the result in. Drives the "support all WASI generations" test
# (tests/go_guest.rs), where the host satisfies wasi:0.2 (locked-down) +
# wasi:http@0.2 (deny-by-default) + hellohq:plugin/* on one linker.
GO_DIR="../plugin-sdk/examples/component-quickstart-go"
GO_BUILT="$GO_DIR/component_quickstart_go.component.wasm"
GO_OUT="tests/fixtures/go_guest.component.wasm"

if [ -d "$GO_DIR" ]; then
  ( cd "$GO_DIR" && bash build.sh )
  cp "$GO_BUILT" "$GO_OUT"
  echo "Wrote $GO_OUT"
  wasm-tools component wit "$GO_OUT"
else
  echo "SKIP: $GO_DIR not found; go_guest fixture not regenerated" >&2
fi
