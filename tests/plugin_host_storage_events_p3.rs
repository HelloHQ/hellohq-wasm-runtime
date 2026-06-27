// SPDX-License-Identifier: Apache-2.0
//
//! C1-3b — the TYPED `storage` + `events` hosts over the REAL P3 transport.
//!
//! `hwr_p3_start_storage_events_compile` runs the existing `storage_events_guest`
//! fixture (imports the typed `hellohq:plugin/{storage,events}`, exports
//! `run() -> list<u8>`). The guest does a realistic round-trip —
//! set/set/get/list-keys/delete/list-keys/emit — and returns the summary
//! `"hello|2|1"`. Each typed call surfaces as a JSON host-call over the existing
//! `hwr_p3_poll`/`hwr_p3_resolve` C ABI, which this test services playing Dart's
//! role. Proves the typed `storage`/`events` bridge end-to-end on both backends.
//!
//! Gated behind `compile` (the Cranelift entrypoint) + `typed-hosts`.
#![cfg(all(feature = "compile", feature = "typed-hosts"))]

use hellohq_wasm_runtime::{
    hwr_p3_free, hwr_p3_poll, hwr_p3_request_len, hwr_p3_request_ptr, hwr_p3_resolve,
    hwr_p3_result_len, hwr_p3_result_ptr, hwr_p3_start_storage_events_compile, HWR_P3_DONE,
    HWR_P3_ERROR, HWR_P3_PENDING,
};

/// Imports the typed `storage`+`events`, runs set/get/list/delete/emit, returns
/// `"<get>|<count1>|<count2>"`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/storage_events_guest.wasm");

/// Drive the typed guest over the real P3 C ABI, playing Dart: service each
/// host-call with `service(request) -> response`. Returns the run result bytes.
fn run_with(use_pulley: bool, mut service: impl FnMut(&[u8]) -> Vec<u8>) -> Vec<u8> {
    unsafe {
        let session = hwr_p3_start_storage_events_compile(
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

fn method_of(req: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(req).expect("request is JSON");
    v.get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn assert_round_trip(use_pulley: bool) {
    let mut list_keys_calls = 0u32;
    let out = run_with(use_pulley, |req| match method_of(req).as_str() {
        // get("greeting") -> the bytes for "hello" as a JSON u8 array.
        "storage_get" => br#"{"ok":true,"data":[104,101,108,108,111]}"#.to_vec(),
        // First list-keys sees 2 keys, second (after delete) sees 1.
        "storage_list_keys" => {
            list_keys_calls += 1;
            if list_keys_calls == 1 {
                br#"{"ok":true,"data":["greeting","count"]}"#.to_vec()
            } else {
                br#"{"ok":true,"data":["greeting"]}"#.to_vec()
            }
        }
        // set / delete / clear / emit_event just acknowledge.
        "storage_set" | "storage_delete" | "storage_clear" | "emit_event" => {
            br#"{"ok":true}"#.to_vec()
        }
        other => panic!("unexpected method {other:?} (use_pulley={use_pulley})"),
    });
    assert_eq!(
        String::from_utf8_lossy(&out),
        "hello|2|1",
        "use_pulley={use_pulley}"
    );
    assert_eq!(list_keys_calls, 2, "both list-keys calls serviced");
}

fn assert_denied(use_pulley: bool) {
    // The gate denies the first storage call; the guest receives a typed
    // api-error and short-circuits to the marker.
    let out = run_with(use_pulley, |_req| {
        br#"{"ok":false,"error":"denied:storage"}"#.to_vec()
    });
    assert_eq!(
        String::from_utf8_lossy(&out),
        "ERR:permission-denied",
        "use_pulley={use_pulley}"
    );
}

#[test]
fn cranelift_typed_storage_events_over_p3() {
    assert_round_trip(false);
    assert_denied(false);
}

#[test]
fn pulley_typed_storage_events_over_p3() {
    assert_round_trip(true);
    assert_denied(true);
}
