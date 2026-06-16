// SPDX-License-Identifier: Apache-2.0
//
//! End-to-end proof of the hand-built `hellohq:plugin/{storage,events}` hosts —
//! the last two doc-53 interfaces. SYNCHRONOUS, non-streaming host imports, so
//! (like the `workspace.read` Option A probe) the host trait methods are plain
//! `&mut self` over in-memory state behind a permission gate — no P3 transport,
//! no streaming, no `store`/`Accessor`.
//!
//! A real guest component (tests/fixtures/storage_events_guest.wasm) imports
//! `hellohq:plugin/{storage,events}`, runs a realistic storage round-trip
//! (`set`/`get`/`list-keys`/`delete`) plus `events.emit({kind:"ready",
//! payload:"ok"})`, and returns a compact summary `"<get-bytes>|<c1>|<c2>"`.
//!
//! - GRANTED: assert the summary is `"hello|2|1"` (get returned "hello", key
//!   counts 2 then 1) AND that the host's event sink captured exactly one
//!   `{kind:"ready", payload:"ok"}` event — both backends (Cranelift + Pulley).
//! - DENIED (`granted:false`): assert the guest surfaces the storage gate error
//!   — the run returns the marker `"ERR:permission-denied"`.
//!
//! Gated behind `wasi-http` (the host module's feature) AND `compile` (compiling
//! the portable component needs Cranelift, like the other component tests).
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::storage_events::StorageEventsHost;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "wit",
    world: "storage-events-guest",
});

/// The fixture guest component. Imports `hellohq:plugin/{storage,events,types}`,
/// exports `run: func() -> list<u8>`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/storage_events_guest.wasm");

/// Build an engine on the chosen backend (mirrors the crate's `make_engine`).
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// A flattened copy of the host's captured event, so this test asserts over a
/// plain shape rather than the host crate's generated `PluginEvent` type.
struct CapturedEvent {
    kind: String,
    payload: Vec<u8>,
}

/// Instantiate the fixture guest with the gated storage/events host, call `run`,
/// and return BOTH the run summary bytes AND the events the host sink captured.
fn call_run(use_pulley: bool, granted: bool) -> wasmtime::Result<(Vec<u8>, Vec<CapturedEvent>)> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::new(&engine);
    StorageEventsHost::add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, StorageEventsHost::new(granted));
    let guest = StorageEventsGuest::instantiate(&mut store, &component, &linker)?;
    let summary = guest.call_run(&mut store)?;

    // Read the captured events back off the host state for assertion.
    let events = store
        .data()
        .captured_events()
        .iter()
        .map(|e| CapturedEvent {
            kind: e.kind.clone(),
            payload: e.payload.clone(),
        })
        .collect();
    Ok((summary, events))
}

fn assert_granted(use_pulley: bool) {
    let (summary, events) = call_run(use_pulley, true).expect("granted run");

    // Storage round-trip: get returned "hello", key counts 2 then 1.
    assert_eq!(
        String::from_utf8_lossy(&summary),
        "hello|2|1",
        "use_pulley={use_pulley}"
    );

    // The event sink captured exactly the one emitted event.
    assert_eq!(events.len(), 1, "use_pulley={use_pulley}");
    assert_eq!(events[0].kind, "ready", "use_pulley={use_pulley}");
    assert_eq!(events[0].payload, b"ok", "use_pulley={use_pulley}");
}

fn assert_denied(use_pulley: bool) {
    let (summary, events) = call_run(use_pulley, false).expect("denied run");

    // The gate denies storage → the guest surfaces the api-error code marker;
    // no data crosses and `run` never reaches the emit.
    assert_eq!(
        String::from_utf8_lossy(&summary),
        "ERR:permission-denied",
        "use_pulley={use_pulley}"
    );
    assert!(events.is_empty(), "use_pulley={use_pulley}");
}

#[test]
fn cranelift_storage_events_round_trip() {
    assert_granted(false);
}

#[test]
fn pulley_storage_events_round_trip() {
    // The no-JIT iOS backend runs the sync storage/events hosts too.
    assert_granted(true);
}

#[test]
fn cranelift_storage_denied() {
    assert_denied(false);
}

#[test]
fn pulley_storage_denied() {
    assert_denied(true);
}
