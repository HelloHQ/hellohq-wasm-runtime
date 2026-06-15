// SPDX-License-Identifier: Apache-2.0
//
// Latency proxy for the doc-52 "Pulley on iOS" gate. Measures the
// device-independent half of the risk: how much slower the Pulley portable
// interpreter (the iOS / no-JIT backend) is than the Cranelift JIT
// (desktop/Android) for a compute-bound workload, on this host.
//
// The ABSOLUTE numbers are host numbers, not device numbers — an A-series
// core is slower than this Mac, so scale accordingly. The RATIO and rough
// per-call magnitude are the portable signal: if Pulley is, say, ~20x slower
// but a representative plugin call is still sub-millisecond, the gate is
// comfortable; if the interpreter blows a plugin into tens of ms, it is not.
//
// Run: cargo run --release --example pulley_latency

use std::time::Instant;
use wasmtime::{Config, Engine, Instance, Module, Store};

// A tight i64 compute loop with a multiply + accumulate — a stand-in for the
// hot path of a real declarative plugin (numeric transforms over workspace
// data). No host calls, so this isolates pure execution-tier cost.
const WAT: &str = r#"
(module
  (func (export "run") (param $n i64) (result i64)
    (local $i i64) (local $acc i64)
    (local.set $acc (i64.const 0))
    (local.set $i  (i64.const 0))
    (block $done
      (loop $loop
        (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
        (local.set $acc
          (i64.xor
            (i64.add (local.get $acc)
              (i64.mul (local.get $i) (i64.const 2654435761)))
            (i64.rotl (local.get $acc) (i64.const 7))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $loop)))
    (local.get $acc)))
"#;

fn bench(use_pulley: bool, n: i64, iters: u32) -> (f64, i64) {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64").expect("pulley64 target");
    }
    let engine = Engine::new(&cfg).expect("engine");
    let module = Module::new(&engine, WAT).expect("compile");
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let run = instance
        .get_typed_func::<i64, i64>(&mut store, "run")
        .expect("run export");

    // Warm up (touch code paths, page in).
    let mut last = run.call(&mut store, n).expect("call");
    let t = Instant::now();
    for _ in 0..iters {
        last = run.call(&mut store, n).expect("call");
    }
    let ms_per_call = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
    (ms_per_call, last)
}

fn row(label: &str, n: i64, iters: u32) {
    let (cl, r1) = bench(false, n, iters);
    let (pl, r2) = bench(true, n, iters);
    assert_eq!(r1, r2, "backends disagree — interpreter bug");
    println!(
        "  {label:<28} cranelift {cl:>8.4} ms   pulley {pl:>8.4} ms   ratio {:>5.1}x",
        pl / cl
    );
}

fn main() {
    println!("Pulley (no-JIT, iOS backend) vs Cranelift JIT — host latency proxy");
    println!("workload: tight i64 mul/xor/rotl accumulate loop\n");
    // A spread of loop sizes ~ a trivial call up to a heavy compute plugin.
    row("light   (n=10k)", 10_000, 2000);
    row("medium  (n=100k)", 100_000, 500);
    row("heavy   (n=1M)", 1_000_000, 100);
    println!(
        "\nnote: host numbers. Device (A-series) is slower in absolute terms;\n\
         the ratio is the portable signal. See docs/52 gate."
    );
}
