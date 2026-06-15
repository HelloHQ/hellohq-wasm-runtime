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
//! ## Status: SPIKE-STAGE
//! Only the ABI surface and a runtime self-test are wired. The component
//! instantiation, the WASI worlds (`wasi:http` etc.), and the async-over-FFI
//! bridge are the P0 spike's deliverables and are **not implemented yet** — do
//! not depend on this crate's shape until the spike gates (Pulley-on-iOS,
//! C-ABI-vs-generic-API, async-FFI) pass.
//!
//! ## Planned C ABI (filled in across the spike → P1/P2)
//! - `hwr_abi_version() -> u32`                                  — ABI version (now)
//! - `hwr_self_test() -> i32`                                    — runtime links + inits (now)
//! - `hwr_engine_new(config) -> *Engine` / `hwr_engine_free`     — engine lifecycle (P1)
//! - `hwr_instantiate(engine, component_bytes, len) -> *Instance`— component load (P2)
//! - `hwr_call(instance, …) -> status`                          — invoke export (P2)
//! - async bridge: `hwr_step(instance) -> {done|pending(call_id)}`
//!   + `hwr_resolve(call_id, bytes, len)`                        — host-call round-trip (P3)
//!
//! Safety: every `extern "C"` entrypoint must `catch_unwind` and never let a
//! panic cross the FFI boundary (see `Cargo.toml` `panic = unwind`).

/// C ABI version. Bumped on any breaking change to the exported surface so the
/// Dart `dart:ffi` loader can refuse a mismatched native library.
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
    add.post_return(&mut store)?;
    Ok(result)
}

/// Core module importing an **async** host `double` and exporting `run` that
/// calls it — proves a Wasm call can trigger an async host operation that
/// suspends/resumes the Wasm fiber (the async-over-FFI mechanism). The
/// component-async canonical ABI (WASI 0.3) lands on Wasmtime 46 and slots into
/// the same bridge.
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
    cfg.async_support(true);
    if use_pulley {
        cfg.target("pulley64")?;
    }
    let engine = Engine::new(&cfg)?;
    let module = Module::new(&engine, ASYNC_WAT)?;
    let mut linker = Linker::new(&engine);
    linker.func_wrap_async(
        "host",
        "double",
        |_caller: Caller<'_, ()>, (v,): (i32,)| {
            // The Wasm guest suspends here while this future runs, then resumes.
            Box::new(async move { wasmtime::Result::Ok(v.wrapping_mul(2)) })
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &module).await?;
    let run = instance.get_typed_func::<i32, i32>(&mut store, "run")?;
    run.call_async(&mut store, x).await
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
    std::panic::catch_unwind(|| {
        match pollster::block_on(run_async_double(use_pulley != 0, x)) {
            Ok(v) => v as i64,
            Err(_) => i64::MIN,
        }
    })
    .unwrap_or(i64::MIN)
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
}
