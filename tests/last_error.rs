// SPDX-License-Identifier: Apache-2.0
//
//! `hwr_last_error` through the C ABI (#30): a start or load call that returns
//! null says why, with a stable prefix and no plugin-supplied text, and a call
//! that succeeds leaves no stale reason behind.
//!
//! The macOS `MAP_JIT` refusal itself needs a hardened-runtime binary without
//! the allow-jit entitlement; `scripts/macos-hardened-runtime-check.sh` checks
//! that case through this same accessor.
#![cfg(feature = "compile")]

use std::ffi::CStr;

use hellohq_wasm_runtime::last_error::{
    ERR_BAD_ARGUMENT, ERR_INCOMPATIBLE_ARTIFACT, ERR_INSTANTIATE, ERR_INVALID_PLUGIN,
};
use hellohq_wasm_runtime::{
    hwr_engine_free, hwr_engine_new, hwr_free_bytes, hwr_instance_free, hwr_instance_new,
    hwr_instance_new_precompiled, hwr_last_error, hwr_p3_free, hwr_p3_start, hwr_p3_start_compile,
    hwr_precompile_component,
};

fn last_error() -> Option<String> {
    let p = hwr_last_error();
    if p.is_null() {
        None
    } else {
        // SAFETY: NUL-terminated, owned by the runtime, read before any other call.
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

fn expect_prefix(what: &str, prefix: &str) -> String {
    let msg = last_error().unwrap_or_else(|| panic!("{what}: no last error"));
    assert!(
        msg.starts_with(&format!("{prefix}: ")),
        "{what}: expected {prefix}, got {msg}"
    );
    msg
}

const ADD_COMPONENT: &str = r#"(component
  (core module $m (func (export "add") (param i32 i32) (result i32)
    local.get 0 local.get 1 i32.add))
  (core instance $i (instantiate $m))
  (func (export "add") (param "a" s32) (param "b" s32) (result s32)
    (canon lift (core func $i "add"))))"#;

#[test]
fn a_component_that_does_not_compile_says_so_on_both_backends() {
    for use_pulley in [0, 1] {
        let bytes = b"\0asm garbage from a plugin called SECRET-PLUGIN";
        // SAFETY: valid buffers for the call.
        let s = unsafe {
            hwr_p3_start_compile(use_pulley, bytes.as_ptr(), bytes.len(), std::ptr::null(), 0)
        };
        assert!(s.is_null());
        let msg = expect_prefix("compile", ERR_INVALID_PLUGIN);
        assert!(!msg.contains("SECRET"), "{msg}");
    }
}

#[test]
fn an_artifact_from_elsewhere_says_it_does_not_match() {
    let bytes = b"not an artifact";
    // SAFETY: valid buffers; the runtime rejects the header before trusting it.
    let s = unsafe { hwr_p3_start(0, bytes.as_ptr(), bytes.len(), std::ptr::null(), 0) };
    assert!(s.is_null());
    expect_prefix("deserialize", ERR_INCOMPATIBLE_ARTIFACT);
}

#[test]
fn a_null_component_is_a_bad_argument() {
    // SAFETY: a null component is the case under test.
    let s = unsafe { hwr_p3_start_compile(0, std::ptr::null(), 0, std::ptr::null(), 0) };
    assert!(s.is_null());
    expect_prefix("null", ERR_BAD_ARGUMENT);
}

#[test]
fn an_unmet_import_names_no_plugin_text() {
    let wat =
        r#"(component (import "secret-plugin-name:pkg/iface" (instance (export "f" (func)))))"#;
    let eng = hwr_engine_new(0);
    assert!(!eng.is_null());
    // SAFETY: live engine, valid buffer.
    let inst = unsafe { hwr_instance_new(eng, wat.as_ptr(), wat.len()) };
    assert!(inst.is_null());
    let msg = expect_prefix("instantiate", ERR_INSTANTIATE);
    assert!(!msg.contains("secret-plugin-name"), "{msg}");
    unsafe { hwr_engine_free(eng) };
}

#[test]
fn a_successful_call_clears_the_previous_reason() {
    let bytes = b"not a component";
    // SAFETY: valid buffers for every call below.
    unsafe {
        assert!(
            hwr_p3_start_compile(0, bytes.as_ptr(), bytes.len(), std::ptr::null(), 0).is_null()
        );
        assert!(last_error().is_some());

        let eng = hwr_engine_new(0);
        assert!(!eng.is_null());
        assert_eq!(last_error(), None, "engine_new left a stale reason");

        let inst = hwr_instance_new(eng, ADD_COMPONENT.as_ptr(), ADD_COMPONENT.len());
        assert!(!inst.is_null(), "{:?}", last_error());
        assert_eq!(last_error(), None);
        hwr_instance_free(inst);

        let mut len = 0usize;
        let art =
            hwr_precompile_component(eng, ADD_COMPONENT.as_ptr(), ADD_COMPONENT.len(), &mut len);
        assert!(!art.is_null(), "{:?}", last_error());
        assert_eq!(last_error(), None);
        let inst = hwr_instance_new_precompiled(eng, art, len);
        assert!(!inst.is_null(), "{:?}", last_error());
        assert_eq!(last_error(), None);
        hwr_instance_free(inst);
        hwr_free_bytes(art, len);

        assert!(
            hwr_p3_start_compile(0, bytes.as_ptr(), bytes.len(), std::ptr::null(), 0).is_null()
        );
        let wat = b"(component)";
        let s = hwr_p3_start_compile(0, wat.as_ptr(), wat.len(), std::ptr::null(), 0);
        assert!(!s.is_null(), "{:?}", last_error());
        assert_eq!(last_error(), None, "a started session left a stale reason");
        hwr_p3_free(s);
        hwr_engine_free(eng);
    }
}

#[test]
fn precompiling_garbage_says_so() {
    let eng = hwr_engine_new(1);
    assert!(!eng.is_null());
    let mut len = 0usize;
    let bytes = b"garbage";
    // SAFETY: live engine, valid buffers.
    let art = unsafe { hwr_precompile_component(eng, bytes.as_ptr(), bytes.len(), &mut len) };
    assert!(art.is_null());
    expect_prefix("precompile", ERR_INVALID_PLUGIN);
    unsafe { hwr_engine_free(eng) };
}
