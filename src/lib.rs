// SPDX-License-Identifier: Apache-2.0
//
//! Async-first WebAssembly plugin runtime for HelloHQ — a thin Rust shim over
//! Wasmtime exposing a purpose-built **C ABI** for `dart:ffi`.
//!
//! ## Why this exists
//! HelloHQ's current Tier-2 runtime drives Wasmtime through the generic C API
//! with a **synchronous** custom host ABI. That cannot express async host
//! calls — which is why `ai:inference` is a stub today. This crate's reason to
//! exist is **async host capabilities**: stream AI inference and run concurrent
//! HTTP from a plugin, on the WebAssembly **Component Model + WASI 0.3 async**.
//! See `hellohq/docs/51_wasi-0.3-async-runtime-migration.md` (decision) and
//! `…/docs/52_wasi-0.3-spike-plan.md` (the P0 spike this crate is the harness
//! for).
//!
//! ## Status (2026-06-16): spike gates PASSED; P1 done; P2 underway
//! The P0 spike gates all passed (doc 52): Pulley-on-iOS (functional + 1.1 MB +
//! verified running in the iOS Simulator), the bespoke C ABI, and async-over-FFI.
//! Wired and tested (24 tests, Cranelift + Pulley):
//! - `hwr_abi_version` / `hwr_self_test` — handshake + runtime self-test.
//! - `hwr_engine_new`/`_free`, `hwr_instance_new`(compile) / `_new_precompiled`
//!   (the no-JIT iOS deserialize path) / `_call_add` / `_free` — engine + instance
//!   lifecycle (P1).
//! - `hwr_precompile_component` / `hwr_free_bytes` — off-device AOT (P1).
//! - `hwr_run_async_double` / `_component_async_double` / `_canonical_async_double`
//!   — async host imports + the canonical async ABI (`task.return`), on
//!   Wasmtime 45 (no 46 needed; see hellohq doc 53 §6.1).
//! - `hwr_read_portfolio_count` — gated `workspace` host import against the doc-53
//!   WIT world (`wit/world.wit`), the gate as chokepoint (P2 Option A).
//! - **P2 Option A complete:** typed `list<portfolio-name>` round-trips
//!   host → guest → host through a real `wit-bindgen` guest component, gated, both
//!   backends (`tests/workspace_probe.rs`, `bindgen!` host impl).
//!
//! Still ahead: the `wasi:http` host impl (H4/H5, Option B); the Dart-serviced
//! gate round-trip (P3); on-device A-series latency (hardware).
//!
//! Safety: every `extern "C"` entrypoint must `catch_unwind` and never let a
//! panic cross the FFI boundary (see `Cargo.toml` `panic = unwind`).

/// C ABI version. Bumped on any breaking change to the exported surface so the
/// Dart `dart:ffi` loader can refuse a mismatched native library.
// STAGE 1 scaffolding: the host state + trait impls are exercised by the
// module's own unit tests and consumed by later stages (the C ABI wiring lands
// when streaming does). Until then the non-test build sees them as unused.
// `pub` so the STAGE 3 end-to-end integration test (tests/http_handle.rs) can
// reach `WasiHttpHost`; the C ABI wiring (and a narrower surface) lands when the
// P3 transport does in STAGE 4.
#[cfg(feature = "wasi-http")]
#[allow(dead_code)]
pub mod wasi_http;

// The hand-built `hellohq:plugin/inference` streaming host — the async-first AI
// inference capability this crate exists for. STRUCTURALLY IDENTICAL to
// `wasi_http`: a concurrent host method (`inference.complete`) routes through the
// P3 v2 streaming transport and maps token deltas onto the streaming bridge.
// Behind the same `wasi-http` feature, so default / `--no-default-features`
// builds are unaffected.
#[cfg(feature = "wasi-http")]
#[allow(dead_code)]
pub mod inference;

// The hand-built `hellohq:plugin/{storage,events}` hosts — the last two doc-53
// interfaces. SYNCHRONOUS, non-streaming (plain `&mut self` host methods +
// gate), so they follow the simple `workspace.read` (Option A) pattern: no P3
// transport, no streaming, no `store`/`Accessor`. In-memory backing for the
// demo, with production routing to Dart documented as the injection point.
// Behind the same `wasi-http` feature, so default / `--no-default-features`
// builds are unaffected.
#[cfg(feature = "wasi-http")]
#[allow(dead_code)]
pub mod storage_events;

// The end-to-end capstone host harness: ONE host struct backing the full
// `capstone-host` world (workspace + storage + events + log), whose import set
// matches the SDK-authored quickstart component's tree-shaken imports exactly.
// Composes `storage_events::StorageEventsHost` for storage/events. Behind the
// same `wasi-http` feature, so default / `--no-default-features` builds are
// unaffected. Drives tests/capstone.rs (the SDK → component → runtime → host
// proof on the real fixture component).
#[cfg(feature = "wasi-http")]
#[allow(dead_code)]
pub mod capstone;

// The store state + WASI wiring to run JS (jco) / Go (TinyGo) SDK plugin
// components, which import a `wasi:*@0.2` surface (a JS engine / the TinyGo
// runtime) in addition to `hellohq:plugin/*`. Holds a locked-down
// `wasmtime_wasi::WasiCtx` + `ResourceTable` (no preopens/env/network) so the
// `wasi:*` imports resolve but grant nothing ambient, plus the `CapstoneHarness`
// backing the gated capabilities. Behind the `wasi-guests` feature (pulls heavy
// deps: tokio, cap-std), so default / `--no-default-features` (iOS no-JIT) are
// unaffected. Drives tests/go_guest.rs (a real TinyGo component on the runtime).
#[cfg(feature = "wasi-guests")]
#[allow(dead_code)]
pub mod wasi_guests;

// The outbound-fetch gate (origin allowlist + H4/H5 SSRF / private-IP block +
// https-only) that `wasi_guests`'s `GatedHttpHooks` runs before letting a
// JS/Go guest's `wasi:http@0.2` outbound request reach the real sender. Plain
// Rust, no Wasmtime types; a faithful port of the hellohq app's
// `PluginNetworkService`. Behind `wasi-guests` since that is its only consumer.
#[cfg(feature = "wasi-guests")]
pub mod fetch_gate;

pub const HWR_ABI_VERSION: u32 = 1;

// In-process Wasm compilation/execution requires Cranelift and is available only
// in `compile` builds (desktop/Android/CI). iOS (no-JIT) builds omit Cranelift
// and will run precompiled Pulley bytecode via the deserialize path (P1).
#[cfg(feature = "compile")]
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store};

/// Tiny module used to prove real Wasm executes end-to-end on each backend.
#[cfg(feature = "compile")]
const ADD_WAT: &str = r#"(module
  (func (export "add") (param i32 i32) (result i32)
    local.get 0 local.get 1 i32.add))"#;

/// Builds an engine on the chosen backend: Cranelift JIT (desktop/Android) or
/// the **Pulley** portable interpreter (`pulley64`) — the no-JIT path that runs
/// under iOS's W^X restriction.
#[cfg(feature = "compile")]
fn make_engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    if use_pulley {
        // 64-bit little-endian host triple; the spike/P1 derives this per target.
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Compiles + runs `add(a, b)` from [ADD_WAT] on the selected backend.
#[cfg(feature = "compile")]
fn run_add(use_pulley: bool, a: i32, b: i32) -> wasmtime::Result<i32> {
    let engine = make_engine(use_pulley)?;
    let module = Module::new(&engine, ADD_WAT)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let add = instance.get_typed_func::<(i32, i32), i32>(&mut store, "add")?;
    add.call(&mut store, (a, b))
}

/// A Wasm **component** exporting `add: func(s32, s32) -> s32` — exercises the
/// Component Model lift/lower path (the basis for WASI 0.3), not just a core
/// module.
#[cfg(feature = "compile")]
const ADD_COMPONENT_WAT: &str = r#"(component
  (core module $m
    (func (export "add") (param i32 i32) (result i32)
      (i32.add (local.get 0) (local.get 1))))
  (core instance $i (instantiate $m))
  (func (export "add") (param "a" s32) (param "b" s32) (result s32)
    (canon lift (core func $i "add"))))"#;

