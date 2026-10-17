// SPDX-License-Identifier: Apache-2.0
//
//! macOS hardened-runtime check (driven by `scripts/macos-hardened-runtime-check.sh`).
//!
//! The shipped macOS app runs with the hardened runtime and only the
//! `com.apple.security.cs.allow-jit` entitlement. That entitlement covers
//! `MAP_JIT` mappings and nothing else: executing JIT code from any other
//! mapping kills the process with `SIGKILL (Code Signature Invalid)`. This
//! binary runs real Cranelift-compiled guest code through the same C ABI the
//! app binds, so the script can codesign it with exactly the app's JIT
//! entitlement and prove the runtime's code memory is `MAP_JIT`.
//!
//! It runs, in order, each printing one `ok:` line:
//! 1. `hwr_eval_add` — a core module compiled in-process (Cranelift).
//! 2. `hwr_p3_start_plugin_compile` — the real hello-world SDK plugin, compiled
//!    in-process; the app's desktop Tier-2 entrypoint.
//! 3. `hwr_precompile_component` + `hwr_p3_start_plugin` — the same plugin as a
//!    native precompiled artifact, deserialized (the load path, not compile).
//!
//! On success it prints `hardened-runtime-check: PASS` and exits 0. A runtime
//! refusal (null handle / wrong result) exits 1. A kill by the kernel shows up
//! to the caller as SIGKILL (exit status 137 in a shell).

use hellohq_wasm_runtime::{
    hwr_engine_free, hwr_engine_new, hwr_eval_add, hwr_free_bytes, hwr_p3_free, hwr_p3_poll,
    hwr_p3_request_len, hwr_p3_request_ptr, hwr_p3_resolve, hwr_p3_result_len, hwr_p3_result_ptr,
    hwr_p3_start_plugin, hwr_p3_start_plugin_compile, hwr_precompile_component, HwrP3Session,
    HWR_P3_DONE, HWR_P3_ERROR, HWR_P3_PENDING,
};

/// hello-world v1.0.1 from HelloHQ/plugin-sdk (sha256 pinned in the script).
const PLUGIN_WASM: &[u8] = include_bytes!("../tests/fixtures/hello_world_plugin.component.wasm");

/// The text the hello-world document carries for the two canned portfolios.
const EXPECTED: &str = "You have 2 portfolios in this workspace.";

fn fail(msg: &str) -> ! {
    eprintln!("hardened-runtime-check: FAIL: {msg}");
    #[cfg(target_os = "macos")]
    eprintln!("hardened-runtime-check: {}", map_jit_probe());
    std::process::exit(1);
}

/// The C ABI reports failures as null/sentinel values, so say whether this
/// process may map `MAP_JIT` memory at all (it may not without allow-jit under
/// the hardened runtime).
#[cfg(target_os = "macos")]
fn map_jit_probe() -> String {
    // SAFETY: a fresh anonymous mapping, unmapped straight away.
    unsafe {
        let len = 16 * 1024;
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            format!(
                "mmap(MAP_JIT) in this process: refused ({})",
                std::io::Error::last_os_error()
            )
        } else {
            libc::munmap(p, len);
            "mmap(MAP_JIT) in this process: allowed".to_string()
        }
    }
}

/// Answer the plugin's host calls the way the app does for a granted run.
fn service(req: &[u8]) -> Vec<u8> {
    let v: serde_json::Value = serde_json::from_slice(req).unwrap_or_default();
    match v.get("method").and_then(serde_json::Value::as_str) {
        Some("read:portfolio_names") => {
            br#"{"ok":true,"data":[{"id":"p1","name":"Personal"},{"id":"p2","name":"Business"}]}"#
                .to_vec()
        }
        _ => br#"{"ok":true}"#.to_vec(),
    }
}

/// Drive a P3 session to completion, servicing host calls; returns the output.
unsafe fn drive(session: *mut HwrP3Session, what: &str) -> String {
    if session.is_null() {
        fail(&format!(
            "{what}: the runtime refused to start the plugin (null session)"
        ));
    }
    let status = loop {
        match hwr_p3_poll(session) {
            HWR_P3_PENDING => {
                let req = std::slice::from_raw_parts(
                    hwr_p3_request_ptr(session),
                    hwr_p3_request_len(session),
                )
                .to_vec();
                let resp = service(&req);
                hwr_p3_resolve(session, resp.as_ptr(), resp.len());
            }
            s @ (HWR_P3_DONE | HWR_P3_ERROR) => break s,
            other => fail(&format!("{what}: unexpected poll status {other}")),
        }
    };
    let out = String::from_utf8_lossy(std::slice::from_raw_parts(
        hwr_p3_result_ptr(session),
        hwr_p3_result_len(session),
    ))
    .into_owned();
    hwr_p3_free(session);
    if status != HWR_P3_DONE {
        fail(&format!("{what}: run errored: {out}"));
    }
    if !out.contains(EXPECTED) {
        fail(&format!("{what}: unexpected output: {out}"));
    }
    out
}

fn main() {
    // 1. Core module, Cranelift, compiled in-process.
    let sum = hwr_eval_add(0, 2, 3);
    if sum != 5 {
        fail(&format!("hwr_eval_add(2, 3) returned {sum}"));
    }
    println!("ok: hwr_eval_add (Cranelift core module) = {sum}");

    unsafe {
        // 2. The real plugin, compiled in-process (the app's desktop path).
        let out = drive(
            hwr_p3_start_plugin_compile(
                0,
                PLUGIN_WASM.as_ptr(),
                PLUGIN_WASM.len(),
                std::ptr::null(),
                0,
            ),
            "hwr_p3_start_plugin_compile",
        );
        println!("ok: hwr_p3_start_plugin_compile (hello-world, Cranelift) -> {out}");

        // 3. The same plugin as a precompiled native artifact, deserialized.
        let engine = hwr_engine_new(0);
        if engine.is_null() {
            fail("hwr_engine_new(0) returned null");
        }
        let mut len = 0usize;
        let artifact =
            hwr_precompile_component(engine, PLUGIN_WASM.as_ptr(), PLUGIN_WASM.len(), &mut len);
        if artifact.is_null() {
            fail("hwr_precompile_component returned null");
        }
        let out = drive(
            hwr_p3_start_plugin(0, artifact, len, std::ptr::null(), 0),
            "hwr_p3_start_plugin",
        );
        hwr_free_bytes(artifact, len);
        hwr_engine_free(engine);
        println!("ok: hwr_p3_start_plugin (hello-world, precompiled native artifact) -> {out}");
    }

    // Prove the code above ran from MAP_JIT memory. On a host that enforces the
    // hardened runtime the fallback could not have run at all, but on one that
    // does not (GitHub's macOS VMs) it would, so count where the runtime
    // published its code: every run must have used MAP_JIT and none the
    // mprotect fallback.
    #[cfg(target_os = "macos")]
    {
        let (map_jit, fallback) = hellohq_wasm_runtime::jit_memory::publish_counts();
        println!("code memory: {map_jit} published from MAP_JIT, {fallback} by the fallback");
        println!("{}", map_jit_probe());
        if map_jit == 0 || fallback != 0 {
            fail("the runtime's code was not all published from MAP_JIT memory");
        }
    }

    println!("hardened-runtime-check: PASS");
}
