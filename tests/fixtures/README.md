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
