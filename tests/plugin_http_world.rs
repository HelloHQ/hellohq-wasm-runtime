// SPDX-License-Identifier: Apache-2.0
//
//! C4: the production `plugin` world (wit/world.wit) imports both the typed
//! `hellohq:plugin/*` capabilities AND `wasi:http/handler`. This test proves the
//! host side of that combination: a guest importing BOTH a capability
//! (`hellohq:plugin/log`) and `wasi:http/{types,handler}` instantiates and runs
//! under ONE host linker — `CapstoneHarness` (capabilities) + `WasiHttpHost`
//! (outbound HTTP) on a SINGLE store, each reached by projection. The two
//! generic `add_to_linker_get::<T>` entrypoints are what make one richer store
//! state satisfy both import sets — exactly what a real plugin needs to do gated
//! outbound fetches alongside the typed capabilities.
//!
//! Runs on Cranelift and Pulley (the no-JIT iOS backend), behind the non-default
//! `wasi-http` feature (the only build where the wasi:http host compiles).

#![cfg(feature = "wasi-http")]

use hellohq_wasm_runtime::capstone::CapstoneHarness;
use hellohq_wasm_runtime::wasi_http::WasiHttpHost;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

/// The combined-world guest fixture: imports `hellohq:plugin/log` +
/// `wasi:http/{types,handler}`, exports async `run`. Built from
/// test-guest-plugin-http (regen via scripts/regen_probe_guest.sh).
const GUEST_WASM: &[u8] = include_bytes!("fixtures/plugin_http_guest.wasm");

/// One store state carrying BOTH hosts. `CapstoneHarness` backs the
/// `hellohq:plugin/*` capability imports; `WasiHttpHost` backs `wasi:http`.
struct PluginHttpHost {
    caps: CapstoneHarness,
    http: WasiHttpHost,
}

/// Engine with Component Model async + concurrency (needed to mint the
/// stream/future values wasi:http uses), optionally on the Pulley backend.
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_component_model_async(true);
    cfg.concurrency_support(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Instantiate the combined guest under one linker holding BOTH hosts, drive
/// `run`, and return (guest output bytes, captured log messages).
async fn call_run(use_pulley: bool) -> wasmtime::Result<(Vec<u8>, Vec<String>)> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::<PluginHttpHost>::new(&engine);
    // hellohq:plugin/* — the guest imports `log`; the other capabilities the
    // capstone registers are unused by this guest and harmless on the linker.
    CapstoneHarness::add_to_linker_get::<PluginHttpHost>(&mut linker, |s| &mut s.caps)?;
    // wasi:http/* — synthetic in-process echo path (transport: None).
    WasiHttpHost::add_to_linker_get::<PluginHttpHost>(&mut linker, |s| &mut s.http)?;

    let mut store = Store::new(
        &engine,
        PluginHttpHost {
            caps: CapstoneHarness::new(true),
            http: WasiHttpHost::new(),
        },
    );
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")?;
    let (out,) = run.call_async(&mut store, ()).await?;

    let logs = store
        .data()
        .caps
        .captured_logs()
        .iter()
        .map(|l| l.message.clone())
        .collect();
    Ok((out, logs))
}

/// Assert BOTH import sets were satisfied on the same store: the wasi:http
/// round-trip (status 200 + synthesized echo body) AND the hellohq:plugin/log
/// capability call (captured log line).
fn assert_combined(use_pulley: bool) {
    let (out, logs) = pollster::block_on(call_run(use_pulley))
        .unwrap_or_else(|e| panic!("run failed (use_pulley={use_pulley}): {e:?}"));

    // (1) wasi:http: LE u16 status prefix + the host's synthesized echo body.
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

    // (2) hellohq:plugin/log capability — emitted on the SAME store.
    assert!(
        logs.iter().any(|m| m.contains("fetching example.com")),
        "expected the guest's log line, got {logs:?} (use_pulley={use_pulley})"
    );
}

#[test]
fn cranelift_plugin_world_capability_plus_http() {
    assert_combined(false);
}

#[test]
fn pulley_plugin_world_capability_plus_http() {
    // The no-JIT iOS backend runs the combined capability + wasi:http store too.
    assert_combined(true);
}