/// Instantiates the [ADD_COMPONENT_WAT] component and calls its `add` export on
/// the selected backend (Cranelift JIT or Pulley interpreter).
#[cfg(feature = "compile")]
fn run_component_add(use_pulley: bool, a: i32, b: i32) -> wasmtime::Result<i32> {
    use wasmtime::component::{Component, Linker};
    let engine = make_engine(use_pulley)?;
    let component = Component::new(&engine, ADD_COMPONENT_WAT)?;
    let linker = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let add = instance.get_typed_func::<(i32, i32), (i32,)>(&mut store, "add")?;
    let (result,) = add.call(&mut store, (a, b))?;
    Ok(result)
}

/// P2 primitive: a **component that imports a host function** (`host-double`)
/// and exports `run` that calls it. This is the mechanism behind every hellohq
/// capability — `hq_read`, `plugin:storage`, `ai:inference`, and `wasi:http`
/// are all component imports the host provides + gates. Here the import is
/// implemented in Rust; P3 routes such imports to a Dart-serviced, gated
/// callback. The component-async runtime is available on 45 (see
/// `run_component_async_double`); standard `wasi:http@0.3` packages mature with
/// the wider WASI 0.3 ecosystem, but hellohq supplies its own gated host imports
/// regardless (doc 53), so it is not blocked on that.
#[cfg(feature = "compile")]
const HOST_IMPORT_COMPONENT_WAT: &str = r#"(component
  (import "host-double" (func $double (param "x" u32) (result u32)))
  (core func $double_core (canon lower (func $double)))
  (core module $m
    (import "host" "double" (func $d (param i32) (result i32)))
    (func (export "run") (param i32) (result i32)
      local.get 0 call $d))
  (core instance $i (instantiate $m
    (with "host" (instance (export "double" (func $double_core))))))
  (func (export "run") (param "x" u32) (result u32)
    (canon lift (core func $i "run"))))"#;

/// Instantiates [HOST_IMPORT_COMPONENT_WAT], providing `host-double` from Rust,
/// and calls `run(x)` (which calls back into the host). Returns `2·x`.
#[cfg(feature = "compile")]
fn run_component_host_import(use_pulley: bool, x: u32) -> wasmtime::Result<u32> {
    use wasmtime::component::{Component, Linker};
    let engine = make_engine(use_pulley)?;
    let component = Component::new(&engine, HOST_IMPORT_COMPONENT_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.root().func_wrap(
        "host-double",
        |_store: wasmtime::StoreContextMut<'_, ()>, (v,): (u32,)| -> wasmtime::Result<(u32,)> {
            Ok((v.wrapping_mul(2),))
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    let (r,) = run.call(&mut store, (x,))?;
    Ok(r)
}

/// P2 Option A — a **gated `workspace.read-portfolio-names` host import** wired
/// against the doc-53 world (`hellohq:plugin/workspace`). The component imports
/// the read; the host implements it; the guest `run` calls it and returns the
/// result. Sentinel for a gate denial.
#[cfg(feature = "compile")]
const WORKSPACE_READ_DENIED: u32 = u32::MAX;

#[cfg(feature = "compile")]
const WORKSPACE_READ_COMPONENT_WAT: &str = r#"(component
  (import "read-portfolio-names" (func $read (result u32)))
  (core func $read_core (canon lower (func $read)))
  (core module $m
    (import "host" "read" (func $r (result i32)))
    (func (export "run") (result i32)
      call $r))
  (core instance $i (instantiate $m
    (with "host" (instance (export "read" (func $read_core))))))
  (func (export "run") (result u32)
    (canon lift (core func $i "run"))))"#;

/// Instantiates [WORKSPACE_READ_COMPONENT_WAT] and provides `read-portfolio-names`
/// from the host. The host closure is the **permission-gate chokepoint** (in
/// production this is serviced app-side via the Dart-supplied resolver — P3):
/// `granted` → return the workspace data (here the portfolio *count*, a flat
/// proxy for the full `list<portfolio-name>`, which uses this same Linker
/// mechanism with wit-bindgen-generated list/string marshaling); denied →
/// [WORKSPACE_READ_DENIED]. Proves a real hellohq read capability flows through
/// the component world and the gate, on both backends.
#[cfg(feature = "compile")]
fn run_gated_workspace_read(use_pulley: bool, granted: bool, count: u32) -> wasmtime::Result<u32> {
    use wasmtime::component::{Component, Linker};
    let engine = make_engine(use_pulley)?;
    let component = Component::new(&engine, WORKSPACE_READ_COMPONENT_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.root().func_wrap(
        "read-portfolio-names",
        move |_store: wasmtime::StoreContextMut<'_, ()>, _: ()| -> wasmtime::Result<(u32,)> {
            // The gate. Ungranted reads never reach workspace data.
            Ok((if granted {
                count
            } else {
                WORKSPACE_READ_DENIED
            },))
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
    let (r,) = run.call(&mut store, ())?;
    Ok(r)
}

/// Core module importing an **async** host `double` and exporting `run` that
/// calls it — proves a Wasm call can trigger an async host operation that
/// suspends/resumes the Wasm fiber (the async-over-FFI mechanism). See
/// `run_component_async_double` for the same at the **component** level.
#[cfg(feature = "compile")]
const ASYNC_WAT: &str = r#"(module
  (import "host" "double" (func $double (param i32) (result i32)))
  (func (export "run") (param i32) (result i32)
    local.get 0 call $double))"#;

/// Instantiates [ASYNC_WAT] with an **async** host import and drives the whole
/// call with `call_async`. Returns `double(x)` = 2·x.
#[cfg(feature = "compile")]
async fn run_async_double(use_pulley: bool, x: i32) -> wasmtime::Result<i32> {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    let engine = Engine::new(&cfg)?;
    let module = Module::new(&engine, ASYNC_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.func_wrap_async("host", "double", |_caller: Caller<'_, ()>, (v,): (i32,)| {
        // The Wasm guest suspends here while this future runs, then resumes.
        Box::new(async move { wasmtime::Result::Ok(v.wrapping_mul(2)) })
    })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let run = instance.get_typed_func::<i32, i32>(&mut store, "run")?;
    run.call_async(&mut store, x).await
}

/// The **component-level** async path: instantiates [HOST_IMPORT_COMPONENT_WAT]
/// with an **async** host import (`component::Linker::func_wrap_async`) and
/// drives it with `component::TypedFunc::call_async`, with
/// `Config::wasm_component_model_async(true)` enabled. This proves Component
/// Model async runs on Wasmtime **45** (gated by the `component-model-async`
/// feature) — the prerequisite for `ai:inference`/`wasi:http` as async component
/// imports (doc 53), not deferred to 46. Returns `2·x`.
///
/// (This exercises an async *host import* under a component ABI + the async
/// config switch; the fully-async *canonical* lift/lower used by `wasi:http`
/// streams/futures is the next increment, also reachable on 45.)
#[cfg(feature = "compile")]
async fn run_component_async_double(use_pulley: bool, x: u32) -> wasmtime::Result<u32> {
    use wasmtime::component::{Component, Linker};
    let mut cfg = Config::new();
    cfg.wasm_component_model_async(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    let engine = Engine::new(&cfg)?;
    let component = Component::new(&engine, HOST_IMPORT_COMPONENT_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.root().func_wrap_async(
        "host-double",
        |_store: wasmtime::StoreContextMut<'_, ()>, (v,): (u32,)| {
            Box::new(async move { wasmtime::Result::Ok((v.wrapping_mul(2),)) })
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    let (r,) = run.call_async(&mut store, (x,)).await?;
    Ok(r)
}

/// A component whose export uses the **canonical async ABI** (callback variant +
/// `task.return`) — the shape `wasi:http`/`stream`/`future` lift through, and
/// the deeper piece beyond [run_component_async_double] (which is a *sync*-ABI
/// export calling an async *host import*). Here the guest's own `run` is async:
/// it computes, delivers its result via the `task.return` intrinsic, then
/// returns `callback_code::EXIT` (0) to signal completion. No host imports —
/// `task.return` is a canonical built-in.
#[cfg(feature = "compile")]
const CANON_ASYNC_WAT: &str = r#"(component
  (core func $task_return (canon task.return (result u32)))
  (core module $m
    (import "" "task-return" (func $task_return (param i32)))
    (func (export "run") (param i32) (result i32)
      (call $task_return (i32.mul (local.get 0) (i32.const 2)))
      (i32.const 0))                                 ;; callback_code::EXIT
    (func (export "cb") (param i32 i32 i32) (result i32)
      (i32.const 0)))
  (core instance $i (instantiate $m
    (with "" (instance (export "task-return" (func $task_return))))))
  (func (export "run") (param "x" u32) (result u32)
    (canon lift (core func $i "run") async (callback (func $i "cb")))))"#;

/// Instantiates [CANON_ASYNC_WAT] and drives the **async-lifted** export with
/// `call_async` (`wasm_component_model_async(true)`). Proves the canonical async
/// lift — the `wasi:http` streaming substrate — compiles and runs on Wasmtime
/// **45** under both backends. Returns `2·x`.
#[cfg(feature = "compile")]
async fn run_canonical_async_double(use_pulley: bool, x: u32) -> wasmtime::Result<u32> {
    use wasmtime::component::{Component, Linker};
    let mut cfg = Config::new();
    cfg.wasm_component_model_async(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    let engine = Engine::new(&cfg)?;
    let component = Component::new(&engine, CANON_ASYNC_WAT)?;
    let linker = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    let (r,) = run.call_async(&mut store, (x,)).await?;
    Ok(r)
}

/// Returns the C ABI version. Stable, dependency-free — the loader's handshake.
///
/// # Safety
/// None — takes no pointers.
#[no_mangle]
pub extern "C" fn hwr_abi_version() -> u32 {
    HWR_ABI_VERSION
}

/// Smoke test: constructs a default Wasmtime engine to prove the native runtime
/// links and initialises on this platform/backend. Returns `1` on success, `0`
/// on failure or panic. Used by the FFI harness and the Spike-1 iOS probe.
///
/// # Safety
/// None — takes no pointers.
#[no_mangle]
pub extern "C" fn hwr_self_test() -> i32 {
    let ok = std::panic::catch_unwind(|| {
        // Under `compile`, prove an engine (with the JIT compiler) initialises.
        // Without it (iOS), success means the native library loaded cleanly.
        #[cfg(feature = "compile")]
        {
            wasmtime::Engine::default();
        }
        true
    })
    .unwrap_or(false);
    if ok {
        1
    } else {
        0
    }
}

/// Runs the embedded `add(a, b)` module on a backend (`use_pulley`: 0 =
/// Cranelift JIT, non-zero = Pulley interpreter) and returns the result, or
/// `i64::MIN` on any error/panic. Proves real Wasm executes through the C ABI
/// on both backends — including the no-JIT Pulley path used on iOS.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_eval_add(use_pulley: i32, a: i32, b: i32) -> i64 {
    std::panic::catch_unwind(|| match run_add(use_pulley != 0, a, b) {
        Ok(v) => v as i64,
        Err(_) => i64::MIN,
    })
    .unwrap_or(i64::MIN)
}

/// Instantiates a Wasm **component** exporting `add` and runs it on the chosen
/// backend (`use_pulley`: 0 = Cranelift, non-zero = Pulley). Returns the result
/// or `i64::MIN`. Proves the Component Model path — the foundation for WASI 0.3
/// — executes through the C ABI.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_eval_component_add(use_pulley: i32, a: i32, b: i32) -> i64 {
    std::panic::catch_unwind(|| match run_component_add(use_pulley != 0, a, b) {
        Ok(v) => v as i64,
        Err(_) => i64::MIN,
    })
    .unwrap_or(i64::MIN)
}

/// Runs a component that **imports a host function** and calls it; the host
/// import is provided from Rust. Returns `2·x`, or `i64::MIN` on error. Proves
/// the host-import mechanism — the foundation for WASI/hellohq capabilities —
/// across the C ABI on both backends.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_eval_host_import(use_pulley: i32, x: i32) -> i64 {
    std::panic::catch_unwind(
        || match run_component_host_import(use_pulley != 0, x as u32) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        },
    )
    .unwrap_or(i64::MIN)
}

/// Runs the async-host-import module ([ASYNC_WAT]) on the chosen backend,
/// driving the async call to completion with a synchronous `block_on` — the
/// bridge a blockable worker isolate uses (docs 51/52). Returns `2·x`, or
/// `i64::MIN` on error. Proves **async** Wasm executes and returns across the C
/// ABI without the caller needing its own async runtime.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_run_async_double(use_pulley: i32, x: i32) -> i64 {
    std::panic::catch_unwind(
        || match pollster::block_on(run_async_double(use_pulley != 0, x)) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        },
    )
    .unwrap_or(i64::MIN)
}

