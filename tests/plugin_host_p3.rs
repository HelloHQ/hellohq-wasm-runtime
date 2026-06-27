// SPDX-License-Identifier: Apache-2.0
//
//! C1 slice 2 — the TYPED `workspace` host over the REAL P3 transport.
//!
//! `tests/workspace_transport.rs` proved the typed<->JSON bridge with a sync
//! closure. This drives it over the production path: `hwr_p3_start_workspace_compile`
//! runs the `workspace-run-guest` fixture (imports the typed
//! `hellohq:plugin/workspace`, exports `run() -> list<u8>`); each typed
//! `read-portfolio-names` call surfaces as a P3 host-call over the EXISTING
//! `hwr_p3_poll`/`hwr_p3_resolve` C ABI, which this test services playing Dart's
//! role — exactly as `servicePluginHostCall` does in the app. The guest receives
//! a fully typed `list<portfolio-name>` decoded from the JSON reply.
//!
//! Gated behind `compile` (the Cranelift entrypoint) + `typed-hosts` (the
//! `plugin_host` module); runs on both backends.
#![cfg(all(feature = "compile", feature = "typed-hosts"))]

use hellohq_wasm_runtime::{
    hwr_p3_free, hwr_p3_poll, hwr_p3_request_len, hwr_p3_request_ptr, hwr_p3_resolve,
    hwr_p3_result_len, hwr_p3_result_ptr, hwr_p3_start_workspace_compile, HWR_P3_DONE,
    HWR_P3_ERROR, HWR_P3_PENDING,
};

/// Imports the typed `workspace`, calls `read-portfolio-names`, returns
/// `"<id>=<name>;…"`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/workspace_run_guest.wasm");

/// Drive the typed guest over the real P3 C ABI, playing Dart: service each
/// host-call with `service(request) -> response`. Returns the run result bytes.
fn run_with(use_pulley: bool, service: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    unsafe {
        let session = hwr_p3_start_workspace_compile(
            use_pulley as i32,
            GUEST_WASM.as_ptr(),
            GUEST_WASM.len(),
        );
        assert!(!session.is_null(), "start failed (use_pulley={use_pulley})");

        loop {
            match hwr_p3_poll(session) {
                HWR_P3_PENDING => {
                    let ptr = hwr_p3_request_ptr(session);
                    let len = hwr_p3_request_len(session);
                    let req = std::slice::from_raw_parts(ptr, len).to_vec();
                    let resp = service(&req);
                    hwr_p3_resolve(session, resp.as_ptr(), resp.len());
                }
                HWR_P3_DONE => break,
                HWR_P3_ERROR => {
                    let ptr = hwr_p3_result_ptr(session);
                    let len = hwr_p3_result_len(session);
                    let msg =
                        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned();
                    hwr_p3_free(session);
                    panic!("run errored (use_pulley={use_pulley}): {msg}");
                }
                other => {
                    hwr_p3_free(session);
                    panic!("unexpected poll status {other} (use_pulley={use_pulley})");
                }
            }
        }

        let ptr = hwr_p3_result_ptr(session);
        let len = hwr_p3_result_len(session);
        let out = std::slice::from_raw_parts(ptr, len).to_vec();
        hwr_p3_free(session);
        out
    }
}

fn assert_granted(use_pulley: bool) {
    let out = run_with(use_pulley, |req| {
        // The typed import emitted exactly the app's host-call wire.
        assert_eq!(
            req, br#"{"method":"read:portfolio_names"}"#,
            "use_pulley={use_pulley}"
        );
        // Dart's success envelope; the typed list must reach the guest intact.
        br#"{"ok":true,"data":[{"id":"pf-1","name":"Growth"},{"id":"pf-2","name":"Income"}]}"#
            .to_vec()
    });
    assert_eq!(
        String::from_utf8_lossy(&out),
        "pf-1=Growth;pf-2=Income",
        "use_pulley={use_pulley}"
    );
}

fn assert_denied(use_pulley: bool) {
    // Dart denies; the guest receives a typed api-error and surfaces its code.
    let out = run_with(use_pulley, |_req| {
        br#"{"ok":false,"error":"denied:read:portfolio_names"}"#.to_vec()
    });
    assert_eq!(
        String::from_utf8_lossy(&out),
        "ERR:permission-denied",
        "use_pulley={use_pulley}"
    );
}

#[test]
fn cranelift_typed_workspace_over_p3() {
    assert_granted(false);
    assert_denied(false);
}

#[test]
fn pulley_typed_workspace_over_p3() {
    // The no-JIT iOS backend drives the typed host over P3 too.
    assert_granted(true);
    assert_denied(true);
}
