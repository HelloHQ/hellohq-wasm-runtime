// SPDX-License-Identifier: Apache-2.0
//
//! **Go SDK plugin component runs on the runtime.** The proof that a richer SDK
//! guest — one that embeds a language runtime importing `wasi:*@0.2` on top of
//! `hellohq:plugin/*` — instantiates and runs end-to-end, with WASI satisfied by
//! a LOCKED-DOWN host ctx.
//!
//! The fixture `tests/fixtures/go_guest.component.wasm` is the HelloHQ Go SDK
//! quickstart (`plugin-sdk/examples/component-quickstart-go`), built with TinyGo
//! `-target=wasip2`. `wasm-tools component wit` on it shows it imports the four
//! capability interfaces (`workspace`/`storage`/`events`/`log` + the type-only
//! `types`) AND the TinyGo runtime's `wasi:0.2` surface
//! (`wasi:cli/{environment,stdin,stdout,stderr}`, `wasi:clocks/{monotonic,wall}`,
//! `wasi:filesystem/{types,preopens}`, `wasi:io/{error,streams}`,
//! `wasi:random/random` — all `@0.2.0`), and exports `guest`.
//!
//! On `run` the Go plugin does the SAME flow the Rust capstone proves: log lines,
//! a gated workspace read (2 canned portfolios), a storage round-trip
//! (`greeting`="hello"), an `events.emit("quickstart-ran","ok")`, and returns the
//! compact summary `"<n-portfolios>|<roundtrip-ok>"` → `"2|1"` when granted.
//!
//! ## Wiring
//! - Engine: component model + async (`wasm_component_model_async` not needed —
//!   the guest exports a plain sync `run` — but async IS needed because
//!   `wasmtime_wasi::p2::add_to_linker_async` registers async host funcs, so the
//!   call must go through `instantiate_async` / `call_run` `_async`).
//! - Store state: `GoGuestState` — a locked-down `WasiCtx` + `ResourceTable`
//!   (`WasiView`) PLUS the `CapstoneHarness` (the gated capabilities).
//! - Linker order: `wasmtime_wasi::p2::add_to_linker_async` (provides `wasi:*`)
//!   THEN `CapstoneHarness::add_to_linker_get` (provides `hellohq:plugin/*`,
//!   reaching the embedded harness).
//!
//! Backend: **Cranelift only**. `wasmtime_wasi`'s async host funcs need the async
//! Wasmtime config; loading the portable component compiles it via Cranelift (the
//! `compile` feature `wasi-guests` implies), like the other component tests. The
//! Pulley (no-JIT iOS) path is out of scope here — the iOS build does not ship
//! `wasmtime-wasi` (size budget; see the `wasi-guests` feature note in Cargo.toml).
//!
//! Gated behind `wasi-guests`.
#![cfg(feature = "wasi-guests")]

use hellohq_wasm_runtime::wasi_guests::GoGuestState;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

// Re-bind the capstone-host world here ONLY for its guest-export accessor
// (`hellohq_plugin_guest().call_run_async`). The host (import) side is provided
// by the linker wiring below, not by this world's `add_to_linker`. `async: true`
// so the generated `instantiate_async` / `call_run_async` match the async engine
// that `add_to_linker_async` requires.
wasmtime::component::bindgen!({
    path: "wit",
    world: "capstone-host",
    // wasmtime 45 async config: the generated export accessors must be the
    // `_async` variants (`call_run_async` + `instantiate_async`) because the
    // linker registers async WASI host funcs (`add_to_linker_async`), so the
    // call goes through the async path. (`async: true` is the OLD spelling and
    // is rejected by the 45 macro.)
    imports: { default: async },
    exports: { default: async },
});

/// The real TinyGo-built Go SDK quickstart component. Imports the four
/// `hellohq:plugin/*` capabilities AND the TinyGo runtime's `wasi:0.2` surface;
/// exports `guest`. Regen: scripts/regen_probe_guest.sh.
const GO_GUEST_WASM: &[u8] = include_bytes!("fixtures/go_guest.component.wasm");

