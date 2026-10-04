# hellohq-wasm-runtime

> Async-first WebAssembly plugin runtime for HelloHQ — Wasmtime (Component Model
> + WASI 0.3 async) behind a C ABI for `dart:ffi`, with the Pulley interpreter
> for no-JIT iOS.

## Why

HelloHQ's Tier-2 plugin runtime used to drive Wasmtime through the generic C
API with a **synchronous** custom host ABI. That can't express **async host
calls**, so `ai:inference` was a stub and async was deferred.

This crate exists to deliver those async capabilities: a plugin should be able to
**stream AI inference** and run **concurrent HTTP**, built on the WebAssembly
**Component Model + WASI 0.3 async** (`async func` / `stream<T>` / `future<T>`).
It is a thin Rust shim that owns the Wasmtime `Store` + async executor and
exposes a purpose-built **C ABI** consumed by the Flutter app via `dart:ffi`
(same integration pattern as [`HelloHQ/mldsa-verify`](https://github.com/HelloHQ/mldsa-verify):
a native crate behind a C ABI, with SHA-pinned + attested release artifacts).

## Status

Consumed by the HelloHQ app's Tier-2 plugin runtime, which pins a tagged
release (SHA-256 + build-provenance attestation, see
`.github/workflows/release.yml`). The spike
gates are passed: Component Model + WASI 0.3 async runs on Pulley (no-JIT iOS),
the bespoke C ABI is in use, and the async-over-FFI bridge drives:

- the typed `hellohq:plugin/*` capabilities (`hwr_p3_start_plugin*`, the
  entrypoint the app binds for a plugin),
- streaming `wasi:http@0.3` (`hwr_p3s_start_http*`) and `ai:inference`
  (`hwr_p3s_start_inference*`) over the P3 v2 framed transport,
- JS (jco) / Go (TinyGo) guests that also import `wasi:*@0.2`
  (`wasi-guests` feature; not yet exposed through the C ABI).

The C ABI is versioned (`HWR_ABI_VERSION`, `hwr_abi_version()`) and declared in
`include/hellohq_wasm_runtime.h`. Design background: `hellohqworkspace`
`docs/plugin/18-wasi-0.3-async-runtime-migration.md` and
`19-wasi-0.3-wit-world-design.md`.

## Security boundary

Capability **gating stays in the app** wherever the app services the call: the
typed capabilities, `ai:inference` and `wasi:http@0.3` are routed to the Dart
side, which applies HelloHQ's permission gate and its network policy
(`PluginNetworkService`: origin allowlist, SSRF/private-IP block, the
`PluginRequestPolicy` header allowlist). This crate provides the *mechanism*
there, not the policy.

The one exception is `wasi:http@0.2` for JS/Go guests (`wasi-guests`), which is
sent from Rust and never reaches Dart. It carries Rust ports of both app
policies: `src/fetch_gate.rs` (https-only, origin allowlist, SSRF block) and
`src/request_policy.rs` (request-header allowlist, `Set-Cookie` stripped, no
redirects, no URLs in errors). The header lists are kept in step with the app
through a shared case table, `tests/fixtures/plugin_request_policy_cases.txt`.
`wasi:sockets` is intentionally **not** exposed.

## Layout

```
src/lib.rs                C ABI entrypoints (engine/instance, P3 round-trip, P3 v2 streaming)
src/wasi_http.rs          hand-built wasi:http@0.3 host; frames requests to the app
src/wasi_http_frames.rs   the kind-tagged OUT-frame format of that transport
src/plugin_host*.rs       typed hellohq:plugin/* hosts bridged to the app (typed-hosts)
src/wasi_guests.rs        wasi:*@0.2 surface for JS/Go guests, gated wasi:http@0.2
src/fetch_gate.rs         origin/SSRF gate for that path
src/request_policy.rs     header policy for that path (port of the app's)
include/                  the C header
wit/, wit-wasi*/          the hellohq world and the vendored WASI packages
tests/                    C-ABI and guest integration tests (fixtures in tests/fixtures)
```

## Build

```bash
cargo build                                        # host (Cranelift JIT)
cargo test --all-targets                           # ABI + runtime tests
cargo test --features "wasi-http typed-hosts" --all-targets
cargo test --features "wasi-guests typed-hosts" --all-targets
```

`.github/workflows/ci.yml` lists every feature combination CI runs, including
the no-JIT (`--no-default-features`) build and the iOS/Android cross-builds.
Release artifacts (desktop libraries, iOS xcframework via Pulley, Android
jniLibs) and their provenance attestations come from
`.github/workflows/release.yml` (`scripts/build-release.sh`).

## License

Apache-2.0. See [LICENSE](LICENSE).
