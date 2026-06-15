// SPDX-License-Identifier: Apache-2.0
//
// The no-JIT iOS execution path, runnable inside an iOS Simulator (and the basis
// for the on-device XCTest). Uses ONLY the always-available handle C ABI (no
// Cranelift, no `wat`) — `hwr_engine_new(pulley)` → `hwr_instance_new_precompiled`
// (deserialize the artifact from `gen_bench_artifact`) → timed `hwr_instance_call_add`.
//
// Build (iOS Simulator, no-JIT) + run via simctl:
//   cargo run --example gen_bench_artifact --release            # once, on host
//   cargo build --release --no-default-features --features ios-bench \
//       --example bench_ios --target aarch64-apple-ios-sim
//   xcrun simctl boot "iPhone 16" 2>/dev/null; \
//   xcrun simctl spawn booted \
//       target/aarch64-apple-ios-sim/release/examples/bench_ios
//
// NOTE: the Simulator runs on the host (Mac) CPU, so the timings here are a
// FUNCTIONAL pass + host-speed numbers — NOT an A-series measurement. The real
// device number comes from the XCTest harness on a tethered iPhone/iPad.

#[cfg(feature = "ios-bench")]
fn main() {
    use hellohq_wasm_runtime::{
        hwr_abi_version, hwr_engine_free, hwr_engine_new, hwr_instance_call_add,
        hwr_instance_free, hwr_instance_new_precompiled, hwr_self_test,
    };
    use std::time::Instant;

    // The Pulley AOT artifact precompiled off-device by `gen_bench_artifact`.
    const ARTIFACT: &[u8] = include_bytes!("../ios-bench/Resources/compute_pulley.cwasm");

    // Sentinel returned by the call ABI on error (i64::MIN).
    const ERR: i64 = i64::MIN;

    println!("hellohq-wasm-runtime bench_ios  (abi v{})", hwr_abi_version());
    assert_eq!(hwr_self_test(), 1, "self_test failed — runtime did not init");
    println!("self_test: OK  (no-JIT runtime links + initialises under iOS)");

    unsafe {
        let engine = hwr_engine_new(1 /* pulley */);
        assert!(!engine.is_null(), "engine_new returned null");
        let inst =
            hwr_instance_new_precompiled(engine, ARTIFACT.as_ptr(), ARTIFACT.len());
        assert!(
            !inst.is_null(),
            "deserialize failed — host-precompiled artifact rejected by no-JIT runtime"
        );

        // ── Functional check ─────────────────────────────────────────────────
        let r = hwr_instance_call_add(inst, 10_000, 0);
        assert_ne!(r, ERR, "call_add errored");
        println!("functional: precompiled component ran under iOS, add(10000,0)={r}");

        // ── Latency ──────────────────────────────────────────────────────────
        println!("\nlatency (Pulley, no-JIT):");
        for (label, n, iters) in
            [("light  (n=10k)", 10_000, 2000), ("medium (n=100k)", 100_000, 500), ("heavy  (n=1M)", 1_000_000, 100)]
        {
            // warm up
            let _ = hwr_instance_call_add(inst, n, 0);
            let t = Instant::now();
            let mut last = 0;
            for _ in 0..iters {
                last = hwr_instance_call_add(inst, n, 0);
            }
            let ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
            assert_ne!(last, ERR);
            println!("  {label:<16} {ms:>9.4} ms/call");
        }

        hwr_instance_free(inst);
        hwr_engine_free(engine);
    }
    println!("\nOK");
}

#[cfg(not(feature = "ios-bench"))]
fn main() {
    eprintln!("build with --features ios-bench (run gen_bench_artifact first)");
}