/// As [hwr_run_async_double] but the async import runs under a **component** ABI
/// with Component Model Async enabled (Wasmtime 45, `component-model-async`).
/// Proves the async component path — the basis for async `ai:inference` /
/// `wasi:http` host imports (doc 53) — works on the shipped runtime version.
/// Returns `2·x`, or `i64::MIN` on error.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_run_component_async_double(use_pulley: i32, x: i32) -> i64 {
    std::panic::catch_unwind(|| {
        match pollster::block_on(run_component_async_double(use_pulley != 0, x as u32)) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        }
    })
    .unwrap_or(i64::MIN)
}

/// Drives the **canonical async-lift** component ([run_canonical_async_double])
/// across the C ABI — the `wasi:http`/stream/future substrate (`task.return` +
/// callback) proven on Wasmtime 45. Returns `2·x`, or `i64::MIN` on error.
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_run_canonical_async_double(use_pulley: i32, x: i32) -> i64 {
    std::panic::catch_unwind(|| {
        match pollster::block_on(run_canonical_async_double(use_pulley != 0, x as u32)) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        }
    })
    .unwrap_or(i64::MIN)
}

/// P2 Option A across the C ABI: runs the gated `workspace.read-portfolio-names`
/// component. `granted != 0` returns `count` (as if the workspace held that many
/// portfolios); denied returns `WORKSPACE_READ_DENIED` (u32::MAX as i64). Returns
/// `i64::MIN` only on an internal error. The gate decision is the caller's
/// (app-side in production).
///
/// # Safety
/// None — takes no pointers.
#[cfg(feature = "compile")]
#[no_mangle]
pub extern "C" fn hwr_read_portfolio_count(use_pulley: i32, granted: i32, count: u32) -> i64 {
    std::panic::catch_unwind(|| {
        match run_gated_workspace_read(use_pulley != 0, granted != 0, count) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        }
    })
    .unwrap_or(i64::MIN)
}

// ── P1: handle-based engine + instance lifecycle ───────────────────────────
//
// The reusable runtime backbone, replacing the one-shot `hwr_eval_*` demo calls:
// create an engine once, instantiate components against it, call exports, free.
// The minimal `call_add` proof is superseded by the byte/host-import ABI at P2.

/// Opaque engine handle (`wasmtime::Engine` + chosen backend).
pub struct HwrEngine {
    engine: wasmtime::Engine,
}

