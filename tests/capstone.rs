// SPDX-License-Identifier: Apache-2.0
//
//! The end-to-end **capstone**: an SDK-authored plugin component, built with
//! `hellohq-plugin-sdk` against the canonical `hellohq:plugin@0.1.0` WIT, run on
//! the real Wasmtime runtime through a full host harness — proving SDK →
//! component → runtime → host all fit.
//!
//! The fixture `tests/fixtures/capstone_plugin.component.wasm` is the SDK
//! quickstart example (`plugin-sdk/examples/component-quickstart`). On `run` it:
//!   1. logs a banner + progress lines (`hq::log`),
//!   2. reads the workspace portfolio names (`hq::workspace`, gated),
//!   3. stores `greeting`="hello" and reads it back (`hq::storage`),
//!   4. emits `quickstart-ran`/"ok" (`hq::events`),
//!   5. returns the compact summary `"<n-portfolios>|<roundtrip-ok>"`.
//!
//! `wasm-tools component wit` on the fixture shows it imports ONLY the four sync
//! capability interfaces (`workspace`/`storage`/`events`/`log` + the type-only
//! `types`) and exports the canonical `guest` — a tree-shaken subset, NO wasi,
//! NO inference. The host harness (`CapstoneHarness`, `capstone-host` world)
//! imports exactly that set, so `add_to_linker` satisfies the component's
//! imports and instantiation succeeds.
//!
//! - GRANTED: assert the run summary is `"2|1"` (2 canned portfolios, storage
//!   round-trip ok), AND the host's storage ended up with `greeting`="hello",
//!   AND the event sink captured `{kind:"quickstart-ran", payload:"ok"}`, AND the
//!   log sink captured the plugin's log lines — both backends (Cranelift +
//!   Pulley).
//! - DENIED (`granted:false`): the gated `workspace.read-portfolio-names` fails;
//!   the plugin maps the `api-error` into its `run` `Err(String)` (it does
//!   `.map_err(|e| e.message)?` on the workspace read), so the call returns
//!   `Err` carrying the gate message, and nothing downstream (storage/events)
//!   fired.
//!
//! ## Why no inference
//! `inference.complete` STREAMS and needs an async `run`, but the canonical
//! `guest.run` is SYNC, so the SDK quickstart (and this capstone) cover only the
//! four sync capabilities. Inference is proven end-to-end separately
//! (`tests/inference_complete.rs`); we do NOT force it through the sync `run`.
//!
//! Gated behind `wasi-http` (the host module's feature) AND `compile` (loading
//! the portable component needs Cranelift, like the other component tests).
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::capstone::CapstoneHarness;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "wit",
    world: "capstone-host",
});

/// The fixture: the SDK quickstart plugin component. Imports
/// `hellohq:plugin/{workspace,storage,events,log,types}`, exports `guest`.
/// Regen: scripts/regen_probe_guest.sh (and the SDK example's build.sh).
const PLUGIN_WASM: &[u8] = include_bytes!("fixtures/capstone_plugin.component.wasm");

/// Build an engine on the chosen backend (mirrors the crate's `make_engine`).
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// What the host captured after a full `guest.run`, for the test to assert over.
struct RunOutcome {
    /// `Ok(summary-bytes)` on a successful run, `Err(message)` on the plugin's
    /// `run` returning the gate error string.
    run_result: Result<Vec<u8>, String>,
    stored_greeting: Option<Vec<u8>>,
    events: Vec<(String, Vec<u8>)>,
    logs: Vec<String>,
}

/// Instantiate the real SDK-authored component with the full host harness
/// (granted/denied gate), call the exported `guest.run`, and read back the
/// host's storage / event sink / log sink for assertion.
fn run_plugin(use_pulley: bool, granted: bool) -> wasmtime::Result<RunOutcome> {
    let engine = engine(use_pulley)?;
    // Component::new (Cranelift compile) on the real fixture component — the
    // `compile` feature path the other portable-component tests use.
    let component = Component::new(&engine, PLUGIN_WASM)?;

    let mut linker = Linker::new(&engine);
    // ONE host satisfies workspace + storage + events + log + types — exactly the
    // component's import set, so instantiation succeeds.
    CapstoneHarness::add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, CapstoneHarness::new(granted));
    let plugin = CapstoneHost::instantiate(&mut store, &component, &linker)?;

    // Call the canonical `guest.run(input)` — the plugin ignores its input.
    let run_result = plugin.hellohq_plugin_guest().call_run(&mut store, &[])?;

    let host = store.data();
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

fn assert_granted(use_pulley: bool) {
    let out = run_plugin(use_pulley, true).expect("granted run instantiates + runs");

    // 1. Summary: 2 canned portfolios, storage round-trip ok → "2|1".
    let summary = out.run_result.expect("granted run returns Ok");
    assert_eq!(
        String::from_utf8_lossy(&summary),
        "2|1",
        "use_pulley={use_pulley}"
    );

    // 2. The plugin's storage.set("greeting","hello") reached the host KV.
    assert_eq!(
        out.stored_greeting.as_deref(),
        Some(b"hello".as_slice()),
        "use_pulley={use_pulley}"
    );

    // 3. The plugin's events.emit reached the host sink, exactly once.
    assert_eq!(out.events.len(), 1, "use_pulley={use_pulley}");
    assert_eq!(out.events[0].0, "quickstart-ran", "use_pulley={use_pulley}");
    assert_eq!(out.events[0].1, b"ok", "use_pulley={use_pulley}");

    // 4. The plugin's log lines reached the host sink. The quickstart logs a
    //    "run start", a "read N portfolio name(s)" line, and a "run done" line.
    assert!(!out.logs.is_empty(), "use_pulley={use_pulley}");
    assert!(
        out.logs.iter().any(|m| m.contains("read 2 portfolio name")),
        "expected the portfolio-count log line, got {:?} (use_pulley={use_pulley})",
        out.logs
    );
}

fn assert_denied(use_pulley: bool) {
    let out = run_plugin(use_pulley, false).expect("denied run still instantiates + runs");

    // The gated workspace read fails; the plugin surfaces it as `Err(message)`
    // (its `read_portfolio_names().map_err(|e| e.message)?`), so `run` returns
    // Err carrying the gate message — and nothing downstream fired.
    let err = out.run_result.expect_err("denied run returns Err");
    assert!(
        err.contains("capability not granted"),
        "expected gate message, got {err:?} (use_pulley={use_pulley})"
    );

    // Storage never written, no event captured (run short-circuited at the gate).
    assert!(out.stored_greeting.is_none(), "use_pulley={use_pulley}");
    assert!(out.events.is_empty(), "use_pulley={use_pulley}");
}

#[test]
fn cranelift_capstone_granted() {
    assert_granted(false);
}

#[test]
fn pulley_capstone_granted() {
    // The no-JIT iOS backend runs the whole SDK → component → host flow too.
    assert_granted(true);
}

#[test]
fn cranelift_capstone_denied() {
    assert_denied(false);
}

#[test]
fn pulley_capstone_denied() {
    assert_denied(true);
}
