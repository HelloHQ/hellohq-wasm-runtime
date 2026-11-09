// SPDX-License-Identifier: Apache-2.0
//
//! The `hellohq-wasm-host` helper, driven as a real child process the way the
//! app drives it: the same plugin and wasi:http round-trips the in-process C ABI
//! tests run (`plugin_host_full_p3.rs`, `http_handle.rs`), plus what only a
//! separate process adds: the app can end a run at any moment, and the helper
//! never outlives an app that went away.
#![cfg(all(feature = "compile", feature = "typed-hosts", feature = "wasi-http"))]

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use hellohq_wasm_runtime::host_protocol::{
    read_frame, write_frame, SessionKind, Start, FRAME_DONE, FRAME_ERROR, FRAME_OUT, FRAME_OUT_END,
    FRAME_PENDING, FRAME_PUSH, FRAME_PUSH_END, FRAME_READY, FRAME_RESOLVE, FRAME_START,
    PROTOCOL_VERSION,
};
use hellohq_wasm_runtime::wasi_http_frames::{parse_request_frames, KIND_HEAD};
use hellohq_wasm_runtime::HWR_ABI_VERSION;

const PLUGIN_WASM: &[u8] = include_bytes!("fixtures/capstone_plugin.component.wasm");
const HTTP_GUEST_WASM: &[u8] = include_bytes!("fixtures/http_guest.wasm");

struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Helper {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hellohq-wasm-host"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut h = Helper {
            child,
            stdin,
            stdout,
        };
        // The helper announces itself before reading anything.
        let (ty, payload) = h.read().expect("READY");
        assert_eq!(ty, FRAME_READY);
        assert_eq!(&payload[..4], &PROTOCOL_VERSION.to_le_bytes());
        assert_eq!(&payload[4..8], &HWR_ABI_VERSION.to_le_bytes());
        h
    }

    fn send(&mut self, ty: u8, payload: &[u8]) {
        write_frame(self.stdin.as_mut().unwrap(), ty, payload).expect("write to helper");
    }

    fn read(&mut self) -> Option<(u8, Vec<u8>)> {
        read_frame(&mut self.stdout).expect("read from helper")
    }

    fn start(&mut self, kind: SessionKind, use_pulley: bool, component: &[u8], fuel: u64) {
        let start = Start {
            kind,
            use_pulley,
            fuel,
            component: component.to_vec(),
            input: vec![],
        };
        self.send(FRAME_START, &start.encode());
    }

    /// Wait for the helper to exit (with a deadline) and return its exit code.
    fn exit_code(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(Instant::now() < deadline, "helper did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn method_of(v: &serde_json::Value) -> &str {
    v.get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Play the app for the capstone plugin; returns the final frame.
fn run_plugin(use_pulley: bool, grant: bool) -> (u8, Vec<u8>) {
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginCompile, use_pulley, PLUGIN_WASM, 0);
    let mut kv: HashMap<String, String> = HashMap::new();
    let last = loop {
        let (ty, payload) = h.read().expect("helper ended early");
        if ty != FRAME_PENDING {
            break (ty, payload);
        }
        let v: serde_json::Value = serde_json::from_slice(&payload).expect("json request");
        let answer = match method_of(&v) {
            "read:portfolio_names" if grant => {
                br#"{"ok":true,"data":[{"id":"p1","name":"Growth"},{"id":"p2","name":"Income"}]}"#
                    .to_vec()
            }
            "read:portfolio_names" => {
                br#"{"ok":false,"error":"denied:read:portfolio_names"}"#.to_vec()
            }
            "storage_set" => {
                kv.insert(
                    v["key"].as_str().unwrap_or_default().into(),
                    v["value"].as_str().unwrap_or_default().into(),
                );
                br#"{"ok":true}"#.to_vec()
            }
            "storage_get" => match kv.get(v["key"].as_str().unwrap_or_default()) {
                Some(b64) => {
                    serde_json::to_vec(&serde_json::json!({"ok": true, "data": b64})).unwrap()
                }
                None => br#"{"ok":true,"data":null}"#.to_vec(),
            },
            "emit_event" | "log" => br#"{"ok":true}"#.to_vec(),
            other => panic!("unexpected method {other:?}"),
        };
        h.send(FRAME_RESOLVE, &answer);
    };
    assert_eq!(h.exit_code(), Some(0), "helper exits cleanly after the run");
    last
}

#[test]
fn a_real_plugin_runs_through_the_helper_on_both_backends() {
    for use_pulley in [false, true] {
        let (ty, out) = run_plugin(use_pulley, true);
        assert_eq!(ty, FRAME_DONE, "use_pulley={use_pulley}");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "2|1",
            "use_pulley={use_pulley}"
        );
    }
}

#[test]
fn a_denied_host_call_ends_the_run_with_its_error() {
    let (ty, msg) = run_plugin(false, false);
    assert_eq!(ty, FRAME_ERROR);
    assert!(String::from_utf8_lossy(&msg).contains("denied"));
}

