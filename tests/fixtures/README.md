# Test fixtures

## `workspace_probe_guest.wasm`

A WebAssembly **component** built against the `workspace-probe` world
(`../../wit/probe.wit` + `../../wit/world.wit`) for the P2 Option A
typed-marshaling proof (`tests/workspace_probe.rs`).

It imports ONLY `hellohq:plugin/workspace` (and the `hellohq:plugin/types`
type-only interface it depends on) and exports `read-names: func() ->
list<portfolio-name>`. Its `read-names` calls the imported
`workspace.read-portfolio-names()` and returns the `Ok` list (empty vec on
`Err`). NO wasi imports — the host test linker provides only `workspace`, so
any wasi import would fail instantiation.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/workspace_probe_guest.wasm
# import hellohq:plugin/types@0.1.0;
# import hellohq:plugin/workspace@0.1.0;
# export read-names: func() -> list<portfolio-name>;
```

### Regenerate

Source crate: `../../test-guest/` (an isolated crate — its own `[workspace]`
root — so it never joins the parent crate's `cargo build`/`cargo test`). It
references the parent `wit/` via `path: "../wit"` in the `wit_bindgen::generate!`
macro, so the WIT stays single-source.

Requires the `wasm32-unknown-unknown` target and `wasm-tools` (1.252 used here):

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-tools   # or: brew install wasm-tools

# from repo root:
( cd test-guest && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest/target/wasm32-unknown-unknown/release/workspace_probe_guest.wasm \
  -o tests/fixtures/workspace_probe_guest.wasm
```

No `--adapt` is needed (no wasi adapter) because the guest is no_std with its
own `dlmalloc` global allocator and imports no wasi.

`scripts/regen_probe_guest.sh` runs the two commands above.

## `p3_probe_guest.wasm`

A WebAssembly **component** built against the `p3-probe` world
(`../../wit/probe.wit`) for the P3 host-call round-trip proof.

