// SPDX-License-Identifier: Apache-2.0
//
//! Executable code memory for the macOS hardened runtime.
//!
//! ## The problem
//!
//! The shipped macOS app runs with the hardened runtime and the
//! `com.apple.security.cs.allow-jit` entitlement. That entitlement allows
//! executing code from **`MAP_JIT`** mappings only. Stock Wasmtime puts
//! compiled code in an ordinary anonymous `mmap` and `mprotect`s the text
//! section to read+execute; under the hardened runtime the `mprotect` succeeds
//! but the first instruction fetched from that page kills the process with
//! `SIGKILL (Code Signature Invalid)`. The only entitlement that would allow it
//! is `allow-unsigned-executable-memory`, which the app does not carry.
//!
//! Wasmtime 46 has no `MAP_JIT` support of its own, and its
//! `wasmtime::CustomCodeMemory` hook only changes page permissions; it cannot choose
//! where code lives. `MAP_JIT` cannot be applied to an existing mapping either:
//! `mmap(MAP_JIT | MAP_FIXED)` returns `EINVAL`.
//!
//! ## The fix
//!
//! Wasmtime can run code from memory the embedder owns
//! (`Component::deserialize_raw` / `Module::deserialize_raw`). On macOS, for
//! native (non-Pulley) engines, this module loads every component and module
//! that way:
//!
//! 1. Compile with `Engine::precompile_*` (raw Wasm only; a precompiled artifact
//!    skips this). The same engine compiles and loads, so the `consume_fuel`
//!    setting the limits rely on is carried through.
//! 2. Copy the artifact into a fresh `MAP_JIT` mapping (read+write+execute).
//!    On Apple Silicon the copy runs with this thread's JIT write protection
//!    switched off (`pthread_jit_write_protect_np(0)`), and switched back on
//!    right after. Then the instruction cache is invalidated.
//! 3. `deserialize_raw` the mapping. The engine's `MapJitCodeMemory` hook is
//!    called to publish the text section; it claims the mapping, which from then
//!    on belongs to that engine.
//!
//! ## W^X on Apple Silicon and Intel
//!
//! `MAP_JIT` pages are never writable and executable for the same thread at the
//! same time on arm64: the toggle is per thread, and a thread's default state
//! is "execute". Only the thread doing the copy flips to "write", only for the
//! copy, and it runs nothing else while flipped. Other threads running guest
//! code are unaffected. Nothing writes to the mapping after the copy: Wasmtime
//! treats memory given to `deserialize_raw` as read-only. On Intel Macs the
//! toggle does not exist; `MAP_JIT` pages there are plain read+write+execute,
//! and only the copy writes to them.
//!
//! ## Lifetime
//!
//! A mapping is unmapped when its engine's `MapJitCodeMemory` is dropped. Every
//! piece of Wasmtime code memory holds an `Arc` to the engine's hook, so that
//! drop happens only after the last component, module, store and engine
//! clone using the code is gone. (It happens after Wasmtime has also
//! unregistered the code's unwind tables.) The cost is that a long-lived engine
//! keeps the code of every component it loaded until the engine is freed. The
//! per-run P3 engines the app uses are freed at the end of each run.
//!
//! ## Everything else
//!
//! Pulley engines (iOS, and `use_pulley` everywhere) never produce executable
//! code and take the stock paths. Off macOS, [`configure`] does nothing and the
//! loaders are the stock Wasmtime calls. On macOS, code that the hook did not
//! allocate (a component built with plain `Component::new`, as some tests do)
//! falls back to what stock Wasmtime does: `mprotect` to read+execute. That
//! still works outside the hardened runtime; the production entrypoints all go
//! through this module.

use wasmtime::component::Component;
use wasmtime::{Config, Engine, Module, Result};

/// Install the `MAP_JIT` code-memory hook on `cfg` (macOS only; a no-op
/// elsewhere). Call it from every engine builder that runs guest code, and load
/// that engine's code with the functions in this module.
pub fn configure(cfg: &mut Config) {
    #[cfg(target_os = "macos")]
    cfg.with_custom_code_memory(Some(std::sync::Arc::new(macos::MapJitCodeMemory::new())));
    #[cfg(not(target_os = "macos"))]
    let _ = cfg;
}