/// What the host captured after a full Go-guest `guest.run`.
struct RunOutcome {
    run_result: Result<Vec<u8>, String>,
    stored_greeting: Option<Vec<u8>>,
    events: Vec<(String, Vec<u8>)>,
    logs: Vec<String>,
}

/// Cranelift engine with the component model + async enabled. Async is required
/// because the WASI host funcs are registered via `add_to_linker_async`.
fn async_engine() -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    Engine::new(&cfg)
}

/// Instantiate the real TinyGo component: link the LOCKED-DOWN WASI surface plus
/// the granted/denied capstone capabilities, call the exported `guest.run`, and
/// read back the host's storage / event sink / log sink for assertion.
async fn run_go_plugin(granted: bool) -> wasmtime::Result<RunOutcome> {
    let engine = async_engine()?;
    let component = Component::new(&engine, GO_GUEST_WASM)?;

    let mut linker = Linker::<GoGuestState>::new(&engine);
    // The unified "support all WASI generations at once" linker:
    //   1. WASI 0.2 runtime interfaces (locked-down ctx) — satisfies the TinyGo
    //      runtime's `wasi:*@0.2` imports, grants no ambient FS/network.
    //   2. WASI 0.2 `wasi:http` (deny-by-default) — present for JS baseline,
    //      outbound refused.
    //   3. `hellohq:plugin/*` — the gated capabilities (the embedded harness).
    GoGuestState::add_full_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, GoGuestState::new(granted));
    let plugin = CapstoneHost::instantiate_async(&mut store, &component, &linker).await?;

    // The canonical `guest.run(input)`; the Go plugin ignores its input.
    // With `exports: { default: async }` the generated `call_run` returns a
    // future (the export runs on the async path, matching the async WASI linker);
    // `.await` drives it.
    let run_result = plugin
        .hellohq_plugin_guest()
        .call_run(&mut store, &[])
        .await?;

    let host = &store.data().harness;
    let stored_greeting = host.stored_value("greeting").cloned();
    let events = host.captured_events();
    let logs = host
        .captured_logs()
        .iter()
        .map(|l| l.message.clone())
        .collect();

    Ok(RunOutcome {
        run_result,
        stored_greeting,
        events,
        logs,
    })
}

/// Drive the async run to completion behind the sync `#[test]` (same bridge the
/// crate's C ABI uses).
fn run_go_plugin_blocking(granted: bool) -> wasmtime::Result<RunOutcome> {
    pollster::block_on(run_go_plugin(granted))
}

#[test]
fn go_guest_granted_runs_end_to_end() {
    let out = run_go_plugin_blocking(true).expect("granted run instantiates + runs");

    // Same summary the Rust capstone proves: 2 canned portfolios, storage
    // round-trip ok → "2|1" — now from a real TinyGo-built component.
    let summary = out.run_result.expect("granted run returns Ok");
    assert_eq!(String::from_utf8_lossy(&summary), "2|1");

    // The Go plugin's storage.set("greeting","hello") reached the host KV.
    assert_eq!(out.stored_greeting.as_deref(), Some(b"hello".as_slice()));

    // The Go plugin's events.emit reached the host sink, exactly once.
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].0, "quickstart-ran");
    assert_eq!(out.events[0].1, b"ok");

    // The Go plugin's log lines reached the host sink (incl. the portfolio count).
    assert!(!out.logs.is_empty());
    assert!(
        out.logs.iter().any(|m| m.contains("read 2 portfolio name")),
        "expected the portfolio-count log line, got {:?}",
        out.logs
    );
}

#[test]
fn go_guest_denied_gate_chokepoint() {
    let out = run_go_plugin_blocking(false).expect("denied run still instantiates + runs");

    // The gated workspace read fails; the Go plugin returns the err from `Run`
    // (`return nil, err`), so `run` returns Err carrying the gate message and
    // nothing downstream fired.
    let err = out.run_result.expect_err("denied run returns Err");
    assert!(
        err.contains("capability not granted"),
        "expected gate message, got {err:?}"
    );
    assert!(out.stored_greeting.is_none());
    assert!(out.events.is_empty());
}
