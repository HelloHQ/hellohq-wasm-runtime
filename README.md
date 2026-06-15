# hellohq-wasm-runtime

> Async-first WebAssembly plugin runtime for HelloHQ — Wasmtime (Component Model
> + WASI 0.3 async) behind a C ABI for `dart:ffi`, with the Pulley interpreter
> for no-JIT iOS.

## Why

HelloHQ's Tier-2 plugin runtime currently drives Wasmtime through the generic C
API with a **synchronous** custom host ABI. That can't express **async host
calls** — which is why `ai:inference` is only a stub today and async was deferred.

This crate exists to deliver those async capabilities: a plugin should be able to
**stream AI inference** and run **concurrent HTTP**, built on the WebAssembly
**Component Model + WASI 0.3 async** (`async func` / `stream<T>` / `future<T>`).
It is a thin Rust shim that owns the Wasmtime `Store` + async executor and
exposes a purpose-built **C ABI** consumed by the Flutter app via `dart:ffi`
(same integration pattern as [`HelloHQ/mldsa-verify`](https://github.com/HelloHQ/mldsa-verify):
a native crate behind a C ABI, with SHA-pinned + attested release artifacts).

## Status — SPIKE-STAGE ⚠️

Only the C-ABI surface and a runtime self-test are wired (`hwr_abi_version`,
`hwr_self_test`). The component instantiation, WASI worlds (`wasi:http`, …), and
the **async-over-FFI bridge** are the deliverables of the **P0 spike** and are
not implemented yet. Do not depend on this crate's shape until the spike gates
pass:

1. **Pulley on iOS** — runs Component-Model + WASI under no-JIT/W^X, within the
   latency (≤150 ms/invocation) and size (< 8 MB iOS lib) budgets.
2. **Shim vs C API** — confirm the bespoke C ABI is required (this crate).
3. **Async-over-FFI + `ai:inference`** — the executor/round-trip bridge.

Plan & gates: `hellohq/docs/51_wasi-0.3-async-runtime-migration.md` (decision)
and `…/docs/52_wasi-0.3-spike-plan.md` (spike).

## Security boundary

Capability **gating stays in the app**, not here: the host implementations of
`wasi:http` (origin allowlist + SSRF/private-IP blocking), storage, and
`ai:inference` are wired on the Dart side through HelloHQ's permission gate.
This crate provides the *mechanism* (run components, surface host imports), not
the *policy*. `wasi:sockets` is intentionally **not** exposed.

## Layout

```
src/lib.rs   C ABI entrypoints (engine/instance/call/async bridge — being filled in)
wit/         the hellohq component world (WASI + custom interfaces) [stub]
```

## Build

```bash
cargo build            # host (Cranelith JIT)
cargo test             # ABI + runtime smoke tests
```

Cross-platform release artifacts (desktop dylibs, iOS xcframework via Pulley,
Android jniLibs) + provenance attestation come with the P1 release pipeline.

## License

Apache-2.0. See [LICENSE](LICENSE).