/// Creates a runtime engine on the chosen backend (`use_pulley`: 0 = Cranelift,
/// non-zero = Pulley interpreter). Returns null on failure. Available on every
/// build — engine creation needs no compiler.
///
/// # Safety
/// The result must be released with [hwr_engine_free] exactly once.
#[no_mangle]
pub extern "C" fn hwr_engine_new(use_pulley: i32) -> *mut HwrEngine {
    std::panic::catch_unwind(|| {
        let mut cfg = wasmtime::Config::new();
        if use_pulley != 0 {
            cfg.target("pulley64").ok()?;
        }
        let engine = wasmtime::Engine::new(&cfg).ok()?;
        Some(Box::into_raw(Box::new(HwrEngine { engine })))
    })
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

/// Releases an engine from [hwr_engine_new].
///
/// # Safety
/// `ptr` must be a live [hwr_engine_new] handle, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn hwr_engine_free(ptr: *mut HwrEngine) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}

/// Opaque instance handle: an instantiated component plus its owning store.
/// Available on every build — instantiation/calling are runtime ops; only
/// *compiling* from source needs Cranelift.
pub struct HwrInstance {
    store: wasmtime::Store<()>,
    instance: wasmtime::component::Instance,
}

/// Instantiates a Wasm **component** (binary, or WAT text in `compile` builds)
/// against [engine]. Returns null on failure. P2 adds the WASI/host-import
/// linker and the byte-oriented call ABI; the no-JIT deserialize path lands with
/// precompiled components.
///
/// # Safety
/// `engine` must be a live [hwr_engine_new] handle; `bytes`/`len` must describe
/// readable memory. Release the result with [hwr_instance_free].
#[cfg(feature = "compile")]
#[no_mangle]
pub unsafe extern "C" fn hwr_instance_new(
    engine: *mut HwrEngine,
    bytes: *const u8,
    len: usize,
) -> *mut HwrInstance {
    if engine.is_null() || bytes.is_null() {
        return std::ptr::null_mut();
    }
    let eng = &(*engine).engine;
    let wasm = std::slice::from_raw_parts(bytes, len);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let component = wasmtime::component::Component::new(eng, wasm).ok()?;
        let linker = wasmtime::component::Linker::new(eng);
        let mut store = wasmtime::Store::new(eng, ());
        let instance = linker.instantiate(&mut store, &component).ok()?;
        Some(Box::into_raw(Box::new(HwrInstance { store, instance })))
    }))
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

/// Releases an instance handle.
///
/// # Safety
/// `ptr` must be a live instance handle, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn hwr_instance_free(ptr: *mut HwrInstance) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr));
    }
}

/// Calls the instance's `add(s32, s32) -> s32` export — a minimal proof of the
/// reusable handle lifecycle. Returns the result, or `i64::MIN` on error. (P2
/// replaces this with the byte/host-import call ABI.)
///
/// # Safety
/// `inst` must be a live instance handle.
#[no_mangle]
pub unsafe extern "C" fn hwr_instance_call_add(inst: *mut HwrInstance, a: i32, b: i32) -> i64 {
    if inst.is_null() {
        return i64::MIN;
    }
    let h = &mut *inst;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let add = h
            .instance
            .get_typed_func::<(i32, i32), (i32,)>(&mut h.store, "add")
            .ok()?;
        let (r,) = add.call(&mut h.store, (a, b)).ok()?;
        Some(r as i64)
    }))
    .ok()
    .flatten()
    .unwrap_or(i64::MIN)
}

/// Precompiles a Wasm **component** to a serialized artifact for [engine]'s
/// backend (e.g. **Pulley bytecode** for the iOS target) — done off-device at
/// build/publish time. Returns a heap buffer (length via `out_len`) the caller
/// must release with [hwr_free_bytes], or null on failure. Requires Cranelift
/// (the `compile` build).
///
/// # Safety
/// `engine` must be a live [hwr_engine_new] handle; `bytes`/`len` readable;
/// `out_len` writable.
#[cfg(feature = "compile")]
#[no_mangle]
pub unsafe extern "C" fn hwr_precompile_component(
    engine: *mut HwrEngine,
    bytes: *const u8,
    len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if engine.is_null() || bytes.is_null() || out_len.is_null() {
        return std::ptr::null_mut();
    }
    let eng = &(*engine).engine;
    let wasm = std::slice::from_raw_parts(bytes, len);
    let artifact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eng.precompile_component(wasm).ok()
    }))
    .ok()
    .flatten();
    match artifact {
        Some(v) => {
            *out_len = v.len();
            // Hand ownership of the boxed slice to C; reclaimed by hwr_free_bytes.
            Box::into_raw(v.into_boxed_slice()) as *mut u8
        }
        None => std::ptr::null_mut(),
    }
}

/// Releases a buffer returned by [hwr_precompile_component].
///
/// # Safety
/// `ptr`/`len` must be exactly what [hwr_precompile_component] returned.
#[no_mangle]
pub unsafe extern "C" fn hwr_free_bytes(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// Instantiates a component from a **precompiled artifact** (from
/// [hwr_precompile_component]) — the **no-JIT / iOS path**: no Cranelift, just
/// deserialize + run on the Pulley interpreter. Returns null on failure.
/// Available on every build.
///
/// # Safety
/// `engine` must be a live [hwr_engine_new] handle whose backend matches the
/// artifact; `bytes`/`len` must be a trusted artifact from this runtime
/// (deserializing untrusted bytes is unsound). Release with [hwr_instance_free].
#[no_mangle]
pub unsafe extern "C" fn hwr_instance_new_precompiled(
    engine: *mut HwrEngine,
    bytes: *const u8,
    len: usize,
) -> *mut HwrInstance {
    if engine.is_null() || bytes.is_null() {
        return std::ptr::null_mut();
    }
    let eng = &(*engine).engine;
    let artifact = std::slice::from_raw_parts(bytes, len);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let component = wasmtime::component::Component::deserialize(eng, artifact).ok()?;
        let linker = wasmtime::component::Linker::new(eng);
        let mut store = wasmtime::Store::new(eng, ());
        let instance = linker.instantiate(&mut store, &component).ok()?;
        Some(Box::into_raw(Box::new(HwrInstance { store, instance })))
    }))
    .ok()
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// ── P3: Dart-serviced host-call round-trip (step/poll bridge) ────────────────
//
// A host import (`hellohq:plugin/hostcall.call`) the guest calls **suspends** the
// run; the request surfaces to the caller (Dart) via the blocking `hwr_p3_poll`;
// the caller services it app-side — gated — and `hwr_p3_resolve`s the value; the
// run resumes. `wasi:http` and `ai:inference` both ride this round-trip.
//
// Design (doc 51 "step() poll loop"): the `Store` is thread-affine, so it lives
// on a dedicated worker thread driven by `block_on`; only byte buffers cross
// between that thread and the caller. The host import's future awaits a
// `oneshot` the caller completes from another thread via `hwr_p3_resolve`,
// unparking the worker. The event channel carries the request out + the run's
// final result; the `oneshot` carries the response in. No JIT needed — works on
// the iOS Pulley build (the C entrypoint deserializes a precompiled component).

/// Poll status. Note: `hwr_p3_poll` BLOCKS until the next event, so it returns
/// only these terminal-ish states (never a "running" spin state).
pub const HWR_P3_PENDING: i32 = 1; // a host call awaits a value (read request, then resolve)
pub const HWR_P3_DONE: i32 = 2; // run finished OK (read result)
pub const HWR_P3_ERROR: i32 = 3; // run errored (result holds the message)

enum P3Event {
    HostCall {
        request: Vec<u8>,
        respond: futures_channel::oneshot::Sender<Vec<u8>>,
    },
    Done(Result<Vec<u8>, String>),
}

/// A P3 run in flight. Opaque to the C ABI.
pub struct HwrP3Session {
    rx: std::sync::mpsc::Receiver<P3Event>,
    thread: Option<std::thread::JoinHandle<()>>,
    pending_request: Vec<u8>,
    pending_respond: Option<futures_channel::oneshot::Sender<Vec<u8>>>,
    result: Vec<u8>,
    finished: bool,
}

