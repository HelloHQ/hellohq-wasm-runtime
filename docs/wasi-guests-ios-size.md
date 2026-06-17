# iOS size cost of `wasmtime-wasi` (the `wasi-guests` WASI host)

Running JS (jco) / Go (TinyGo) guests needs the WASI 0.2 host surface
(`wasmtime-wasi` + `wasmtime-wasi-http`), which pulls heavy deps (tokio,
cap-std, hyper, wiggle). The question: **what does that cost the iOS no-JIT
(Pulley) build**, which targets a `< 8 MB` shipped library?

## TL;DR

`wasmtime-wasi` is **cheap in the shipped binary (~0.7 MB)**, not the ~91 MB the
raw static archive suggests. The static `.a` is not dead-stripped; the final
link (Xcode for iOS, the linker for the desktop cdylib) discards the
unreferenced bulk and keeps only the reachable WASI-host surface. It fits the
iOS budget. **Cranelift**, not `wasmtime-wasi`, is the expensive piece — and it
is deliberately excluded from iOS (the no-JIT Pulley build).

## Measurement

`rustc` pinned per `rust-toolchain.toml`; `--release` (the `opt-level="z"`, `lto`,
`strip` profile). The measurement-only feature `wasi-guests-measure` pulls the
WASI host **without** `compile` (Cranelift), to isolate it from the JIT.

### Raw static archive (`aarch64-apple-ios`, pre-dead-strip — UPPER BOUND)

| Build | `libhellohq_wasm_runtime.a` | Δ vs baseline |
|---|---|---|
| baseline (`--no-default-features`, Pulley) | 59.8 MB | — |
| `+ wasi-guests-measure` (wasmtime-wasi, no JIT) | 151.1 MB | **+91.3 MB** |
| `+ wasi-guests` (wasmtime-wasi + Cranelift) | 218.1 MB | +158.3 MB (Cranelift ≈ +67 MB) |

The `.a` contains every object file — including code the final app never calls.
It overstates the shipped cost and is only useful as an upper bound.

### Dead-stripped cdylib (host `aarch64-apple-darwin`, LTO + strip — REPRESENTATIVE)

The release cdylib is LTO'd and dead-stripped at link, like the final iOS app,
so its delta is the realistic shipped cost:

| Build | `libhellohq_wasm_runtime.dylib` | Δ vs baseline |
|---|---|---|
| baseline (`--no-default-features`) | 1.71 MB | — |
| `+ wasi-guests-measure` (wasmtime-wasi) | 2.38 MB | **+0.67 MB** |

~91 MB of `wasmtime-wasi` object code collapses to **~0.67 MB** once the linker
drops everything unreachable. The realized iOS cost tracks this, not the `.a`.

### Caveat: the cost is only paid once the C ABI reaches it

Today no `hwr_*` C-ABI entrypoint runs a JS/Go guest through `GoGuestState`, so a
shipping iOS app would dead-strip essentially all of `wasmtime-wasi` (Δ → ~0).
The ~0.67 MB above is what survives once a guest-runner entrypoint makes the
WASI-host surface reachable; expect the realized iOS delta in that low-MB range
when JS/Go guest execution is wired to the C ABI — well within the `< 8 MB`
budget. Cranelift stays out of the iOS build regardless (Pulley interprets the
precompiled modules).

## Reproduce

```sh
sz() { stat -f%z "$1"; }   # macOS

# Raw static archive (upper bound):
cargo build --release --target aarch64-apple-ios --no-default-features
cargo build --release --target aarch64-apple-ios --no-default-features --features wasi-guests-measure
cargo build --release --target aarch64-apple-ios --features wasi-guests   # + Cranelift

# Dead-stripped cdylib (representative):
cargo build --release --no-default-features
cargo build --release --no-default-features --features wasi-guests-measure
sz target/release/libhellohq_wasm_runtime.dylib
```