It imports ONLY `hellohq:plugin/hostcall` and exports `run: func(input:
list<u8>) -> list<u8>`. Its `run` forwards `input` through the imported
`hostcall.call(input)` and returns the result unchanged — so the host test can
assert a `list<u8>` survives the suspend/resume round-trip host -> guest
(import) -> host (export). NO wasi imports — the host test linker provides only
`hostcall`, so any wasi import would fail instantiation.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/p3_probe_guest.wasm
# import hellohq:plugin/hostcall@0.1.0;
# export run: func(input: list<u8>) -> list<u8>;
```

### Regenerate

Source crate: `../../test-guest-p3/` (an isolated crate — its own `[workspace]`
root — so it never joins the parent crate's `cargo build`/`cargo test`). It
references the parent `wit/` via `path: "../wit"` in the
`wit_bindgen::generate!` macro, so the WIT stays single-source.

Same toolchain as above (`wasm32-unknown-unknown` target + `wasm-tools`):

```sh
# from repo root:
( cd test-guest-p3 && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest-p3/target/wasm32-unknown-unknown/release/p3_probe_guest.wasm \
  -o tests/fixtures/p3_probe_guest.wasm
```

`scripts/regen_probe_guest.sh` regenerates all fixtures.

## `http_guest.wasm`

A WebAssembly **component** built against the `http-guest` world
(`../../wit-wasi/world.wit`) for the STAGE 3 `wasi:http@0.3-rc` end-to-end proof
(`tests/http_handle.rs`).

It imports ONLY `wasi:http/types` and `wasi:http/handler` (no clocks, no cli —
the host's `WasiHttpHost::add_to_linker` satisfies both) and exports `run: async
func() -> list<u8>`. Its `run` builds a `GET example.com/` request via
`request::new`, calls `handler.handle`, reads the response status + body stream,
and returns `status (LE u16) ++ body` — so the host test can assert the
host-synthesized echo (`"GET example.com/"`, status 200) survives the full
wasi:http 0.3 resource + stream + concurrent-handle round-trip.

`run` is `async func` (and `handler.handle` is `async func` in the WIT) because
the guest awaits the handler import and drains the body stream — both yield, so
the export task must be async. The four `types` stream-minting methods
(`request::new`, `consume-body`, …) are sync WIT funcs lowered synchronously;
Wasmtime blocks the guest while the host's concurrent impl resolves.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/http_guest.wasm
# import wasi:http/types@0.3.0-rc-2026-01-06;
# import wasi:http/handler@0.3.0-rc-2026-01-06;
# export run: async func() -> list<u8>;
```

### Regenerate

Source crate: `../../test-guest-http/` (an isolated crate — its own
`[workspace]` root). It references the parent `wit-wasi/` via `path:
"../wit-wasi"` in the `wit_bindgen::generate!` macro (with `generate_all` to pull
in the transitively-used `wasi:http/types` + `wasi:clocks/types` interfaces), so
the WIT stays single-source.

Same toolchain as above (`wasm32-unknown-unknown` target + `wasm-tools`):

```sh
# from repo root:
( cd test-guest-http && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest-http/target/wasm32-unknown-unknown/release/http_guest.wasm \
  -o tests/fixtures/http_guest.wasm
```

`scripts/regen_probe_guest.sh` regenerates all four fixtures.

## `http_guest_post.wasm`

A WebAssembly **component** built against the `http-guest-post` world
(`../../wit-wasi/world.wit`) for the streaming-REQUEST-body (POST) proof
(`tests/http_handle.rs`).

Same import/export surface as `http_guest.wasm` (imports ONLY `wasi:http/types`
+ `wasi:http/handler`; exports `run: async func() -> list<u8>`), but `run`
builds a **POST** request to `example.com/submit` carrying a body `stream<u8>`.
It mints the body stream via `wit_stream::new::<u8>()`, hands the reader half to
`request::new(headers, Some(reader), …)`, and `wit_bindgen::spawn_local`s a task
that writes the bytes `"req-body-123"` to the writer half and then drops it
(closing the stream). The write runs concurrently with `handle().await` because
the host only drains the request body while servicing `handle`. The host reads
that stream host-side (via a `StreamConsumer`) and emits it as OUT frames after
the request head — so the host test can assert the body bytes reached the
servicer.

The `async-spawn` feature of `wit-bindgen` is enabled (in the guest crate's
`Cargo.toml`) for `spawn_local`.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/http_guest_post.wasm
# import wasi:http/types@0.3.0-rc-2026-01-06;
# import wasi:http/handler@0.3.0-rc-2026-01-06;
# export run: async func() -> list<u8>;
```

### Regenerate

Source crate: `../../test-guest-http-post/` (an isolated crate — its own
`[workspace]` root). References the parent `wit-wasi/` via `path: "../wit-wasi"`.

```sh
# from repo root:
( cd test-guest-http-post && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest-http-post/target/wasm32-unknown-unknown/release/http_guest_post.wasm \
  -o tests/fixtures/http_guest_post.wasm
```

`scripts/regen_probe_guest.sh` regenerates all six fixtures.

## `inference_guest.wasm`

A WebAssembly **component** built against the `inference-guest` world
(`../../wit/world.wit`) for the ai:inference streaming end-to-end proof
(`tests/inference_complete.rs`).

It imports ONLY `hellohq:plugin/inference` and `hellohq:plugin/types` (no wasi —
the host's `InferenceHost::add_to_linker` satisfies both) and exports `run: async
func() -> list<u8>`. Its `run` calls
`inference.complete([{role:"user", content:"hello"}], {max-tokens:64})`, drains
the returned `stream<string>` concatenating each token-delta string, and returns
the concatenation as bytes — so the host test can assert streamed token deltas
("Hel" + "lo " + "world" == "Hello world") round-trip through the full inference
resource + stream + concurrent-complete path.

`run` is `async func` because it drains the returned `stream<string>` (which
yields). `complete` is a sync WIT func the host binds `store` (so it can mint the
`stream<string>`); a guest may lower a concurrent host import synchronously, so
`complete` is called blocking.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/inference_guest.wasm
# import hellohq:plugin/types@0.1.0;
# import hellohq:plugin/inference@0.1.0;
# export run: async func() -> list<u8>;
```

### Regenerate

Source crate: `../../test-guest-inference/` (an isolated crate — its own
`[workspace]` root). It references the parent `wit/` via `path: "../wit"` in the
`wit_bindgen::generate!` macro (with `generate_all` to pull in the
transitively-used `hellohq:plugin/types` interface), so the WIT stays
single-source.

Same toolchain as above (`wasm32-unknown-unknown` target + `wasm-tools`):

```sh
# from repo root:
( cd test-guest-inference && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest-inference/target/wasm32-unknown-unknown/release/inference_guest.wasm \
  -o tests/fixtures/inference_guest.wasm
```

`scripts/regen_probe_guest.sh` regenerates all fixtures.

## `storage_events_guest.wasm`

A WebAssembly **component** built against the `storage-events-guest` world
(`../../wit/probe.wit` + `../../wit/world.wit`) for the SYNCHRONOUS storage +
events end-to-end proof (`tests/storage_events.rs`) — the last two doc-53
interfaces.

It imports ONLY `hellohq:plugin/storage`, `hellohq:plugin/events`, and the
type-only `hellohq:plugin/types` (no wasi — the host's
`StorageEventsHost::add_to_linker` satisfies all three) and exports `run: func()
-> list<u8>` (a PLAIN, non-async func — `storage`/`events` are sync, non-streaming
host imports). Its `run` runs a storage round-trip (`set("greeting","hello")`,
`set("count",…)`, `get("greeting")`, `list-keys` -> 2, `delete("count")`,
`list-keys` -> 1) plus `events.emit({kind:"ready", payload:"ok"})`, and returns a
compact summary `"<get-bytes>|<count1>|<count2>"` (e.g. `hello|2|1`). On any
storage `Err` (the gate-denied case) it short-circuits and returns the marker
`"ERR:<code>"` (e.g. `ERR:permission-denied`).

Note: `wasm-tools` tree-shakes the unused `storage.clear` func out of the
imported interface (the guest never calls it), so the imported `storage` shows
only `get`/`set`/`delete`/`list-keys`. The full interface (incl. `clear`) lives
in `wit/world.wit` and is implemented + unit-tested host-side.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/storage_events_guest.wasm
# import hellohq:plugin/types@0.1.0;
# import hellohq:plugin/storage@0.1.0;
# import hellohq:plugin/events@0.1.0;
# export run: func() -> list<u8>;
```

### Regenerate

Source crate: `../../test-guest-storage-events/` (an isolated crate — its own
`[workspace]` root). no_std with its own `dlmalloc` global allocator so it pulls
in NO wasi. It references the parent `wit/` via `path: "../wit"` in the
`wit_bindgen::generate!` macro (with `generate_all` to pull in the
transitively-used `hellohq:plugin/types` interface), so the WIT stays
single-source.

Same toolchain as above (`wasm32-unknown-unknown` target + `wasm-tools`):

```sh
# from repo root:
( cd test-guest-storage-events && cargo build --release --target wasm32-unknown-unknown )
wasm-tools component new \
  test-guest-storage-events/target/wasm32-unknown-unknown/release/storage_events_guest.wasm \
  -o tests/fixtures/storage_events_guest.wasm
```

`scripts/regen_probe_guest.sh` regenerates all fixtures.

## `capstone_plugin.component.wasm`

The end-to-end **capstone** fixture (`tests/capstone.rs`): a real SDK-authored
plugin component, NOT a hand-rolled probe guest. It is the `hellohq-plugin-sdk`
quickstart example (`../../../plugin-sdk/examples/component-quickstart`), built
with the SDK against the canonical `hellohq:plugin@0.1.0` WIT.

On `guest.run` it logs a banner + progress lines (`hq::log`), reads the workspace
portfolio names (`hq::workspace`, gated), stores `greeting`="hello" and reads it
back (`hq::storage`), emits `quickstart-ran`/"ok" (`hq::events`), and returns the
compact summary `"<n-portfolios>|<roundtrip-ok>"` (e.g. `2|1`). On a gate denial
of the workspace read the plugin maps the `api-error` into its `run` `Err(String)`.

`wasm-tools component new` tree-shakes the build down to the four sync capability
imports the plugin actually calls (`workspace.read-portfolio-names`,
`storage.get`/`set`, `events.emit`, `log.write`) plus the type-only `types`, and
exports the canonical `guest` interface. NO wasi, NO inference (inference needs
an async `run` and is proven separately — `inference_guest.wasm`). The host
harness world (`capstone-host` in `../../wit/probe.wit`) imports exactly this set
so `CapstoneHarness::add_to_linker` satisfies the component's imports.

Verify imports/exports:

```sh
wasm-tools component wit tests/fixtures/capstone_plugin.component.wasm
# import hellohq:plugin/types@0.1.0;
# import hellohq:plugin/workspace@0.1.0;
# import hellohq:plugin/storage@0.1.0;
# import hellohq:plugin/events@0.1.0;
# import hellohq:plugin/log@0.1.0;
# export hellohq:plugin/guest@0.1.0;
```

### Regenerate

Source: the SDK example crate
`../../../plugin-sdk/examples/component-quickstart/` (its own `[workspace]`
root). It depends on `hellohq-plugin-sdk` (`../../sdks/rust`) and builds against
the SDK's vendored canonical WIT (SSOT: `plugin-protocol/wit/`). The example's
own `build.sh` does the two build steps; the regen step just copies the result
into the fixture dir:

```sh
# from repo root:
( cd ../plugin-sdk/examples/component-quickstart && bash build.sh )
cp ../plugin-sdk/examples/component-quickstart/component_quickstart.component.wasm \
  tests/fixtures/capstone_plugin.component.wasm
```

`scripts/regen_probe_guest.sh` regenerates all fixtures (incl. this one).

## `go_guest.component.wasm`

The **Go SDK quickstart** built with **TinyGo** (`-target=wasip2`) — the proof a
richer SDK guest, one that embeds a language runtime, runs on the "support all
WASI generations at once" linker (`tests/go_guest.rs`). It is the
`plugin-sdk/examples/component-quickstart-go` example.

Unlike the no_std Rust capstone (which tree-shakes to ONLY `hellohq:plugin/*`),
the TinyGo runtime ALSO imports the `wasi:0.2` surface. So the host must satisfy
BOTH the four capabilities AND the wasi imports, or instantiation fails. The
runtime does that on ONE `component::Linker<GoGuestState>` carrying every WASI
generation:

- **WASI 0.2 runtime** (`wasmtime-wasi@45`, `add_to_linker_async`) — LOCKED DOWN
  (no preopens/env/network/stdio), so the wasi imports resolve but grant nothing.
- **WASI 0.2 `wasi:http`** (`wasmtime-wasi-http@45`,
  `add_only_http_to_linker_async`) — GATED (`GatedHttpHooks::send_request` runs
  the origin allowlist + SSRF / private-IP block + https-only before any real
  send; an empty allowlist — the `GoGuestState` default here — denies all and
  returns `HttpRequestDenied`). Present for the JS/jco baseline; outbound refused.
- **`hellohq:plugin/*`** (`CapstoneHarness::add_to_linker_get`) — the gated
  capabilities, reused verbatim from the Rust capstone host.

On `guest.run` the Go plugin does the same flow the capstone proves: log lines, a
gated workspace read (2 canned portfolios), a storage round-trip
(`greeting`="hello"), an `events.emit("quickstart-ran","ok")`, and returns the
compact summary `"2|1"` when granted (or `Err` carrying the gate message when
denied).

Verify imports/exports (note: the TinyGo `wasip2` guest imports the `wasi:0.2`
surface but NOT `wasi:http` — that is the JS/jco case the linker also supports):

```sh
wasm-tools component wit tests/fixtures/go_guest.component.wasm
# import hellohq:plugin/{types,workspace,storage,events,log}@0.1.0;
# import wasi:cli/{environment,stdin,stdout,stderr}@0.2.0;
# import wasi:clocks/{monotonic-clock,wall-clock}@0.2.0;
# import wasi:filesystem/{types,preopens}@0.2.0;
# import wasi:io/{error,streams}@0.2.0;
# import wasi:random/random@0.2.0;
# export hellohq:plugin/guest@0.1.0;
```

> The guest imports `wasi:*@0.2.0`; `wasmtime-wasi@45` provides `wasi:*@0.2.x`.
> Wasmtime resolves the import against the compatible `0.2.x` it provides
> (semver-compat within `0.2`), so instantiation succeeds — confirmed by the
> passing `tests/go_guest.rs`.

### Regenerate

Source: the SDK example `../../../plugin-sdk/examples/component-quickstart-go/`.
It depends on the Go SDK (`../../sdks/go`) and builds against the SDK's vendored
WIT. Requires **TinyGo >= 0.41** and `wasm-tools`. TinyGo's `wasip2` target emits
a Component Model component directly — no `wasm-tools component new` step:

```sh
# from repo root:
( cd ../plugin-sdk/examples/component-quickstart-go && bash build.sh )
cp ../plugin-sdk/examples/component-quickstart-go/component_quickstart_go.component.wasm \
  tests/fixtures/go_guest.component.wasm
```

`scripts/regen_probe_guest.sh` regenerates all fixtures (incl. this one).
