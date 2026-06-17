// SPDX-License-Identifier: Apache-2.0
//
//! **JS (jco) SDK plugin component runs on the runtime.** The heaviest guest:
//! a ~12 MB component built by `jco componentize` (the SpiderMonkey-derived
//! StarlingMonkey JS engine + the JS SDK quickstart), proving the runtime's
//! "support all WASI generations at once" linker satisfies a full JS engine's
//! import surface — not just the lean TinyGo/Rust guests.
//!
//! `wasm-tools component wit` on the fixture shows it imports the four
//! capability interfaces (`workspace`/`storage`/`events`/`log` + type-only
//! `types`) AND the JS engine's `wasi:*@0.2.10` surface — including
//! `wasi:filesystem`, `wasi:cli/terminal-*`, `wasi:clocks`, `wasi:random`,
//! `wasi:io`, AND `wasi:http@0.2` (`types` + `outgoing-handler`) — and exports
//! `guest`. The locked-down [`GoGuestState`] WASI ctx + the gated `wasi:http`
//! hooks + the capstone capabilities satisfy all of them.
//!
//! On `run` the JS plugin does the SAME flow the Rust/Go quickstarts prove: a
//! gated workspace read (2 canned portfolios), a storage round-trip
//! (`greeting`="hello"), an `events.emit("quickstart-ran","ok")`, and returns
//! the compact summary `"2|1"` when granted.
//!
//! ## The 12 MB fixture is NOT vendored into this repo
//! At ~12 MB the jco component would bloat the runtime repo (cf. the 0.8 MB Go
//! fixture). Instead this test loads it from the sibling `plugin-sdk` build
//! (overridable via `HQ_JS_GUEST_WASM`) and **skips gracefully** when the file
//! is absent — so a checkout without the built JS example (CI, fresh clones)
//! stays green, while a workspace that has built it gets the full end-to-end
//! proof. Build it with `plugin-sdk/examples/component-quickstart-js/build.sh`.
//!
//! Backend: **Cranelift only** (same rationale as `go_guest.rs`). Gated behind
//! `wasi-guests`.
#![cfg(feature = "wasi-guests")]

use std::path::PathBuf;

use hellohq_wasm_runtime::wasi_guests::GoGuestState;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

// Same capstone-host world as `go_guest.rs`, for its `guest`-export accessor.
wasmtime::component::bindgen!({
    path: "wit",
    world: "capstone-host",
    imports: { default: async },
    exports: { default: async },
});

/// Locate the jco JS component: `HQ_JS_GUEST_WASM` if set, else the sibling
/// plugin-sdk build relative to this crate. Returns `None` (→ skip) if absent.
fn locate_js_guest() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HQ_JS_GUEST_WASM") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../plugin-sdk/examples/component-quickstart-js/component_quickstart_js.component.wasm",
    );
    p.is_file().then_some(p)
}

struct RunOutcome {
    /// `Ok(inner)` = the export returned (inner is its WIT `result<…, string>`);
    /// `Err(msg)` = the guest TRAPPED (e.g. an uncaught JS throw). The JS
    /// quickstart does not catch the gate-denial `ApiError`, so the denied path
    /// traps rather than returning `Err` the way the Go/Rust guests do.
    run_result: Result<Result<Vec<u8>, String>, String>,
    stored_greeting: Option<Vec<u8>>,
    events: Vec<(String, Vec<u8>)>,
    logs: Vec<String>,
}

fn async_engine() -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    Engine::new(&cfg)
}

async fn run_js_plugin(wasm: &[u8], granted: bool) -> wasmtime::Result<RunOutcome> {
    let engine = async_engine()?;
    let component = Component::new(&engine, wasm)?;

    let mut linker = Linker::<GoGuestState>::new(&engine);
    GoGuestState::add_full_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, GoGuestState::new(granted));
    let plugin = CapstoneHost::instantiate_async(&mut store, &component, &linker).await?;

    // Capture (don't `?`) the call: a guest trap surfaces as the outer `Err`,
    // and we still want to read the host state afterward to prove nothing
    // downstream of a denied read fired. The store survives a guest trap.
    let run_result = match plugin
        .hellohq_plugin_guest()
        .call_run(&mut store, &[])
        .await
    {
        Ok(inner) => Ok(inner),
        Err(trap) => Err(trap.to_string()),
    };

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

fn run_js_plugin_blocking(wasm: &[u8], granted: bool) -> wasmtime::Result<RunOutcome> {
    pollster::block_on(run_js_plugin(wasm, granted))
}

// `#[ignore]`: compiling the ~12 MB component via Cranelift takes ~80s, too slow
// for the default `cargo test --features wasi-guests` run. Run on demand with
// `cargo test --features wasi-guests --test js_guest -- --ignored`.
#[test]
#[ignore = "heavyweight: ~12MB jco component, ~80s Cranelift compile; run with --ignored"]
fn js_guest_granted_runs_end_to_end() {
    let Some(path) = locate_js_guest() else {
        eprintln!("SKIP js_guest_granted_runs_end_to_end: JS component not built (set HQ_JS_GUEST_WASM or run component-quickstart-js/build.sh)");
        return;
    };
    let wasm = std::fs::read(&path).expect("read JS guest component");
    let out = run_js_plugin_blocking(&wasm, true).expect("granted run instantiates + runs");

    // Same summary the Rust/Go quickstarts prove: 2 canned portfolios, storage
    // round-trip ok → "2|1" — now from a real ~12 MB jco-built JS component.
    let inner = out.run_result.expect("granted run must not trap");
    let summary = inner.expect("granted run returns Ok");
    assert_eq!(String::from_utf8_lossy(&summary), "2|1");

    assert_eq!(out.stored_greeting.as_deref(), Some(b"hello".as_slice()));

    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].0, "quickstart-ran");
    assert_eq!(out.events[0].1, b"ok");

    assert!(
        out.logs.iter().any(|m| m.contains("read 2 portfolio name")),
        "expected the portfolio-count log line, got {:?}",
        out.logs
    );
}

#[test]
#[ignore = "heavyweight: ~12MB jco component, ~80s Cranelift compile; run with --ignored"]
fn js_guest_denied_gate_chokepoint() {
    let Some(path) = locate_js_guest() else {
        eprintln!("SKIP js_guest_denied_gate_chokepoint: JS component not built");
        return;
    };
    let wasm = std::fs::read(&path).expect("read JS guest component");
    let out = run_js_plugin_blocking(&wasm, false).expect("denied run still instantiates + runs");

    // The gate denied the workspace read (the chokepoint held). The JS quickstart
    // does NOT catch the resulting ApiError, so the unhandled JS throw becomes a
    // wasm trap rather than a WIT `Err` (the Go/Rust guests `return … err`). Either
    // way the run does NOT succeed, and nothing downstream of the denied read fired.
    let succeeded = matches!(out.run_result, Ok(Ok(_)));
    assert!(
        !succeeded,
        "denied run must not succeed, got {:?}",
        out.run_result
    );
    assert!(out.stored_greeting.is_none());
    assert!(out.events.is_empty());
}
