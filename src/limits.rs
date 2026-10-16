// SPDX-License-Identifier: Apache-2.0
//
//! Per-run resource limits for a plugin guest (hellohq-wasm-runtime#25).
//!
//! Before this module the runtime bounded a guest with nothing: a plugin stuck
//! in a loop kept its thread and its CPU, and its linear memory could grow until
//! the host process ran out. The app's wall-clock watchdog (`plugin_p3_runner.dart`)
//! kills the worker isolate after a time budget, but that **cannot stop native
//! code mid-call**, and it caps nothing about memory. The limits the app used to
//! apply in its retired `PluginWasmService` — fuel at 1e9 per run, memory at
//! 256 MiB — did not move over when that service was replaced by this runtime.
//! This module restores them, at the runtime, where the guest actually runs.
//!
//! ## CPU: fuel, not a wall clock
//!
//! CPU is bounded with Wasmtime **fuel** (`Config::consume_fuel` + `Store::set_fuel`),
//! not epoch interruption. Fuel counts *executed WebAssembly instructions* and is
//! — deliberately — **not** consumed while the guest is parked on a host call
//! waiting for the app to `hwr_p3_resolve` it. So a plugin that legitimately
//! waits on streaming inference, a user gesture or a slow read is never charged
//! for the app's latency, and never killed for it. A wall-clock epoch deadline
//! would misfire exactly there (the epoch keeps advancing while the guest is
//! suspended, so it would trap the instant the guest resumed). Fuel is also
//! deterministic and needs no ticker thread. `#25` asked for "epoch interruption
//! … optionally also fuel"; fuel is the mechanism that meets the goal (a hard CPU
//! bound) without that misfire.
//!
//! ## Memory: a store limiter
//!
//! Every store a guest runs on gets a [`wasmtime::StoreLimits`] cap (linear
//! memory, tables, instances). `memory.grow` past the cap returns -1; an
//! initial memory already over the cap fails instantiation. This is the "hard
//! memory cap" `#25` asks for.
//!
//! ## Typed errors
//!
//! A resource-limit trap is annotated with a stable prefix the app can match to
//! say "the plugin used too much memory" or "the plugin took too long" instead of
//! surfacing a raw Wasmtime string.

use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};

/// Default linear-memory cap per guest instance: 256 MiB. The value the retired
/// in-app `PluginWasmService` used (`#25`).
pub const DEFAULT_MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Default table-element cap; generous for the SDK guests we ship, far below
/// anything that would exhaust host memory.
pub const DEFAULT_TABLE_ELEMENTS: usize = 100_000;
/// Default cap on instances/tables/memories per store.
pub const DEFAULT_INSTANCES: usize = 64;
pub const DEFAULT_TABLES: usize = 64;
pub const DEFAULT_MEMORIES: usize = 8;

/// Default CPU budget per run, in WebAssembly fuel units. 1e9 again matches the
/// retired `PluginWasmService` (`#25`).
pub const DEFAULT_FUEL: u64 = 1_000_000_000;

/// Stable error prefixes the app matches on. A resource-limit trap's message is
/// prefixed with one of these (see [`annotate`]).
pub const ERR_CPU_LIMIT: &str = "hellohq-plugin-cpu-limit";
pub const ERR_MEMORY_LIMIT: &str = "hellohq-plugin-memory-limit";

/// Process-wide fuel-budget override. `0` means "use [`DEFAULT_FUEL`]". Set from
/// the C ABI (`hwr_set_fuel_budget`) or from tests.
static FUEL_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// Override the per-run CPU budget (fuel units). `0` restores the default.
pub fn set_fuel_budget(fuel: u64) {
    FUEL_OVERRIDE.store(fuel, Ordering::Relaxed);
}

/// The fuel budget the next run gets.
pub fn fuel_budget() -> u64 {
    match FUEL_OVERRIDE.load(Ordering::Relaxed) {
        0 => DEFAULT_FUEL,
        v => v,
    }
}

/// The per-store resource limiter to attach to every guest store.
pub fn store_limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(DEFAULT_MEMORY_BYTES)
        .table_elements(DEFAULT_TABLE_ELEMENTS)
        .instances(DEFAULT_INSTANCES)
        .tables(DEFAULT_TABLES)
        .memories(DEFAULT_MEMORIES)
        .build()
}

/// Turn the CPU budget on for an engine. Call from every engine builder that can
/// run plugin code; without it `Store::set_fuel` errors.
pub fn configure_engine(cfg: &mut wasmtime::Config) {
    cfg.consume_fuel(true);
}

