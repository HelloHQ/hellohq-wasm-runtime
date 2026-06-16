// SPDX-License-Identifier: Apache-2.0
//
//! Proof of the no-JIT (iOS) streaming path: PRECOMPILE a component to a
//! pulley64 artifact off-device (Cranelift, `Component::serialize`), then run it
//! through `hwr_p3s_start_inference_precompiled` — which `Component::deserialize`s
//! the artifact with NO Cranelift, exactly as the iOS Pulley build does.
//!
//! The precompile step needs `compile` (Cranelift), so this test is gated on it;
//! but the deserialize+run entrypoint it exercises is `wasi-http`-only (ships in
//! the `--no-default-features` iOS slice). Together with the no-default-features
//! build compiling these symbols, this proves the streaming inference round-trip
//! works from a precompiled artifact on the Pulley backend.
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::{
    hwr_p3s_free, hwr_p3s_out_len, hwr_p3s_out_ptr, hwr_p3s_poll, hwr_p3s_push, hwr_p3s_push_end,
    hwr_p3s_result_len, hwr_p3s_result_ptr, hwr_p3s_start_inference_precompiled, HWR_P3S_DONE,
    HWR_P3S_OUT, HWR_P3S_OUT_END,
};

const INFERENCE_GUEST_WASM: &[u8] = include_bytes!("fixtures/inference_guest.wasm");

/// Precompile a component to a pulley64 artifact. The engine config MUST match
/// the one `hwr_p3s_*_precompiled` deserializes against (component-model + async
/// + concurrency, target pulley64), or `Component::deserialize` rejects it.
fn precompile_pulley(wasm: &[u8]) -> Vec<u8> {
    let mut cfg = wasmtime::Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_component_model_async(true);
    cfg.concurrency_support(true);
    cfg.target("pulley64").expect("target pulley64");
    let engine = wasmtime::Engine::new(&cfg).expect("engine");
    let component = wasmtime::component::Component::new(&engine, wasm).expect("compile component");
    component.serialize().expect("serialize")
}

#[test]
fn precompiled_inference_streams_on_pulley() {
    let artifact = precompile_pulley(INFERENCE_GUEST_WASM);

    unsafe {
        // use_pulley=1: deserialize + run on the no-JIT Pulley backend.
        let session = hwr_p3s_start_inference_precompiled(1, artifact.as_ptr(), artifact.len());
        assert!(!session.is_null(), "precompiled inference start failed");

        // Drain the request head (must name the prompt "hello").
        let mut head = Vec::new();
        loop {
            match hwr_p3s_poll(session) {
                HWR_P3S_OUT => {
                    let ptr = hwr_p3s_out_ptr(session);
                    let len = hwr_p3s_out_len(session);
                    head.extend_from_slice(std::slice::from_raw_parts(ptr, len));
                }
                HWR_P3S_OUT_END => break,
                other => panic!("unexpected status while draining head: {other}"),
            }
        }
        let head_str = String::from_utf8_lossy(&head);
        assert!(head_str.contains("hello"), "request head: {head_str:?}");

        // Push token deltas, close inbound, drive to DONE.
        for token in [b"Hel".as_slice(), b"lo ".as_slice(), b"world".as_slice()] {
            hwr_p3s_push(session, token.as_ptr(), token.len());
        }
        hwr_p3s_push_end(session);

        assert_eq!(hwr_p3s_poll(session), HWR_P3S_DONE, "expected DONE");
        let rptr = hwr_p3s_result_ptr(session);
        let rlen = hwr_p3s_result_len(session);
        let result = std::slice::from_raw_parts(rptr, rlen).to_vec();
        hwr_p3s_free(session);

        assert_eq!(
            String::from_utf8_lossy(&result),
            "Hello world",
            "precompiled inference should concatenate the streamed deltas",
        );
    }
}
