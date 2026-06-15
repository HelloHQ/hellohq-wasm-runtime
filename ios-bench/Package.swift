// swift-tools-version:5.9
// SPDX-License-Identifier: Apache-2.0
//
// SwiftPM package wrapping the no-JIT iOS static xcframework and an XCTest
// latency harness that mirrors examples/bench_ios.rs.
//
// The binaryTarget points at the xcframework produced by scripts/build-release.sh
// (unzipped) at ../dist/HelloHQWasmRuntime.xcframework. The xcframework carries
// a Clang module map (Headers/module.modulemap), so the C ABI is importable as
// `import HelloHQWasmRuntime`.
import PackageDescription

let package = Package(
    name: "RuntimeBench",
    platforms: [.iOS(.v16)],
    targets: [
        .binaryTarget(
            name: "HelloHQWasmRuntime",
            path: "../dist/HelloHQWasmRuntime.xcframework"
        ),
        .testTarget(
            name: "RuntimeBenchTests",
            dependencies: ["HelloHQWasmRuntime"],
            resources: [.copy("Resources/compute_pulley.cwasm")]
        ),
    ]
)
