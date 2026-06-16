// SPDX-License-Identifier: Apache-2.0
//
//! End-to-end proof of the hand-built `hellohq:plugin/inference` streaming host —
//! the async-first AI inference capability this crate exists for. A real guest
//! component (tests/fixtures/inference_guest.wasm) imports
//! `hellohq:plugin/{inference,types}`, calls
//! `inference.complete([{role:"user", content:"hello"}], {max-tokens:64})`,
//! drains the returned `stream<string>` concatenating the token deltas, and
//! returns the concatenation as bytes.
//!
//! Driven through `hwr_p3s_start_inference` over the P3 v2 transport: the runtime
//! frames the request head OUT (we act as the caller/servicer, draining it and
//! asserting it names the prompt "hello"), we push several token-delta IN frames
//! ("Hel", "lo ", "world"), close inbound, and assert the guest's returned bytes
//! == "Hello world" — proving streamed token deltas round-trip, both backends.
//!
//! Behind `wasi-http` (the host module's feature) AND `compile` (instantiating
//! the portable component needs Cranelift, like the other component tests).
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::{
    hwr_p3s_free, hwr_p3s_out_len, hwr_p3s_out_ptr, hwr_p3s_poll, hwr_p3s_push, hwr_p3s_push_end,
    hwr_p3s_result_len, hwr_p3s_result_ptr, hwr_p3s_start_inference, HWR_P3S_DONE, HWR_P3S_OUT,
    HWR_P3S_OUT_END,
};

/// The fixture guest component. Imports `hellohq:plugin/{inference,types}`,
/// exports `run: async func() -> list<u8>`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/inference_guest.wasm");

/// Drive the full inference streaming round-trip: start the session over the P3
/// v2 transport, act as the servicer (drain the request head, push token-delta
/// frames), and return the guest's run result bytes (the concatenated tokens).
fn run_via_transport(use_pulley: bool) -> Vec<u8> {
    unsafe {
        let session =
            hwr_p3s_start_inference(use_pulley as i32, GUEST_WASM.as_ptr(), GUEST_WASM.len());
        assert!(!session.is_null(), "start failed (use_pulley={use_pulley})");

        // (1) Drain the outbound request head until OUT_END. The head must carry
        //     the opts line and the prompt message "hello".
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
        // Wire framing (Half B must match):
        //   "opts: max-tokens={n} temperature={t-or-default}\n"
        //   then per message "{role}\n{content}\n---\n"
        assert!(
            head_str.contains("opts: max-tokens=64"),
            "request head missing opts line: {head_str:?} (use_pulley={use_pulley})"
        );
        assert!(
            head_str.contains("hello"),
            "request head missing the prompt 'hello': {head_str:?} (use_pulley={use_pulley})"
        );

        // (2) Push the token-delta frames IN (each frame = one stream<string>
        //     element), then close inbound. The guest concatenates them.
        for token in [b"Hel".as_slice(), b"lo ".as_slice(), b"world".as_slice()] {
            hwr_p3s_push(session, token.as_ptr(), token.len());
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

/// Assert the guest's run result is the concatenation of the streamed token
/// deltas: "Hel" + "lo " + "world" == "Hello world".
fn assert_streamed_tokens(use_pulley: bool) {
    let out = run_via_transport(use_pulley);
    let text = String::from_utf8_lossy(&out);
    assert_eq!(
        text, "Hello world",
        "streamed token deltas should concatenate to 'Hello world' \
         (use_pulley={use_pulley})"
    );
}

#[test]
fn cranelift_inference_stream_tokens() {
    assert_streamed_tokens(false);
}

#[test]
fn pulley_inference_stream_tokens() {
    // The no-JIT iOS backend runs the concurrent inference host too.
    assert_streamed_tokens(true);
}
