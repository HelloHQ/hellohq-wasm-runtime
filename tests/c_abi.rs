//! Integration tests that exercise the **public C ABI** the way a consumer
//! (Dart FFI / dlopen) does — through the exported `extern "C"` entrypoints,
//! not the private Rust helpers. Linked against the crate's `rlib`.
//!
//! These cover the boundary the unit tests can't: engine + instance **handle
//! lifecycle**, the **precompile → no-JIT deserialize** path, null-pointer
//! safety, and the one-shot eval/async entrypoints on both backends (Cranelift
//! and Pulley). The `compile`-gated tests need in-process compilation (default
//! features); the rest also run under `--no-default-features` (the iOS no-JIT
//! build), proving the runtime-only library links and basic ABI works there too.

use hellohq_wasm_runtime::*;

/// A minimal Wasm **component** exporting `add(a: s32, b: s32) -> s32` — the
/// proof vehicle for the handle-lifecycle + precompile paths. (Copy of the
/// crate's internal constant; integration tests can't see private items.)
#[cfg(feature = "compile")]
const ADD_COMPONENT_WAT: &str = r#"(component
  (core module $m
    (func (export "add") (param i32 i32) (result i32)
      (i32.add (local.get 0) (local.get 1))))
  (core instance $i (instantiate $m))
  (func (export "add") (param "a" s32) (param "b" s32) (result s32)
    (canon lift (core func $i "add"))))"#;

#[test]
fn abi_version_is_nonzero() {
    assert!(hwr_abi_version() >= 1);
}

#[test]
fn self_test_passes() {
    assert_eq!(hwr_self_test(), 1);
}

#[test]
fn engine_lifecycle_both_backends() {
    for use_pulley in [0, 1] {
        let eng = hwr_engine_new(use_pulley);
        assert!(
            !eng.is_null(),
            "engine_new failed (use_pulley={use_pulley})"
        );
        unsafe { hwr_engine_free(eng) };
    }
}

#[test]
fn null_handles_are_safe() {
    unsafe {
        // Freeing null is a no-op; calling on a null instance returns the error
        // sentinel rather than dereferencing.
        hwr_engine_free(std::ptr::null_mut());
        hwr_instance_free(std::ptr::null_mut());
        assert_eq!(hwr_instance_call_add(std::ptr::null_mut(), 1, 2), i64::MIN);
        hwr_free_bytes(std::ptr::null_mut(), 0);
    }
}

#[cfg(feature = "compile")]
#[test]
fn eval_entrypoints_both_backends() {
    for use_pulley in [0, 1] {
        assert_eq!(hwr_eval_add(use_pulley, 2, 3), 5);
        assert_eq!(hwr_eval_component_add(use_pulley, 20, 22), 42);
        assert_eq!(hwr_eval_host_import(use_pulley, 21), 42);
        assert_eq!(hwr_run_async_double(use_pulley, 21), 42);
        assert_eq!(hwr_run_component_async_double(use_pulley, 21), 42);
        assert_eq!(hwr_run_canonical_async_double(use_pulley, 21), 42);
    }
}

#[cfg(feature = "compile")]
#[test]
fn handle_lifecycle_call_add() {
    let wat = ADD_COMPONENT_WAT.as_bytes();
    unsafe {
        let eng = hwr_engine_new(0);
        assert!(!eng.is_null());
        let inst = hwr_instance_new(eng, wat.as_ptr(), wat.len());
        assert!(!inst.is_null(), "instance_new failed");
        assert_eq!(hwr_instance_call_add(inst, 40, 2), 42);
        hwr_instance_free(inst);
        hwr_engine_free(eng);
    }
}

/// The iOS no-JIT path: precompile a component with Cranelift, then instantiate
/// it from the serialized artifact (which needs no compiler) and run it.
#[cfg(feature = "compile")]
#[test]
fn precompile_then_run_precompiled() {
    let wat = ADD_COMPONENT_WAT.as_bytes();
    unsafe {
        let eng = hwr_engine_new(0);
        assert!(!eng.is_null());

        let mut out_len: usize = 0;
        let artifact = hwr_precompile_component(eng, wat.as_ptr(), wat.len(), &mut out_len);
        assert!(!artifact.is_null() && out_len > 0, "precompile failed");

        let inst = hwr_instance_new_precompiled(eng, artifact, out_len);
        assert!(!inst.is_null(), "instance_new_precompiled failed");
        assert_eq!(hwr_instance_call_add(inst, 19, 23), 42);

        hwr_instance_free(inst);
        hwr_free_bytes(artifact, out_len);
        hwr_engine_free(eng);
    }
}
