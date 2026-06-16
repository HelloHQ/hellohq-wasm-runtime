// SPDX-License-Identifier: Apache-2.0
//
//! STAGE 3 — end-to-end proof of the hand-built `wasi:http@0.3-rc` host. A real
//! guest component (tests/fixtures/http_guest.wasm) imports `wasi:http/{types,
//! handler}`, constructs a `GET example.com/` request, calls `handler.handle`,
//! and reads the response status + body stream back. The host
//! ([`WasiHttpHost`]) synthesizes the response IN-PROCESS: status 200, an
//! `x-hellohq: ok` header, and a body echoing `"GET example.com/"`.
//!
//! This exercises the full wasi:http 0.3 host mechanics — resources (fields /
//! request / response), the concurrent (`Accessor`-based) stream-minting methods
//! (`request::new`, `response::consume_body`, …), and the concurrent
//! `handler.handle` — driven via `call_async` under `pollster::block_on`, on
//! BOTH backends (Cranelift + Pulley). STAGE 4 swaps the synthetic response for
//! a real P3 round-trip to Dart.
//!
//! Behind `wasi-http` (the host module) AND `compile` (instantiating the
//! portable component needs Cranelift, like the other component tests).
#![cfg(all(feature = "wasi-http", feature = "compile"))]

use hellohq_wasm_runtime::wasi_http::WasiHttpHost;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

/// The fixture guest component. Imports `wasi:http/{types,handler}`, exports
/// `run() -> list<u8>`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/http_guest.wasm");

/// Build an engine with Component Model async + concurrency support (required to
/// mint streams/futures), optionally on the Pulley (no-JIT) backend.
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_component_model_async(true);
    // Required for `StreamReader::new` / `FutureReader::new` to mint values
    // (also implies the async/component-model-async runtime in this build).
    cfg.concurrency_support(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Instantiate the guest, link the `wasi:http` host, drive `run`, return bytes.
async fn call_run(use_pulley: bool) -> wasmtime::Result<Vec<u8>> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::<WasiHttpHost>::new(&engine);
    WasiHttpHost::add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, WasiHttpHost::new());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")?;
    let (out,) = run.call_async(&mut store, ()).await?;
    Ok(out)
}

/// Assert the guest received the host's synthesized echo response: status 200
/// (LE u16 prefix) followed by the body `"GET example.com/"`.
fn assert_echo(use_pulley: bool) {
    let out = pollster::block_on(call_run(use_pulley))
        .unwrap_or_else(|e| panic!("run failed (use_pulley={use_pulley}): {e:?}"));

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
}

#[test]
fn cranelift_http_handle_echo() {
    assert_echo(false);
}

#[test]
fn pulley_http_handle_echo() {
    // The no-JIT iOS backend runs the concurrent wasi:http host too.
    assert_echo(true);
}
