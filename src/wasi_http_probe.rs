// SPDX-License-Identifier: Apache-2.0
//! Option B feasibility anchor: `bindgen!` generates host bindings for
//! wasi:http@0.3.0-rc (resources + stream<u8> bodies + future<trailers> + the
//! async `handler.handle`) and COMPILES on Wasmtime 45 — proven 2026-06-16.
//! Gated behind `wasi-http-probe`; not part of normal builds. The full host
//! impl (handler::Host + http::types::Host + the resource traits, routed through
//! the P3 round-trip to Dart's gated fetch) builds on this generated surface.
#![cfg(feature = "wasi-http-probe")]

wasmtime::component::bindgen!({
    path: "wit-wasi",
    world: "http-probe",
    imports: { default: async },
});
