// SPDX-License-Identifier: Apache-2.0
//
//! STAGE 3 — end-to-end proof of the hand-built `wasi:http@0.3-rc` host. A real
//! guest component (tests/fixtures/http_guest.wasm) imports `wasi:http/{types,
//! handler}`, constructs a `GET example.com/` request, calls `handler.handle`,
//! and reads the response status + body stream back. The host
//! ([`WasiHttpHost`]) synthesizes the response IN-PROCESS: status 200, an
//! `x-hellohq: ok` header, and a body echoing `"GET example.com/"`.
//!
//! This exercises the full wasi:http 0.3 host mechanics — resources (fields /
//! request / response), the concurrent (`Accessor`-based) stream-minting methods
//! (`request::new`, `response::consume_body`, …), and the concurrent
//! `handler.handle` — driven via `call_async` under `pollster::block_on`, on
//! BOTH backends (Cranelift + Pulley). STAGE 4 swaps the synthetic response for
//! a real P3 round-trip to Dart.
//!
//! Behind `wasi-http` (the host module) AND `compile` (instantiating the
//! portable component needs Cranelift, like the other component tests).
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::wasi_http::WasiHttpHost;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

/// The fixture guest component. Imports `wasi:http/{types,handler}`, exports
/// `run() -> list<u8>`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/http_guest.wasm");

/// The POST fixture guest. Imports `wasi:http/{types,handler}`, exports
/// `run() -> list<u8>`; builds a POST request carrying a body `stream<u8>`
/// ("req-body-123"). Regen: scripts/regen_probe_guest.sh.
const POST_GUEST_WASM: &[u8] = include_bytes!("fixtures/http_guest_post.wasm");

/// Build an engine with Component Model async + concurrency support (required to
/// mint streams/futures), optionally on the Pulley (no-JIT) backend.
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_component_model_async(true);
    // Required for `StreamReader::new` / `FutureReader::new` to mint values
    // (also implies the async/component-model-async runtime in this build).
    cfg.concurrency_support(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Instantiate the guest, link the `wasi:http` host, drive `run`, return bytes.
async fn call_run(use_pulley: bool) -> wasmtime::Result<Vec<u8>> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::<WasiHttpHost>::new(&engine);
    WasiHttpHost::add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, WasiHttpHost::new());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")?;
    let (out,) = run.call_async(&mut store, ()).await?;
    Ok(out)
}

/// Assert the guest received the host's synthesized echo response: status 200
/// (LE u16 prefix) followed by the body `"GET example.com/"`.
fn assert_echo(use_pulley: bool) {
    let out = pollster::block_on(call_run(use_pulley))
        .unwrap_or_else(|e| panic!("run failed (use_pulley={use_pulley}): {e:?}"));

    assert!(
        out.len() >= 2,
        "missing status prefix (use_pulley={use_pulley})"
    );
    let status = u16::from_le_bytes([out[0], out[1]]);
    assert_eq!(status, 200, "status (use_pulley={use_pulley})");

    let body = String::from_utf8_lossy(&out[2..]);
    assert_eq!(
        body, "GET example.com/",
        "echo body (use_pulley={use_pulley})"
    );
}

#[test]
fn cranelift_http_handle_echo() {
    assert_echo(false);
}

#[test]
fn pulley_http_handle_echo() {
    // The no-JIT iOS backend runs the concurrent wasi:http host too.
    assert_echo(true);
}

// ─── STAGE 4: handle routed through the P3 v2 transport ──────────────────────
//
// Instead of the synthetic in-process response, drive the guest's `handle`
// through `hwr_p3s_start_http`: the runtime frames the request OUT (we act as
// the caller/servicer, draining it), we push a framed response back IN, and the
// guest's returned bytes reflect that response (status 200 + body).

use hellohq_wasm_runtime::{
    hwr_p3s_free, hwr_p3s_out_len, hwr_p3s_out_ptr, hwr_p3s_poll, hwr_p3s_push, hwr_p3s_push_end,
    hwr_p3s_result_len, hwr_p3s_result_ptr, hwr_p3s_start_http, HWR_P3S_DONE, HWR_P3S_OUT,
    HWR_P3S_OUT_END,
};

/// Drive the full transport round-trip: start the http session over the P3 v2
/// transport, act as the servicer (drain the request, push a 200 response), and
/// return the guest's run result bytes.
fn run_via_transport(use_pulley: bool) -> Vec<u8> {
    unsafe {
        let session = hwr_p3s_start_http(use_pulley as i32, GUEST_WASM.as_ptr(), GUEST_WASM.len());
        assert!(!session.is_null(), "start failed (use_pulley={use_pulley})");

        // (1) Drain the outbound request frames until OUT_END; the head must name
        //     the GET request the guest built.
        let mut req_head = Vec::new();
        loop {
            match hwr_p3s_poll(session) {
                HWR_P3S_OUT => {
                    let ptr = hwr_p3s_out_ptr(session);
                    let len = hwr_p3s_out_len(session);
                    req_head.extend_from_slice(std::slice::from_raw_parts(ptr, len));
                }
                HWR_P3S_OUT_END => break,
                other => {
                    panic!("unexpected status while draining: {other} (use_pulley={use_pulley})")
                }
            }
        }
        let head_str = String::from_utf8_lossy(&req_head);
        // Wire framing: "{METHOD} {scheme}://{authority}{path}" → here
        // "GET https://example.com/". Half B must match this exact shape.
        assert!(
            head_str.starts_with("GET ") && head_str.contains("example.com/"),
            "request head missing GET line: {head_str:?} (use_pulley={use_pulley})"
        );

        // (2) Push the framed response back IN: head ("{status}\n{header}") then
        //     MULTIPLE body chunks (proving the host streams them through to the
        //     guest frame-by-frame via `ResponseBodyProducer`, not buffered into
        //     one `Vec`), then close inbound. The guest must observe the
        //     concatenation of all three chunks.
        let resp_head = b"200\nx-test: yes";
        hwr_p3s_push(session, resp_head.as_ptr(), resp_head.len());
        for chunk in [
            b"chunk-A".as_slice(),
            b"chunk-B".as_slice(),
            b"chunk-C".as_slice(),
        ] {
            hwr_p3s_push(session, chunk.as_ptr(), chunk.len());
        }
        hwr_p3s_push_end(session);

        // (3) Poll to DONE and read the run result.
        assert_eq!(
            hwr_p3s_poll(session),
            HWR_P3S_DONE,
            "expected DONE (use_pulley={use_pulley})"
        );
        let rptr = hwr_p3s_result_ptr(session);
        let rlen = hwr_p3s_result_len(session);
        let result = std::slice::from_raw_parts(rptr, rlen).to_vec();
        hwr_p3s_free(session);
        result
    }
}

