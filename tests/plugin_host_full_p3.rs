// SPDX-License-Identifier: Apache-2.0
//
//! C1 keystone — a REAL SDK plugin run through the UNIFIED typed host over P3.
//!
//! `hwr_p3_start_plugin_compile` runs the SDK-authored `capstone_plugin` fixture
//! (imports `hellohq:plugin/{workspace,storage,events,log}`, exports the
//! canonical `guest`). On `guest.run` it logs, reads portfolio names, does a
//! storage round-trip, emits an event, and returns `"<n-portfolios>|<roundtrip>"`.
//! Every typed capability call surfaces as a JSON host-call over the existing
//! `hwr_p3_poll`/`hwr_p3_resolve` C ABI, which this test services playing Dart's
//! role (with a tiny in-test KV so the storage round-trip is genuine). Proves the
//! production entrypoint — a real multi-capability plugin over the typed
//! transport — on both backends.
//!
//! Gated behind `compile` (the Cranelift entrypoint) + `typed-hosts`.
#![cfg(all(feature = "compile", feature = "typed-hosts"))]

use std::collections::HashMap;

use hellohq_wasm_runtime::{
    hwr_p3_free, hwr_p3_poll, hwr_p3_request_len, hwr_p3_request_ptr, hwr_p3_resolve,
    hwr_p3_result_len, hwr_p3_result_ptr, hwr_p3_start_plugin_compile, HWR_P3_DONE, HWR_P3_ERROR,
    HWR_P3_PENDING,
};

/// The real SDK quickstart plugin: imports workspace/storage/events/log, exports
/// `guest`. Regen: scripts/regen_probe_guest.sh (+ the SDK example's build.sh).
const PLUGIN_WASM: &[u8] = include_bytes!("fixtures/capstone_plugin.component.wasm");

/// Drive the plugin over the real P3 C ABI, playing Dart: service each host-call
/// with `service`. Returns `Ok(run-bytes)` on `HWR_P3_DONE`, or `Err(message)` on
/// `HWR_P3_ERROR` (the plugin's `run` returning `Err`, e.g. a gate denial).
fn run_with(
    use_pulley: bool,
    mut service: impl FnMut(&[u8]) -> Vec<u8>,
) -> Result<Vec<u8>, String> {
    unsafe {
        let session = hwr_p3_start_plugin_compile(
            use_pulley as i32,
            PLUGIN_WASM.as_ptr(),
            PLUGIN_WASM.len(),
            std::ptr::null(),
            0,
        );
        assert!(!session.is_null(), "start failed (use_pulley={use_pulley})");

        let status = loop {
            match hwr_p3_poll(session) {
                HWR_P3_PENDING => {
                    let ptr = hwr_p3_request_ptr(session);
                    let len = hwr_p3_request_len(session);
                    let req = std::slice::from_raw_parts(ptr, len).to_vec();
                    let resp = service(&req);
                    hwr_p3_resolve(session, resp.as_ptr(), resp.len());
                }
                s @ (HWR_P3_DONE | HWR_P3_ERROR) => break s,
                other => {
                    hwr_p3_free(session);
                    panic!("unexpected poll status {other} (use_pulley={use_pulley})");
                }
            }
        };

        let ptr = hwr_p3_result_ptr(session);
        let len = hwr_p3_result_len(session);
        let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
        hwr_p3_free(session);
        if status == HWR_P3_DONE {
            Ok(bytes)
        } else {
            Err(String::from_utf8_lossy(&bytes).into_owned())
        }
    }
}

fn method_of(v: &serde_json::Value) -> &str {
    v.get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Service the capstone plugin's calls, granting the workspace read. A tiny KV
/// makes the storage round-trip genuine (get echoes what set stored).
fn assert_granted(use_pulley: bool) {
    let mut kv: HashMap<String, Vec<u8>> = HashMap::new();
    let out =
        run_with(use_pulley, |req| {
            let v: serde_json::Value = serde_json::from_slice(req).expect("json request");
            match method_of(&v) {
            // Two canned portfolios → the summary's first field is "2".
            "read:portfolio_names" => {
                br#"{"ok":true,"data":[{"id":"p1","name":"Growth"},{"id":"p2","name":"Income"}]}"#
                    .to_vec()
            }
            "storage_set" => {
                let key = v["key"].as_str().unwrap_or_default().to_string();
                let value = v["value"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|n| n.as_u64().map(|x| x as u8)).collect())
                    .unwrap_or_default();
                kv.insert(key, value);
                br#"{"ok":true}"#.to_vec()
            }
            "storage_get" => {
                let key = v["key"].as_str().unwrap_or_default();
                match kv.get(key) {
                    Some(bytes) => {
                        serde_json::to_vec(&serde_json::json!({ "ok": true, "data": bytes }))
                            .unwrap()
                    }
                    None => br#"{"ok":true,"data":null}"#.to_vec(),
                }
            }
            "emit_event" | "log" => br#"{"ok":true}"#.to_vec(),
            other => panic!("unexpected method {other:?} (use_pulley={use_pulley})"),
        }
        })
        .expect("granted run succeeds");

    // 2 portfolios, storage round-trip ok → "2|1".
    assert_eq!(
        String::from_utf8_lossy(&out),
        "2|1",
        "use_pulley={use_pulley}"
    );
    // The plugin really stored greeting="hello" through the typed host.
    assert_eq!(
        kv.get("greeting").map(Vec::as_slice),
        Some(b"hello".as_slice())
    );
}

/// Deny the gated workspace read; the plugin maps the api-error into its `run`
/// `Err(String)`, so the run surfaces as HWR_P3_ERROR carrying the message.
fn assert_denied(use_pulley: bool) {
    let err = run_with(use_pulley, |req| {
        let v: serde_json::Value = serde_json::from_slice(req).expect("json request");
        match method_of(&v) {
            "read:portfolio_names" => {
                br#"{"ok":false,"error":"denied:read:portfolio_names"}"#.to_vec()
            }
            "log" => br#"{"ok":true}"#.to_vec(),
            other => panic!("denied run should not reach {other:?} (use_pulley={use_pulley})"),
        }
    })
    .expect_err("denied run returns Err");
    assert!(
        err.contains("denied"),
        "expected the gate denial in the run error, got {err:?} (use_pulley={use_pulley})"
    );
}

#[test]
fn cranelift_real_plugin_over_p3() {
    assert_granted(false);
    assert_denied(false);
}

#[test]
fn pulley_real_plugin_over_p3() {
    // The no-JIT iOS backend runs the whole real-plugin flow over P3 too.
    assert_granted(true);
    assert_denied(true);
}
