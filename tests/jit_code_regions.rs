// SPDX-License-Identifier: Apache-2.0
//
//! #29: on macOS, a long-lived engine that loads and drops code keeps a bounded
//! number of `MAP_JIT` mappings, not one per load, and frees the rest with the
//! engine. Its own test binary (one test) so the process-wide mapping count
//! (`jit_memory::live_code_regions`) sees no other test's loads.
#![cfg(all(target_os = "macos", feature = "compile"))]

use hellohq_wasm_runtime::jit_memory::live_code_regions;
use hellohq_wasm_runtime::{
    hwr_engine_free, hwr_engine_new, hwr_free_bytes, hwr_instance_call_add, hwr_instance_free,
    hwr_instance_new, hwr_instance_new_precompiled, hwr_precompile_component,
};

const ADD_COMPONENT: &str = r#"(component
  (core module $m (func (export "add") (param i32 i32) (result i32)
    local.get 0 local.get 1 i32.add))
  (core instance $i (instantiate $m))
  (func (export "add") (param "a" s32) (param "b" s32) (result s32)
    (canon lift (core func $i "add"))))"#;

const LOADS: usize = 40;

#[test]
fn repeated_loads_on_one_engine_keep_mappings_bounded() {
    let before = live_code_regions();
    let eng = hwr_engine_new(0);
    assert!(!eng.is_null());
    let mut peak = 0;
    // SAFETY: live engine and valid buffers throughout.
    unsafe {
        let mut len = 0usize;
        let art =
            hwr_precompile_component(eng, ADD_COMPONENT.as_ptr(), ADD_COMPONENT.len(), &mut len);
        assert!(!art.is_null());

        for i in 0..LOADS {
            // Compile path and precompiled path, alternately.
            let inst = if i % 2 == 0 {
                hwr_instance_new(eng, ADD_COMPONENT.as_ptr(), ADD_COMPONENT.len())
            } else {
                hwr_instance_new_precompiled(eng, art, len)
            };
            assert!(!inst.is_null());
            assert_eq!(hwr_instance_call_add(inst, 40, 2), 42);
            hwr_instance_free(inst);
            peak = peak.max(live_code_regions() - before);
        }

        // Drop on another thread: that thread retires its mapping and never
        // calls in again, so the engine frees it.
        let inst = hwr_instance_new(eng, ADD_COMPONENT.as_ptr(), ADD_COMPONENT.len());
        assert!(!inst.is_null());
        let send = inst as usize;
        std::thread::spawn(move || hwr_instance_free(send as *mut _))
            .join()
            .unwrap();

        hwr_free_bytes(art, len);
        hwr_engine_free(eng);
    }
    // One live load plus at most one retired mapping on this thread.
    assert!(
        peak <= 2,
        "{peak} MAP_JIT mappings live at once over {LOADS} loads on one engine"
    );
    assert_eq!(
        live_code_regions(),
        before,
        "freeing the engine left MAP_JIT mappings behind"
    );
}