/// Assert the guest's run result reflects the servicer's 200 response: a LE u16
/// status prefix of 200 followed by the body the servicer streamed in.
fn assert_transport(use_pulley: bool) {
    let out = run_via_transport(use_pulley);
    assert!(
        out.len() >= 2,
        "missing status prefix (use_pulley={use_pulley})"
    );
    let status = u16::from_le_bytes([out[0], out[1]]);
    assert_eq!(status, 200, "status (use_pulley={use_pulley})");
    let body = String::from_utf8_lossy(&out[2..]);
    assert_eq!(
        body, "chunk-Achunk-Bchunk-C",
        "transport body should be the concatenation of all streamed chunks \
         (use_pulley={use_pulley})"
    );
}

#[test]
fn cranelift_http_handle_via_transport() {
    assert_transport(false);
}

#[test]
fn pulley_http_handle_via_transport() {
    assert_transport(true);
}

// ─── Follow-on #1: streaming REQUEST body (POST) ─────────────────────────────
//
// The POST guest builds a request carrying a body `stream<u8>` ("req-body-123").
// The host reads that stream host-side and emits it as OUT frames after the
// head, before ending the outbound stream. We drain the OUT frames and assert
// they include BOTH the head (the POST line) AND the body bytes — proving the
// request body streamed through to the servicer — then push a 200 response and
// assert the guest received it.

/// Drive the POST round-trip: start the session with the POST guest, drain the
/// outbound frames (head + request body), push a 200 response, return the
/// guest's run result and the concatenated outbound bytes.
fn run_post_via_transport(use_pulley: bool) -> (Vec<u8>, Vec<u8>) {
    unsafe {
        let session = hwr_p3s_start_http(
            use_pulley as i32,
            POST_GUEST_WASM.as_ptr(),
            POST_GUEST_WASM.len(),
        );
        assert!(!session.is_null(), "start failed (use_pulley={use_pulley})");

        // (1) Drain ALL outbound frames (head + streamed request body) to
        //     OUT_END. We concatenate them so we can assert both the head line
        //     and the body bytes appear.
        let mut outbound = Vec::new();
        loop {
            match hwr_p3s_poll(session) {
                HWR_P3S_OUT => {
                    let ptr = hwr_p3s_out_ptr(session);
                    let len = hwr_p3s_out_len(session);
                    outbound.extend_from_slice(std::slice::from_raw_parts(ptr, len));
                }
                HWR_P3S_OUT_END => break,
                other => {
                    panic!("unexpected status while draining: {other} (use_pulley={use_pulley})")
                }
            }
        }

        // (2) Push a framed 200 response, then close inbound.
        let resp_head = b"200\nx-test: yes";
        hwr_p3s_push(session, resp_head.as_ptr(), resp_head.len());
        let body = b"post-ok";
        hwr_p3s_push(session, body.as_ptr(), body.len());
        hwr_p3s_push_end(session);

        // (3) Poll to DONE and read the run result.
        assert_eq!(
            hwr_p3s_poll(session),
            HWR_P3S_DONE,
            "expected DONE (use_pulley={use_pulley})"
        );
        let rptr = hwr_p3s_result_ptr(session);
        let rlen = hwr_p3s_result_len(session);
        let result = std::slice::from_raw_parts(rptr, rlen).to_vec();
        hwr_p3s_free(session);
        (result, outbound)
    }
}

fn assert_post_transport(use_pulley: bool) {
    let (out, outbound) = run_post_via_transport(use_pulley);

    // The outbound frames must carry the POST head AND the streamed request body
    // bytes — proving the request body reached the servicer via streaming.
    let outbound_str = String::from_utf8_lossy(&outbound);
    assert!(
        outbound_str.starts_with("POST ") && outbound_str.contains("example.com/submit"),
        "outbound missing POST head: {outbound_str:?} (use_pulley={use_pulley})"
    );
    assert!(
        outbound_str.contains("req-body-123"),
        "outbound missing streamed request body bytes: {outbound_str:?} \
         (use_pulley={use_pulley})"
    );

    // The guest received the servicer's 200 response.
    assert!(
        out.len() >= 2,
        "missing status prefix (use_pulley={use_pulley})"
    );
    let status = u16::from_le_bytes([out[0], out[1]]);
    assert_eq!(status, 200, "status (use_pulley={use_pulley})");
    let body = String::from_utf8_lossy(&out[2..]);
    assert_eq!(body, "post-ok", "response body (use_pulley={use_pulley})");
}

#[test]
fn cranelift_http_handle_post_request_body() {
    assert_post_transport(false);
}

#[test]
fn pulley_http_handle_post_request_body() {
    assert_post_transport(true);
}