/// Plain engine builder available in every build (no Cranelift) — the P3 thread
/// only instantiates + runs; compilation happened off-device.
fn p3_engine(use_pulley: bool) -> wasmtime::Result<wasmtime::Engine> {
    let mut cfg = wasmtime::Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    wasmtime::Engine::new(&cfg)
}

/// The worker-thread future: wire the gated `hostcall.call` host import (routes
/// each request out the channel and awaits the caller's `oneshot`), instantiate
/// the `p3-probe` guest, and drive `run(input) -> output`.
async fn p3_drive(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    input: Vec<u8>,
    tx: std::sync::mpsc::Sender<P3Event>,
) -> Result<Vec<u8>, String> {
    let e2s = |e: wasmtime::Error| e.to_string();
    let mut linker = wasmtime::component::Linker::<std::sync::mpsc::Sender<P3Event>>::new(engine);
    linker
        .instance("hellohq:plugin/hostcall@0.1.0")
        .map_err(e2s)?
        .func_wrap_async(
            "call",
            |store: wasmtime::StoreContextMut<'_, std::sync::mpsc::Sender<P3Event>>,
             (request,): (Vec<u8>,)| {
                // Surface the request synchronously; the future just waits for
                // the caller-supplied response (the guest is suspended here).
                let (respond, rx) = futures_channel::oneshot::channel();
                let _ = store.data().send(P3Event::HostCall { request, respond });
                Box::new(async move {
                    match rx.await {
                        Ok(resp) => Ok((resp,)),
                        Err(_) => Err(wasmtime::Error::msg("p3: resolve channel cancelled")),
                    }
                })
            },
        )
        .map_err(e2s)?;

    let mut store = wasmtime::Store::new(engine, tx);
    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(e2s)?;
    let run = instance
        .get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "run")
        .map_err(e2s)?;
    let (out,) = run.call_async(&mut store, (input,)).await.map_err(e2s)?;
    Ok(out)
}

/// Spawn the worker thread for a P3 run. `engine`/`component` are `Send` and move
/// onto the thread, which owns the `Store`. Available in every build.
fn p3_run_session(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    input: Vec<u8>,
) -> HwrP3Session {
    let (tx, rx) = std::sync::mpsc::channel::<P3Event>();
    let done_tx = tx.clone();
    let thread = std::thread::spawn(move || {
        let result = pollster::block_on(p3_drive(&engine, &component, input, tx));
        let _ = done_tx.send(P3Event::Done(result));
    });
    HwrP3Session {
        rx,
        thread: Some(thread),
        pending_request: Vec::new(),
        pending_respond: None,
        result: Vec::new(),
        finished: false,
    }
}

impl HwrP3Session {
    /// Block until the next event: a pending host call, or run completion.
    fn poll(&mut self) -> i32 {
        if self.finished {
            return HWR_P3_DONE;
        }
        match self.rx.recv() {
            Ok(P3Event::HostCall { request, respond }) => {
                self.pending_request = request;
                self.pending_respond = Some(respond);
                HWR_P3_PENDING
            }
            Ok(P3Event::Done(Ok(out))) => {
                self.result = out;
                self.finished = true;
                HWR_P3_DONE
            }
            Ok(P3Event::Done(Err(msg))) => {
                self.result = msg.into_bytes();
                self.finished = true;
                HWR_P3_ERROR
            }
            // Worker thread vanished without a Done (panic) — treat as error.
            Err(_) => {
                self.finished = true;
                HWR_P3_ERROR
            }
        }
    }

    fn request(&self) -> &[u8] {
        &self.pending_request
    }

    /// Supply the response to the pending host call, resuming the guest.
    fn resolve(&mut self, response: Vec<u8>) {
        if let Some(respond) = self.pending_respond.take() {
            let _ = respond.send(response);
        }
        self.pending_request.clear();
    }

    fn result(&self) -> &[u8] {
        &self.result
    }
}

impl Drop for HwrP3Session {
    fn drop(&mut self) {
        // Cancel any in-flight host call so the worker's await errors out and the
        // run unwinds, then join. Dropping `rx` also drops buffered events
        // (cancelling their responders) so a never-polled call can't wedge join.
        self.pending_respond.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start a P3 run: deserialize a precompiled component (the iOS no-JIT path) and
/// begin executing its `run(input) -> output` export on a worker thread. Returns
/// a session handle, or null on failure. `use_pulley != 0` selects the Pulley
/// backend. Drive it with [hwr_p3_poll] / [hwr_p3_resolve]; free with
/// [hwr_p3_free].
///
/// # Safety
/// `component`/`input` must point to `component_len`/`input_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_start(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
    input: *const u8,
    input_len: usize,
) -> *mut HwrP3Session {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len);
        let input_bytes = if input.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(input, input_len).to_vec()
        };
        let engine = match p3_engine(use_pulley != 0) {
            Ok(e) => e,
            Err(_) => return std::ptr::null_mut(),
        };
        // SAFETY: caller guarantees the bytes came from this runtime's
        // precompile/serialize (the trust boundary is the SHA-pinned fetch).
        let component = match wasmtime::component::Component::deserialize(&engine, comp_bytes) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(p3_run_session(engine, component, input_bytes)))
    })
    .unwrap_or(std::ptr::null_mut())
}

/// As [hwr_p3_start] but **compiles** a raw component (Cranelift) instead of
/// deserializing a precompiled one — the desktop/Android path and what the Dart
/// host tests use directly (no off-device precompile step). Not in the iOS slice.
///
/// # Safety
/// `component`/`input` must point to the given number of readable bytes.
#[cfg(feature = "compile")]
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_start_compile(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
    input: *const u8,
    input_len: usize,
) -> *mut HwrP3Session {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len);
        let input_bytes = if input.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(input, input_len).to_vec()
        };
        let engine = match p3_engine(use_pulley != 0) {
            Ok(e) => e,
            Err(_) => return std::ptr::null_mut(),
        };
        let component = match wasmtime::component::Component::new(&engine, comp_bytes) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        Box::into_raw(Box::new(p3_run_session(engine, component, input_bytes)))
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Block until the run yields a pending host call or finishes. Returns
/// [HWR_P3_PENDING] / [HWR_P3_DONE] / [HWR_P3_ERROR].
///
/// # Safety
/// `session` must be a live pointer from [hwr_p3_start].
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_poll(session: *mut HwrP3Session) -> i32 {
    if session.is_null() {
        return HWR_P3_ERROR;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (*session).poll()))
        .unwrap_or(HWR_P3_ERROR)
}

/// Pointer to the pending host-call request bytes (valid after [HWR_P3_PENDING]
/// until the next [hwr_p3_resolve]). Use with [hwr_p3_request_len].
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_request_ptr(session: *mut HwrP3Session) -> *const u8 {
    if session.is_null() {
        return std::ptr::null();
    }
    (*session).request().as_ptr()
}

/// Length of the pending host-call request bytes.
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_request_len(session: *mut HwrP3Session) -> usize {
    if session.is_null() {
        return 0;
    }
    (*session).request().len()
}

/// Supply the response to the pending host call, resuming the guest.
///
/// # Safety
/// `session` must be live; `response` must point to `response_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_resolve(
    session: *mut HwrP3Session,
    response: *const u8,
    response_len: usize,
) {
    if session.is_null() {
        return;
    }
    let resp = if response.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(response, response_len).to_vec()
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (*session).resolve(resp)));
}

/// Pointer to the final result bytes (valid after [HWR_P3_DONE]; after
/// [HWR_P3_ERROR] it is a UTF-8 error message). Use with [hwr_p3_result_len].
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_result_ptr(session: *mut HwrP3Session) -> *const u8 {
    if session.is_null() {
        return std::ptr::null();
    }
    (*session).result().as_ptr()
}

/// Length of the final result bytes.
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_result_len(session: *mut HwrP3Session) -> usize {
    if session.is_null() {
        return 0;
    }
    (*session).result().len()
}

/// Free a P3 session, cancelling any in-flight call and joining the worker.
///
/// # Safety
/// `session` must be a pointer from [hwr_p3_start] not already freed.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3_free(session: *mut HwrP3Session) {
    if !session.is_null() {
        drop(Box::from_raw(session));
    }
}

