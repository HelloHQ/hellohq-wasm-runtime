// SPDX-License-Identifier: Apache-2.0
//
//! Guest component proving the PRODUCTION `plugin` world's defining combination
//! (audit C4): a guest that imports a `hellohq:plugin` capability AND standard
//! `wasi:http` runs under ONE host linker.
//!
//! Built against the `plugin-http-guest` world (../wit/probe.wit): it imports
//! `hellohq:plugin/log` (a sync capability) and `wasi:http/{types,handler}` (the
//! async outbound surface), and exports `run: async func() -> list<u8>`. `run`
//! writes a log line through the `hellohq:plugin/log` capability, then builds a
//! GET request to `example.com/`, calls `handler.handle`, drains the response
//! body, and returns the status (LE u16) + body — so the host test can assert
//! BOTH the capability call (captured log line) AND the gated `wasi:http`
//! round-trip happened on the same store.
//!
//! `log.write` is a SYNC WIT func (lowered blocking); `handler.handle` is async
//! (awaited). Together they exercise the exact mix a real plugin uses now that
//! `world plugin` imports `wasi:http/handler` alongside the typed capabilities.

wit_bindgen::generate!({
    path: "../wit",
    world: "plugin-http-guest",
    // Generate the transitively-used interfaces (hellohq:plugin/log,
    // wasi:http/types, wasi:clocks/types) inline rather than requiring `with`
    // mappings. `handler.handle` (imported) and `run` (exported) are `async func`
    // in the WIT, so wit-bindgen binds them async by default; the sync
    // capability funcs (`log.write`) and the stream-minting `types` methods are
    // lowered blocking.
    generate_all,
});

use hellohq::plugin::log::{write as log_write, Level};
use wasi::http::handler::handle;
use wasi::http::types::{ErrorCode, Fields, Request, Response};

struct Component;

impl Guest for Component {
    async fn run() -> Vec<u8> {
        // (1) hellohq:plugin capability — a sync host call on this store. The
        //     host (CapstoneHarness) captures the line; the test asserts it.
        log_write(Level::Info, "plugin-http-guest: fetching example.com");

        // (2) wasi:http outbound — the async surface on the SAME store.
        let headers = Fields::new();
        let (_req_trailers_tx, req_trailers) =
            wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));
        let (request, _transmit) = Request::new(headers, None, req_trailers, None);
        request.set_authority(Some("example.com")).unwrap();
        request.set_path_with_query(Some("/")).unwrap();

        let response = match handle(request).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let status = response.get_status_code();

        let (_res_tx, res) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body_stream, _resp_trailers) = Response::consume_body(response, res);
        let body = body_stream.collect().await;

        // status (LE u16) + body, so the host test asserts both.
        let mut out = Vec::with_capacity(2 + body.len());
        out.extend_from_slice(&status.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }
}

export!(Component);
