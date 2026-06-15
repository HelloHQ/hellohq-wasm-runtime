# ios-bench — WebAssembly-runtime latency XCTest harness

A SwiftPM package whose XCTest target (`RuntimeBenchTests`) runs the no-JIT
(Pulley) WebAssembly runtime and times `add(n, 0)` calls. It is the Swift port of
`examples/bench_ios.rs`:

1. `hwr_self_test() == 1` (runtime links + initialises),
2. `hwr_engine_new(1 /* pulley */)`,
3. `hwr_instance_new_precompiled(engine, bytes)` — deserialize the precompiled
   Pulley artifact `compute_pulley.cwasm` (no Cranelift, no JIT on device),
4. functional assert `add(10000, 0) == -1847199397` (and `!= HWR_CALL_ERROR`),
5. timed loops for `n = 10k / 100k / 1M`, printing `ms/call`.

The runtime is the static `HelloHQWasmRuntime.xcframework` (built by
`scripts/build-release.sh`, `--no-default-features`), wrapped as a
`.binaryTarget`. It is imported from Swift via the Clang module map shipped in
the xcframework (`Headers/module.modulemap`): `import HelloHQWasmRuntime`.

## Prerequisites

The package depends on `../dist/HelloHQWasmRuntime.xcframework` (relative path).
Build it first if it is missing or stale:

```sh
# from the crate root (one dir up)
bash scripts/build-release.sh        # recompiles wasmtime for iOS targets — minutes
```

The artifact `Tests/RuntimeBenchTests/Resources/compute_pulley.cwasm` is already
committed; regenerate it on a host with Cranelift via
`cargo run --release --example gen_bench_artifact` if the ABI changes.

## Run on the iOS Simulator (functional + host-speed)

The booted simulator used during development is **iPhone 16 Pro**, udid
`247B237C-09B3-4976-9DD3-9613D920D908`.

```sh
cd ios-bench
xcodebuild test \
  -scheme RuntimeBench-Package \
  -destination 'platform=iOS Simulator,id=247B237C-09B3-4976-9DD3-9613D920D908'
```

Or target by name instead of udid:

```sh
xcodebuild test -scheme RuntimeBench-Package \
  -destination 'platform=iOS Simulator,name=iPhone 16 Pro'
```

No code signing is required for the Simulator (the test bundle is signed
"to run locally" automatically).

> **The Simulator runs on the Mac (host) CPU.** Its `ms/call` numbers are a
> FUNCTIONAL pass + host-speed measurement — they are **NOT** an A-series
> (iPhone/iPad SoC) measurement. Use the Simulator only to confirm correctness
> and that the runtime links/initialises. The real latency gate is the
> on-device run below.

## Run on a physical device (the real latency gate)

A tethered iPhone/iPad gives the actual A-series number. Unlike the Simulator,
the device build must be **code signed**, so you must supply a development team.

1. Find the connected device name (or udid):

   ```sh
   xcrun xctrace list devices
   ```

2. Run, passing your Apple Developer team id (`DEVELOPMENT_TEAM`) and letting
   Xcode manage signing:

   ```sh
   cd ios-bench
   xcodebuild test \
     -scheme RuntimeBench-Package \
     -destination 'platform=iOS,name=<Your Device Name>' \
     -allowProvisioningUpdates \
     CODE_SIGN_STYLE=Automatic \
     DEVELOPMENT_TEAM=<YOUR_TEAM_ID>
   ```

   Find your team id with:

   ```sh
   security find-identity -p codesigning -v   # "Apple Development: … (TEAMID)"
   ```

   Alternatively, open the package in Xcode (`xed .` from `ios-bench/`), select
   the `RuntimeBenchTests` target → Signing & Capabilities → check
   *Automatically manage signing* and pick your Team, then run the test
   (Cmd-U). This is the simplest way to satisfy the device signing requirement.

> Signing (`DEVELOPMENT_TEAM` / a selected team) is required **only for the
> physical device**, not for the Simulator.

## Expected output (Simulator, host CPU — illustrative, not a device number)

```
hellohq-wasm-runtime XCTest bench  (abi v1)
self_test: OK  (no-JIT runtime links + initialises under iOS)
functional: precompiled component ran under iOS, add(10000,0)=-1847199397

latency (Pulley, no-JIT):
  light  (n=10k)      0.2370 ms/call
  medium (n=100k)     2.2514 ms/call
  heavy  (n=1M)      23.2001 ms/call

OK
** TEST SUCCEEDED **
```

There are two test cases:
- `testRuntimeLatencyBenchmark` — manual `DispatchTime` timing, prints the lines
  above.
- `testRuntimeLatencyMeasured` — wraps the light workload in XCTest `measure {}`
  so the result shows up as a performance metric in the `.xcresult` bundle.