/// Classify a Wasmtime trap/instantiation error as a resource-limit trap, if it
/// is one.
pub fn classify(err: &str) -> Option<&'static str> {
    let e = err.to_ascii_lowercase();
    // Fuel exhaustion and epoch/interrupt traps are both the CPU bound.
    if e.contains("fuel") || e.contains("epoch") || e.contains("interrupt") {
        Some(ERR_CPU_LIMIT)
    } else if e.contains("exceeds memory limits")
        || e.contains("memory limits")
        || (e.contains("memory") && (e.contains("limit") || e.contains("exceed")))
    {
        Some(ERR_MEMORY_LIMIT)
    } else {
        None
    }
}

/// Like [`classify`], but walks the whole error source chain. Wasmtime reports a
/// resource-limit trap as the *cause* of an outer "error while executing at wasm
/// backtrace" error, so the outer `to_string()` alone does not name the limit.
pub fn classify_chain(err: &wasmtime::Error) -> Option<&'static str> {
    if let Some(code) = classify(&err.to_string()) {
        return Some(code);
    }
    let mut src = err.source();
    while let Some(s) = src {
        if let Some(code) = classify(&s.to_string()) {
            return Some(code);
        }
        src = s.source();
    }
    None
}

/// Prefix a resource-limit trap's message with its stable code, so the app can
/// match it ("the plugin used too much memory" / "took too long"). Ordinary
/// plugin/host errors are returned unchanged.
pub fn annotate(err: &wasmtime::Error) -> String {
    match classify_chain(err) {
        Some(code) => format!("{code}: {err:#}"),
        None => err.to_string(),
    }
}

/// The `StoreLimits` type, re-exported so callers can name the field type without
/// importing Wasmtime directly.
pub type Limits = StoreLimits;

/// A no-op `ResourceLimiter` used in tests that want a store with no cap.
pub struct NoLimits;
impl ResourceLimiter for NoLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

#[cfg(all(test, feature = "compile"))]
mod tests {
    use super::*;
    use wasmtime::{Config, Engine, Module, Store};

    /// Engine with the CPU budget on, matching `configure_engine`.
    fn budgeted_engine() -> Engine {
        let mut cfg = Config::new();
        configure_engine(&mut cfg);
        Engine::new(&cfg).unwrap()
    }

    /// A core module that spins forever — the CPU-limit case.
    const SPIN_WAT: &str = r#"(module
      (func (export "spin") (result i32)
        (local i32)
        (loop $l
          (local.set 0 (i32.add (local.get 0) (i32.const 1)))
          (br $l))
        (local.get 0)))"#;

    /// A core module that tries to grow its memory far past any sane cap.
    const GROW_WAT: &str = r#"(module
      (memory (export "memory") 1)
      (func (export "grow") (result i32)
        (memory.grow (i32.const 1_000_000))))"#;

    #[test]
    fn fuel_bounds_a_spin() {
        let engine = budgeted_engine();
        let module = Module::new(&engine, SPIN_WAT).unwrap();
        let mut store = Store::new(&engine, NoLimits);
        store.set_fuel(100_000).unwrap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), i32>(&mut store, "spin")
            .unwrap();
        let err = spin.call(&mut store, ()).unwrap_err();
        assert_eq!(
            classify_chain(&err),
            Some(ERR_CPU_LIMIT),
            "a spinning guest must exhaust fuel and classify as the CPU limit: {err}"
        );
        // The annotation the C ABI surfaces to the app.
        assert!(annotate(&err).starts_with(ERR_CPU_LIMIT));
    }

    #[test]
    fn store_limiter_caps_memory_growth() {
        let engine = Engine::default();
        let module = Module::new(&engine, GROW_WAT).unwrap();
        // Store data *is* the limiter here; the runtime attaches one per host.
        let mut store = Store::new(&engine, store_limits());
        store.limiter(|limits| limits);
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let grow = instance
            .get_typed_func::<(), i32>(&mut store, "grow")
            .unwrap();
        // Grow past the cap: the limiter refuses, so `memory.grow` returns -1.
        assert_eq!(grow.call(&mut store, ()).unwrap(), -1);
    }

    #[test]
    fn fuel_override_is_honoured() {
        set_fuel_budget(12345);
        assert_eq!(fuel_budget(), 12345);
        set_fuel_budget(0);
        assert_eq!(fuel_budget(), DEFAULT_FUEL);
    }
}