// ── P3 v2: streaming host-call round-trip (framed bidirectional channel) ─────
//
// The atomic P3 above is one request -> one response. wasi:http (Option B) needs
// to STREAM bodies: the request body flows OUT (host -> caller) chunk by chunk,
// the response body flows IN (caller -> host). This bridge frames that exchange:
//   - The worker runs a `driver(out, in_rx)` future. It emits outbound chunks via
//     `out.chunk(..)` / `out.end()` and consumes inbound chunks by awaiting
//     `in_rx` (closed by the caller via `hwr_p3s_push_end`).
//   - The caller drains OUT/OUT_END with `hwr_p3s_poll`, then pushes IN chunks
//     with `hwr_p3s_push` + `hwr_p3s_push_end`, then polls for DONE.
// Stage 3 plugs the wasi:http `handle` in as the driver; stage 2 (here) is the
// transport, proven with a synthetic driver in the tests.

/// `hwr_p3s_poll` status.
pub const HWR_P3S_OUT: i32 = 0; // an outbound chunk is available (read via out ptr/len)
pub const HWR_P3S_OUT_END: i32 = 1; // outbound stream finished; now push inbound
pub const HWR_P3S_DONE: i32 = 2; // run finished OK (read result)
pub const HWR_P3S_ERROR: i32 = 3; // run errored (result holds a message)

enum P3sEvent {
    Out(Vec<u8>),
    OutEnd,
    Done(Result<Vec<u8>, String>),
}

/// Handed to the driver: emit outbound chunks (the request body) to the caller.
pub struct P3sOut {
    tx: std::sync::mpsc::Sender<P3sEvent>,
}

// `chunk`/`end` are called by the driver (the wasi:http `handle` in stage 3; a
// synthetic driver in tests today).
#[allow(dead_code)]
impl P3sOut {
    fn chunk(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(P3sEvent::Out(bytes));
    }
    fn end(&self) {
        let _ = self.tx.send(P3sEvent::OutEnd);
    }
}

/// Inbound chunks (the response body) the caller pushes; the driver awaits them.
type P3sIn = futures_channel::mpsc::UnboundedReceiver<Vec<u8>>;

/// A streaming host-call in flight. Opaque to the C ABI.
pub struct HwrP3Stream {
    rx: std::sync::mpsc::Receiver<P3sEvent>,
    in_tx: Option<futures_channel::mpsc::UnboundedSender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    current_out: Vec<u8>,
    result: Vec<u8>,
    finished: bool,
}

/// Spawn a streaming session driven by `driver`. The driver owns the outbound
/// emitter + the inbound receiver and returns the run's final result bytes.
/// Wired to a C entrypoint with the wasi:http `handle` driver in stage 3; used
/// by the synthetic streaming test today.
#[allow(dead_code)]
fn p3s_run_session<F, Fut>(driver: F) -> HwrP3Stream
where
    F: FnOnce(P3sOut, P3sIn) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let (tx, rx) = std::sync::mpsc::channel::<P3sEvent>();
    let (in_tx, in_rx) = futures_channel::mpsc::unbounded::<Vec<u8>>();
    let done_tx = tx.clone();
    let thread = std::thread::spawn(move || {
        let out = P3sOut { tx };
        let result = pollster::block_on(driver(out, in_rx));
        let _ = done_tx.send(P3sEvent::Done(result));
    });
    HwrP3Stream {
        rx,
        in_tx: Some(in_tx),
        thread: Some(thread),
        current_out: Vec::new(),
        result: Vec::new(),
        finished: false,
    }
}

impl HwrP3Stream {
    fn poll(&mut self) -> i32 {
        if self.finished {
            return HWR_P3S_DONE;
        }
        match self.rx.recv() {
            Ok(P3sEvent::Out(chunk)) => {
                self.current_out = chunk;
                HWR_P3S_OUT
            }
            Ok(P3sEvent::OutEnd) => HWR_P3S_OUT_END,
            Ok(P3sEvent::Done(Ok(out))) => {
                self.result = out;
                self.finished = true;
                HWR_P3S_DONE
            }
            Ok(P3sEvent::Done(Err(msg))) => {
                self.result = msg.into_bytes();
                self.finished = true;
                HWR_P3S_ERROR
            }
            Err(_) => {
                self.finished = true;
                HWR_P3S_ERROR
            }
        }
    }

    fn out_chunk(&self) -> &[u8] {
        &self.current_out
    }

    fn push(&mut self, chunk: Vec<u8>) {
        if let Some(tx) = &self.in_tx {
            let _ = tx.unbounded_send(chunk);
        }
    }

    /// Close the inbound stream — the driver's `in_rx` then yields `None`.
    fn push_end(&mut self) {
        self.in_tx.take();
    }

    fn result(&self) -> &[u8] {
        &self.result
    }
}

impl Drop for HwrP3Stream {
    fn drop(&mut self) {
        // Close inbound so the driver's await unblocks and the run unwinds.
        self.in_tx.take();
        // Drain any buffered outbound events so the worker's sync sends don't
        // wedge, then join.
        while self.rx.try_recv().is_ok() {}
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Block until the next streaming event: an outbound chunk, end-of-outbound, or
/// completion. Returns [HWR_P3S_OUT] / [HWR_P3S_OUT_END] / [HWR_P3S_DONE] /
/// [HWR_P3S_ERROR].
///
/// # Safety
/// `session` must be a live pointer from a streaming-start entrypoint.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_poll(session: *mut HwrP3Stream) -> i32 {
    if session.is_null() {
        return HWR_P3S_ERROR;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (*session).poll()))
        .unwrap_or(HWR_P3S_ERROR)
}

/// Pointer to the current outbound chunk (valid after [HWR_P3S_OUT] until the
/// next poll). Use with [hwr_p3s_out_len].
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_out_ptr(session: *mut HwrP3Stream) -> *const u8 {
    if session.is_null() {
        return std::ptr::null();
    }
    (*session).out_chunk().as_ptr()
}

/// Length of the current outbound chunk.
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_out_len(session: *mut HwrP3Stream) -> usize {
    if session.is_null() {
        return 0;
    }
    (*session).out_chunk().len()
}

/// Push an inbound chunk (part of the response body) to the driver.
///
/// # Safety
/// `session` must be live; `chunk` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_push(session: *mut HwrP3Stream, chunk: *const u8, len: usize) {
    if session.is_null() {
        return;
    }
    let bytes = if chunk.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(chunk, len).to_vec()
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (*session).push(bytes)));
}

/// Close the inbound stream (no more response chunks).
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_push_end(session: *mut HwrP3Stream) {
    if session.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (*session).push_end()));
}

/// Pointer to the final result bytes (after [HWR_P3S_DONE]; a UTF-8 message
/// after [HWR_P3S_ERROR]). Use with [hwr_p3s_result_len].
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_result_ptr(session: *mut HwrP3Stream) -> *const u8 {
    if session.is_null() {
        return std::ptr::null();
    }
    (*session).result().as_ptr()
}

/// Length of the final result bytes.
///
/// # Safety
/// `session` must be live.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_result_len(session: *mut HwrP3Stream) -> usize {
    if session.is_null() {
        return 0;
    }
    (*session).result().len()
}

// ── P3 v2 streaming: shared engine + session runners ─────────────────────────
//
// The engine config and the per-transport driver are identical whether the
// component is freshly compiled (Cranelift, `Component::new`) or deserialized
// from a precompiled pulley64 artifact (`Component::deserialize`, no Cranelift).
// Factor them out so the `compile` and no-JIT (`_precompiled`) entrypoints share
// one body and differ only in how they obtain the `Component`.