/// Compile a component (binary or, with `wat`, text) for `engine`. Replaces
/// `Component::new`.
#[cfg(feature = "compile")]
pub fn compile_component(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Component> {
    let bytes = bytes.as_ref();
    #[cfg(target_os = "macos")]
    if !engine.is_pulley() {
        let artifact = engine.precompile_component(bytes)?;
        // SAFETY: `artifact` was produced by this engine just now.
        return unsafe { macos::load_component(engine, &artifact) };
    }
    Component::new(engine, bytes)
}

/// Load a precompiled component artifact. Replaces `Component::deserialize`.
///
/// # Safety
/// As `Component::deserialize`: `bytes` must be a trusted artifact produced by
/// this runtime (Wasmtime does not validate it).
pub unsafe fn deserialize_component(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Component> {
    let bytes = bytes.as_ref();
    #[cfg(target_os = "macos")]
    if !engine.is_pulley() {
        return unsafe { macos::load_component(engine, bytes) };
    }
    unsafe { Component::deserialize(engine, bytes) }
}

/// Compile a core module (binary or, with `wat`, text) for `engine`. Replaces
/// `Module::new`.
#[cfg(feature = "compile")]
pub fn compile_module(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Module> {
    let bytes = bytes.as_ref();
    #[cfg(target_os = "macos")]
    if !engine.is_pulley() {
        let artifact = engine.precompile_module(bytes)?;
        // SAFETY: `artifact` was produced by this engine just now.
        return unsafe { macos::load_module(engine, &artifact) };
    }
    Module::new(engine, bytes)
}

/// Load a precompiled core-module artifact. Replaces `Module::deserialize`.
///
/// # Safety
/// As `Module::deserialize`: `bytes` must be a trusted artifact produced by
/// this runtime.
pub unsafe fn deserialize_module(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Module> {
    let bytes = bytes.as_ref();
    #[cfg(target_os = "macos")]
    if !engine.is_pulley() {
        return unsafe { macos::load_module(engine, bytes) };
    }
    unsafe { Module::deserialize(engine, bytes) }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ptr::NonNull;
    use std::sync::{Mutex, MutexGuard};

    use wasmtime::component::Component;
    use wasmtime::{CustomCodeMemory, Engine, Module, Result};

    extern "C" {
        // libSystem (<libkern/OSCacheControl.h>): make freshly written code
        // visible to instruction fetch. A no-op on x86_64.
        fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
    }

    fn page_size() -> usize {
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(16 * 1024)
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        // No code holding these locks can panic halfway through an update.
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One `MAP_JIT` mapping, unmapped on drop.
    struct Region {
        base: usize,
        len: usize,
    }

    impl Region {
        /// Map `bytes.len()` rounded up to whole pages as `MAP_JIT` and copy
        /// `bytes` in.
        fn with_contents(bytes: &[u8]) -> Result<Region> {
            let page = page_size();
            let len = bytes.len().max(1).div_ceil(page) * page;
            // SAFETY: a fresh anonymous mapping; no existing memory is touched.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                let err = std::io::Error::last_os_error();
                wasmtime::bail!(
                    "MAP_JIT allocation of {len} bytes for Wasm code failed: {err} \
                     (under the hardened runtime this needs the \
                     com.apple.security.cs.allow-jit entitlement)"
                );
            }
            let region = Region {
                base: ptr as usize,
                len,
            };
            {
                let _write = JitWriteWindow::open();
                // SAFETY: `ptr` is a fresh private mapping of at least
                // `bytes.len()` bytes, writable by this thread while `_write`
                // lives. The ranges cannot overlap.
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len()) };
            }
            // SAFETY: the range is mapped.
            unsafe { sys_icache_invalidate(ptr, len) };
            Ok(region)
        }

        fn contains(&self, ptr: usize, len: usize) -> bool {
            ptr >= self.base && ptr.saturating_add(len) <= self.base + self.len
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: we own this mapping and nothing references it any more
            // (see the lifetime notes on the module).
            unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
        }
    }

    /// Lets the current thread write `MAP_JIT` pages while it lives (Apple
    /// Silicon). The thread must not run JIT code while the window is open.
    struct JitWriteWindow;

    impl JitWriteWindow {
        fn open() -> JitWriteWindow {
            #[cfg(target_arch = "aarch64")]
            // SAFETY: toggles this thread's own JIT write protection only.
            unsafe {
                libc::pthread_jit_write_protect_np(0)
            };
            JitWriteWindow
        }
    }

    impl Drop for JitWriteWindow {
        fn drop(&mut self) {
            // Back to "execute", every thread's default state.
            #[cfg(target_arch = "aarch64")]
            // SAFETY: as in `open`.
            unsafe {
                libc::pthread_jit_write_protect_np(1)
            };
        }
    }

    /// Mappings filled by [load] that no engine has published yet.
    static PENDING: Mutex<Vec<Region>> = Mutex::new(Vec::new());

    fn take_pending(f: impl Fn(&Region) -> bool) -> Option<Region> {
        let mut pending = lock(&PENDING);
        let i = pending.iter().position(f)?;
        Some(pending.swap_remove(i))
    }

    /// The engine's code-memory hook: claims `MAP_JIT` mappings when Wasmtime
    /// publishes code in them, and owns them until the engine is gone.
    pub(super) struct MapJitCodeMemory {
        owned: Mutex<Vec<Region>>,
    }

    impl MapJitCodeMemory {
        pub(super) fn new() -> Self {
            MapJitCodeMemory {
                owned: Mutex::new(Vec::new()),
            }
        }

        fn owns(&self, ptr: usize, len: usize) -> bool {
            lock(&self.owned).iter().any(|r| r.contains(ptr, len))
        }
    }

    impl CustomCodeMemory for MapJitCodeMemory {
        fn required_alignment(&self) -> usize {
            // Wasmtime page-aligns the text section, which the stock fallback
            // below needs for `mprotect`.
            page_size()
        }

        fn publish_executable(&self, ptr: *const u8, len: usize) -> Result<()> {
            let addr = ptr as usize;
            if let Some(region) = take_pending(|r| r.contains(addr, len)) {
                // Already read+write+execute and flushed by `with_contents`.
                lock(&self.owned).push(region);
                stats::note_map_jit();
                return Ok(());
            }
            // Not ours: do what stock Wasmtime does. Only works outside the
            // hardened runtime.
            // SAFETY: Wasmtime hands us its own page-aligned text range.
            let rc = unsafe {
                libc::mprotect(
                    ptr as *mut libc::c_void,
                    len,
                    libc::PROT_READ | libc::PROT_EXEC,
                )
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                wasmtime::bail!("unable to make memory executable: {err}");
            }
            // SAFETY: the range is mapped.
            unsafe { sys_icache_invalidate(ptr as *mut libc::c_void, len) };
            stats::note_fallback();
            Ok(())
        }

        fn unpublish_executable(&self, ptr: *const u8, len: usize) -> Result<()> {
            if self.owns(ptr as usize, len) {
                // Wasmtime calls this when the code is dropped. The mapping is
                // freed with this hook (module docs), so nothing to do.
                return Ok(());
            }
            // SAFETY: the inverse of the fallback in `publish_executable`.
            let rc = unsafe {
                libc::mprotect(
                    ptr as *mut libc::c_void,
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                wasmtime::bail!("unable to make memory read-write: {err}");
            }
            Ok(())
        }
    }

    /// Copy `bytes` into a `MAP_JIT` mapping and hand it to `raw`
    /// (`deserialize_raw`). `copy` is the stock loader, used when the artifact
    /// turns out to need no executable memory.
    ///
    /// # Safety
    /// `bytes` must be a trusted artifact for `engine`.
    unsafe fn load<T>(
        bytes: &[u8],
        raw: impl FnOnce(NonNull<[u8]>) -> Result<T>,
        copy: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let region = Region::with_contents(bytes)?;
        let base = region.base;
        let memory = NonNull::slice_from_raw_parts(
            NonNull::new(base as *mut u8).expect("mmap never returns null on success"),
            bytes.len(),
        );
        lock(&PENDING).push(region);
        stats::note_alloc(base);

        let result = raw(memory);

        match take_pending(|r| r.base == base) {
            // Claimed by the engine's publish hook: the engine owns it now.
            None => result,
            // Never published. On error, nothing references the mapping any
            // more. On success, the code needs no executable memory (an empty
            // text section): drop it, free the mapping, and load the stock way.
            Some(region) => match result {
                Err(e) => {
                    drop(region);
                    Err(e.context(
                        "loading Wasm code into MAP_JIT memory (was the engine \
                         built with jit_memory::configure?)",
                    ))
                }
                Ok(loaded) => {
                    drop(loaded);
                    drop(region);
                    copy()
                }
            },
        }
    }

    /// # Safety
    /// `bytes` must be a trusted component artifact for `engine`.
    pub(super) unsafe fn load_component(engine: &Engine, bytes: &[u8]) -> Result<Component> {
        unsafe {
            load(
                bytes,
                // SAFETY: the mapping outlives the component (it is owned by the
                // engine's hook once published, or freed here only after the
                // component is dropped) and is never written again.
                |memory| Component::deserialize_raw(engine, memory),
                || Component::deserialize(engine, bytes),
            )
        }
    }

    /// # Safety
    /// `bytes` must be a trusted module artifact for `engine`.
    pub(super) unsafe fn load_module(engine: &Engine, bytes: &[u8]) -> Result<Module> {
        unsafe {
            load(
                bytes,
                // SAFETY: as in `load_component`.
                |memory| Module::deserialize_raw(engine, memory),
                || Module::deserialize(engine, bytes),
            )
        }
    }

    /// Per-thread publish counters, for tests. Wasmtime publishes code on the
    /// thread that compiles or deserializes it.
    pub(super) mod stats {
        #[cfg(test)]
        thread_local! {
            static COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
            static LAST_ALLOC: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }

        pub(in super::super) fn note_alloc(_base: usize) {
            #[cfg(test)]
            LAST_ALLOC.with(|c| c.set(_base));
        }

        /// Whether the last mapping this thread allocated is still waiting to
        /// be published (it never should be once a load returns).
        #[cfg(test)]
        pub(in super::super) fn last_alloc_pending() -> bool {
            let base = LAST_ALLOC.with(|c| c.get());
            super::lock(&super::PENDING).iter().any(|r| r.base == base)
        }

        pub(in super::super) fn note_map_jit() {
            #[cfg(test)]
            COUNTS.with(|c| c.set((c.get().0 + 1, c.get().1)));
        }

        pub(in super::super) fn note_fallback() {
            #[cfg(test)]
            COUNTS.with(|c| c.set((c.get().0, c.get().1 + 1)));
        }

        /// `(published from MAP_JIT, published by the mprotect fallback)` on
        /// this thread so far.
        #[cfg(test)]
        pub(in super::super) fn get() -> (usize, usize) {
            COUNTS.with(|c| c.get())
        }
    }
}

#[cfg(all(test, target_os = "macos", feature = "compile"))]
mod tests {
    use super::macos::stats;
    use super::*;

    const ADD_WAT: &str = r#"(module
      (func (export "add") (param i32 i32) (result i32)
        local.get 0 local.get 1 i32.add))"#;

    const ADD_COMPONENT_WAT: &str = r#"(component
      (core module $m
        (func (export "add") (param i32 i32) (result i32)
          local.get 0 local.get 1 i32.add))
      (core instance $i (instantiate $m))
      (func (export "add") (param "a" s32) (param "b" s32) (result s32)
        (canon lift (core func $i "add"))))"#;

    fn engine() -> Engine {
        let mut cfg = Config::new();
        configure(&mut cfg);
        crate::limits::configure_engine(&mut cfg);
        Engine::new(&cfg).unwrap()
    }

    fn call_module(engine: &Engine, module: &Module) -> i32 {
        let mut store = wasmtime::Store::new(engine, ());
        store.set_fuel(1_000_000).unwrap();
        let instance = wasmtime::Instance::new(&mut store, module, &[]).unwrap();
        let add = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "add")
            .unwrap();
        add.call(&mut store, (40, 2)).unwrap()
    }

    fn call_component(engine: &Engine, component: &Component) -> i32 {
        let mut store = wasmtime::Store::new(engine, ());
        store.set_fuel(1_000_000).unwrap();
        let linker = wasmtime::component::Linker::new(engine);
        let instance = linker.instantiate(&mut store, component).unwrap();
        let add = instance
            .get_typed_func::<(i32, i32), (i32,)>(&mut store, "add")
            .unwrap();
        add.call(&mut store, (40, 2)).unwrap().0
    }

    /// Compiled and deserialized code lands in MAP_JIT memory, never in the
    /// mprotect fallback, and runs.
    #[test]
    fn modules_and_components_publish_from_map_jit() {
        let engine = engine();
        let before = stats::get();

        let module = compile_module(&engine, ADD_WAT.as_bytes()).unwrap();
        assert_eq!(call_module(&engine, &module), 42);

        let component = compile_component(&engine, ADD_COMPONENT_WAT.as_bytes()).unwrap();
        assert_eq!(call_component(&engine, &component), 42);

        let artifact = engine
            .precompile_component(ADD_COMPONENT_WAT.as_bytes())
            .unwrap();
        let loaded = unsafe { deserialize_component(&engine, &artifact) }.unwrap();
        assert_eq!(call_component(&engine, &loaded), 42);

        let artifact = engine.precompile_module(ADD_WAT.as_bytes()).unwrap();
        let loaded = unsafe { deserialize_module(&engine, &artifact) }.unwrap();
        assert_eq!(call_module(&engine, &loaded), 42);

        let after = stats::get();
        assert_eq!(after.0 - before.0, 4, "every load publishes from MAP_JIT");
        assert_eq!(after.1, before.1, "nothing used the mprotect fallback");
    }

    /// Code keeps working after the component handle and engine handle that
    /// loaded it are dropped, as long as a store still uses it: the mapping
    /// lives until the last user is gone.
    #[test]
    fn code_outlives_its_component_handle() {
        let engine = engine();
        let component = compile_component(&engine, ADD_COMPONENT_WAT.as_bytes()).unwrap();
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(1_000_000).unwrap();
        let linker = wasmtime::component::Linker::new(&engine);
        let instance = linker.instantiate(&mut store, &component).unwrap();
        drop(linker);
        drop(component);
        drop(engine);
        let add = instance
            .get_typed_func::<(i32, i32), (i32,)>(&mut store, "add")
            .unwrap();
        assert_eq!(add.call(&mut store, (1, 2)).unwrap().0, 3);
    }

    /// Loading many components on one engine and dropping engines repeatedly
    /// neither leaks pending mappings nor breaks later loads.
    #[test]
    fn repeated_loads_and_engine_drops() {
        for _ in 0..8 {
            let engine = engine();
            for _ in 0..4 {
                let component = compile_component(&engine, ADD_COMPONENT_WAT.as_bytes()).unwrap();
                assert_eq!(call_component(&engine, &component), 42);
            }
        }
    }

    /// Guest code runs on a different thread from the one that copied it in
    /// (the P3 worker shape): that thread is in the default "execute" state.
    #[test]
    fn runs_on_another_thread() {
        let engine = engine();
        let component = compile_component(&engine, ADD_COMPONENT_WAT.as_bytes()).unwrap();
        let got = std::thread::spawn(move || call_component(&engine, &component))
            .join()
            .unwrap();
        assert_eq!(got, 42);
    }

    /// A precompiled artifact from an engine with different settings is
    /// refused (Wasmtime's compatibility check), and the mapping is freed.
    #[test]
    fn incompatible_artifact_is_refused() {
        let other = Engine::default(); // no fuel → different compile settings
        let artifact = other
            .precompile_component(ADD_COMPONENT_WAT.as_bytes())
            .unwrap();
        let engine = engine();
        assert!(unsafe { deserialize_component(&engine, &artifact) }.is_err());
        assert!(
            !stats::last_alloc_pending(),
            "a refused load leaves no pending mapping"
        );
    }

    /// Pulley engines take the stock path (no executable code at all).
    #[test]
    fn pulley_takes_the_stock_path() {
        let mut cfg = Config::new();
        configure(&mut cfg);
        crate::limits::configure_engine(&mut cfg);
        cfg.target("pulley64").unwrap();
        let engine = Engine::new(&cfg).unwrap();
        let before = stats::get();
        let component = compile_component(&engine, ADD_COMPONENT_WAT.as_bytes()).unwrap();
        assert_eq!(call_component(&engine, &component), 42);
        assert_eq!(
            stats::get(),
            before,
            "Pulley code is never published as executable"
        );
    }

    /// Code built with plain `Component::new` on a configured engine still
    /// runs, via the stock mprotect fallback (outside the hardened runtime).
    #[test]
    fn stock_compile_uses_the_fallback() {
        let engine = engine();
        let before = stats::get();
        let component = Component::new(&engine, ADD_COMPONENT_WAT).unwrap();
        assert_eq!(call_component(&engine, &component), 42);
        let after = stats::get();
        assert_eq!(after, (before.0, before.1 + 1));
    }

    /// The C entrypoints the app binds load their code through MAP_JIT memory
    /// (each compiles or deserializes on the calling thread, so the per-thread
    /// counters see it).
    #[test]
    fn c_entrypoints_publish_from_map_jit() {
        let before = stats::get();
        assert_eq!(crate::hwr_eval_add(0, 2, 3), 5);
        assert_eq!(crate::hwr_eval_component_add(0, 2, 3), 5);
        unsafe {
            let eng = crate::hwr_engine_new(0);
            assert!(!eng.is_null());
            let wat = ADD_COMPONENT_WAT.as_bytes();
            let inst = crate::hwr_instance_new(eng, wat.as_ptr(), wat.len());
            assert!(!inst.is_null());
            assert_eq!(crate::hwr_instance_call_add(inst, 40, 2), 42);
            crate::hwr_instance_free(inst);

            let mut len = 0usize;
            let artifact = crate::hwr_precompile_component(eng, wat.as_ptr(), wat.len(), &mut len);
            assert!(!artifact.is_null());
            let inst = crate::hwr_instance_new_precompiled(eng, artifact, len);
            assert!(!inst.is_null());
            assert_eq!(crate::hwr_instance_call_add(inst, 40, 2), 42);
            crate::hwr_instance_free(inst);
            crate::hwr_free_bytes(artifact, len);
            crate::hwr_engine_free(eng);
        }
        #[cfg(feature = "typed-hosts")]
        unsafe {
            let plugin = include_bytes!("../tests/fixtures/hello_world_plugin.component.wasm");
            let s = crate::hwr_p3_start_plugin_compile(
                0,
                plugin.as_ptr(),
                plugin.len(),
                std::ptr::null(),
                0,
            );
            assert!(!s.is_null());
            // Drive it to the end (answering every host call) before freeing.
            loop {
                match crate::hwr_p3_poll(s) {
                    crate::HWR_P3_PENDING => {
                        let resp = br#"{"ok":true,"data":[]}"#;
                        crate::hwr_p3_resolve(s, resp.as_ptr(), resp.len());
                    }
                    status => {
                        assert_eq!(status, crate::HWR_P3_DONE);
                        break;
                    }
                }
            }
            crate::hwr_p3_free(s);
        }
        let after = stats::get();
        // eval_add + eval_component_add + instance_new + instance_new_precompiled
        // (+ the plugin with typed-hosts).
        let expected = if cfg!(feature = "typed-hosts") { 5 } else { 4 };
        assert_eq!(after.0 - before.0, expected);
        assert_eq!(after.1, before.1, "nothing used the mprotect fallback");
    }

    /// Fuel still bounds a guest loaded through MAP_JIT memory.
    #[test]
    fn fuel_still_traps() {
        let engine = engine();
        let module = compile_module(
            &engine,
            br#"(module (func (export "spin") (loop $l (br $l))))"#,
        )
        .unwrap();
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(10_000).unwrap();
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .unwrap();
        let err = spin.call(&mut store, ()).unwrap_err();
        assert_eq!(
            crate::limits::classify(&format!("{err:?}")),
            Some(crate::limits::ERR_CPU_LIMIT)
        );
    }
}
