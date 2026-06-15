// SPDX-License-Identifier: Apache-2.0
//
// iOS XCTest latency harness for the no-JIT (Pulley) WebAssembly runtime.
// Mirrors examples/bench_ios.rs exactly:
//   self_test == 1 -> engine_new(1 = pulley) -> instance_new_precompiled(artifact)
//   -> functional add(10000, 0) assert -> timed loops for n = 10k / 100k / 1M.
//
// Runs on:
//   - iOS Simulator: functional + HOST-CPU (Mac) latency — NOT an A-series number.
//   - Physical device: the real on-device latency gate (needs code signing).
//
// The runtime is the static xcframework HelloHQWasmRuntime, imported as a Clang
// module via its bundled module.modulemap.

import XCTest
import HelloHQWasmRuntime

final class RuntimeBenchTests: XCTestCase {

    /// Sentinel returned by the i64 call ABI on error (== INT64_MIN / HWR_CALL_ERROR).
    private let callError = Int64.min

    /// Reference value of add(10000, 0): a tight compute loop returning an s32
    /// accumulator, sign-extended into the i64 ABI return.
    private let expectedAdd10k: Int64 = -1_847_199_397

    /// Load the precompiled Pulley component artifact from the test bundle.
    private func loadArtifact() throws -> [UInt8] {
        let url = try XCTUnwrap(
            Bundle.module.url(forResource: "compute_pulley", withExtension: "cwasm"),
            "compute_pulley.cwasm not bundled in test resources"
        )
        let data = try Data(contentsOf: url)
        XCTAssertGreaterThan(data.count, 0, "artifact is empty")
        return [UInt8](data)
    }

    /// Full functional + latency benchmark, mirroring examples/bench_ios.rs.
    func testRuntimeLatencyBenchmark() throws {
        print("hellohq-wasm-runtime XCTest bench  (abi v\(hwr_abi_version()))")

        // ── Handshake ────────────────────────────────────────────────────────
        XCTAssertEqual(hwr_self_test(), 1, "self_test failed — runtime did not init")
        print("self_test: OK  (no-JIT runtime links + initialises under iOS)")

        let bytes = try loadArtifact()

        // ── Engine + precompiled instance (the iOS execution path) ───────────
        let engine = hwr_engine_new(1 /* pulley */)
        XCTAssertNotNil(engine, "engine_new returned null")
        defer { hwr_engine_free(engine) }

        let inst: OpaquePointer? = bytes.withUnsafeBufferPointer { buf in
            hwr_instance_new_precompiled(engine, buf.baseAddress, buf.count)
        }
        XCTAssertNotNil(
            inst,
            "deserialize failed — host-precompiled artifact rejected by no-JIT runtime"
        )
        defer { hwr_instance_free(inst) }

        // ── Functional check ─────────────────────────────────────────────────
        let r = hwr_instance_call_add(inst, 10_000, 0)
        XCTAssertNotEqual(r, callError, "call_add errored (HWR_CALL_ERROR)")
        XCTAssertEqual(r, expectedAdd10k, "add(10000,0) returned unexpected value")
        print("functional: precompiled component ran under iOS, add(10000,0)=\(r)")

        // ── Latency ──────────────────────────────────────────────────────────
        print("\nlatency (Pulley, no-JIT):")
        let workloads: [(label: String, n: Int32, iters: Int)] = [
            ("light  (n=10k)",  10_000,    2000),
            ("medium (n=100k)", 100_000,   500),
            ("heavy  (n=1M)",   1_000_000, 100),
        ]
        for w in workloads {
            // warm up
            _ = hwr_instance_call_add(inst, w.n, 0)

            let start = DispatchTime.now()
            var last: Int64 = 0
            for _ in 0..<w.iters {
                last = hwr_instance_call_add(inst, w.n, 0)
            }
            let elapsedNs = DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds
            let msPerCall = Double(elapsedNs) / 1e6 / Double(w.iters)

            XCTAssertNotEqual(last, callError, "call_add errored in loop \(w.label)")
            let pad = w.label.padding(toLength: 16, withPad: " ", startingAt: 0)
            print(String(format: "  %@ %9.4f ms/call", pad, msPerCall))
        }

        print("\nOK")
    }

    /// XCTest `measure {}` variant — reports the light workload as a metric so it
    /// shows up in Xcode's performance results / xcresult bundle.
    func testRuntimeLatencyMeasured() throws {
        XCTAssertEqual(hwr_self_test(), 1)
        let bytes = try loadArtifact()
        let engine = hwr_engine_new(1)
        defer { hwr_engine_free(engine) }
        let inst: OpaquePointer? = bytes.withUnsafeBufferPointer { buf in
            hwr_instance_new_precompiled(engine, buf.baseAddress, buf.count)
        }
        XCTAssertNotNil(inst)
        defer { hwr_instance_free(inst) }

        // warm up
        _ = hwr_instance_call_add(inst, 10_000, 0)

        measure {
            var last: Int64 = 0
            for _ in 0..<2000 {
                last = hwr_instance_call_add(inst, 10_000, 0)
            }
            XCTAssertNotEqual(last, callError)
        }
    }
}