/// Build the concurrency-enabled engine the streaming transports need (component
/// model + async + concurrency, to mint streams/futures), optionally targeting
/// the Pulley interpreter. Returns None on misconfiguration.
#[cfg(feature = "wasi-http")]
fn p3s_engine(use_pulley: bool) -> Option<wasmtime::Engine> {
    let mut cfg = wasmtime::Config::new();
    cfg.wasm_component_model(true);
    cfg.wasm_component_model_async(true);
    cfg.concurrency_support(true);
    if use_pulley && cfg.target("pulley64").is_err() {
        return None;
    }
    wasmtime::Engine::new(&cfg).ok()
}

/// Run the `wasi:http` guest [component] on [engine] over the P3 v2 transport.
#[cfg(feature = "wasi-http")]
fn p3s_http_session(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
) -> *mut HwrP3Stream {
    let session = p3s_run_session(move |out, in_rx| async move {
        let e2s = |e: wasmtime::Error| e.to_string();
        let mut linker =
            wasmtime::component::Linker::<crate::wasi_http::WasiHttpHost>::new(&engine);
        crate::wasi_http::WasiHttpHost::add_to_linker(&mut linker).map_err(e2s)?;

        let host = crate::wasi_http::WasiHttpHost::with_transport(out, in_rx);
        let mut store = wasmtime::Store::new(&engine, host);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .map_err(e2s)?;
        let run = instance
            .get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")
            .map_err(e2s)?;
        let (output,) = run.call_async(&mut store, ()).await.map_err(e2s)?;
        Ok(output)
    });
    Box::into_raw(Box::new(session))
}

/// Run the `hellohq:plugin/inference` guest [component] on [engine] over P3 v2.
#[cfg(feature = "wasi-http")]
fn p3s_inference_session(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
) -> *mut HwrP3Stream {
    let session = p3s_run_session(move |out, in_rx| async move {
        let e2s = |e: wasmtime::Error| e.to_string();
        let mut linker =
            wasmtime::component::Linker::<crate::inference::InferenceHost>::new(&engine);
        crate::inference::InferenceHost::add_to_linker(&mut linker).map_err(e2s)?;

        let host = crate::inference::InferenceHost::with_transport(out, in_rx);
        let mut store = wasmtime::Store::new(&engine, host);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .map_err(e2s)?;
        let run = instance
            .get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")
            .map_err(e2s)?;
        let (output,) = run.call_async(&mut store, ()).await.map_err(e2s)?;
        Ok(output)
    });
    Box::into_raw(Box::new(session))
}

/// STAGE 4: start a streaming `wasi:http` session by COMPILING `component`
/// (Cranelift). The guest's `handler.handle` frames the request OUT (drained via
/// [hwr_p3s_poll]/[hwr_p3s_out_ptr]) and builds the response from pushed-IN
/// frames ([hwr_p3s_push]/[hwr_p3s_push_end]); the run result is the guest's
/// `list<u8>` (status prefix + body).
///
/// Gated on `compile` (Cranelift). For the no-JIT (iOS) path use
/// [hwr_p3s_start_http_precompiled].
///
/// # Safety
/// `component` must point to `component_len` readable bytes.
#[cfg(all(feature = "wasi-http", feature = "compile"))]
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_start_http(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
) -> *mut HwrP3Stream {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len).to_vec();
        let engine = match p3s_engine(use_pulley != 0) {
            Some(e) => e,
            None => return std::ptr::null_mut(),
        };
        let component = match wasmtime::component::Component::new(&engine, &comp_bytes) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        p3s_http_session(engine, component)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// No-JIT twin of [hwr_p3s_start_http]: DESERIALIZE a precompiled (pulley64)
/// component artifact instead of compiling it, so the streaming `wasi:http` path
/// runs on the iOS Pulley build (no Cranelift). Gated on `wasi-http` only.
///
/// # Safety
/// `component` must point to `component_len` readable bytes that are a trusted
/// artifact produced by [hwr_precompile_component] for this runtime version;
/// `Component::deserialize` does not validate untrusted input.
#[cfg(feature = "wasi-http")]
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_start_http_precompiled(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
) -> *mut HwrP3Stream {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len).to_vec();
        let engine = match p3s_engine(use_pulley != 0) {
            Some(e) => e,
            None => return std::ptr::null_mut(),
        };
        let component =
            match unsafe { wasmtime::component::Component::deserialize(&engine, &comp_bytes) } {
                Ok(c) => c,
                Err(_) => return std::ptr::null_mut(),
            };
        p3s_http_session(engine, component)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Start a streaming `hellohq:plugin/inference` session by COMPILING `component`
