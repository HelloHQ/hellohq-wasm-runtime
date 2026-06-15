// SPDX-License-Identifier: Apache-2.0
//
// Host-side generator: precompiles the compute-benchmark component to a Pulley
// AOT artifact (`Component::serialize`) — the exact iOS shipping model (compile
// with Cranelift in CI, ship bytecode, deserialize on-device with no JIT). The
// artifact is consumed by `examples/bench_ios.rs` and the XCTest harness via the
// always-available `hwr_instance_new_precompiled` deserialize path.
//
// Run: cargo run --release --example gen_bench_artifact
//
// The component exports `add(s32, s32) -> s32` (so it reuses the existing
// `hwr_instance_call_add` C ABI): it runs a tight compute loop of `a` iterations
// and returns the accumulator's low 32 bits (`b` is ignored). Same workload as
// `examples/pulley_latency.rs`, so host and device numbers are comparable.

#[cfg(feature = "compile")]
fn main() {
    use wasmtime::{component::Component, Config, Engine};

    const COMPUTE_COMPONENT_WAT: &str = r#"(component
  (core module $m
    (func (export "add") (param $n i32) (param $unused i32) (result i32)
      (local $i i32) (local $acc i32)
      (block $done
        (loop $loop
          (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
          (local.set $acc
            (i32.xor
              (i32.add (local.get $acc)
                (i32.mul (local.get $i) (i32.const 2654435761)))
              (i32.rotl (local.get $acc) (i32.const 7))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $loop)))
      (local.get $acc)))
  (core instance $i (instantiate $m))
  (func (export "add") (param "a" s32) (param "b" s32) (result s32)
    (canon lift (core func $i "add"))))"#;

    // Target pulley64 — the no-JIT bytecode iOS deserializes. Cranelift (compile
    // feature) does the compilation here, off-device.
    let mut cfg = Config::new();
    cfg.target("pulley64").expect("pulley64 target");
    let engine = Engine::new(&cfg).expect("engine");
    let component = Component::new(&engine, COMPUTE_COMPONENT_WAT).expect("compile");
    let bytes = component.serialize().expect("serialize");

    let out = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/ios-bench/Resources/compute_pulley.cwasm"
    );
    std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/ios-bench/Resources"))
        .expect("mkdir");
    std::fs::write(out, &bytes).expect("write artifact");
    println!("wrote {} bytes -> {out}", bytes.len());
}

#[cfg(not(feature = "compile"))]
fn main() {
    eprintln!("gen_bench_artifact needs the `compile` feature (Cranelift). Run on host.");
}
