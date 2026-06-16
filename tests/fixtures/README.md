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