#[test]
fn a_wasi_http_request_streams_out_and_the_response_streams_in() {
    let mut h = Helper::spawn();
    h.start(SessionKind::Http, false, HTTP_GUEST_WASM, 0);
    let mut frames = Vec::new();
    loop {
        let (ty, payload) = h.read().expect("helper ended early");
        match ty {
            FRAME_OUT => frames.push(payload),
            FRAME_OUT_END => break,
            other => panic!("unexpected frame {other:#04x} before OUT_END"),
        }
    }
    assert_eq!(frames.first().map(|f| f[0]), Some(KIND_HEAD));
    let req = parse_request_frames(frames.iter().map(Vec::as_slice)).expect("frames parse");
    let head = String::from_utf8_lossy(&req.head);
    assert!(
        head.starts_with("GET ") && head.contains("example.com/"),
        "{head:?}"
    );

    h.send(FRAME_PUSH, b"200\nx-test: yes");
    for chunk in [&b"chunk-A"[..], b"chunk-B", b"chunk-C"] {
        h.send(FRAME_PUSH, chunk);
    }
    h.send(FRAME_PUSH_END, b"");

    let (ty, result) = h.read().expect("DONE");
    assert_eq!(ty, FRAME_DONE);
    // `[u16 LE status][body]`, as the in-process transport returns it.
    assert_eq!(u16::from_le_bytes([result[0], result[1]]), 200);
    assert_eq!(&result[2..], b"chunk-Achunk-Bchunk-C");
    assert_eq!(h.exit_code(), Some(0));
}

#[test]
fn the_app_can_end_a_run_while_a_host_call_waits() {
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginCompile, false, PLUGIN_WASM, 0);
    let (ty, _) = h.read().expect("first host call");
    assert_eq!(ty, FRAME_PENDING);
    // The watchdog's move: end the process instead of answering.
    let started = Instant::now();
    h.child.kill().expect("kill");
    h.child.wait().expect("wait");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_helper_exits_when_the_app_goes_away_mid_run() {
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginCompile, false, PLUGIN_WASM, 0);
    let (ty, _) = h.read().expect("first host call");
    assert_eq!(ty, FRAME_PENDING);
    // The app closes its end without answering: the helper must not linger.
    assert_eq!(h.exit_code(), Some(2));
}

#[test]
fn a_broken_component_is_an_error_not_a_crash() {
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginCompile, false, b"not a component", 0);
    let (ty, msg) = h.read().expect("ERROR");
    let msg = String::from_utf8_lossy(&msg).into_owned();
    assert_eq!(ty, FRAME_ERROR, "{msg}");
    // The runtime's reason, not a generic "could not be started" (#30).
    assert!(msg.starts_with("hellohq-plugin-invalid: "), "{msg}");
    assert_eq!(h.exit_code(), Some(0));
}

#[test]
fn a_precompiled_artifact_from_elsewhere_says_so() {
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginPrecompiled, false, b"not an artifact", 0);
    let (ty, msg) = h.read().expect("ERROR");
    let msg = String::from_utf8_lossy(&msg).into_owned();
    assert_eq!(ty, FRAME_ERROR, "{msg}");
    assert!(
        msg.starts_with("hellohq-plugin-artifact-incompatible: "),
        "{msg}"
    );
    assert_eq!(h.exit_code(), Some(0));
}

#[test]
fn a_broken_streaming_component_says_why() {
    for kind in [SessionKind::Http, SessionKind::Inference] {
        let mut h = Helper::spawn();
        h.start(kind, false, b"not a component", 0);
        let (ty, msg) = h.read().expect("ERROR");
        let msg = String::from_utf8_lossy(&msg).into_owned();
        assert_eq!(ty, FRAME_ERROR, "{msg}");
        assert!(
            msg.starts_with("hellohq-plugin-invalid: "),
            "{kind:?}: {msg}"
        );
        assert_eq!(h.exit_code(), Some(0));
    }
}

#[test]
fn a_frame_out_of_turn_or_garbage_ends_the_helper() {
    // RESOLVE before START.
    let mut h = Helper::spawn();
    h.send(FRAME_RESOLVE, b"{}");
    assert_eq!(h.exit_code(), Some(2));

    // A malformed START.
    let mut h = Helper::spawn();
    h.send(FRAME_START, &[9, 0]);
    assert_eq!(h.exit_code(), Some(2));

    // Raw garbage that is not a frame at all.
    let mut h = Helper::spawn();
    h.stdin.as_mut().unwrap().write_all(&[0, 0, 0, 0]).unwrap();
    assert_eq!(h.exit_code(), Some(2));
}

#[test]
fn the_fuel_budget_reaches_the_runtime() {
    // A budget too small for the plugin to finish: the run ends with the
    // runtime's CPU-limit error instead of DONE.
    let mut h = Helper::spawn();
    h.start(SessionKind::PluginCompile, false, PLUGIN_WASM, 1);
    let last = loop {
        let (ty, payload) = h.read().expect("helper ended early");
        if ty != FRAME_PENDING {
            break (ty, payload);
        }
        h.send(FRAME_RESOLVE, br#"{"ok":true}"#);
    };
    assert_eq!(last.0, FRAME_ERROR);
    assert!(
        String::from_utf8_lossy(&last.1).contains("hellohq-plugin-cpu-limit"),
        "{}",
        String::from_utf8_lossy(&last.1)
    );
}