/// (Cranelift) — the async-first AI inference round-trip. The guest's
/// `inference.complete` frames the request head OUT and builds the returned
/// `stream<string>` from pushed-IN token-delta frames; the run result is the
/// concatenated token deltas.
///
/// Gated on `compile` (Cranelift). For the no-JIT (iOS) path use
/// [hwr_p3s_start_inference_precompiled].
///
/// # Safety
/// `component` must point to `component_len` readable bytes.
#[cfg(all(feature = "wasi-http", feature = "compile"))]
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_start_inference(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
) -> *mut HwrP3Stream {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len).to_vec();
        let engine = match p3s_engine(use_pulley != 0) {
            Some(e) => e,
            None => return std::ptr::null_mut(),
        };
        let component = match wasmtime::component::Component::new(&engine, &comp_bytes) {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        p3s_inference_session(engine, component)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// No-JIT twin of [hwr_p3s_start_inference]: DESERIALIZE a precompiled (pulley64)
/// component artifact so the streaming inference path runs on the iOS Pulley
/// build (no Cranelift). Gated on `wasi-http` only.
///
/// # Safety
/// `component` must point to `component_len` readable bytes that are a trusted
/// artifact produced by [hwr_precompile_component]; `Component::deserialize`
/// does not validate untrusted input.
#[cfg(feature = "wasi-http")]
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_start_inference_precompiled(
    use_pulley: i32,
    component: *const u8,
    component_len: usize,
) -> *mut HwrP3Stream {
    std::panic::catch_unwind(|| {
        if component.is_null() {
            return std::ptr::null_mut();
        }
        let comp_bytes = std::slice::from_raw_parts(component, component_len).to_vec();
        let engine = match p3s_engine(use_pulley != 0) {
            Some(e) => e,
            None => return std::ptr::null_mut(),
        };
        let component =
            match unsafe { wasmtime::component::Component::deserialize(&engine, &comp_bytes) } {
                Ok(c) => c,
                Err(_) => return std::ptr::null_mut(),
            };
        p3s_inference_session(engine, component)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Free a streaming session, closing inbound and joining the worker.
///
/// # Safety
/// `session` must be a pointer from a streaming-start entrypoint, not freed.
#[no_mangle]
pub unsafe extern "C" fn hwr_p3s_free(session: *mut HwrP3Stream) {
    if !session.is_null() {
        drop(Box::from_raw(session));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(hwr_abi_version(), HWR_ABI_VERSION);
    }

    #[test]
    fn engine_initialises() {
        // The runtime links and a default engine can be created on the host.
        assert_eq!(hwr_self_test(), 1);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_runs_wasm() {
        assert_eq!(run_add(false, 2, 3).unwrap(), 5);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_interpreter_runs_wasm() {
        // The no-JIT path executes real Wasm and agrees with Cranelift.
        assert_eq!(run_add(true, 20, 22).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn eval_add_c_abi_pulley() {
        assert_eq!(hwr_eval_add(1, 2, 3), 5);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_runs_component() {
        assert_eq!(run_component_add(false, 2, 3).unwrap(), 5);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_runs_component() {
        // Component Model lift/lower works under the no-JIT interpreter too.
        assert_eq!(run_component_add(true, 20, 22).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn component_c_abi() {
        assert_eq!(hwr_eval_component_add(1, 2, 3), 5);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_component_host_import() {
        assert_eq!(run_component_host_import(false, 21).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_component_host_import() {
        // A component calling back into a host import works under no-JIT too.
        assert_eq!(run_component_host_import(true, 21).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_async_host_import() {
        assert_eq!(pollster::block_on(run_async_double(false, 21)).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_async_host_import() {
        // Async host import suspends/resumes under the no-JIT interpreter too.
        assert_eq!(pollster::block_on(run_async_double(true, 21)).unwrap(), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn async_double_c_abi() {
        assert_eq!(hwr_run_async_double(1, 21), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_component_async() {
        // Component Model Async on Wasmtime 45: async host import under a
        // component ABI with wasm_component_model_async(true).
        assert_eq!(
            pollster::block_on(run_component_async_double(false, 21)).unwrap(),
            42
        );
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_component_async() {
        // Same, no-JIT — the iOS backend runs async components too.
        assert_eq!(
            pollster::block_on(run_component_async_double(true, 21)).unwrap(),
            42
        );
    }

    #[cfg(feature = "compile")]
    #[test]
    fn component_async_c_abi() {
        assert_eq!(hwr_run_component_async_double(1, 21), 42);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn cranelift_canonical_async_lift() {
        // Canonical async ABI (task.return + callback) — the wasi:http shape.
        assert_eq!(
            pollster::block_on(run_canonical_async_double(false, 21)).unwrap(),
            42
        );
    }

    #[cfg(feature = "compile")]
    #[test]
    fn pulley_canonical_async_lift() {
        assert_eq!(
            pollster::block_on(run_canonical_async_double(true, 21)).unwrap(),
            42
        );
    }

    #[cfg(feature = "compile")]
    #[test]
    fn workspace_read_granted() {
        // Gate grants → the read host import returns workspace data, both backends.
        assert_eq!(run_gated_workspace_read(false, true, 3).unwrap(), 3);
        assert_eq!(run_gated_workspace_read(true, true, 3).unwrap(), 3);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn workspace_read_denied() {
        // Gate denies → the plugin never receives workspace data.
        assert_eq!(
            run_gated_workspace_read(false, false, 3).unwrap(),
            WORKSPACE_READ_DENIED
        );
    }

    #[cfg(feature = "compile")]
    #[test]
    fn workspace_read_c_abi() {
        assert_eq!(hwr_read_portfolio_count(1, 1, 5), 5);
        assert_eq!(
            hwr_read_portfolio_count(1, 0, 5),
            WORKSPACE_READ_DENIED as i64
        );
    }

    // P3: the Dart-serviced host-call round-trip. The guest forwards its input
    // through the gated `hostcall.call`; the test plays the servicer (Dart's role
    // in production): poll → read request → resolve → poll → read result.
    #[cfg(feature = "compile")]
    fn p3_roundtrip(use_pulley: bool) {
        let bytes = include_bytes!("../tests/fixtures/p3_probe_guest.wasm");
        let engine = p3_engine(use_pulley).unwrap();
        let component = wasmtime::component::Component::new(&engine, bytes.as_slice()).unwrap();
        let mut s = p3_run_session(engine, component, b"ping".to_vec());

        assert_eq!(s.poll(), HWR_P3_PENDING);
        assert_eq!(s.request(), b"ping"); // guest forwarded input to hostcall.call
        s.resolve(b"pong".to_vec()); // servicer (stand-in for Dart) answers
        assert_eq!(s.poll(), HWR_P3_DONE);
        assert_eq!(s.result(), b"pong"); // guest returned what the host resolved
    }

    #[cfg(feature = "compile")]
    #[test]
    fn p3_roundtrip_cranelift() {
        p3_roundtrip(false);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn p3_roundtrip_pulley() {
        p3_roundtrip(true);
    }

    // The free-mid-flight path: drop a session while a host call is pending and
    // confirm Drop cancels + joins without hanging.
    #[cfg(feature = "compile")]
    #[test]
    fn p3_free_while_pending() {
        let bytes = include_bytes!("../tests/fixtures/p3_probe_guest.wasm");
        let engine = p3_engine(false).unwrap();
        let component = wasmtime::component::Component::new(&engine, bytes.as_slice()).unwrap();
        let mut s = p3_run_session(engine, component, b"x".to_vec());
        assert_eq!(s.poll(), HWR_P3_PENDING);
        drop(s); // must not hang — Drop cancels the oneshot and joins the worker
    }

    // P3 v2 streaming transport (stage 2). Synthetic driver: emit two outbound
    // "request" chunks, then concatenate the inbound "response" chunks. Proves
    // the framed bidirectional exchange end to end. Runtime-only (no wasmtime),
    // so it runs in every build incl. --no-default-features (iOS).
    fn p3s_echo_driver(
        out: P3sOut,
        mut in_rx: P3sIn,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> {
        use futures_util::StreamExt;
        async move {
            out.chunk(b"req-1".to_vec());
            out.chunk(b"req-2".to_vec());
            out.end();
            let mut acc = Vec::new();
            while let Some(chunk) = in_rx.next().await {
                acc.extend_from_slice(&chunk);
            }
            Ok(acc)
        }
    }

    #[test]
    fn p3s_streaming_roundtrip() {
        let mut s = p3s_run_session(p3s_echo_driver);
        assert_eq!(s.poll(), HWR_P3S_OUT);
        assert_eq!(s.out_chunk(), b"req-1");
        assert_eq!(s.poll(), HWR_P3S_OUT);
        assert_eq!(s.out_chunk(), b"req-2");
        assert_eq!(s.poll(), HWR_P3S_OUT_END); // request fully emitted
        s.push(b"resp-A".to_vec()); // caller streams the response in
        s.push(b"resp-B".to_vec());
        s.push_end();
        assert_eq!(s.poll(), HWR_P3S_DONE);
        assert_eq!(s.result(), b"resp-Aresp-B");
    }

    #[test]
    fn p3s_free_mid_stream() {
        // Drop after draining one outbound chunk — must not hang.
        let mut s = p3s_run_session(p3s_echo_driver);
        assert_eq!(s.poll(), HWR_P3S_OUT);
        drop(s);
    }

    #[cfg(feature = "compile")]
    #[test]
    fn handle_lifecycle_component() {
        // The reusable P1 path: one engine → instantiate component → call → free,
        // on the no-JIT Pulley backend.
        unsafe {
            let eng = hwr_engine_new(1);
            assert!(!eng.is_null());
            let wat = ADD_COMPONENT_WAT.as_bytes();
            let inst = hwr_instance_new(eng, wat.as_ptr(), wat.len());
            assert!(!inst.is_null());
            assert_eq!(hwr_instance_call_add(inst, 20, 22), 42);
            // A second call on the same instance reuses the store.
            assert_eq!(hwr_instance_call_add(inst, 1, 2), 3);
            hwr_instance_free(inst);
            hwr_engine_free(eng);
        }
    }

    #[cfg(feature = "compile")]
    #[test]
    fn precompile_then_deserialize_runs() {
        // The iOS model: precompile to a Pulley artifact off-device (Cranelift),
        // then deserialize + run with no compiler (Pulley interpreter only).
        unsafe {
            let ceng = hwr_engine_new(1); // pulley target
            assert!(!ceng.is_null());
            let wat = ADD_COMPONENT_WAT.as_bytes();
            let mut out_len: usize = 0;
            let artifact = hwr_precompile_component(ceng, wat.as_ptr(), wat.len(), &mut out_len);
            assert!(!artifact.is_null());
            assert!(out_len > 0);

            let reng = hwr_engine_new(1);
            let inst = hwr_instance_new_precompiled(reng, artifact, out_len);
            assert!(!inst.is_null());
            assert_eq!(hwr_instance_call_add(inst, 20, 22), 42);

            hwr_instance_free(inst);
            hwr_engine_free(reng);
            hwr_free_bytes(artifact, out_len);
            hwr_engine_free(ceng);
        }
    }

    #[test]
    fn engine_handle_null_safe() {
        // Freeing null and a fresh engine must not crash.
        unsafe {
            hwr_engine_free(std::ptr::null_mut());
            let eng = hwr_engine_new(0);
            assert!(!eng.is_null());
            hwr_engine_free(eng);
        }
    }
}
