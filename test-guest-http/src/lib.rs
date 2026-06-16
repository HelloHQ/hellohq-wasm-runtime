// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the STAGE 3 wasi:http@0.3-rc end-to-end proof.
//!
//! Built against the `http-guest` world (../wit-wasi): it imports
//! `wasi:http/{types,handler}` and exports `run: func() -> list<u8>`. `run`
//! builds a GET request to `example.com/`, calls `handler.handle`, reads the
//! response status + body stream, and returns the body bytes — so the host test
//! can assert the synthesized echo ("GET example.com/") survives the full
//! wasi:http 0.3 resource + stream + concurrent-handle round-trip.
//!
//! The host binds the four stream-minting methods (`request::new`,
//! `request::consume-body`, `response::new`, `response::consume-body`) as
//! *concurrent* (`async | store`) so they can mint `stream`/`future` values, and
//! `handler.handle` is `async func` in the WIT — so the guest binds those five
//! async and drives them with `wit_bindgen::block_on`. These primitives use the
//! component-model-async intrinsics (no OS threads), so the guest still imports
//! ONLY wasi:http (confirmed via `wasm-tools component wit`).

wit_bindgen::generate!({
    path: "../wit-wasi",
    world: "http-guest",
    // Generate the transitively-used interfaces (wasi:http/types,
    // wasi:clocks/types) inline rather than requiring `with` mappings.
    generate_all,
    // The four stream-minting methods (`request::new`, …) are *sync* WIT funcs;
    // the host binds them concurrent (`async | store`) so it can mint streams,
    // but per wasmtime's docs a guest may still lower a concurrent host import
    // synchronously — Wasmtime blocks the guest while the host future resolves.
    // So the guest binds everything blocking EXCEPT `handler.handle`, which is
    // an `async func` in the WIT (always async). `handle` is awaited below.
    // `handler.handle` (imported) and `run` (exported) are both `async func` in
    // the WIT, so wit-bindgen binds them async by default — no explicit filter
    // needed. The four stream-minting `types` methods are sync WIT funcs lowered
    // synchronously (Wasmtime blocks the guest while the concurrent host fn
    // resolves), so they stay blocking too.
});

use wasi::http::handler::handle;
use wasi::http::types::{ErrorCode, Fields, Request};

struct Component;

impl Guest for Component {
    // `run` is exported async (see the `export:run` async filter) because it
    // awaits the async `handle` import and drains the response body stream.
    async fn run() -> Vec<u8> {
        // Empty request headers.
        let headers = Fields::new();

        // Trailers future for the (absent) request body: a ready `Ok(None)`.
        // `request::new` requires a `future<result<option<trailers>, _>>`.
        let (_req_trailers_tx, req_trailers) =
            wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));

        // Construct a GET request with no body. The transmission future is not
        // awaited for the in-process echo path.
        let (request, _transmit) = Request::new(headers, None, req_trailers, None);
        request.set_authority(Some("example.com")).unwrap();
        request.set_path_with_query(Some("/")).unwrap();

        // Call the host handler (synthesizes the echo response in-process).
        let response = match handle(request).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let status = response.get_status_code();

        // `response::consume_body` takes a `res` error-future; supply a ready
        // `Ok(())`, then drain the body stream to completion.
        let (_res_tx, res) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body_stream, _resp_trailers) =
            wasi::http::types::Response::consume_body(response, res);
        let body = body_stream.collect().await;

        // Prefix the body with the status code (LE u16) so the host test can
        // assert both the status and the echoed summary.
        let mut out = Vec::with_capacity(2 + body.len());
        out.extend_from_slice(&status.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }
}

export!(Component);
